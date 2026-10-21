//! Widget do terminal: pinta a grade do `Term` (cores, negrito, inverso,
//! cursor, seleção) com cache por linha e traduz teclado/mouse em bytes.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{SelectionRange, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Color32, Context, CornerRadius, Event, EventFilter, FontId, Galley, Id, Key, Modifiers,
    MouseWheelUnit, Painter, PointerButton, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui,
    Vec2, ViewportCommand, pos2, vec2,
};
use orquestra_core::TermSize;

use super::keys::{self, MouseButton};
use super::{AgentTerm, palette};
use crate::theme;

/// Fonte monoespaçada e tamanho da célula.
#[derive(Clone, Debug, PartialEq)]
pub struct TermFont {
    pub font: FontId,
    pub cell: Vec2,
}

impl TermFont {
    pub fn new(ctx: &Context, size: f32) -> Self {
        let font = FontId::monospace(size);
        let cell = ctx.fonts_mut(|f| vec2(f.glyph_width(&font, 'M'), f.row_height(&font)));
        Self { font, cell }
    }

    /// Colunas e linhas que cabem em `area`.
    pub fn grid_size(&self, area: Vec2) -> TermSize {
        let cols = (area.x / self.cell.x).floor().clamp(2.0, 1000.0) as u16;
        let rows = (area.y / self.cell.y).floor().clamp(1.0, 500.0) as u16;
        TermSize {
            cols,
            rows,
            cell_width: self.cell.x.round() as u16,
            cell_height: self.cell.y.round() as u16,
        }
    }
}

/// Estado do widget por agente (entre frames).
#[derive(Default)]
pub struct ViewState {
    scroll_px: f32,
    /// Alt+tecla já mandou `ESC x`; o `Event::Text` seguinte é ignorado.
    suppress_text: bool,
    cache: Vec<RowCache>,
    cache_cell: Vec2,
}

#[derive(Default)]
struct RowCache {
    hash: u64,
    bgs: Vec<(Rect, Color32)>,
    texts: Vec<(Pos2, Arc<Galley>)>,
}

/// O que o widget produziu neste frame.
#[derive(Default)]
pub struct TermOutput {
    /// Bytes para o PTY (`Command::SendInput`).
    pub input: Vec<u8>,
    /// Novo tamanho (`Command::Resize`).
    pub resized: Option<TermSize>,
}

const PAD: f32 = 6.0;

/// Mostra o terminal ocupando todo o espaço disponível de `ui`.
pub fn show(
    ui: &mut Ui,
    id: Id,
    term: &mut AgentTerm,
    font: &TermFont,
    state: &mut ViewState,
) -> (Response, TermOutput) {
    let mut out = TermOutput::default();
    let rect = ui.available_rect_before_wrap();
    let response = ui.interact(rect, id, Sense::click_and_drag());
    ui.advance_cursor_after_rect(rect);

    let inner = rect.shrink(PAD);
    let size = font.grid_size(inner.size());
    if term.resize(size) {
        out.resized = Some(size);
    }

    if response.clicked() || response.drag_started() {
        response.request_focus();
    }
    let focused = response.has_focus();
    if focused {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            );
        });
    }

    handle_mouse(ui, &response, inner, term, font, state, &mut out);
    if focused {
        handle_keyboard(ui, term, state, &mut out);
    }
    if !out.input.is_empty() {
        term.scroll_to_bottom();
    }

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(6), theme::TERM_BG);
    paint(&painter, inner.min, term, font, state, focused);
    if term.display_offset() > 0 {
        let label = format!("↑ {} linhas", term.display_offset());
        painter.text(
            rect.right_top() + vec2(-10.0, 8.0),
            egui::Align2::RIGHT_TOP,
            label,
            FontId::proportional(11.0),
            theme::TEXT_DIM,
        );
    }
    (response, out)
}

fn cell_at(pos: Pos2, origin: Pos2, font: &TermFont, term: &AgentTerm) -> (usize, usize, Side) {
    let rel = pos - origin;
    let cols = term.term().columns();
    let rows = term.term().screen_lines();
    let x = rel.x.max(0.0);
    let col = ((x / font.cell.x) as usize).min(cols - 1);
    let row = ((rel.y.max(0.0) / font.cell.y) as usize).min(rows - 1);
    let side = if x % font.cell.x > font.cell.x / 2.0 {
        Side::Right
    } else {
        Side::Left
    };
    (col, row, side)
}

