//! Rascunho de um agente novo no canvas: o botão direito no fundo ("Iniciar
//! novo agente"), "+ Novo agente", Ctrl+Shift+N e `orq <pasta>` abrem esta
//! janela. Ela é desenhada dentro da `Scene`, em coordenadas do canvas, então
//! acompanha pan e zoom.
//!
//! Não há "orquestrador" à parte: por padrão o agente pode criar outros, e
//! ele mesmo decide quando dividir o trabalho (as instruções vão escondidas,
//! no prompt de sistema). A primeira mensagem é só o que a pessoa escreveu.
//!
//! Sem popups (ComboBox) aqui dentro: o popup sai na posição certa, mas em
//! escala 1:1 e com a largura do botão em unidades do canvas, então destoa da
//! janela com zoom. Tipo e recentes são escolhas inline.

use std::path::{Path, PathBuf};

use egui::{
    Align, DragValue, FontId, Frame, Key, KeyboardShortcut, Layout, Margin, Modifiers, Painter,
    Pos2, Rect, RichText, Sense, Stroke, TextEdit, Ui, UiBuilder, Vec2, pos2, vec2,
};
use orquestra_core::{AgentKind, AgentPreset, AppConfig, SessionId, folder};

use super::widgets::{self, kind_label};
use super::{Action, Ctx, UiState};
use crate::actions::{self, NewAgent, can_delegate};
use crate::model::StatusKind;
use crate::theme;

pub const DRAFT_W: f32 = 360.0;
/// Altura estimada antes do primeiro frame (para caber na área visível).
const COMPACT_H: f32 = 170.0;
const EXPANDED_H: f32 = 600.0;

