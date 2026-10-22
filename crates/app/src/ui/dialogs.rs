//! Diálogos: agente avulso, configurações e confirmações. A nova
//! orquestração é um rascunho no canvas (`draft.rs`).

use egui::{ComboBox, DragValue, Id, Modal, RichText, TextEdit, Ui};
use orquestra_core::{AgentId, AppConfig, Command, SessionId, WorktreeCleanup};

use super::widgets;
use super::{Action, Ctx, UiState};
use crate::actions;
use crate::theme;

pub struct SoloAgent {
    session: Option<SessionId>,
    preset: String,
    name: String,
    task: String,
    browser: bool,
}

pub struct Settings {
    font_size: f32,
    scrollback: usize,
    max_workers: u32,
    notifications: bool,
}

pub enum Confirm {
    Remove {
        agent: AgentId,
        cleanup: WorktreeCleanup,
    },
    CloseSession {
        session: SessionId,
        cleanup: WorktreeCleanup,
    },
}

impl Confirm {
    pub fn remove(agent: AgentId) -> Self {
        Self::Remove {
            agent,
            cleanup: WorktreeCleanup::Keep,
        }
    }

    pub fn close_session(session: SessionId) -> Self {
        Self::CloseSession {
            session,
            cleanup: WorktreeCleanup::Keep,
        }
    }
}

#[derive(Default)]
pub struct Dialogs {
    solo: Option<SoloAgent>,
    settings: Option<Settings>,
    pub confirm: Option<Confirm>,
}

impl Dialogs {
    pub fn any_open(&self) -> bool {
        self.solo.is_some() || self.settings.is_some() || self.confirm.is_some()
    }

    pub fn open_solo(&mut self, cx: &Ctx, st: &UiState) {
        let session = st
            .focus
            .or_else(|| {
                st.selected
                    .and_then(|a| cx.model.agent(a))
                    .map(|a| a.config.session)
            })
            .or_else(|| cx.model.sessions().first().map(|s| s.id));
        let preset = cx.config.presets.first().cloned();
        let n = cx.model.agent_count() + 1;
        self.solo = Some(SoloAgent {
            session,
            preset: preset.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
            name: format!("agente-{n}"),
            task: String::new(),
            browser: preset.is_some_and(|p| p.browser),
        });
    }

    pub fn open_settings(&mut self, config: &AppConfig) {
        self.settings = Some(Settings {
            font_size: config.ui.font_size,
            scrollback: config.ui.scrollback_lines,
            max_workers: config.orchestration.max_concurrent_workers,
            notifications: config.notifications.enabled,
        });
    }

    pub fn show(&mut self, ctx: &egui::Context, cx: &Ctx, st: &mut UiState) -> Vec<Action> {
        let mut actions = Vec::new();
        if let Some(d) = self.solo.as_mut() {
            let mut close = false;
            let modal = Modal::new(Id::unique("solo-agent")).show(ctx, |ui| {
                ui.set_width(460.0);
                solo_agent(ui, cx, st, d, &mut close);
            });
            if close || modal.should_close() {
                self.solo = None;
            }
        }
        if let Some(d) = self.settings.as_mut() {
            let mut close = false;
            let modal = Modal::new(Id::unique("settings")).show(ctx, |ui| {
                ui.set_width(420.0);
                if let Some(action) = settings(ui, cx, d, &mut close) {
                    actions.push(action);
                }
            });
            if close || modal.should_close() {
                self.settings = None;
            }
        }
        if let Some(c) = self.confirm.as_mut() {
            let mut close = false;
            let modal = Modal::new(Id::unique("confirm")).show(ctx, |ui| {
                ui.set_width(420.0);
                confirm(ui, cx, st, c, &mut close);
            });
            if close || modal.should_close() {
                self.confirm = None;
            }
        }
        actions
    }
}

fn title(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(17.0).strong().color(theme::TEXT));
    ui.add_space(6.0);
}

fn field_label(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(text).size(12.5).color(theme::TEXT_DIM));
}