fn handle_mouse(
    ui: &Ui,
    response: &Response,
    origin_rect: Rect,
    term: &mut AgentTerm,
    font: &TermFont,
    state: &mut ViewState,
    out: &mut TermOutput,
) {
    let origin = origin_rect.min;
    let mode = term.mode();
    let (events, modifiers) = ui.input(|i| (i.events.clone(), i.modifiers));
    let mouse_mode = mode.intersects(TermMode::MOUSE_MODE) && !modifiers.shift;

    if response.hovered() {
        for event in &events {
            let Event::MouseWheel { unit, delta, .. } = event else {
                continue;
            };
            let lines = match unit {
                MouseWheelUnit::Line => delta.y.round() as i32,
                MouseWheelUnit::Point => {
                    state.scroll_px += delta.y;
                    let lines = (state.scroll_px / font.cell.y).trunc();
                    state.scroll_px -= lines * font.cell.y;
                    lines as i32
                }
                MouseWheelUnit::Page => (delta.y * term.term().screen_lines() as f32) as i32,
            };
            if lines == 0 {
                continue;
            }
            if mouse_mode {
                let pos = ui.input(|i| i.pointer.hover_pos()).unwrap_or(origin);
                let (col, row, _) = cell_at(pos, origin, font, term);
                let button = if lines > 0 {
                    MouseButton::WheelUp
                } else {
                    MouseButton::WheelDown
                };
                for _ in 0..lines.unsigned_abs().min(10) {
                    if let Some(b) = keys::mouse_report(button, col, row, true, modifiers, mode) {
                        out.input.extend(b);
                    }
                }
            } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
                let key = if lines > 0 {
                    Key::ArrowUp
                } else {
                    Key::ArrowDown
                };
                for _ in 0..lines.unsigned_abs().min(20) {
                    out.input
                        .extend(keys::encode_key(key, Modifiers::NONE, mode).unwrap_or_default());
                }
            } else {
                term.scroll(lines);
            }
        }
    }

    if mouse_mode {
        for event in &events {
            if let Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers,
            } = event
                && origin_rect.expand(PAD).contains(*pos)
            {
                let button = match button {
                    PointerButton::Primary => MouseButton::Left,
                    PointerButton::Middle => MouseButton::Middle,
                    PointerButton::Secondary => MouseButton::Right,
                    _ => continue,
                };
                let (col, row, _) = cell_at(*pos, origin, font, term);
                if let Some(b) = keys::mouse_report(button, col, row, *pressed, *modifiers, mode) {
                    out.input.extend(b);
                }
            }
        }
        if response.dragged_by(PointerButton::Primary)
            && mode.contains(TermMode::MOUSE_DRAG)
            && let Some(pos) = response.interact_pointer_pos()
        {
            let (col, row, _) = cell_at(pos, origin, font, term);
            if let Some(b) =
                keys::mouse_report(MouseButton::LeftDrag, col, row, true, modifiers, mode)
            {
                out.input.extend(b);
            }
        }
        return;
    }

    // Seleção local.
    let pointer = response.interact_pointer_pos();
    if response.drag_started_by(PointerButton::Primary) {
        let start = ui.input(|i| i.pointer.press_origin()).or(pointer);
        if let Some(pos) = start {
            let (col, row, side) = cell_at(pos, origin, font, term);
            let point = term.viewport_point(col, row);
            term.start_selection(SelectionType::Simple, point, side);
        }
    }
    if response.dragged_by(PointerButton::Primary)
        && let Some(pos) = pointer
    {
        // Arrastar para fora rola o histórico.
        if pos.y < origin_rect.top() {
            term.scroll(1);
        } else if pos.y > origin_rect.bottom() {
            term.scroll(-1);
        }
        let (col, row, side) = cell_at(pos, origin, font, term);
        let point = term.viewport_point(col, row);
        term.update_selection(point, side);
    }
    if let Some(pos) = pointer {
        let ty = if response.triple_clicked() {
            Some(SelectionType::Lines)
        } else if response.double_clicked() {
            Some(SelectionType::Semantic)
        } else {
            None
        };
        if let Some(ty) = ty {
            let (col, row, side) = cell_at(pos, origin, font, term);
            let point = term.viewport_point(col, row);
            term.start_selection(ty, point, side);
            term.update_selection(point, side);
        } else if response.clicked() {
            term.clear_selection();
        }
    }
}

