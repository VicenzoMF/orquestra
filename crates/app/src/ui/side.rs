//! Painel lateral: com um nó selecionado, terminal ou navegador do agente;
//! sem seleção, "Precisa de você", "Orquestrações" e "Recursos".

use egui::{
    Align, Color32, CornerRadius, Frame, Id, Layout, Margin, RichText, ScrollArea, Stroke, Ui,
};
use orquestra_core::{AgentStatus, Command};

use super::canvas::CanvasState;
use super::dialogs::{Confirm, Dialogs};
use super::widgets::{self, kind_label};
use super::{Ctx, Resources, SideTab, UiState};
use crate::feed::lock;
use crate::model::{AgentView, AttentionKind, BrowserState, StatusKind, plural, status_label};
use crate::term::view;
use crate::theme;

pub fn show(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    res: &mut Resources,
    canvas: &mut CanvasState,
    dialogs: &mut Dialogs,
) {
    match st.selected.and_then(|id| cx.model.agent(id)) {
        Some(agent) => agent_panel(ui, cx, st, res, agent, dialogs),
        None => overview(ui, cx, st, res, canvas, dialogs),
    }
}

fn agent_panel(
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
        ui.label(RichText::new(status_label(&agent.status)).color(theme::status_color(kind)));
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

fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    Frame::new()
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn overview(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    res: &mut Resources,
    canvas: &mut CanvasState,
    dialogs: &mut Dialogs,
) {
    ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        let attention = cx.model.attention();
        widgets::section_title(ui, "Precisa de você", Some(attention.len()));
        if attention.is_empty() {
            ui.label(RichText::new("Nada pendente.").color(theme::TEXT_DIM));
        }
        for item in attention {
            let (Some(agent), Some(session)) =
                (cx.model.agent(item.agent), cx.model.session(item.session))
            else {
                continue;
            };
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    widgets::status_icon(ui, agent.kind(), 14.0);
                    let name = if agent.is_orchestrator() {
                        "orquestrador"
                    } else {
                        agent.config.name.as_str()
                    };
                    ui.label(RichText::new(name).strong().color(theme::TEXT));
                    ui.label(
                        RichText::new(&session.config.name)
                            .size(12.0)
                            .color(theme::TEXT_DIM),
                    );
                });
                let preview = cx.shared.preview(agent.id);
                let detail = match item.kind {
                    // Numa pergunta, as linhas de cima dizem o que está em jogo
                    // (o comando, o arquivo); senão basta a última.
                    AttentionKind::Input => {
                        let n = if preview.choice.is_some() { 3 } else { 1 };
                        let lines = &preview.lines[preview.lines.len().saturating_sub(n)..];
                        if lines.is_empty() {
                            "Aguardando sua resposta".to_owned()
                        } else {
                            lines.join("\n")
                        }
                    }
                    AttentionKind::Approval => format!(
                        "Quer criar este agente: {}",
                        agent.config.initial_prompt.as_deref().unwrap_or("")
                    ),
                    AttentionKind::Failure => match &agent.status {
                        AgentStatus::Failed { reason } => reason.clone(),
                        _ => String::new(),
                    },
                };
                let color = match item.kind {
                    AttentionKind::Input => Color32::from_rgb(0xc9, 0xcd, 0xd4),
                    AttentionKind::Approval => theme::APPROVAL,
                    AttentionKind::Failure => theme::FAILED,
                };
                ui.label(RichText::new(detail).monospace().size(11.5).color(color));
                ui.horizontal(|ui| match item.kind {
                    AttentionKind::Input => {
                        // Sim/Não só quando a tela pergunta; senão, o terminal.
                        if let Some(choice) = &preview.choice {
                            if ui.add(widgets::tinted_button("Sim", theme::DONE)).clicked() {
                                cx.send(Command::SendInput {
                                    id: agent.id,
                                    data: choice.yes.clone(),
                                });
                            }
                            if ui
                                .add(widgets::tinted_button("Não", theme::FAILED))
                                .clicked()
                            {
                                cx.send(Command::SendInput {
                                    id: agent.id,
                                    data: choice.no.clone(),
                                });
                            }
                        }
                        if ui.button("Abrir terminal").clicked() {
                            st.select(agent.id);
                        }
                    }
                    AttentionKind::Approval => {
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
                    }
                    AttentionKind::Failure => {
                        if ui.button("Abrir terminal").clicked() {
                            st.select(agent.id);
                        }
                        if ui.button("Remover…").clicked() {
                            dialogs.confirm = Some(Confirm::remove(agent.id));
                        }
                    }
                });
            });
            ui.add_space(4.0);
        }

        ui.add_space(8.0);
        let sessions = cx.model.sessions();
        widgets::section_title(ui, "Orquestrações", Some(sessions.len()));
        if sessions.is_empty() {
            ui.label(
                RichText::new(
                    "Nenhuma. Clique com o botão direito no canvas para iniciar um orquestrador.",
                )
                .color(theme::TEXT_DIM),
            );
        }
        for session in sessions {
            let c = cx.model.counts(Some(session.id));
            let (summary, kind) = cx.model.session_summary(session.id);
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&session.config.name)
                            .strong()
                            .color(theme::TEXT),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(plural(c.workers, "agente", "agentes"))
                            .color(theme::TEXT_DIM),
                    );
                    ui.label(RichText::new("·").color(theme::TEXT_DIM));
                    ui.label(
                        RichText::new(summary)
                            .color(kind.map_or(theme::TEXT_DIM, theme::status_color)),
                    );
                    if let Some(orq) = session.orchestrator.and_then(|o| cx.model.agent(o)) {
                        let (done, total) = cx.model.progress(orq.id);
                        ui.label(
                            RichText::new(format!("· {done} de {total} prontas"))
                                .color(theme::TEXT_DIM),
                        );
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Focar no canvas").clicked() {
                        st.focus = Some(session.id);
                        canvas.request_fit();
                    }
                    if let Some(orq) = session.orchestrator
                        && ui.button("Orquestrador").clicked()
                    {
                        st.select(orq);
                    }
                    if ui
                        .add(widgets::tinted_button("Encerrar…", theme::FAILED))
                        .clicked()
                    {
                        dialogs.confirm = Some(Confirm::close_session(session.id));
                    }
                });
            });
            ui.add_space(4.0);
        }

        ui.add_space(8.0);
        widgets::section_title(ui, "Recursos", None);
        card(ui, |ui| {
            let used = cx.model.slots_used();
            let max = cx.model.max_workers.max(1);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Vagas de agentes").color(theme::TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{used} de {max} em uso")).color(theme::TEXT_DIM),
                    );
                });
            });
            let frac = used as f32 / max as f32;
            let color = if frac >= 1.0 {
                theme::WAITING
            } else {
                theme::ACCENT
            };
            ui.add(
                egui::ProgressBar::new(frac.min(1.0))
                    .desired_height(6.0)
                    .fill(color),
            );
            let counts = cx.model.counts(None);
            if counts.queued > 0 {
                ui.label(
                    RichText::new(format!("{} na fila esperando vaga", counts.queued))
                        .color(theme::TEXT_DIM),
                );
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Navegadores abertos").color(theme::TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(cx.model.browsers_open().to_string()).color(theme::TEXT_DIM),
                    );
                });
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("RAM do app").color(theme::TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(res.ram.label()).color(theme::TEXT_DIM));
                });
            });
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Botão direito no canvas ou Ctrl+Shift+N novo orquestrador · \
                 Ctrl+Shift+↑/↓ agente · Ctrl+Shift+B navegador",
            )
            .size(11.5)
            .color(theme::TEXT_DIM),
        );
    });
}