fn solo_agent(ui: &mut Ui, cx: &Ctx, st: &mut UiState, d: &mut SoloAgent, close: &mut bool) {
    title(ui, "Agente avulso");
    field_label(ui, "Orquestração");
    let sessions = cx.model.sessions();
    let current = d
        .session
        .and_then(|s| cx.model.session(s))
        .map_or_else(|| "escolha".to_owned(), |s| s.config.name.clone());
    ComboBox::from_id_salt("solo-session")
        .selected_text(current)
        .width(300.0)
        .show_ui(ui, |ui| {
            for s in &sessions {
                ui.selectable_value(&mut d.session, Some(s.id), &s.config.name);
            }
        });
    field_label(ui, "Tipo");
    let before = d.preset.clone();
    ComboBox::from_id_salt("solo-preset")
        .selected_text(d.preset.clone())
        .width(300.0)
        .show_ui(ui, |ui| {
            for p in &cx.config.presets {
                ui.selectable_value(
                    &mut d.preset,
                    p.name.clone(),
                    format!("{} ({})", p.name, p.program),
                );
            }
        });
    let preset = cx.config.presets.iter().find(|p| p.name == d.preset);
    if before != d.preset
        && let Some(p) = preset
    {
        d.browser = p.browser;
    }
    field_label(ui, "Nome");
    ui.add(TextEdit::singleline(&mut d.name).desired_width(300.0));
    field_label(ui, "Tarefa (opcional)");
    ui.add(
        TextEdit::multiline(&mut d.task)
            .hint_text("Primeira instrução do agente")
            .desired_rows(3)
            .desired_width(f32::INFINITY),
    );
    ui.checkbox(&mut d.browser, "Dar um navegador a este agente");
    ui.label(
        RichText::new("Roda num git worktree novo do repositório da orquestração.")
            .size(12.0)
            .color(theme::TEXT_DIM),
    );
    let session = d.session.and_then(|s| cx.model.session(s));
    let valid = session.is_some() && preset.is_some() && !d.name.trim().is_empty();
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(valid, widgets::accent_button("Criar agente"))
            .clicked()
            && let (Some(session), Some(preset)) = (session, preset)
        {
            let (id, cmd) = actions::solo_agent(
                session.id,
                &session.config,
                preset,
                d.name.trim().to_owned(),
                Some(d.task.clone()),
                d.browser,
                st.spawn_size(),
            );
            cx.send(cmd);
            st.select_new(id);
            *close = true;
        }
        if ui.button("Cancelar").clicked() {
            *close = true;
        }
    });
}

fn settings(ui: &mut Ui, cx: &Ctx, d: &mut Settings, close: &mut bool) -> Option<Action> {
    title(ui, "Configurações");
    egui::Grid::new("settings-grid")
        .num_columns(2)
        .spacing([16.0, 8.0])
        .show(ui, |ui| {
            ui.label("Fonte do terminal");
            ui.add(
                DragValue::new(&mut d.font_size)
                    .range(9.0..=24.0)
                    .speed(0.25)
                    .suffix(" pt"),
            );
            ui.end_row();
            ui.label("Scrollback (linhas)");
            ui.add(
                DragValue::new(&mut d.scrollback)
                    .range(200..=50_000)
                    .speed(50),
            );
            ui.end_row();
            ui.label("Agentes simultâneos (global)");
            ui.add(DragValue::new(&mut d.max_workers).range(1..=64));
            ui.end_row();
            ui.label("Notificações");
            ui.checkbox(&mut d.notifications, "ativadas");
            ui.end_row();
        });
    ui.label(
        RichText::new("O scrollback vale para terminais novos.")
            .size(12.0)
            .color(theme::TEXT_DIM),
    );
    ui.add_space(10.0);
    let mut action = None;
    ui.horizontal(|ui| {
        if ui.add(widgets::accent_button("Salvar")).clicked() {
            let mut config = cx.config.clone();
            config.ui.font_size = d.font_size;
            config.ui.scrollback_lines = d.scrollback;
            config.orchestration.max_concurrent_workers = d.max_workers;
            config.notifications.enabled = d.notifications;
            action = Some(Action::SaveConfig(Box::new(config)));
            *close = true;
        }
        if ui.button("Cancelar").clicked() {
            *close = true;
        }
    });
    action
}

fn cleanup_choice(ui: &mut Ui, cleanup: &mut WorktreeCleanup) {
    ui.radio_value(cleanup, WorktreeCleanup::Keep, "Manter worktree e branch");
    ui.radio_value(
        cleanup,
        WorktreeCleanup::RemoveWorktree,
        "Apagar o worktree (a branch fica)",
    );
    ui.radio_value(
        cleanup,
        WorktreeCleanup::RemoveWorktreeAndBranch,
        "Apagar worktree e branch",
    );
}

fn confirm(ui: &mut Ui, cx: &Ctx, st: &mut UiState, c: &mut Confirm, close: &mut bool) {
    match c {
        Confirm::Remove { agent, cleanup } => {
            let name = cx
                .model
                .agent(*agent)
                .map_or_else(|| "agente".to_owned(), |a| a.config.name.clone());
            title(ui, &format!("Remover {name}?"));
            ui.label("Encerra o processo, fecha o navegador e esquece o agente.");
            ui.add_space(4.0);
            cleanup_choice(ui, cleanup);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(widgets::tinted_button("Remover", theme::FAILED))
                    .clicked()
                {
                    cx.send(Command::Remove {
                        id: *agent,
                        cleanup: *cleanup,
                    });
                    if st.selected == Some(*agent) {
                        st.selected = None;
                    }
                    *close = true;
                }
                if ui.button("Cancelar").clicked() {
                    *close = true;
                }
            });
        }
        Confirm::CloseSession { session, cleanup } => {
            let name = cx
                .model
                .session(*session)
                .map_or_else(|| "orquestração".to_owned(), |s| s.config.name.clone());
            title(ui, &format!("Encerrar {name}?"));
            ui.label("Remove todos os agentes desta orquestração.");
            ui.add_space(4.0);
            cleanup_choice(ui, cleanup);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(widgets::tinted_button("Encerrar", theme::FAILED))
                    .clicked()
                {
                    cx.send(Command::CloseSession {
                        id: *session,
                        cleanup: *cleanup,
                    });
                    if st.focus == Some(*session) {
                        st.focus = None;
                    }
                    *close = true;
                }
                if ui.button("Cancelar").clicked() {
                    *close = true;
                }
            });
        }
    }
}
