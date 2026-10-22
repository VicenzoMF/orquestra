//! Rascunho no canvas de uma orquestração ou de um agente sozinho numa pasta
//! (como uma conversa no app do Claude): o botão direito no fundo, "+ Nova
//! orquestração", "+ Novo agente", Ctrl+Shift+N e `orq <pasta>` abrem esta
//! janela. Ela é desenhada dentro da `Scene`, em coordenadas do canvas, então
//! acompanha pan e zoom.
//!
//! Sem popups (ComboBox) aqui dentro: o popup sai na posição certa, mas em
//! escala 1:1 e com a largura do botão em unidades do canvas, então destoa da
//! janela com zoom. Preset e recentes são escolhas inline.

use std::path::{Path, PathBuf};

use egui::{
    Align, DragValue, FontId, Frame, Key, KeyboardShortcut, Layout, Margin, Modifiers, Painter,
    Pos2, Rect, RichText, Sense, Stroke, TextEdit, Ui, UiBuilder, Vec2, pos2, vec2,
};
use orquestra_core::{AgentKind, AgentPreset, AppConfig, SessionConfig, SessionId, folder};

use super::widgets::{self, kind_label};
use super::{Action, Ctx, UiState};
use crate::actions;
use crate::theme;

pub const DRAFT_W: f32 = 360.0;
/// Altura estimada antes do primeiro frame (para caber na área visível).
const COMPACT_H: f32 = 200.0;
const EXPANDED_H: f32 = 500.0;

/// O que o rascunho cria.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DraftKind {
    /// Um orquestrador com um objetivo, que cria os agentes.
    #[default]
    Orchestration,
    /// Um agente só, numa pasta qualquer, com uma tarefa ou nenhuma.
    Agent,
}

pub struct Draft {
    /// Canto superior esquerdo, em coordenadas do canvas.
    pub at: Pos2,
    pub kind: DraftKind,
    preset: String,
    /// Modo agente: o preset, o navegador e o worktree.
    agent_preset: String,
    agent_browser: bool,
    isolate: bool,
    /// Pasta do último teste do git e se ela é um repositório.
    git: Option<(String, bool)>,
    repo: String,
    goal: String,
    own_limit: bool,
    max_workers: u32,
    kinds: Vec<(AgentKind, bool)>,
    browse: bool,
    approve: bool,
    /// Todas as opções à mostra.
    pub more: bool,
    /// Põe o cursor no objetivo no próximo frame.
    focus_goal: bool,
    /// O canvas se ajustava sozinho antes de o rascunho abrir.
    pub resume_fit: bool,
    /// Retângulo do último frame.
    rect: Rect,
    /// Onde a janela estava ao começar a arrastar o título.
    drag_from: Option<Pos2>,
}

/// O que a pessoa fez no rascunho neste frame.
pub enum Event {
    Start,
    Cancel,
}

impl Draft {
    pub fn new(config: &AppConfig, repo: Option<&Path>, resume_fit: bool) -> Self {
        let preset = actions::default_orchestrator(config)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let repo = repo
            .or(config.recent_repos.first().map(PathBuf::as_path))
            .map(|p| tilde(&p.display().to_string()))
            .unwrap_or_default();
        let agent = config.presets.first();
        Self {
            at: Pos2::ZERO,
            kind: DraftKind::Orchestration,
            preset,
            agent_preset: agent.map(|p| p.name.clone()).unwrap_or_default(),
            agent_browser: agent.is_some_and(|p| p.browser),
            isolate: false,
            git: None,
            more: repo.is_empty(),
            repo,
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
            focus_goal: true,
            resume_fit,
            rect: Rect::NOTHING,
            drag_from: None,
        }
    }

    #[cfg(test)]
    pub fn repo(&self) -> &str {
        &self.repo
    }

    pub fn set_repo(&mut self, repo: &Path) {
        self.repo = tilde(&repo.display().to_string());
    }

    #[cfg(test)]
    pub fn set_goal(&mut self, goal: &str) {
        self.goal = goal.to_owned();
    }

    pub fn focus_goal(&mut self) {
        self.focus_goal = true;
    }

