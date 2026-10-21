//! App eframe: barra superior, canvas, painel lateral, barra de status e
//! diálogos. Lê o `Model` (montado pela task de eventos) e só manda `Command`.

mod bars;
mod browser_view;
mod canvas;
#[cfg(feature = "screenshot")]
mod dev;
mod dialogs;
mod side;
mod widgets;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use egui::{CentralPanel, Frame, Key, Modifiers, Panel, TextureHandle};
use orquestra_core::{AgentId, AppConfig, Command, CommandSender, SessionId, TermSize};

use crate::feed::Shared;
use crate::model::{BrowserState, Model};
use crate::sysinfo::RamMeter;
use crate::term::view::{TermFont, ViewState};
use crate::theme;

/// Aba do painel lateral com um nó selecionado.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideTab {
    Terminal,
    Browser,
}

/// Estado da UI que não vem de eventos.
pub struct UiState {
    pub selected: Option<AgentId>,
    /// Sessão em foco (pill); `None` = todas.
    pub focus: Option<SessionId>,
    pub tab: SideTab,
    /// Dar foco ao terminal no próximo frame.
    pub focus_terminal: bool,
    /// Texto do campo "Fale com o orquestrador…" de cada card.
    pub chat: HashMap<AgentId, String>,
    /// Agentes com `SetBrowserVisible(true)` enviado.
    pub streaming: HashSet<AgentId>,
    /// Navegador mostrado no painel neste frame.
    pub panel_browser: Option<AgentId>,
    /// Miniaturas de navegador visíveis no canvas neste frame.
    pub thumb_browsers: Vec<AgentId>,
    /// Último tamanho do terminal do painel (para o PTY de spawns novos).
    pub last_term_size: Option<TermSize>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            selected: None,
            focus: None,
            tab: SideTab::Terminal,
            focus_terminal: false,
            chat: HashMap::new(),
            streaming: HashSet::new(),
            panel_browser: None,
            thumb_browsers: Vec::new(),
            last_term_size: None,
        }
    }
}

impl UiState {
    pub fn select(&mut self, id: AgentId) {
        if self.selected != Some(id) {
            self.selected = Some(id);
            self.tab = SideTab::Terminal;
        }
        self.focus_terminal = true;
    }

    /// Tamanho inicial do PTY de um spawn novo.
    pub fn spawn_size(&self) -> TermSize {
        self.last_term_size.unwrap_or_default()
    }
}

/// O que as telas leem neste frame.
pub struct Ctx<'a> {
    pub model: &'a Model,
    pub shared: &'a Shared,
    pub cmd: &'a CommandSender,
    pub config: &'a AppConfig,
}

impl Ctx<'_> {
    pub fn send(&self, cmd: Command) {
        let _ = self.cmd.send(cmd);
    }
}

/// Texturas e caches do lado da UI.
pub struct Resources {
    pub term_font: Option<TermFont>,
    pub term_views: HashMap<AgentId, ViewState>,
    pub browser: browser_view::BrowserView,
    pub thumbs: HashMap<AgentId, TextureHandle>,
    pub ram: RamMeter,
}

pub struct OrquestraApp {
    cmd: CommandSender,
    shared: Arc<Shared>,
    config: AppConfig,
    st: UiState,
    canvas: canvas::CanvasState,
    dialogs: dialogs::Dialogs,
    res: Resources,
    shutdown_sent: bool,
    #[cfg(feature = "screenshot")]
    dev: dev::DevHooks,
}

impl OrquestraApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cmd: CommandSender,
        shared: Arc<Shared>,
        config: AppConfig,
    ) -> Self {
        setup_style(&cc.egui_ctx, &config);
        Self {
            cmd,
            shared,
            config,
            st: UiState::default(),
            canvas: canvas::CanvasState::default(),
            dialogs: dialogs::Dialogs::default(),
            res: Resources {
                term_font: None,
                term_views: HashMap::new(),
                browser: browser_view::BrowserView::default(),
                thumbs: HashMap::new(),
                ram: RamMeter::default(),
            },
            shutdown_sent: false,
            #[cfg(feature = "screenshot")]
            dev: dev::DevHooks::from_env(),
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context, model: &Model) {
        let combo = Modifiers::COMMAND | Modifiers::SHIFT;
        let (new, up, down, browser) = ctx.input_mut(|i| {
            (
                i.consume_key(combo, Key::N),
                i.consume_key(combo, Key::ArrowUp),
                i.consume_key(combo, Key::ArrowDown),
                i.consume_key(combo, Key::B),
            )
        });
        if new {
            self.dialogs.open_new_orchestration(&self.config);
        }
        if up || down {
            let order = model.canvas_order(self.st.focus);
            if !order.is_empty() {
                let pos = self
                    .st
                    .selected
                    .and_then(|s| order.iter().position(|a| *a == s));
                let next = match (pos, down) {
                    (None, true) => 0,
                    (None, false) => order.len() - 1,
                    (Some(p), true) => (p + 1) % order.len(),
                    (Some(p), false) => (p + order.len() - 1) % order.len(),
                };
                self.st.select(order[next]);
            }
        }
        if browser && self.st.selected.is_some() {
            self.st.tab = match self.st.tab {
                SideTab::Terminal => SideTab::Browser,
                SideTab::Browser => SideTab::Terminal,
            };
            self.st.focus_terminal = self.st.tab == SideTab::Terminal;
        }
        let nothing_focused = ctx.memory(|m| m.focused().is_none());
        if nothing_focused
            && !self.dialogs.any_open()
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.st.selected = None;
        }
    }
}

