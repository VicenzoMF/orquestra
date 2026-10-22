//! App eframe: barra superior, canvas com os painéis flutuantes, painel do
//! agente selecionado, barra de status e diálogos. Lê o `Model` (montado pela
//! task de eventos) e só manda `Command`.

mod bars;
mod browser_view;
mod canvas;
#[cfg(feature = "screenshot")]
mod dev;
mod dialogs;
mod overlay;
mod side;
mod widgets;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use egui::{
    CentralPanel, Frame, Key, Modifiers, Panel, TextureHandle, UserAttentionType, ViewportCommand,
};
use orquestra_core::{
    AgentId, AppConfig, Command, CommandSender, SessionConfig, SessionId, TermSize,
};
use tracing::warn;

use crate::actions;
use crate::feed::Shared;
use crate::instance::OpenRequest;
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
    /// Agente recém-pedido ao hub e já selecionado; o `Created` ainda não chegou.
    pub awaiting: Option<(AgentId, std::time::Instant)>,
    /// Lista "Orquestrações" do canvas.
    pub menu: overlay::Menu,
    /// "Precisa de você" recolhido.
    pub attention_collapsed: bool,
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
            awaiting: None,
            menu: overlay::Menu::default(),
            attention_collapsed: false,
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

    /// Seleciona um agente que acabou de ser pedido (ainda fora do modelo).
    pub fn select_new(&mut self, id: AgentId) {
        self.awaiting = Some((id, std::time::Instant::now()));
        self.select(id);
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
        Self::with_context(&cc.egui_ctx, cmd, shared, config)
    }

    /// Como `new`, só com o `egui::Context` (testes sem janela).
    pub fn with_context(
        ctx: &egui::Context,
        cmd: CommandSender,
        shared: Arc<Shared>,
        config: AppConfig,
    ) -> Self {
        setup_style(ctx, &config);
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
            // Primeiro fecha a lista de orquestrações; depois, a seleção.
            if self.st.menu.open {
                self.st.menu.set(false, ctx.input(|i| i.time));
            } else {
                self.st.selected = None;
            }
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
        if let Some((id, since)) = self.st.awaiting
            && (model.agent(id).is_some() || since.elapsed() > std::time::Duration::from_secs(10))
        {
            self.st.awaiting = None;
        }
        let awaited = self.st.awaiting.map(|(id, _)| id);
        if self
            .st
            .selected
            .is_some_and(|id| model.agent(id).is_none() && Some(id) != awaited)
        {
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
        self.draw(ui);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG.to_normalized_gamma_f32()
    }
}