fn handle_keyboard(ui: &Ui, term: &mut AgentTerm, state: &mut ViewState, out: &mut TermOutput) {
    let mode = term.mode();
    let events = ui.input(|i| i.events.clone());
    for event in events {
        match event {
            Event::Text(text) => {
                if std::mem::take(&mut state.suppress_text) {
                    continue;
                }
                out.input.extend(text.bytes());
            }
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                state.suppress_text = false;
                if modifiers.ctrl && modifiers.shift && key == Key::C {
                    if let Some(text) = term.selection_text() {
                        ui.ctx().copy_text(text);
                    }
                    continue;
                }
                if modifiers.ctrl && modifiers.shift && key == Key::V {
                    ui.ctx().send_viewport_cmd(ViewportCommand::RequestPaste);
                    continue;
                }
                if let Some(bytes) = keys::encode_key(key, modifiers, mode) {
                    out.input.extend(bytes);
                    state.suppress_text = modifiers.alt && !modifiers.ctrl;
                }
            }
            // O egui-winit transforma Ctrl+C/X/V nestes eventos.
            Event::Copy => out.input.push(0x03),
            Event::Cut => out.input.push(0x18),
            Event::Paste(text) => out.input.extend(keys::encode_paste(&text, mode)),
            Event::Ime(egui::ImeEvent::Commit(text)) => out.input.extend(text.bytes()),
            _ => {}
        }
    }
}

fn color_key(color: Color) -> u32 {
    match color {
        Color::Named(n) => n as u32,
        Color::Spec(c) => {
            (1 << 24) | (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b)
        }
        Color::Indexed(i) => (2 << 24) | u32::from(i),
    }
}