/// Liga o screencast de quem aparece (painel ou miniatura) e desliga o resto.
fn reconcile_browsers(st: &mut UiState, cx: &Ctx) {
    {
        let mut wanted: HashMap<AgentId, bool> = HashMap::new();
        for id in &st.thumb_browsers {
            wanted.insert(*id, false);
        }
        if let Some(id) = st.panel_browser {
            wanted.insert(id, true);
        }
        wanted.retain(|id, _| {
            cx.model
                .agent(*id)
                .is_some_and(|a| matches!(a.browser, BrowserState::Ready { .. }))
        });
        let now: HashSet<AgentId> = wanted.keys().copied().collect();
        for id in now.difference(&st.streaming) {
            cx.send(Command::SetBrowserVisible {
                id: *id,
                visible: true,
            });
        }
        for id in st.streaming.difference(&now) {
            if cx.model.agent(*id).is_some_and(|a| a.browser.is_open()) {
                cx.send(Command::SetBrowserVisible {
                    id: *id,
                    visible: false,
                });
            }
        }
        st.streaming = now;
        cx.shared.frames.set_wanted(wanted);
    }
}

impl OrquestraApp {
    /// Esquece caches de agentes que saíram do modelo.
    fn gc(&mut self, model: &Model) {
        if self.st.selected.is_some_and(|id| model.agent(id).is_none()) {
            self.st.selected = None;
        }
        if self.st.focus.is_some_and(|id| model.session(id).is_none()) {
            self.st.focus = None;
        }
        self.res
            .term_views
            .retain(|id, _| model.agent(*id).is_some());
        self.res.thumbs.retain(|id, _| model.agent(*id).is_some());
        self.st.chat.retain(|id, _| model.agent(*id).is_some());
        let keep: HashSet<AgentId> = model.canvas_order(None).into_iter().collect();
        self.shared.frames.forget(&keep);
    }
}

impl eframe::App for OrquestraApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) && !self.shutdown_sent {
            self.shutdown_sent = true;
            let _ = self.cmd.send(Command::Shutdown);
        }
        let font_size = self.config.ui.font_size;
        let font_stale = self
            .res
            .term_font
            .as_ref()
            .is_none_or(|f| f.font.size != font_size);
        if font_stale {
            self.res.term_font = Some(TermFont::new(&ctx, font_size));
        }

        let shared = self.shared.clone();
        let model = shared.model();
        self.gc(&model);
        self.shortcuts(&ctx, &model);
        #[cfg(feature = "screenshot")]
        self.dev.frame(&ctx, &model, &mut self.st, &mut self.canvas);

        let cx = Ctx {
            model: &model,
            shared: &shared,
            cmd: &self.cmd,
            config: &self.config,
        };
        self.st.panel_browser = None;
        self.st.thumb_browsers.clear();

        Panel::top("top")
            .frame(
                Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(14, 8)),
            )
            .show(ui, |ui| {
                bars::top_bar(ui, &cx, &mut self.st, &mut self.canvas, &mut self.dialogs)
            });
        Panel::bottom("status")
            .frame(
                Frame::new()
                    .fill(theme::PANEL_ALT)
                    .inner_margin(egui::Margin::symmetric(14, 5)),
            )
            .show(ui, |ui| bars::status_bar(ui, &cx, &mut self.res.ram));
        Panel::right("side")
            .resizable(true)
            .default_size(600.0)
            .size_range(340.0..=1100.0)
            .frame(
                Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(ui, |ui| {
                side::show(
                    ui,
                    &cx,
                    &mut self.st,
                    &mut self.res,
                    &mut self.canvas,
                    &mut self.dialogs,
                )
            });
        CentralPanel::default()
            .frame(Frame::new().fill(theme::BG))
            .show(ui, |ui| {
                canvas::show(
                    ui,
                    &cx,
                    &mut self.st,
                    &mut self.canvas,
                    &mut self.res,
                    &mut self.dialogs,
                )
            });

        let actions = self.dialogs.show(&ctx, &cx, &mut self.st);
        reconcile_browsers(&mut self.st, &cx);
        drop(model);
        for action in actions {
            match action {
                dialogs::Action::SaveConfig(config) => {
                    shared.model().max_workers = config.orchestration.max_concurrent_workers;
                    self.config = *config.clone();
                    let _ = self.cmd.send(Command::SaveConfig(config));
                }
                dialogs::Action::Created { session, repo } => {
                    self.st.focus = None;
                    self.canvas.request_fit();
                    let _ = session;
                    let recent = &mut self.config.recent_repos;
                    recent.retain(|r| *r != repo);
                    recent.insert(0, repo);
                    recent.truncate(8);
                    let _ = self
                        .cmd
                        .send(Command::SaveConfig(Box::new(self.config.clone())));
                }
            }
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG.to_normalized_gamma_f32()
    }
}

fn setup_style(ctx: &egui::Context, config: &AppConfig) {
    crate::fonts::install(ctx);
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, theme::visuals());
    ctx.all_styles_mut(|style| {
        use egui::{FontFamily, FontId, TextStyle};
        style.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(17.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(13.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(11.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(config.ui.font_size, FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        style.spacing.interact_size.y = 26.0;
    });
}
