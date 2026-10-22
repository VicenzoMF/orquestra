//! Diálogos: nova orquestração, agente avulso, configurações e confirmações.

use std::path::{Path, PathBuf};

use egui::{ComboBox, DragValue, Id, Modal, RichText, TextEdit, Ui};
use orquestra_core::{
    AgentId, AgentKind, AppConfig, Command, SessionConfig, SessionId, WorktreeCleanup,
};

use super::widgets::{self, kind_label};
use super::{Ctx, UiState};
use crate::actions;
use crate::theme;

/// O que a App aplica depois do frame (mexe na config local).
pub enum Action {
    SaveConfig(Box<AppConfig>),
    Created { repo: PathBuf },
}

pub struct NewOrchestration {
    preset: String,
    repo: String,
    goal: String,
    own_limit: bool,
    max_workers: u32,
    kinds: Vec<(AgentKind, bool)>,
    browse: bool,
    approve: bool,
    /// Põe o cursor no objetivo no próximo frame.
    focus_goal: bool,
}

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
    new_orch: Option<NewOrchestration>,
    solo: Option<SoloAgent>,
    settings: Option<Settings>,
    pub confirm: Option<Confirm>,
}

impl Dialogs {
    /// Repositório do diálogo de nova orquestração aberto, se houver.
    #[cfg(test)]
    pub fn new_orchestration_repo(&self) -> Option<&str> {
        self.new_orch.as_ref().map(|d| d.repo.as_str())
    }

    pub fn any_open(&self) -> bool {
        self.new_orch.is_some()
            || self.solo.is_some()
            || self.settings.is_some()
            || self.confirm.is_some()
    }

    pub fn open_new_orchestration(&mut self, config: &AppConfig) {
        let preset = actions::default_orchestrator(config)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        self.new_orch = Some(NewOrchestration {
            preset,
            repo: config
                .recent_repos
                .first()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            goal: String::new(),
            own_limit: false,
            max_workers: config.orchestration.max_concurrent_workers.min(4),
            kinds: vec![
                (AgentKind::ClaudeCode, true),
                (AgentKind::Codex, true),
                (AgentKind::Shell, false),
            ],
            browse: true,
            approve: false,
            focus_goal: false,
        });
    }

    /// Nova orquestração já neste repositório (`orq <pasta>`): falta só o objetivo.
    pub fn open_new_orchestration_in(&mut self, config: &AppConfig, repo: &Path) {
        self.open_new_orchestration(config);
        if let Some(d) = self.new_orch.as_mut() {
            d.repo = repo.display().to_string();
            d.focus_goal = true;
        }
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
        if let Some(d) = self.new_orch.as_mut() {
            let mut close = false;
            let modal = Modal::new(Id::unique("new-orchestration")).show(ctx, |ui| {
                ui.set_width(520.0);
                if let Some(action) = new_orchestration(ui, cx, st, d, &mut close) {
                    actions.push(action);
                }
            });
            if close || modal.should_close() {
                self.new_orch = None;
            }
        }
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

fn new_orchestration(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    d: &mut NewOrchestration,
    close: &mut bool,
) -> Option<Action> {
    title(ui, "Nova orquestração");
    field_label(ui, "Orquestrador");
    let presets = actions::orchestrator_presets(cx.config);
    ComboBox::from_id_salt("orq-preset")
        .selected_text(d.preset.clone())
        .width(240.0)
        .show_ui(ui, |ui| {
            for p in &presets {
                ui.selectable_value(
                    &mut d.preset,
                    p.name.clone(),
                    format!("{} ({})", p.name, p.program),
                );
            }
        });

    field_label(ui, "Repositório");
    ui.horizontal(|ui| {
        ui.add(
            TextEdit::singleline(&mut d.repo)
                .hint_text("~/projetos/meu-repo")
                .desired_width(380.0),
        );
        if !cx.config.recent_repos.is_empty() {
            ComboBox::from_id_salt("orq-recent")
                .selected_text("Recentes")
                .width(110.0)
                .show_ui(ui, |ui| {
                    for repo in &cx.config.recent_repos {
                        let text = repo.display().to_string();
                        if ui.selectable_label(d.repo == text, &text).clicked() {
                            d.repo = text;
                        }
                    }
                });
        }
    });

    field_label(ui, "Objetivo");
    let goal = ui.add(
        TextEdit::multiline(&mut d.goal)
            .hint_text("O que o orquestrador deve entregar")
            .desired_rows(3)
            .desired_width(f32::INFINITY),
    );
    if std::mem::take(&mut d.focus_goal) {
        goal.request_focus();
    }

    field_label(ui, "Agentes simultâneos desta orquestração");
    ui.horizontal(|ui| {
        ui.checkbox(&mut d.own_limit, "limitar a");
        ui.add_enabled(
            d.own_limit,
            DragValue::new(&mut d.max_workers)
                .range(1..=cx.config.orchestration.max_concurrent_workers.max(1)),
        );
        ui.label(
            RichText::new(format!(
                "(limite global: {})",
                cx.config.orchestration.max_concurrent_workers
            ))
            .color(theme::TEXT_DIM),
        );
    });

    field_label(ui, "Tipos de agente que ele pode criar");
    ui.horizontal(|ui| {
        for (kind, on) in d.kinds.iter_mut() {
            ui.checkbox(on, kind_label(*kind));
        }
    });
    ui.add_space(4.0);
    ui.checkbox(&mut d.browse, "Agentes podem pedir navegador");
    ui.checkbox(
        &mut d.approve,
        "Pedir minha aprovação antes de criar cada agente",
    );

    let repo = expand_home(d.repo.trim());
    let kinds: Vec<AgentKind> = d
        .kinds
        .iter()
        .filter(|(_, on)| *on)
        .map(|(k, _)| *k)
        .collect();
    let valid = !d.repo.trim().is_empty() && !d.goal.trim().is_empty() && !kinds.is_empty();
    ui.add_space(10.0);
    let mut action = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(valid, widgets::accent_button("Criar orquestração"))
            .clicked()
        {
            let Some(preset) = presets.iter().find(|p| p.name == d.preset) else {
                return;
            };
            let session = SessionConfig {
                max_workers: d.own_limit.then_some(d.max_workers),
                allowed_kinds: kinds.clone(),
                workers_may_browse: d.browse,
                approve_spawns: d.approve,
                ..SessionConfig::new(
                    actions::session_name(&repo, &d.goal),
                    repo.clone(),
                    d.goal.trim(),
                )
            };
            let (_, orchestrator, commands) =
                actions::new_orchestration(cx.config, session, preset, st.spawn_size());
            for c in commands {
                cx.send(c);
            }
            st.select_new(orchestrator);
            action = Some(Action::Created { repo: repo.clone() });
            *close = true;
        }
        if ui.button("Cancelar").clicked() {
            *close = true;
        }
        if !valid {
            ui.label(RichText::new("Informe repositório e objetivo.").color(theme::TEXT_DIM));
        }
    });
    action
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~") {
        Some(rest) => {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(format!("{home}{rest}"))
        }
        None => PathBuf::from(path),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home() {
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(expand_home("~/x"), PathBuf::from(format!("{home}/x")));
        assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
    }
}