    #[cfg(test)]
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// Tamanho do último frame ou, antes dele, uma estimativa.
    pub fn size(&self) -> Vec2 {
        if self.rect.is_positive() {
            self.rect.size()
        } else {
            vec2(DRAFT_W, if self.more { EXPANDED_H } else { COMPACT_H })
        }
    }

    /// Sessão e preset do orquestrador, ou o que falta para iniciar.
    pub fn session<'a>(
        &self,
        config: &'a AppConfig,
    ) -> Result<(SessionConfig, &'a AgentPreset), &'static str> {
        let goal = self.goal.trim();
        match (self.repo.trim().is_empty(), goal.is_empty()) {
            (true, true) => return Err("Falta a pasta e o objetivo"),
            (true, false) => return Err("Falta a pasta"),
            (false, true) => return Err("Falta o objetivo"),
            (false, false) => {}
        }
        let kinds: Vec<AgentKind> = self
            .kinds
            .iter()
            .filter(|(_, on)| *on)
            .map(|(k, _)| *k)
            .collect();
        if kinds.is_empty() {
            return Err("Marque um tipo de agente");
        }
        let preset = actions::orchestrator_presets(config)
            .into_iter()
            .find(|p| p.name == self.preset)
            .ok_or("Escolha o orquestrador")?;
        let repo = expand_home(self.repo.trim());
        let session = SessionConfig {
            max_workers: self.own_limit.then_some(self.max_workers),
            allowed_kinds: kinds,
            workers_may_browse: self.browse,
            approve_spawns: self.approve,
            ..SessionConfig::new(actions::session_name(&repo, goal), repo.clone(), goal)
        };
        Ok((session, preset))
    }

    /// Pasta e preset do agente (modo agente), ou o que falta. A tarefa é
    /// opcional: sem ela o agente só abre, esperando você.
    pub fn agent<'a>(
        &self,
        config: &'a AppConfig,
    ) -> Result<(PathBuf, &'a AgentPreset), &'static str> {
        if self.repo.trim().is_empty() {
            return Err("Falta a pasta");
        }
        let preset = config
            .presets
            .iter()
            .find(|p| p.name == self.agent_preset)
            .ok_or("Escolha o agente")?;
        Ok((expand_home(self.repo.trim()), preset))
    }

    /// O que falta para iniciar, no modo atual.
    fn missing(&self, config: &AppConfig) -> Result<(), &'static str> {
        match self.kind {
            DraftKind::Orchestration => self.session(config).map(drop),
            DraftKind::Agent => self.agent(config).map(drop),
        }
    }

    /// A pasta é um repositório git (para oferecer o worktree). O git só roda
    /// quando a pasta muda.
    fn refresh_git(&mut self) -> bool {
        let repo = self.repo.trim();
        if self.git.as_ref().is_none_or(|(f, _)| f != repo) {
            let git = !repo.is_empty() && folder::git_root(&expand_home(repo)).is_some();
            self.git = Some((repo.to_owned(), git));
        }
        self.git.as_ref().is_some_and(|(_, git)| *git)
    }

    /// Manda `CreateSession` + `Spawn` (do orquestrador ou do agente) e
    /// seleciona quem subiu.
    pub fn start(&self, cx: &Ctx, st: &mut UiState) -> Option<(SessionId, Action)> {
        let (id, selected, commands, repo) = match self.kind {
            DraftKind::Orchestration => {
                let (session, preset) = self.session(cx.config).ok()?;
                let repo = session.repo.clone();
                let (id, orchestrator, commands) =
                    actions::new_orchestration(cx.config, session, preset, st.spawn_size());
                (id, orchestrator, commands, repo)
            }
            DraftKind::Agent => {
                let (repo, preset) = self.agent(cx.config).ok()?;
                let git = self.git.as_ref().is_some_and(|(_, git)| *git);
                let (id, agent, commands) = actions::new_agent(
                    &repo,
                    preset,
                    Some(self.goal.clone()),
                    self.agent_browser,
                    self.isolate && git,
                    st.spawn_size(),
                );
                (id, agent, commands, repo)
            }
        };
        for c in commands {
            cx.send(c);
        }
        st.select_new(selected);
        Some((id, Action::Created { repo }))
    }

    /// Desenha a janela na `Scene`, em `self.at`.
    pub fn show(&mut self, ui: &mut Ui, cx: &Ctx) -> Option<Event> {
        let builder = UiBuilder::new()
            .id_salt("draft")
            .max_rect(Rect::from_min_size(self.at, vec2(DRAFT_W, 2000.0)))
            // Sente o clique no retângulo todo (por baixo dos widgets): clicar
            // ou usar o botão direito aqui não chega ao fundo do canvas.
            .sense(Sense::click());
        let inner = ui.scope_builder(builder, |ui| {
            Frame::new()
                .fill(theme::CARD)
                .stroke(Stroke::new(1.0, theme::BORDER))
                .corner_radius(10)
                .inner_margin(Margin::same(14))
                .shadow(egui::Shadow {
                    offset: [0, 6],
                    blur: 18,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(90),
                })
                .show(ui, |ui| {
                    ui.set_width(DRAFT_W - 28.0);
                    self.contents(ui, cx)
                })
                .inner
        });
        self.rect = inner.response.rect;
        inner.inner
    }

    fn contents(&mut self, ui: &mut Ui, cx: &Ctx) -> Option<Event> {
        // O fundo dos widgets inativos é o dos cards: aqui some sem isto.
        let inactive = &mut ui.visuals_mut().widgets.inactive;
        inactive.bg_fill = theme::TERM_BG;
        inactive.weak_bg_fill = theme::PANEL;

        // Arrastar o título move a janela no canvas.
        let (title, grab) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::drag());
        match grab.total_drag_delta() {
            Some(total) => {
                let from = *self.drag_from.get_or_insert(self.at);
                self.at = from + total;
                grab.on_hover_and_drag_cursor(egui::CursorIcon::Grabbing);
            }
            None => {
                self.drag_from = None;
                grab.on_hover_cursor(egui::CursorIcon::Grab);
            }
        }
        let painter = ui.painter();
        let icon = pos2(title.left() + 6.0, title.center().y);
        let heading = match self.kind {
            DraftKind::Orchestration => {
                theme::paint_star(painter, icon, 7.0, theme::WAITING);
                "Novo orquestrador"
            }
            DraftKind::Agent => {
                theme::paint_status_icon(painter, icon, 5.0, crate::model::StatusKind::Running);
                "Novo agente"
            }
        };
        painter.text(
            pos2(title.left() + 20.0, title.center().y),
            egui::Align2::LEFT_CENTER,
            heading,
            FontId::proportional(14.5),
            theme::TEXT,
        );
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let kind = self.kind;
            ui.selectable_value(&mut self.kind, DraftKind::Orchestration, "Orquestração")
                .on_hover_text("Um orquestrador divide o objetivo entre vários agentes");
            ui.selectable_value(&mut self.kind, DraftKind::Agent, "Agente")
                .on_hover_text("Um agente só, numa pasta qualquer, como uma conversa");
            if self.kind != kind {
                self.focus_goal = true;
            }
        });
        let git = self.refresh_git();

        // Enter inicia; Shift+Enter quebra a linha.
        let hint = match self.kind {
            DraftKind::Orchestration => "O que o orquestrador deve entregar",
            DraftKind::Agent => "O que o agente deve fazer (opcional)",
        };
        let goal = ui.add(
            TextEdit::multiline(&mut self.goal)
                .id_salt("draft-goal")
                .hint_text(hint)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter)),
        );
        if std::mem::take(&mut self.focus_goal) {
            goal.request_focus();
        }
        let enter = ui.input(|i| {
            i.events.iter().any(|e| {
                matches!(e, egui::Event::Key { key: Key::Enter, pressed: true, modifiers, .. }
                    if !modifiers.shift)
            })
        });
        let mut start = goal.has_focus() && enter;

        ui.horizontal(|ui| {
            let toggle = if self.more {
                "Mostrar menos opções"
            } else {
                "Mostrar mais opções"
            };
            let toggle_w = 132.0;
            let max = ui.available_width() - toggle_w - 8.0;
            let font = FontId::proportional(12.0);
            let summary = self.summary(cx.config, ui.painter(), &font, max);
            ui.label(RichText::new(summary).font(font).color(theme::TEXT_DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .link(RichText::new(toggle).size(12.0).color(theme::ACCENT))
                    .clicked()
                {
                    self.more = !self.more;
                }
            });
        });

        if self.more {
            ui.add_space(2.0);
            ui.separator();
            let folder_focus = match self.kind {
                DraftKind::Orchestration => self.options(ui, cx.config),
                DraftKind::Agent => self.agent_options(ui, cx.config, git),
            };
            start |= folder_focus && enter;
        }

        let check = self.missing(cx.config);
        ui.add_space(6.0);
        let mut event = None;
        ui.horizontal(|ui| {
            let resp = ui.add_enabled(check.is_ok(), widgets::accent_button("Iniciar"));
            if let Err(missing) = check {
                resp.on_disabled_hover_text(missing);
                ui.label(RichText::new(missing).size(12.0).color(theme::TEXT_DIM));
            } else if resp.on_hover_text("Enter").clicked() {
                start = true;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancelar").on_hover_text("Esc").clicked() {
                    event = Some(Event::Cancel);
                }
            });
        });
        if start && check.is_ok() {
            event = Some(Event::Start);
        }
        event
    }

    /// Resumo do que vai ser usado: `~/projetos/loja · Claude Code`. Sem
    /// espaço, corta o começo do caminho, não o nome da pasta.
    fn summary(&self, config: &AppConfig, painter: &Painter, font: &FontId, max: f32) -> String {
        let repo = self.repo.trim();
        let repo = if repo.is_empty() {
            "sem pasta".to_owned()
        } else {
            tilde(repo)
        };
        let preset = match self.kind {
            DraftKind::Orchestration => actions::orchestrator_presets(config)
                .into_iter()
                .find(|p| p.name == self.preset)
                .map_or_else(|| "sem orquestrador".to_owned(), |p| p.name.clone()),
            DraftKind::Agent => {
                let name = config
                    .presets
                    .iter()
                    .find(|p| p.name == self.agent_preset)
                    .map_or("sem agente", |p| p.name.as_str());
                let git = self.git.as_ref().is_some_and(|(_, git)| *git);
                if self.isolate && git {
                    format!("{name} · worktree")
                } else {
                    name.to_owned()
                }
            }
        };
        let full = format!("{repo} · {preset}");
        let fits = |s: &str| {
            painter
                .layout_no_wrap(s.to_owned(), font.clone(), theme::TEXT)
                .size()
                .x
                <= max
        };
        if fits(&full) {
            return full;
        }
        // Menor corte do começo que cabe.
        let chars: Vec<char> = repo.chars().collect();
        let cut = |n: usize| format!("…{} · {preset}", chars[n..].iter().collect::<String>());
        let (mut lo, mut hi) = (1, chars.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if fits(&cut(mid)) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        if fits(&cut(lo)) {
            cut(lo)
        } else {
            widgets::elide(painter, &full, font, max)
        }
    }

    /// As opções completas. Devolve se a pasta tinha o foco (Enter nela inicia).
    fn options(&mut self, ui: &mut Ui, config: &AppConfig) -> bool {
        field_label(ui, "Orquestrador");
        ui.horizontal_wrapped(|ui| {
            for p in actions::orchestrator_presets(config) {
                ui.radio_value(&mut self.preset, p.name.clone(), &p.name)
                    .on_hover_text(&p.program);
            }
        });

        let repo = self.folder_field(ui, config);

        let global = config.orchestration.max_concurrent_workers;
        field_label(ui, "Agentes simultâneos desta orquestração");
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.own_limit, "limitar a");
            ui.add_enabled(
                self.own_limit,
                DragValue::new(&mut self.max_workers).range(1..=global.max(1)),
            );
            ui.label(
                RichText::new(format!("(limite global: {global})"))
                    .size(12.0)
                    .color(theme::TEXT_DIM),
            );
        });

        field_label(ui, "Tipos de agente que ele pode criar");
        ui.horizontal_wrapped(|ui| {
            for (kind, on) in self.kinds.iter_mut() {
                ui.checkbox(on, kind_label(*kind));
            }
        });
        ui.add_space(4.0);
        ui.checkbox(&mut self.browse, "Agentes podem pedir navegador");
        ui.checkbox(
            &mut self.approve,
            "Pedir minha aprovação antes de criar cada agente",
        );
        repo.lost_focus()
    }

    /// Opções do modo agente. Devolve se a pasta tinha o foco.
    fn agent_options(&mut self, ui: &mut Ui, config: &AppConfig, git: bool) -> bool {
        field_label(ui, "Agente");
        let before = self.agent_preset.clone();
        ui.horizontal_wrapped(|ui| {
            for p in &config.presets {
                ui.radio_value(&mut self.agent_preset, p.name.clone(), &p.name)
                    .on_hover_text(&p.program);
            }
        });
        if before != self.agent_preset
            && let Some(p) = config.presets.iter().find(|p| p.name == self.agent_preset)
        {
            self.agent_browser = p.browser;
        }

        let repo = self.folder_field(ui, config);
        ui.add_space(4.0);
        ui.add_enabled(
            git,
            egui::Checkbox::new(&mut self.isolate, "Num git worktree e branch próprios"),
        )
        .on_hover_text("Uma cópia isolada da pasta: para mudar código sem atrapalhar ninguém")
        .on_disabled_hover_text("A pasta não é um repositório git: o agente trabalha nela");
        ui.checkbox(&mut self.agent_browser, "Dar um navegador a este agente");
        repo.lost_focus()
    }

    /// Campo da pasta, com as recentes.
    fn folder_field(&mut self, ui: &mut Ui, config: &AppConfig) -> egui::Response {
        field_label(ui, "Pasta");
        let repo = ui.add(
            TextEdit::singleline(&mut self.repo)
                .id_salt("draft-repo")
                .hint_text("~/projetos/meu-repo ou qualquer pasta")
                .desired_width(f32::INFINITY),
        );
        if !config.recent_repos.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Recentes").size(12.0).color(theme::TEXT_DIM));
                for path in &config.recent_repos {
                    let text = path.display().to_string();
                    let name = path
                        .file_name()
                        .map_or_else(|| text.clone(), |n| n.to_string_lossy().into_owned());
                    let chosen = expand_home(self.repo.trim()) == *path;
                    if ui
                        .selectable_label(chosen, RichText::new(name).size(12.5))
                        .on_hover_text(tilde(&text))
                        .clicked()
                    {
                        self.repo = tilde(&text);
                    }
                }
            });
        }
        repo
    }
}

