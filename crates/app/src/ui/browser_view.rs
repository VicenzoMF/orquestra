//! Aba "Navegador": barra (voltar/avançar/recarregar/URL), a textura do
//! último quadro e o input sobre ela, mapeado por `BrowserFrame::to_viewport`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use egui::{
    Align2, Color32, CornerRadius, Event, EventFilter, FontId, Id, Key, MouseWheelUnit,
    PointerButton, Pos2, Rect, RichText, Sense, TextEdit, TextureHandle, TextureOptions, Ui, pos2,
    vec2,
};
use orquestra_core::{
    AgentId, BrowserInput, Command, MouseAction, MouseButton, MouseInput, Navigation, Viewport,
    WheelInput,
};

use super::Ctx;
use crate::browser::{self, dom_key, fit_rect, key_pair, to_css};
use crate::model::{AgentView, BrowserState};
use crate::theme;

#[derive(Default)]
pub struct BrowserView {
    texture: Option<(AgentId, TextureHandle, u64)>,
    url: String,
    url_for: Option<(AgentId, String)>,
    viewports: HashMap<AgentId, Viewport>,
    last_move: Option<Pos2>,
    last_press: Option<(Instant, Pos2, u32)>,
}

impl BrowserView {
    pub fn show(&mut self, ui: &mut Ui, cx: &Ctx, agent: &AgentView) {
        let id = agent.id;
        let ready = matches!(agent.browser, BrowserState::Ready { .. });
        self.toolbar(ui, cx, agent, ready);
        ui.add_space(4.0);
        match &agent.browser {
            BrowserState::Ready { .. } => {}
            BrowserState::Launching => {
                ui.label(RichText::new("Iniciando o navegador…").color(theme::TEXT_DIM));
                return;
            }
            BrowserState::Failed(reason) => {
                ui.label(
                    RichText::new(format!("O navegador falhou: {reason}")).color(theme::FAILED),
                );
                if ui.button("Tentar de novo").clicked() {
                    cx.send(Command::OpenBrowser { id, url: None });
                }
                return;
            }
            BrowserState::None | BrowserState::Closed => {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("Este agente não tem navegador aberto.")
                            .color(theme::TEXT_DIM),
                    );
                    ui.add_space(6.0);
                    if ui
                        .add(super::widgets::accent_button("Abrir navegador"))
                        .clicked()
                    {
                        cx.send(Command::OpenBrowser { id, url: None });
                    }
                });
                return;
            }
        }

        // Textura: só atualiza quando chega quadro novo.
        if let Some((seq, image)) = cx.shared.frames.take_image(id) {
            match &mut self.texture {
                Some((tid, tex, tseq)) if *tid == id => {
                    tex.set(image, TextureOptions::LINEAR);
                    *tseq = seq;
                }
                _ => {
                    let tex = ui.ctx().load_texture(
                        format!("browser-{}", id.short()),
                        image,
                        TextureOptions::LINEAR,
                    );
                    self.texture = Some((id, tex, seq));
                }
            }
        }
        if self.texture.as_ref().is_some_and(|(tid, _, _)| *tid != id) {
            self.texture = None;
        }

        let area = ui.available_rect_before_wrap();
        let response = ui.interact(area, Id::unique(("browser", id)), Sense::click_and_drag());
        ui.advance_cursor_after_rect(area);
        let painter = ui.painter_at(area);
        painter.rect_filled(area, CornerRadius::same(6), theme::TERM_BG);

        // O viewport acompanha o painel (sem mandar no meio de um arraste).
        let want = Viewport {
            width: area.width().max(100.0) as u32,
            height: area.height().max(100.0) as u32,
            device_scale_factor: 1.0,
        };
        let dragging = ui.input(|i| i.pointer.any_down());
        let last = self.viewports.get(&id);
        let changed = last
            .is_none_or(|v| v.width.abs_diff(want.width) > 4 || v.height.abs_diff(want.height) > 4);
        if changed && !dragging {
            self.viewports.insert(id, want);
            cx.send(Command::BrowserResize { id, viewport: want });
        }

        let frame = cx.shared.frames.frame(id);
        let Some(((_, tex, _), frame)) = self.texture.as_ref().zip(frame) else {
            painter.text(
                area.center(),
                Align2::CENTER_CENTER,
                "Aguardando o primeiro quadro…",
                FontId::proportional(13.0),
                theme::TEXT_DIM,
            );
            return;
        };
        let image_rect = fit_rect(area, tex.size());
        painter.image(
            tex.id(),
            image_rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );

        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        if response.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    response.id,
                    EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });
        }
        let send = |input: BrowserInput| cx.send(Command::BrowserInput { id, input });
        let (events, modifiers) = ui.input(|i| (i.events.clone(), i.modifiers));
        let mods = browser::modifiers(modifiers);
        let hovered = response.hovered();
        for event in &events {
            match event {
                Event::PointerMoved(pos) if hovered || response.dragged() => {
                    if self.last_move == Some(*pos) {
                        continue;
                    }
                    self.last_move = Some(*pos);
                    if let Some((x, y)) = to_css(&frame, image_rect, *pos) {
                        let button = if response.dragged_by(PointerButton::Primary) {
                            MouseButton::Left
                        } else {
                            MouseButton::None
                        };
                        send(BrowserInput::Mouse(MouseInput {
                            action: MouseAction::Move,
                            x,
                            y,
                            button,
                            click_count: 0,
                            modifiers: mods,
                        }));
                    }
                }
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } => {
                    let Some((x, y)) = to_css(&frame, image_rect, *pos) else {
                        continue;
                    };
                    let button = match button {
                        PointerButton::Primary => MouseButton::Left,
                        PointerButton::Secondary => MouseButton::Right,
                        PointerButton::Middle => MouseButton::Middle,
                        PointerButton::Extra1 => MouseButton::Back,
                        PointerButton::Extra2 => MouseButton::Forward,
                    };
                    let click_count = if *pressed {
                        let count = match self.last_press {
                            Some((at, p, n))
                                if at.elapsed() < Duration::from_millis(400)
                                    && p.distance(*pos) < 5.0 =>
                            {
                                n + 1
                            }
                            _ => 1,
                        };
                        self.last_press = Some((Instant::now(), *pos, count));
                        count
                    } else {
                        self.last_press.map_or(1, |(_, _, n)| n)
                    };
                    send(BrowserInput::Mouse(MouseInput {
                        action: if *pressed {
                            MouseAction::Down
                        } else {
                            MouseAction::Up
                        },
                        x,
                        y,
                        button,
                        click_count,
                        modifiers: mods,
                    }));
                }
                Event::MouseWheel { unit, delta, .. } if hovered => {
                    let pos = ui
                        .input(|i| i.pointer.hover_pos())
                        .unwrap_or(image_rect.center());
                    let Some((x, y)) = to_css(&frame, image_rect, pos) else {
                        continue;
                    };
                    let step = match unit {
                        MouseWheelUnit::Point => 1.0,
                        MouseWheelUnit::Line => 40.0,
                        MouseWheelUnit::Page => f64::from(area.height()),
                    };
                    // No egui, positivo = conteúdo desce; no CDP, positivo = rola para baixo.
                    send(BrowserInput::Wheel(WheelInput {
                        x,
                        y,
                        delta_x: -f64::from(delta.x) * step,
                        delta_y: -f64::from(delta.y) * step,
                        modifiers: mods,
                    }));
                }
                Event::Text(text) if response.has_focus() => {
                    send(BrowserInput::Text(text.clone()));
                }
                Event::Paste(text) if response.has_focus() => {
                    send(BrowserInput::Text(text.clone()));
                }
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } if response.has_focus() => {
                    if let Some((k, code)) = dom_key(*key, *modifiers) {
                        let text = (*key == Key::Enter).then(|| "\r".to_owned());
                        for input in key_pair(k, code, text, mods) {
                            send(BrowserInput::Key(input));
                        }
                    }
                }
                Event::Copy | Event::Cut if response.has_focus() => {
                    let letter = if matches!(event, Event::Copy) {
                        "c"
                    } else {
                        "x"
                    };
                    for input in key_pair(
                        letter.to_owned(),
                        format!("Key{}", letter.to_uppercase()),
                        None,
                        mods,
                    ) {
                        send(BrowserInput::Key(input));
                    }
                }
                _ => {}
            }
        }
        if response.has_focus() {
            painter.rect_stroke(
                image_rect.expand(1.0),
                CornerRadius::same(2),
                egui::Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.6)),
                egui::StrokeKind::Outside,
            );
        }
    }

    fn toolbar(&mut self, ui: &mut Ui, cx: &Ctx, agent: &AgentView, ready: bool) {
        let id = agent.id;
        let current = agent
            .page
            .as_ref()
            .map(|(u, _)| u.clone())
            .unwrap_or_default();
        // Atualiza o campo quando a página navega (e o campo não está em edição).
        let field_id = Id::unique(("browser-url", id));
        let editing = ui.memory(|m| m.has_focus(field_id));
        if !editing && self.url_for.as_ref() != Some(&(id, current.clone())) {
            self.url = current.clone();
            self.url_for = Some((id, current));
        }
        ui.add_enabled_ui(ready, |ui| {
            ui.horizontal(|ui| {
                let nav = |n: Navigation| cx.send(Command::BrowserNavigate { id, nav: n });
                if ui.button("←").on_hover_text("Voltar").clicked() {
                    nav(Navigation::Back);
                }
                if ui.button("→").on_hover_text("Avançar").clicked() {
                    nav(Navigation::Forward);
                }
                if ui.button("↻").on_hover_text("Recarregar").clicked() {
                    nav(Navigation::Reload);
                }
                let close = ui.button("×").on_hover_text("Fechar o navegador");
                let edit = ui.add(
                    TextEdit::singleline(&mut self.url)
                        .id(field_id)
                        .desired_width(ui.available_width())
                        .hint_text("URL"),
                );
                if edit.lost_focus()
                    && ui.input(|i| i.key_pressed(Key::Enter))
                    && !self.url.trim().is_empty()
                {
                    nav(Navigation::Url(self.url.trim().to_owned()));
                }
                if close.clicked() {
                    cx.send(Command::CloseBrowser { id });
                }
            });
        });
        if let Some((_, title)) = &agent.page
            && !title.is_empty()
        {
            ui.label(RichText::new(title).size(12.0).color(theme::TEXT_DIM));
        }
        let _ = vec2;
    }
}