fn hash_cell(h: &mut DefaultHasher, cell: &Cell) {
    (cell.c as u32).hash(h);
    color_key(cell.fg).hash(h);
    color_key(cell.bg).hash(h);
    cell.flags.bits().hash(h);
    if let Some(zw) = cell.zerowidth() {
        zw.hash(h);
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Style {
    fg: Color32,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

struct Run {
    col: usize,
    end: usize,
    style: Style,
    text: String,
}

fn flush_run(
    run: Option<Run>,
    row: usize,
    font: &TermFont,
    painter: &Painter,
    cache: &mut RowCache,
) {
    let Some(run) = run else { return };
    let text = run.text.trim_end();
    if text.is_empty() && !run.style.underline && !run.style.strike {
        return;
    }
    let text = if text.is_empty() {
        run.text.as_str()
    } else {
        text
    };
    let galley = layout(painter, text, run.style, font);
    let pos = pos2(run.col as f32 * font.cell.x, row as f32 * font.cell.y);
    push_text(cache, pos, galley, run.style.bold);
}

fn push_text(cache: &mut RowCache, pos: Pos2, galley: Arc<Galley>, bold: bool) {
    if bold {
        // Negrito falso: a fonte monoespaçada padrão só tem o peso regular.
        cache.texts.push((pos + vec2(0.6, 0.0), galley.clone()));
    }
    cache.texts.push((pos, galley));
}

fn layout(painter: &Painter, text: &str, style: Style, font: &TermFont) -> Arc<Galley> {
    let line = |on: bool| {
        if on {
            Stroke::new(1.0, style.fg)
        } else {
            Stroke::NONE
        }
    };
    let format = TextFormat {
        font_id: font.font.clone(),
        color: style.fg,
        italics: style.italic,
        underline: line(style.underline),
        strikethrough: line(style.strike),
        ..TextFormat::default()
    };
    painter.layout_job(LayoutJob::single_section(text.to_owned(), format))
}

#[allow(clippy::too_many_arguments)]
fn build_row(
    painter: &Painter,
    term: &AgentTerm,
    line: Line,
    row: usize,
    selection: Option<SelectionRange>,
    default_bg: Color32,
    font: &TermFont,
    cache: &mut RowCache,
) {
    let t = term.term();
    let colors = t.colors();
    let cols = t.columns();
    let grid_row = &t.grid()[line];
    cache.bgs.clear();
    cache.texts.clear();
    let mut run: Option<Run> = None;
    for col in 0..cols {
        let cell = &grid_row[Column(col)];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let width = if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        let selected = selection.is_some_and(|s| s.contains(Point::new(line, Column(col))));
        let mut fg = if cell.flags.intersects(Flags::DIM) {
            palette::resolve_dim(colors, cell.fg)
        } else {
            palette::resolve(colors, cell.fg)
        };
        let mut bg = palette::resolve(colors, cell.bg);
        if cell.flags.contains(Flags::INVERSE) != selected {
            std::mem::swap(&mut fg, &mut bg);
            if selected && bg == default_bg {
                bg = theme::ACCENT;
            }
        }
        if cell.flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        if bg != default_bg {
            let x0 = col as f32 * font.cell.x;
            let x1 = (col + width) as f32 * font.cell.x;
            let y0 = row as f32 * font.cell.y;
            match cache.bgs.last_mut() {
                Some((r, c)) if *c == bg && (r.right() - x0).abs() < 0.5 => r.max.x = x1,
                _ => cache.bgs.push((
                    Rect::from_min_max(pos2(x0, y0), pos2(x1, y0 + font.cell.y)),
                    bg,
                )),
            }
        }
        let style = Style {
            fg,
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
            strike: cell.flags.contains(Flags::STRIKEOUT),
        };
        let c = cell.c;
        let wide_or_unicode = width == 2 || !(' '..='~').contains(&c) || cell.zerowidth().is_some();
        if wide_or_unicode {
            flush_run(run.take(), row, font, painter, cache);
            let mut text = String::from(c);
            text.extend(cell.zerowidth().into_iter().flatten());
            let galley = layout(painter, &text, style, font);
            // Glifos de outras fontes podem ter largura diferente: centraliza.
            let span = width as f32 * font.cell.x;
            let x = col as f32 * font.cell.x + (span - galley.size().x) / 2.0;
            push_text(cache, pos2(x, row as f32 * font.cell.y), galley, style.bold);
            continue;
        }
        match run.as_mut() {
            Some(r) if r.style == style && r.end == col => {
                r.text.push(c);
                r.end = col + 1;
            }
            _ => {
                flush_run(run.take(), row, font, painter, cache);
                if c != ' ' || style.underline || style.strike {
                    run = Some(Run {
                        col,
                        end: col + 1,
                        style,
                        text: String::from(c),
                    });
                }
            }
        }
    }
    flush_run(run, row, font, painter, cache);
}

fn paint(
    painter: &Painter,
    origin: Pos2,
    term: &AgentTerm,
    font: &TermFont,
    state: &mut ViewState,
    focused: bool,
) {
    let t = term.term();
    let content = t.renderable_content();
    let selection = content.selection;
    let cursor = content.cursor;
    let offset = content.display_offset;
    let colors = content.colors;
    let default_bg = palette::resolve(colors, Color::Named(NamedColor::Background));
    let rows = t.screen_lines();
    let ppp = painter.pixels_per_point();
    let origin = pos2(
        (origin.x * ppp).round() / ppp,
        (origin.y * ppp).round() / ppp,
    );

    if state.cache_cell != font.cell {
        state.cache.clear();
        state.cache_cell = font.cell;
    }
    state.cache.resize_with(rows, RowCache::default);

    for row in 0..rows {
        let line = Line(row as i32 - offset as i32);
        let mut h = DefaultHasher::new();
        for cell in &t.grid()[line] {
            hash_cell(&mut h, cell);
        }
        if let Some(sel) = selection
            && sel.start.line <= line
            && sel.end.line >= line
        {
            for p in [sel.start, sel.end] {
                (p.line.0, p.column.0).hash(&mut h);
            }
            sel.is_block.hash(&mut h);
        }
        t.columns().hash(&mut h);
        let hash = h.finish();
        let cache = &mut state.cache[row];
        if cache.hash != hash {
            build_row(painter, term, line, row, selection, default_bg, font, cache);
            cache.hash = hash;
        }
        for (rect, color) in &cache.bgs {
            painter.rect_filled(rect.translate(origin.to_vec2()), CornerRadius::ZERO, *color);
        }
        for (pos, galley) in &cache.texts {
            painter.galley(origin + pos.to_vec2(), galley.clone(), theme::TEXT);
        }
    }

    // Cursor.
    let row = cursor.point.line.0 + offset as i32;
    if cursor.shape != CursorShape::Hidden && row >= 0 && (row as usize) < rows {
        let col = cursor.point.column.0;
        let min = origin + vec2(col as f32 * font.cell.x, row as f32 * font.cell.y);
        let rect = Rect::from_min_size(min, font.cell);
        let color = palette::resolve(colors, Color::Named(NamedColor::Cursor));
        match (cursor.shape, focused) {
            (CursorShape::Beam, _) => {
                painter.rect_filled(
                    Rect::from_min_size(min, vec2(2.0, font.cell.y)),
                    CornerRadius::ZERO,
                    color,
                );
            }
            (CursorShape::Underline, _) => {
                painter.rect_filled(
                    Rect::from_min_size(min + vec2(0.0, font.cell.y - 2.0), vec2(font.cell.x, 2.0)),
                    CornerRadius::ZERO,
                    color,
                );
            }
            (CursorShape::Block, true) => {
                painter.rect_filled(rect, CornerRadius::ZERO, color);
                let cell = &t.grid()[cursor.point];
                if cell.c != ' ' {
                    let style = Style {
                        fg: default_bg,
                        bold: false,
                        italic: false,
                        underline: false,
                        strike: false,
                    };
                    let galley = layout(painter, &cell.c.to_string(), style, font);
                    painter.galley(min, galley, default_bg);
                }
            }
            _ => {
                painter.rect_stroke(
                    rect,
                    CornerRadius::ZERO,
                    Stroke::new(1.0, color),
                    StrokeKind::Inside,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use egui::{RawInput, vec2};

    use super::*;

    fn frame(
        ctx: &Context,
        events: Vec<Event>,
        term: &mut AgentTerm,
        state: &mut ViewState,
        focus: bool,
    ) -> TermOutput {
        let font = TermFont {
            font: FontId::monospace(13.0),
            cell: vec2(8.0, 16.0),
        };
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0))),
            events,
            ..RawInput::default()
        };
        let mut result = None;
        let mut out = ctx.run_ui(input, |ui| {
            let (response, output) = show(ui, Id::unique("term-test"), term, &font, state);
            if focus {
                response.request_focus();
            }
            result = Some(output);
        });
        out.textures_delta.clear();
        result.unwrap()
    }

    fn key(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn focused_terminal_turns_keys_into_pty_bytes() {
        let ctx = Context::default();
        let mut term = AgentTerm::new(TermSize::default(), 100);
        let mut state = ViewState::default();

        let first = frame(&ctx, vec![], &mut term, &mut state, true);
        // 400×200 menos a margem, em células de 8×16.
        assert_eq!(first.resized.map(|s| (s.cols, s.rows)), Some((48, 11)));

        let ctrl = Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::NONE
        };
        let alt = Modifiers {
            alt: true,
            ..Modifiers::NONE
        };
        let events = vec![
            Event::Text("ls".to_owned()),
            key(Key::Enter, Modifiers::NONE),
            key(Key::ArrowUp, Modifiers::NONE),
            Event::Copy,
            key(Key::D, ctrl),
            key(Key::B, alt),
            Event::Text("b".to_owned()),
            Event::Paste("x\ny".to_owned()),
        ];
        let out = frame(&ctx, events, &mut term, &mut state, false);
        assert_eq!(out.input, b"ls\r\x1b[A\x03\x04\x1bbx\ry");
        assert!(out.resized.is_none());
    }

    #[test]
    fn unfocused_terminal_ignores_keys_and_paints_content() {
        let ctx = Context::default();
        let mut term = AgentTerm::new(TermSize::default(), 100);
        term.on_output(
            0,
            b"\x1b[1;31mvermelho\x1b[0m \x1b[7minverso\x1b[0m \xe2\x97\x8f",
        );
        let mut state = ViewState::default();
        let out = frame(
            &ctx,
            vec![Event::Text("x".to_owned())],
            &mut term,
            &mut state,
            false,
        );
        assert!(out.input.is_empty());
        // A primeira linha virou galleys (texto) e fundos (inverso).
        assert!(!state.cache[0].texts.is_empty());
        assert!(!state.cache[0].bgs.is_empty());
    }
}