fn field_label(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(text).size(12.0).color(theme::TEXT_DIM));
}

/// `/home/eu/x` → `~/x`, para mostrar.
fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home() {
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(expand_home("~/x"), PathBuf::from(format!("{home}/x")));
        assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
        assert_eq!(tilde(&format!("{home}/x")), "~/x");
    }

    /// Sem recentes, abre com as opções à mostra e não inicia sem pasta e objetivo.
    #[test]
    fn needs_folder_and_goal() {
        let config = AppConfig::default();
        let mut d = Draft::new(&config, None, true);
        assert!(d.more);
        assert_eq!(d.session(&config).err(), Some("Falta a pasta e o objetivo"));
        d.set_repo(Path::new("/src/loja"));
        assert_eq!(d.session(&config).err(), Some("Falta o objetivo"));
        d.set_goal("  login com e-mail\n");
        let (session, preset) = d.session(&config).unwrap();
        assert_eq!(session.repo, PathBuf::from("/src/loja"));
        assert_eq!(session.goal, "login com e-mail");
        assert_eq!(session.name, "loja · login com e-mail");
        assert_eq!(session.max_workers, None);
        assert!(session.workers_may_browse);
        assert!(!session.approve_spawns);
        assert_eq!(
            session.allowed_kinds,
            vec![AgentKind::ClaudeCode, AgentKind::Codex]
        );
        assert_eq!(preset.name, config.orchestration.orchestrator_preset);

        let compact = Draft::new(&config, Some(Path::new("/src/blog")), false);
        assert!(!compact.more);
        assert_eq!(compact.repo(), "/src/blog");
    }
}
