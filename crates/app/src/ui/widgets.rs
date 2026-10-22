//! Peças pequenas reaproveitadas pelas telas.

use egui::{
    Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Response, RichText, Sense, Stroke,
    StrokeKind, Ui, Vec2, pos2, vec2,
};
use orquestra_core::AgentKind;

use crate::model::StatusKind;
use crate::theme;

pub fn kind_label(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::ClaudeCode => "Claude Code",
        AgentKind::Codex => "Codex",
        AgentKind::Shell => "Shell",
        AgentKind::Custom => "Personalizado",
    }
}

/// Ícone de status ocupando um quadrado de `size`.
pub fn status_icon(ui: &mut Ui, kind: StatusKind, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    theme::paint_status_icon(ui.painter(), rect.center(), size * 0.32, kind);
    response
}

/// Corta `text` com "…" para caber em `max_width`.
pub fn elide(painter: &Painter, text: &str, font: &FontId, max_width: f32) -> String {
    let width = |s: &str| {
        painter
            .layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE)
            .size()
            .x
    };
    if max_width <= 0.0 {
        return String::new();
    }
    if width(text) <= max_width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&candidate) <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>() + "…"
}

/// Texto com elipse, pintado direto.
pub fn paint_elided(
    painter: &Painter,
    pos: Pos2,
    align: Align2,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> Rect {
    let text = elide(painter, text, &font, max_width);
    painter.text(pos, align, text, font, color)
}

/// Pill de sessão: rótulo e, opcionalmente, um resumo com a cor do estado.
pub fn pill(
    ui: &mut Ui,
    selected: bool,
    label: &str,
    summary: Option<(&str, Option<StatusKind>)>,
) -> Response {
    let font = FontId::proportional(13.0);
    let small = FontId::proportional(12.0);
    let painter = ui.painter().clone();
    let label_g = painter.layout_no_wrap(label.to_owned(), font, theme::TEXT);
    let summary_g = summary.map(|(text, kind)| {
        let color = kind.map_or(theme::TEXT_DIM, theme::status_color);
        (painter.layout_no_wrap(text.to_owned(), small, color), kind)
    });
    let pad = vec2(12.0, 6.0);
    let mut width = label_g.size().x + pad.x * 2.0;
    if let Some((g, kind)) = &summary_g {
        width += 10.0 + g.size().x + if kind.is_some() { 14.0 } else { 0.0 };
    }
    let size = vec2(width, label_g.size().y + pad.y * 2.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let fill = if selected {
        Color32::from_rgb(0x2a, 0x33, 0x40)
    } else if response.hovered() {
        Color32::from_rgb(0x25, 0x2a, 0x32)
    } else {
        theme::CARD
    };
    let stroke = if selected {
        Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.7))
    } else {
        Stroke::new(1.0, theme::BORDER)
    };
    painter.rect(
        rect,
        CornerRadius::same(255),
        fill,
        stroke,
        StrokeKind::Inside,
    );
    let mut x = rect.left() + pad.x;
    let cy = rect.center().y;
    painter.galley(
        pos2(x, cy - label_g.size().y / 2.0),
        label_g.clone(),
        theme::TEXT,
    );
    x += label_g.size().x + 10.0;
    if let Some((g, kind)) = summary_g {
        if let Some(kind) = kind {
            theme::paint_status_icon(&painter, pos2(x + 4.0, cy), 4.0, kind);
            x += 14.0;
        }
        painter.galley(pos2(x, cy - g.size().y / 2.0), g, theme::TEXT_DIM);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Botão de engrenagem desenhado (a fonte padrão não tem ⚙).
pub fn gear_button(ui: &mut Ui) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(30.0, 28.0), Sense::click());
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(
            rect,
            CornerRadius::same(6),
            Color32::from_rgb(0x25, 0x2a, 0x32),
        );
    }
    let c = rect.center();
    let color = if response.hovered() {
        theme::TEXT
    } else {
        theme::TEXT_DIM
    };
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::TAU / 8.0;
        let dir = vec2(a.cos(), a.sin());
        painter.line_segment([c + dir * 5.0, c + dir * 8.0], Stroke::new(2.4, color));
    }
    painter.circle_stroke(c, 5.5, Stroke::new(2.0, color));
    response.on_hover_text("Configurações")
}

/// Botão com destaque (ação principal).
pub fn accent_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_owned()).color(Color32::from_rgb(0x0d, 0x1b, 0x2a)))
        .fill(theme::ACCENT)
        .corner_radius(CornerRadius::same(6))
}

/// Botão pequeno colorido (aprovar/recusar).
pub fn tinted_button(text: &str, color: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_owned()).size(12.5).color(color))
        .fill(color.gamma_multiply(0.12))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.5)))
        .corner_radius(CornerRadius::same(5))
}

/// Retângulo com borda tracejada (agente na fila).
pub fn dashed_rect(painter: &Painter, rect: Rect, color: Color32) {
    let pts = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
        rect.left_top(),
    ];
    painter.extend(egui::Shape::dashed_line(
        &pts,
        Stroke::new(1.2, color),
        6.0,
        4.0,
    ));
}
