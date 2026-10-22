//! Barra superior (logo, pills de sessão, nova orquestração, configurações)
//! e barra de status.

use egui::{Align, Align2, Color32, FontId, Layout, RichText, Sense, Stroke, Ui, pos2, vec2};

use super::canvas::CanvasState;
use super::dialogs::Dialogs;
use super::widgets;
use super::{Ctx, UiState};
use crate::model::plural;
use crate::sysinfo::RamMeter;
use crate::theme;

fn logo(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
    let p = ui.painter();
    let c = rect.center();
    let a = c + vec2(-6.0, 0.0);
    let targets = [c + vec2(6.0, -6.0), c + vec2(6.0, 0.0), c + vec2(6.0, 6.0)];
    for t in targets {
        p.line_segment([a, t], Stroke::new(1.4, theme::TEXT_DIM));
        p.circle_filled(t, 2.4, theme::RUNNING);
    }
    p.circle_filled(a, 3.4, theme::WAITING);
    ui.label(
        RichText::new("orquestra")
            .size(17.0)
            .strong()
            .color(theme::TEXT),
    );
}

pub fn top_bar(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    canvas: &mut CanvasState,
    dialogs: &mut Dialogs,
) {
    ui.horizontal(|ui| {
        logo(ui);
        ui.add_space(14.0);
        if widgets::pill(ui, st.focus.is_none(), "Todas", None).clicked() {
            st.focus = None;
            canvas.request_fit();
        }
        for session in cx.model.sessions() {
            let (summary, kind) = cx.model.session_summary(session.id);
            let selected = st.focus == Some(session.id);
            let label = session
                .config
                .name
                .split(" · ")
                .next()
                .unwrap_or(&session.config.name);
            let resp = widgets::pill(ui, selected, label, Some((&summary, kind)))
                .on_hover_text(&session.config.name);
            if resp.clicked() {
                st.focus = if selected { None } else { Some(session.id) };
                canvas.request_fit();
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::gear_button(ui).clicked() {
                dialogs.open_settings(cx.config);
            }
            ui.add_space(4.0);
            if ui
                .add(widgets::accent_button("+ Nova orquestração"))
                .on_hover_text("Abre um rascunho no canvas (Ctrl+Shift+N)")
                .clicked()
            {
                canvas.open_draft(cx.config);
            }
        });
    });
}

pub fn status_bar(ui: &mut Ui, cx: &Ctx, ram: &mut RamMeter) {
    let m = cx.model;
    let c = m.counts(None);
    let sessions = m.sessions().len() as u32;
    ui.horizontal(|ui| {
        let small = |text: String, color: Color32| RichText::new(text).size(12.0).color(color);
        ui.label(small(
            plural(sessions, "orquestração", "orquestrações"),
            theme::TEXT_DIM,
        ));
        ui.label(small("·".to_owned(), theme::TEXT_DIM));
        ui.label(small(
            plural(c.workers, "agente", "agentes"),
            theme::TEXT_DIM,
        ));
        if c.waiting > 0 {
            ui.label(small("·".to_owned(), theme::TEXT_DIM));
            ui.label(small(
                format!("{} aguardando você", c.waiting),
                theme::WAITING,
            ));
        }
        if c.approval > 0 {
            ui.label(small("·".to_owned(), theme::TEXT_DIM));
            ui.label(small(
                format!("{} aguardando aprovação", c.approval),
                theme::APPROVAL,
            ));
        }
        if c.failed > 0 {
            ui.label(small("·".to_owned(), theme::TEXT_DIM));
            let verb = if c.failed == 1 { "falhou" } else { "falharam" };
            ui.label(small(format!("{} {verb}", c.failed), theme::FAILED));
        }
        ui.separator();
        ui.label(small(
            plural(m.browsers_open(), "navegador", "navegadores"),
            theme::TEXT_DIM,
        ));
        ui.separator();
        ui.label(small(
            format!("RAM do app {}", ram.label()),
            theme::TEXT_DIM,
        ));
        ui.separator();
        let (text, color) = if !m.hub_alive {
            ("hub encerrado", theme::FAILED)
        } else if m.lagged {
            ("hub ok (eventos perdidos)", theme::WAITING)
        } else {
            ("hub ok", theme::DONE)
        };
        let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.5, color);
        ui.label(small(text.to_owned(), theme::TEXT_DIM));
        if let Some((_, err)) = m.errors.back() {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let max = ui.available_width() - 10.0;
                let painter = ui.painter().clone();
                let text = widgets::elide(
                    &painter,
                    &format!("último erro: {err}"),
                    &FontId::proportional(12.0),
                    max,
                );
                let (rect, _) = ui.allocate_exact_size(vec2(max.max(0.0), 16.0), Sense::hover());
                painter.text(
                    pos2(rect.right(), rect.center().y),
                    Align2::RIGHT_CENTER,
                    text,
                    FontId::proportional(12.0),
                    theme::FAILED.gamma_multiply(0.8),
                );
            });
        }
    });
}