impl OrquestraApp {
    /// Um frame inteiro da UI.
    pub fn draw(&mut self, ui: &mut egui::Ui) {
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
        for (request, remote) in shared.take_opens() {
            self.open(&ctx, request, remote);
        }
        let model = shared.model();
        self.gc(&model);
        self.shortcuts(&ctx, &model);

        let cx = Ctx {
            model: &model,
            shared: &shared,
            cmd: &self.cmd,
            config: &self.config,
        };
        #[cfg(feature = "screenshot")]
        self.dev
            .frame(&ctx, &cx, &mut self.st, &mut self.canvas, &mut self.dialogs);
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
            .show(ui, |ui| bars::status_bar(ui, &cx));
        // O painel só existe com um agente selecionado: o resto flutua no canvas.
        if let Some(agent) = self.st.selected.and_then(|id| model.agent(id)) {
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
                        agent,
                        &mut self.dialogs,
                    )
                });
        }
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
                    shared.set_scrollback(config.ui.scrollback_lines);
                    self.config = *config.clone();
                    let _ = self.cmd.send(Command::SaveConfig(config));
                }
                dialogs::Action::Created { repo } => self.created(repo),
            }
        }
    }

    /// Pedido do `orq`: abre o repositório (no diálogo, ou já a orquestração
    /// quando veio com objetivo). Vindo de outro terminal, traz a janela para
    /// a frente (no X11 isso também a restaura); no Wayland o compositor não
    /// deixa, então ela pede atenção.
    fn open(&mut self, ctx: &egui::Context, request: OpenRequest, remote: bool) {
        if remote {
            ctx.send_viewport_cmd(ViewportCommand::Focus);
            ctx.send_viewport_cmd(ViewportCommand::RequestUserAttention(
                UserAttentionType::Informational,
            ));
        }
        let Some(repo) = request.repo else {
            return;
        };
        let Some(goal) = request.goal.filter(|g| !g.trim().is_empty()) else {
            self.dialogs.open_new_orchestration_in(&self.config, &repo);
            return;
        };
        let Some(preset) = actions::default_orchestrator(&self.config) else {
            warn!("nenhum preset pode orquestrar; abrindo o diálogo");
            self.dialogs.open_new_orchestration_in(&self.config, &repo);
            return;
        };
        let session = SessionConfig::new(
            actions::session_name(&repo, &goal),
            repo.clone(),
            goal.trim(),
        );
        let (_, orchestrator, commands) =
            actions::new_orchestration(&self.config, session, preset, self.st.spawn_size());
        for c in commands {
            let _ = self.cmd.send(c);
        }
        self.st.select_new(orchestrator);
        self.created(repo);
    }

    /// Orquestração nova: o canvas reenquadra e o repositório vai para os recentes.
    fn created(&mut self, repo: PathBuf) {
        self.st.focus = None;
        self.canvas.request_fit();
        let recent = &mut self.config.recent_repos;
        recent.retain(|r| *r != repo);
        recent.insert(0, repo);
        recent.truncate(8);
        let _ = self
            .cmd
            .send(Command::SaveConfig(Box::new(self.config.clone())));
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use egui::{RawInput, Rect, pos2, vec2};
    use orquestra_core::AppConfig;

    use super::*;
    use crate::backend::{self, Backend};
    use crate::feed::lock;

    fn frame(ctx: &egui::Context, app: &mut OrquestraApp) {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1440.0, 900.0))),
            ..RawInput::default()
        };
        let mut out = ctx.run_ui(input, |ui| app.draw(ui));
        out.textures_delta.clear();
    }

    /// Pedidos do `orq`: sem objetivo, o diálogo abre já no repositório; com
    /// objetivo, a orquestração começa e o repositório vai para os recentes.
    #[test]
    fn open_requests_from_orq() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let config = AppConfig::default();
        let Backend {
            bus: _bus,
            cmd,
            events,
            frames,
            hub,
        } = backend::start_mock(runtime.handle(), &config, 0.02, false).unwrap();
        let shared = Shared::new(8, 500);
        backend::spawn_ui_feeds(
            runtime.handle(),
            events,
            frames,
            cmd.clone(),
            shared.clone(),
            Arc::new(|| {}),
        );
        let ctx = egui::Context::default();
        let mut app = OrquestraApp::with_context(&ctx, cmd.clone(), shared.clone(), config);

        let loja = OpenRequest {
            repo: Some("/src/loja".into()),
            goal: None,
        };
        shared.push_open(loja, true);
        frame(&ctx, &mut app);
        assert_eq!(app.dialogs.new_orchestration_repo(), Some("/src/loja"));
        assert!(shared.model().sessions().is_empty());

        app.dialogs = dialogs::Dialogs::default();
        let blog = OpenRequest {
            repo: Some("/src/blog".into()),
            goal: Some("migrar para Astro".to_owned()),
        };
        shared.push_open(blog, false);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while shared.model().sessions().is_empty() {
            assert!(std::time::Instant::now() < deadline, "sessão não apareceu");
            frame(&ctx, &mut app);
            std::thread::sleep(Duration::from_millis(20));
        }
        {
            let model = shared.model();
            let session = &model.sessions()[0].config;
            assert_eq!(session.repo, PathBuf::from("/src/blog"));
            assert_eq!(session.goal, "migrar para Astro");
        }
        assert!(!app.dialogs.any_open());
        assert_eq!(app.config.recent_repos[0], PathBuf::from("/src/blog"));

        cmd.send(Command::Shutdown).unwrap();
        runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), hub).await })
            .expect("hub não encerrou")
            .unwrap();
    }

    /// A UI inteira, sem janela, sobre o hub real com os mocks semeados:
    /// canvas, painel com terminal e navegador, foco de sessão e diálogos.
    #[test]
    fn whole_ui_renders_over_the_mock_hub() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let config = AppConfig::default();
        let Backend {
            bus: _bus,
            cmd,
            events,
            frames,
            hub,
        } = backend::start_mock(runtime.handle(), &config, 0.02, true).unwrap();
        let shared = Shared::new(8, 500);
        backend::spawn_ui_feeds(
            runtime.handle(),
            events,
            frames,
            cmd.clone(),
            shared.clone(),
            Arc::new(|| {}),
        );
        let ctx = egui::Context::default();
        let mut app = OrquestraApp::with_context(&ctx, cmd.clone(), shared.clone(), config);

        // Espera as duas sessões com todos os workers.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while shared.model().agent_count() < 9 {
            assert!(std::time::Instant::now() < deadline, "mocks não povoaram");
            frame(&ctx, &mut app);
            std::thread::sleep(Duration::from_millis(20));
        }
        for _ in 0..3 {
            frame(&ctx, &mut app);
        }

        // Seleciona o agente com navegador: terminal redimensiona ao painel.
        let qa = {
            let model = shared.model();
            model
                .canvas_order(None)
                .into_iter()
                .find(|id| model.agent(*id).is_some_and(|a| a.config.browser))
                .unwrap()
        };
        app.st.select(qa);
        for _ in 0..3 {
            frame(&ctx, &mut app);
        }
        let size = lock(&shared.term(qa).unwrap()).size();
        assert_ne!(
            (size.cols, size.rows),
            (120, 32),
            "terminal não acompanhou o painel"
        );

        // Aba do navegador: o screencast liga quando o navegador está pronto.
        app.st.tab = SideTab::Browser;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !app.st.streaming.contains(&qa) {
            assert!(std::time::Instant::now() < deadline, "screencast não ligou");
            frame(&ctx, &mut app);
            std::thread::sleep(Duration::from_millis(20));
        }
        // Volta ao terminal: o screencast desliga.
        app.st.tab = SideTab::Terminal;
        frame(&ctx, &mut app);
        assert!(!app.st.streaming.contains(&qa));

        // Foco numa sessão (cards detalhados) e os diálogos.
        let first = shared.model().sessions()[0].id;
        app.st.focus = Some(first);
        app.canvas.request_fit();
        frame(&ctx, &mut app);
        app.st.selected = None;
        frame(&ctx, &mut app);
        app.dialogs.open_new_orchestration(&app.config);
        frame(&ctx, &mut app);
        app.dialogs = dialogs::Dialogs::default();
        app.dialogs.open_settings(&app.config);
        frame(&ctx, &mut app);
        assert!(app.dialogs.any_open());

        cmd.send(Command::Shutdown).unwrap();
        runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), hub).await })
            .expect("hub não encerrou")
            .unwrap();
    }
}
