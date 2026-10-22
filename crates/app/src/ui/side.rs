//! Painel do agente selecionado: cabeçalho, terminal ou navegador.

use egui::{Align, Id, Layout, RichText, Ui};
use orquestra_core::{AgentStatus, Command};

use super::dialogs::{Confirm, Dialogs};
use super::widgets::{self, kind_label};
use super::{Ctx, Resources, SideTab, UiState};
use crate::feed::lock;
use crate::model::{AgentView, BrowserState, StatusKind};
use crate::term::view;
use crate::theme;

pub fn show(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    res: &mut Resources,
    agent: &AgentView,
    dialogs: &mut Dialogs,
) {
    let kind = agent.kind();
    ui.horizontal(|ui| {
        widgets::status_icon(ui, kind, 16.0);
        let title = if agent.is_orchestrator() {
            format!(
                "orquestrador · {}",
                cx.model
                    .session(agent.config.session)
                    .map_or("", |s| s.config.name.as_str())
            )
        } else {
            agent.config.name.clone()
        };
        ui.label(RichText::new(title).size(15.0).strong().color(theme::TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("×").on_hover_text("Fechar (Esc)").clicked() {
                st.selected = None;
            }
            if ui.button("Remover…").clicked() {
                dialogs.confirm = Some(Confirm::remove(agent.id));
            }
            let alive = agent.status.is_alive();
            if ui
                .add_enabled(alive, egui::Button::new("Kill"))
                .on_hover_text("Encerra o processo; worktree e navegador ficam")
                .clicked()
            {
                cx.send(Command::Kill { id: agent.id });
            }
        });
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new(kind_label(agent.config.kind)).color(theme::TEXT_DIM));
        ui.label(RichText::new("·").color(theme::TEXT_DIM));
        ui.label(RichText::new(agent.status_text()).color(theme::status_color(kind)));
        if let Some(parent) = agent.config.parent.and_then(|p| cx.model.agent(p)) {
            ui.label(RichText::new("·").color(theme::TEXT_DIM));
            let who = if parent.is_orchestrator() {
                "orquestrador".to_owned()
            } else {
                parent.config.name.clone()
            };
            ui.label(RichText::new(format!("criado por {who}")).color(theme::TEXT_DIM));
        } else if !agent.is_orchestrator() {
            ui.label(RichText::new("· avulso").color(theme::TEXT_DIM));
        }
    });
    if let Some(info) = &agent.info {
        let (branch, path) = match &info.worktree {
            Some(wt) => (Some(wt.branch.clone()), wt.path.display().to_string()),
            None => (None, info.workdir.display().to_string()),
        };
        ui.horizontal_wrapped(|ui| {
            if let Some(branch) = branch {
                ui.label(
                    RichText::new(format!("branch {branch}"))
                        .monospace()
                        .size(11.5)
                        .color(theme::TEXT_DIM),
                );
            }
            ui.label(
                RichText::new(path)
                    .monospace()
                    .size(11.5)
                    .color(theme::TEXT_DIM),
            );
        });
    }
    if let AgentStatus::Failed { reason } = &agent.status {
        ui.label(RichText::new(format!("Falhou: {reason}")).color(theme::FAILED));
    }
    if agent.kind() == StatusKind::Approval {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("O orquestrador quer criar este agente.").color(theme::APPROVAL),
            );
            if ui
                .add(widgets::tinted_button("Aprovar", theme::DONE))
                .clicked()
            {
                cx.send(Command::ApproveSpawn { id: agent.id });
            }
            if ui
                .add(widgets::tinted_button("Recusar", theme::FAILED))
                .clicked()
            {
                cx.send(Command::RejectSpawn { id: agent.id });
            }
        });
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui
            .selectable_label(st.tab == SideTab::Terminal, "Terminal")
            .clicked()
        {
            st.tab = SideTab::Terminal;
            st.focus_terminal = true;
        }
        let browser_label = match agent.browser.port() {
            Some(port) => format!("Navegador :{port}"),
            None => "Navegador".to_owned(),
        };
        if ui
            .selectable_label(st.tab == SideTab::Browser, browser_label)
            .on_hover_text("Ctrl+Shift+B")
            .clicked()
        {
            st.tab = SideTab::Browser;
        }
    });
    ui.separator();
    match st.tab {
        SideTab::Terminal => terminal(ui, cx, st, res, agent),
        SideTab::Browser => {
            if matches!(agent.browser, BrowserState::Ready { .. }) {
                st.panel_browser = Some(agent.id);
            }
            res.browser.show(ui, cx, agent);
        }
    }
}

fn terminal(ui: &mut Ui, cx: &Ctx, st: &mut UiState, res: &mut Resources, agent: &AgentView) {
    let Some(term) = cx.shared.term(agent.id) else {
        ui.label(RichText::new("Sem terminal.").color(theme::TEXT_DIM));
        return;
    };
    if matches!(agent.status, AgentStatus::Queued { .. }) {
        ui.label(
            RichText::new("Na fila: o terminal aparece quando o agente subir.")
                .color(theme::TEXT_DIM),
        );
    }
    let Some(font) = res.term_font.clone() else {
        return;
    };
    let id = Id::unique(("terminal", agent.id));
    let view_state = res.term_views.entry(agent.id).or_default();
    let (response, out) = {
        let mut t = lock(&term);
        let r = view::show(ui, id, &mut t, &font, view_state);
        if let Some(text) = t.take_clipboard() {
            ui.ctx().copy_text(text);
        }
        r
    };
    if std::mem::take(&mut st.focus_terminal) {
        response.request_focus();
    }
    if let Some(size) = out.resized {
        st.last_term_size = Some(size);
        if agent.status.is_alive() && !matches!(agent.status, AgentStatus::Queued { .. }) {
            cx.send(Command::Resize { id: agent.id, size });
        }
    }
    if st.last_term_size.is_none() {
        st.last_term_size = Some(lock(&term).size());
    }
    if !out.input.is_empty() && agent.status.is_alive() {
        cx.send(Command::SendInput {
            id: agent.id,
            data: out.input.into(),
        });
    }
}