pub struct Draft {
    /// Canto superior esquerdo, em coordenadas do canvas.
    pub at: Pos2,
    /// Tipo (nome do preset).
    preset: String,
    repo: String,
    task: String,
    browser: bool,
    /// Num worktree próprio (só numa pasta git).
    isolate: bool,
    /// Pode criar outros agentes.
    delegate: bool,
    /// Pasta do último teste do git e se ela é um repositório.
    git: Option<(String, bool)>,
    /// Regras para os agentes que ele criar.
    own_limit: bool,
    max_workers: u32,
    kinds: Vec<(AgentKind, bool)>,
    browse: bool,
    approve: bool,
    /// Todas as opções à mostra.
    pub more: bool,
    /// Põe o cursor na tarefa no próximo frame.
    focus_task: bool,
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
        let preset = actions::default_preset(config);
        let repo = repo
            .or(config.recent_repos.first().map(PathBuf::as_path))
            .map(|p| tilde(&p.display().to_string()))
            .unwrap_or_default();
        Self {
            at: Pos2::ZERO,
            preset: preset.map(|p| p.name.clone()).unwrap_or_default(),
            browser: preset.is_some_and(|p| p.browser),
            isolate: false,
            delegate: true,
            git: None,
            more: repo.is_empty(),
            repo,
            task: String::new(),
            own_limit: false,
            max_workers: config.orchestration.max_concurrent_workers.min(4),
            kinds: vec![
                (AgentKind::ClaudeCode, true),
                (AgentKind::Codex, true),
                (AgentKind::Shell, false),
            ],
            browse: true,
            approve: false,
            focus_task: true,
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
    pub fn set_task(&mut self, task: &str) {
        self.task = task.to_owned();
    }

    pub fn focus_task(&mut self) {
        self.focus_task = true;
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

    /// O agente a criar e o tipo dele, ou o que falta. A tarefa é opcional:
    /// sem ela o agente só abre, esperando você.
    pub fn spec<'a>(
        &self,
        config: &'a AppConfig,
    ) -> Result<(NewAgent, &'a AgentPreset), &'static str> {
        if self.repo.trim().is_empty() {
            return Err("Falta a pasta");
        }
        let preset = config
            .presets
            .iter()
            .find(|p| p.name == self.preset)
            .ok_or("Escolha o tipo")?;
        let delegate = self.delegate && can_delegate(preset.kind);
        let kinds: Vec<AgentKind> = self
            .kinds
            .iter()
            .filter(|(_, on)| *on)
            .map(|(k, _)| *k)
            .collect();
        if delegate && kinds.is_empty() {
            return Err("Marque um tipo para os agentes que ele criar");
        }
        let git = self.git.as_ref().is_some_and(|(_, git)| *git);
        let spec = NewAgent {
            browser: self.browser,
            isolate: self.isolate && git,
            delegate,
            max_workers: self.own_limit.then_some(self.max_workers),
            allowed_kinds: kinds,
            workers_may_browse: self.browse,
            approve_spawns: self.approve,
            ..NewAgent::new(expand_home(self.repo.trim()), Some(self.task.clone()))
        };
        Ok((spec, preset))
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

    /// Manda `CreateSession` + `Spawn` e seleciona o agente.
    pub fn start(&self, cx: &Ctx, st: &mut UiState) -> Option<(SessionId, Action)> {
        let (spec, preset) = self.spec(cx.config).ok()?;
        let repo = spec.folder.clone();
        let (id, agent, commands) = actions::new_agent(spec, preset, st.spawn_size());
        for c in commands {
            cx.send(c);
        }
        st.select_new(agent);
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

    /// Vai poder criar outros agentes (a opção e o tipo permitem).
    fn delegates(&self, config: &AppConfig) -> bool {
        self.delegate
            && config
                .presets
                .iter()
                .find(|p| p.name == self.preset)
                .is_some_and(|p| can_delegate(p.kind))
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
        // A estrela marca quem pode criar outros agentes, como no card.
        if self.delegates(cx.config) {
            theme::paint_star(painter, icon, 7.0, theme::WAITING);
        } else {
            theme::paint_status_icon(painter, icon, 5.0, StatusKind::Running);
        }
        painter.text(
            pos2(title.left() + 20.0, title.center().y),
            egui::Align2::LEFT_CENTER,
            "Novo agente",
            FontId::proportional(14.5),
            theme::TEXT,
        );
        ui.add_space(2.0);
        let git = self.refresh_git();

        // Enter inicia; Shift+Enter quebra a linha.
        let task = ui.add(
            TextEdit::multiline(&mut self.task)
                .id_salt("draft-goal")
                .hint_text("O que o agente deve fazer (opcional)")
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter)),
        );
        if std::mem::take(&mut self.focus_task) {
            task.request_focus();
        }
        let enter = ui.input(|i| {
            i.events.iter().any(|e| {
                matches!(e, egui::Event::Key { key: Key::Enter, pressed: true, modifiers, .. }
                    if !modifiers.shift)
            })
        });
        let mut start = task.has_focus() && enter;

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
            start |= self.options(ui, cx.config, git) && enter;
        }

        let check = self.spec(cx.config).map(drop);
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
        let mut preset = config
            .presets
            .iter()
            .find(|p| p.name == self.preset)
            .map_or_else(|| "sem tipo".to_owned(), |p| p.name.clone());
        if self.isolate && self.git.as_ref().is_some_and(|(_, git)| *git) {
            preset.push_str(" · worktree");
        }
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
    fn options(&mut self, ui: &mut Ui, config: &AppConfig, git: bool) -> bool {
        field_label(ui, "Tipo");
        let before = self.preset.clone();
        ui.horizontal_wrapped(|ui| {
            for p in &config.presets {
                ui.radio_value(&mut self.preset, p.name.clone(), &p.name)
                    .on_hover_text(&p.program);
            }
        });
        if before != self.preset
            && let Some(p) = config.presets.iter().find(|p| p.name == self.preset)
        {
            self.browser = p.browser;
        }

        let repo = self.folder_field(ui, config);
        ui.add_space(4.0);
        ui.add_enabled(
            git,
            egui::Checkbox::new(&mut self.isolate, "Num git worktree e branch próprios"),
        )
        .on_hover_text("Uma cópia isolada da pasta: para mudar código sem atrapalhar ninguém")
        .on_disabled_hover_text("A pasta não é um repositório git: o agente trabalha nela");
        ui.checkbox(&mut self.browser, "Dar um navegador a este agente");
        let can = config
            .presets
            .iter()
            .find(|p| p.name == self.preset)
            .is_some_and(|p| can_delegate(p.kind));
        ui.add_enabled(
            can,
            egui::Checkbox::new(&mut self.delegate, "Pode criar outros agentes"),
        )
        .on_hover_text("Ele decide quando vale dividir o trabalho com outros agentes")
        .on_disabled_hover_text("Só Claude Code e Codex criam agentes");

        if self.delegate && can {
            ui.indent("draft-delegation", |ui| {
                let global = config.orchestration.max_concurrent_workers;
                field_label(ui, "Agentes que ele criar, ao mesmo tempo");
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
                field_label(ui, "Tipos que ele pode criar");
                ui.horizontal_wrapped(|ui| {
                    for (kind, on) in self.kinds.iter_mut() {
                        ui.checkbox(on, kind_label(*kind));
                    }
                });
                ui.checkbox(&mut self.browse, "Eles podem pedir navegador");
                ui.checkbox(&mut self.approve, "Pedir minha aprovação antes de cada um");
            });
        }
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
    use orquestra_core::AgentRole;

    use super::*;

    #[test]
    fn expands_home() {
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(expand_home("~/x"), PathBuf::from(format!("{home}/x")));
        assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
        assert_eq!(tilde(&format!("{home}/x")), "~/x");
    }

    /// Sem recentes, abre com as opções à mostra. Só falta a pasta: a tarefa
    /// é opcional, e por padrão o agente pode criar outros.
    #[test]
    fn needs_only_a_folder() {
        let config = AppConfig::default();
        let mut d = Draft::new(&config, None, true);
        assert!(d.more);
        assert_eq!(d.spec(&config).err(), Some("Falta a pasta"));
        d.set_repo(Path::new("/src/loja"));
        let (spec, preset) = d.spec(&config).unwrap();
        assert_eq!(spec.folder, PathBuf::from("/src/loja"));
        assert!(spec.delegate);
        assert_eq!(preset.name, config.orchestration.orchestrator_preset);
        assert_eq!(spec.max_workers, None);
        assert!(spec.workers_may_browse);
        assert!(!spec.approve_spawns);
        assert_eq!(
            spec.allowed_kinds,
            vec![AgentKind::ClaudeCode, AgentKind::Codex]
        );

        d.set_task("  login com e-mail\n");
        let (spec, preset) = d.spec(&config).unwrap();
        let (_, _, [create, spawn]) =
            actions::new_agent(spec, preset, orquestra_core::TermSize::default());
        assert!(
            matches!(create, orquestra_core::Command::CreateSession { config, .. }
            if config.name == "loja · login com e-mail" && config.goal == "login com e-mail")
        );
        assert!(
            matches!(spawn, orquestra_core::Command::Spawn { config, .. }
            if config.role == AgentRole::Orchestrator
                && config.initial_prompt.as_deref() == Some("login com e-mail"))
        );

        // Quem delega precisa de ao menos um tipo para criar.
        d.kinds.iter_mut().for_each(|(_, on)| *on = false);
        assert!(d.spec(&config).is_err());
        d.delegate = false;
        assert!(d.spec(&config).is_ok());

        let compact = Draft::new(&config, Some(Path::new("/src/blog")), false);
        assert!(!compact.more);
        assert_eq!(compact.repo(), "/src/blog");
    }
}
