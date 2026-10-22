//! App eframe: barra superior, canvas (com o rascunho de orquestração),
//! painel lateral, barra de status e diálogos. Lê o `Model` (montado pela
//! task de eventos) e só manda `Command`.

mod bars;
mod browser_view;
mod canvas;
#[cfg(feature = "screenshot")]
mod dev;
mod dialogs;
mod draft;
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

/// O que a App aplica depois do frame (mexe na config local).
pub enum Action {
    SaveConfig(Box<AppConfig>),
    Created { repo: PathBuf },
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
            self.canvas.open_draft(&self.config);
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
        // O Esc já tirou o foco de um campo de texto (só o terminal e o
        // navegador seguram o Esc): fecha o rascunho ou, sem ele, a seleção.
        let nothing_focused = ctx.memory(|m| m.focused().is_none());
        if nothing_focused
            && !self.dialogs.any_open()
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
            && !self.canvas.cancel_draft()
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
        let created = CentralPanel::default()
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
            })
            .inner;

        let mut actions = self.dialogs.show(&ctx, &cx, &mut self.st);
        actions.extend(created);
        reconcile_browsers(&mut self.st, &cx);
        drop(model);
        for action in actions {
            match action {
                Action::SaveConfig(config) => {
                    shared.model().max_workers = config.orchestration.max_concurrent_workers;
                    shared.set_scrollback(config.ui.scrollback_lines);
                    self.config = *config.clone();
                    let _ = self.cmd.send(Command::SaveConfig(config));
                }
                Action::Created { repo } => self.created(repo),
            }
        }
    }

    /// Pedido do `orq`: abre o repositório (no rascunho do canvas, ou já a
    /// orquestração quando veio com objetivo). Vindo de outro terminal, traz a janela para
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
            self.canvas.open_draft_in(&self.config, &repo);
            return;
        };
        let Some(preset) = actions::default_orchestrator(&self.config) else {
            warn!("nenhum preset pode orquestrar; abrindo o rascunho");
            self.canvas.open_draft_in(&self.config, &repo);
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
    use std::path::Path;
    use std::time::Duration;

    use egui::{Event, PointerButton, Pos2, RawInput, Rect, pos2, vec2};
    use orquestra_core::AppConfig;

    use super::*;
    use crate::backend::{self, Backend};
    use crate::feed::lock;

    fn frame(ctx: &egui::Context, app: &mut OrquestraApp) {
        frame_with(ctx, app, Vec::new());
    }

    fn frame_with(ctx: &egui::Context, app: &mut OrquestraApp, events: Vec<Event>) {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1440.0, 900.0))),
            events,
            ..RawInput::default()
        };
        let mut out = ctx.run_ui(input, |ui| app.draw(ui));
        out.textures_delta.clear();
    }

    /// Tecla apertada e solta no mesmo frame.
    fn key(ctx: &egui::Context, app: &mut OrquestraApp, key: Key, modifiers: Modifiers) {
        let event = |pressed| Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers,
        };
        frame_with(
            ctx,
            app,
            vec![
                Event::ModifiersChanged(modifiers),
                event(true),
                event(false),
                Event::ModifiersChanged(Modifiers::NONE),
            ],
        );
    }

    fn type_text(ctx: &egui::Context, app: &mut OrquestraApp, text: &str) {
        frame_with(ctx, app, vec![Event::Text(text.to_owned())]);
    }

    /// Clique (um frame para apertar, outro para soltar) num ponto da tela.
    fn click(ctx: &egui::Context, app: &mut OrquestraApp, pos: Pos2, button: PointerButton) {
        let event = |pressed| Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::NONE,
        };
        frame_with(ctx, app, vec![Event::PointerMoved(pos)]);
        frame_with(ctx, app, vec![event(true)]);
        frame_with(ctx, app, vec![event(false)]);
    }

    /// Arrasta com o botão principal de `from` a `to`, em alguns frames.
    fn drag(ctx: &egui::Context, app: &mut OrquestraApp, from: Pos2, to: Pos2) {
        let button = |pos, pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        frame_with(ctx, app, vec![Event::PointerMoved(from)]);
        frame_with(ctx, app, vec![button(from, true)]);
        for t in [0.1, 0.4, 0.7, 1.0] {
            let pos = from + (to - from) * t;
            frame_with(ctx, app, vec![Event::PointerMoved(pos)]);
        }
        frame_with(ctx, app, vec![button(to, false)]);
        frame(ctx, app);
    }

    fn wait_sessions(ctx: &egui::Context, app: &mut OrquestraApp, shared: &Shared, n: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while shared.model().sessions().len() < n {
            assert!(std::time::Instant::now() < deadline, "sessão não apareceu");
            frame(ctx, app);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Hub com os mocks (`seed`: com as duas sessões de demonstração) e a
    /// App sem janela.
    fn mock_app(
        runtime: &tokio::runtime::Runtime,
        seed: bool,
    ) -> (
        egui::Context,
        OrquestraApp,
        Arc<Shared>,
        CommandSender,
        tokio::task::JoinHandle<()>,
    ) {
        let config = AppConfig::default();
        let Backend {
            bus: _bus,
            cmd,
            events,
            frames,
            hub,
        } = backend::start_mock(runtime.handle(), &config, 0.02, seed).unwrap();
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
        let app = OrquestraApp::with_context(&ctx, cmd.clone(), shared.clone(), config);
        (ctx, app, shared, cmd, hub)
    }

    fn shutdown(
        runtime: &tokio::runtime::Runtime,
        cmd: &CommandSender,
        hub: tokio::task::JoinHandle<()>,
    ) {
        cmd.send(Command::Shutdown).unwrap();
        runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), hub).await })
            .expect("hub não encerrou")
            .unwrap();
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    /// Pedidos do `orq`: sem objetivo, o rascunho abre no canvas já na pasta;
    /// com objetivo, a orquestração começa e a pasta vai para os recentes.
    #[test]
    fn open_requests_from_orq() {
        let runtime = runtime();
        let (ctx, mut app, shared, cmd, hub) = mock_app(&runtime, false);

        let loja = OpenRequest {
            repo: Some("/src/loja".into()),
            goal: None,
        };
        shared.push_open(loja, true);
        frame(&ctx, &mut app);
        let draft = app.canvas.draft().expect("rascunho não abriu");
        assert_eq!(draft.repo(), "/src/loja");
        assert!(!draft.more, "com a pasta, o rascunho abre compacto");
        assert!(!app.dialogs.any_open());
        assert!(shared.model().sessions().is_empty());

        app.canvas.cancel_draft();
        let blog = OpenRequest {
            repo: Some("/src/blog".into()),
            goal: Some("migrar para Astro".to_owned()),
        };
        shared.push_open(blog, false);
        wait_sessions(&ctx, &mut app, &shared, 1);
        {
            let model = shared.model();
            let session = &model.sessions()[0].config;
            assert_eq!(session.repo, PathBuf::from("/src/blog"));
            assert_eq!(session.goal, "migrar para Astro");
        }
        assert!(app.canvas.draft().is_none());
        assert!(!app.dialogs.any_open());
        assert_eq!(app.config.recent_repos[0], PathBuf::from("/src/blog"));

        shutdown(&runtime, &cmd, hub);
    }

    /// O rascunho pelo teclado: Shift+Enter quebra a linha, abrir de novo
    /// mantém o texto, Enter inicia com a pasta e o objetivo (e a pasta vai
    /// para os recentes); Esc fecha um rascunho sem criar nada.
    #[test]
    fn draft_starts_an_orchestration() {
        let runtime = runtime();
        let (ctx, mut app, shared, cmd, hub) = mock_app(&runtime, false);
        frame(&ctx, &mut app);

        app.canvas
            .open_draft_in(&app.config, Path::new("/src/loja"));
        frame(&ctx, &mut app);
        type_text(&ctx, &mut app, "login");
        key(&ctx, &mut app, Key::Enter, Modifiers::SHIFT);
        type_text(&ctx, &mut app, "com e-mail");
        assert!(app.canvas.draft().is_some());
        assert!(shared.model().sessions().is_empty());

        // De novo (Ctrl+Shift+N): o mesmo rascunho, com o texto.
        key(
            &ctx,
            &mut app,
            Key::N,
            Modifiers::COMMAND | Modifiers::SHIFT,
        );
        frame(&ctx, &mut app);
        // A moldura da sessão nova fica onde estava a janela.
        let at = pos2(640.0, 420.0);
        app.canvas.draft_mut().unwrap().at = at;
        key(&ctx, &mut app, Key::Enter, Modifiers::NONE);
        assert!(app.canvas.draft().is_none(), "Enter não iniciou");
        wait_sessions(&ctx, &mut app, &shared, 1);
        frame(&ctx, &mut app);
        {
            let model = shared.model();
            let session = &model.sessions()[0];
            assert_eq!(session.config.repo, PathBuf::from("/src/loja"));
            assert_eq!(session.config.goal, "login\ncom e-mail");
            assert_eq!(session.config.name, "loja · login");
            assert_eq!(app.st.selected, session.orchestrator);
            assert_eq!(app.canvas.position(session.id), Some(at));
            let frame = app.canvas.session_frame(&model, session.id).unwrap();
            assert_eq!(frame.min, at);
        }
        assert_eq!(app.config.recent_repos[0], PathBuf::from("/src/loja"));

        // Um rascunho novo já vem com a pasta recente; Esc fecha.
        app.canvas.open_draft(&app.config);
        frame(&ctx, &mut app);
        assert_eq!(app.canvas.draft().map(|d| d.repo()), Some("/src/loja"));
        key(&ctx, &mut app, Key::Escape, Modifiers::NONE);
        assert!(app.canvas.draft().is_none(), "Esc não fechou");
        frame(&ctx, &mut app);
        assert_eq!(shared.model().sessions().len(), 1);

        shutdown(&runtime, &cmd, hub);
    }

    /// Botão direito no fundo do canvas: menu com "Iniciar novo agente
    /// orquestrador", que abre o rascunho onde foi o clique.
    #[test]
    fn right_click_on_the_canvas_opens_the_draft() {
        let runtime = runtime();
        let (ctx, mut app, _shared, cmd, hub) = mock_app(&runtime, false);
        for _ in 0..2 {
            frame(&ctx, &mut app);
        }
        let at = pos2(200.0, 300.0);
        click(&ctx, &mut app, at, PointerButton::Secondary);
        assert!(egui::Popup::is_any_open(&ctx), "menu não abriu");
        assert!(app.canvas.draft().is_none());
        // O menu sai no ponteiro; o item é a primeira linha dele.
        click(
            &ctx,
            &mut app,
            at + vec2(60.0, 18.0),
            PointerButton::Primary,
        );
        frame(&ctx, &mut app);
        assert!(!egui::Popup::is_any_open(&ctx), "menu não fechou");
        let draft = app.canvas.draft().expect("o item não abriu o rascunho");
        assert!(draft.more, "sem recentes, as opções já vêm à mostra");
        assert!(
            app.canvas.visible().contains_rect(draft.rect()),
            "rascunho fora da área visível"
        );
        // O canto fica no ponto do clique (coube na tela).
        let corner = app.canvas.to_screen(draft.rect().min);
        assert!(
            (corner - at).length() < 1.0,
            "canto em {corner:?}, clique em {at:?}"
        );
        // O cursor já está no objetivo.
        type_text(&ctx, &mut app, "login");
        let draft = app.canvas.draft().unwrap();
        assert_eq!(
            draft.session(&app.config).err(),
            Some("Falta a pasta do repositório")
        );

        shutdown(&runtime, &cmd, hub);
    }

    /// Arrastar o cabeçalho de uma moldura move a sessão sem fazer pan (e
    /// arrastar o fundo faz); clique simples no cabeçalho continua focando.
    /// Arrastar o título do rascunho move o rascunho.
    #[test]
    fn dragging_headers_moves_sessions_and_the_draft() {
        let runtime = runtime();
        let (ctx, mut app, shared, cmd, hub) = mock_app(&runtime, false);
        frame(&ctx, &mut app);
        // Com objetivo: entra sem posição e ganha uma automática, em (0, 0).
        shared.push_open(
            OpenRequest {
                repo: Some("/src/loja".into()),
                goal: Some("login".to_owned()),
            },
            false,
        );
        wait_sessions(&ctx, &mut app, &shared, 1);
        // O orquestrador chega depois da sessão e mexe no ajuste automático.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while shared.model().agent_count() < 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "orquestrador não chegou"
            );
            frame(&ctx, &mut app);
            std::thread::sleep(Duration::from_millis(20));
        }
        for _ in 0..3 {
            frame(&ctx, &mut app);
        }
        let id = shared.model().sessions()[0].id;
        assert_eq!(app.canvas.position(id), Some(Pos2::ZERO));
        app.st.selected = None;
        frame(&ctx, &mut app);

        let view = app.canvas.scene_rect;
        let zoom = app.canvas.zoom;
        let grab = app.canvas.to_screen(pos2(300.0, 14.0));
        drag(&ctx, &mut app, grab, grab + vec2(120.0, 80.0));
        let moved = app.canvas.position(id).unwrap();
        let expected = vec2(120.0, 80.0) / zoom;
        assert!(
            (moved.to_vec2() - expected).length() < 1.0,
            "foi para {moved:?}, esperava {expected:?}"
        );
        assert_eq!(app.canvas.scene_rect, view, "o canvas fez pan");
        assert_eq!(app.st.focus, None, "arrastar não é clique");
        let frame_now = app.canvas.session_frame(&shared.model(), id).unwrap();
        assert_eq!(frame_now.min, moved);

        // Clique simples no cabeçalho foca, como antes.
        let header = app.canvas.to_screen(moved + vec2(300.0, 14.0));
        click(&ctx, &mut app, header, PointerButton::Primary);
        assert_eq!(app.st.focus, Some(id));
        app.st.focus = None;

        // Arrastar o fundo, longe da moldura, faz pan e não move a sessão.
        let empty = app
            .canvas
            .to_screen(moved + vec2(0.0, frame_now.height() + 60.0));
        drag(&ctx, &mut app, empty, empty + vec2(-50.0, -30.0));
        assert_ne!(app.canvas.scene_rect, view, "o fundo não fez pan");
        assert_eq!(app.canvas.position(id), Some(moved));

        // O título do rascunho arrasta a janela.
        app.canvas.open_draft(&app.config);
        frame(&ctx, &mut app);
        frame(&ctx, &mut app);
        let view = app.canvas.scene_rect;
        let zoom = app.canvas.zoom;
        let before = app.canvas.draft().unwrap().at;
        let title = app.canvas.to_screen(before + vec2(150.0, 24.0));
        drag(&ctx, &mut app, title, title + vec2(-90.0, 60.0));
        let after = app.canvas.draft().unwrap().at;
        let expected = vec2(-90.0, 60.0) / zoom;
        assert!(
            (after - before - expected).length() < 1.0,
            "rascunho foi de {before:?} para {after:?}"
        );
        assert_eq!(app.canvas.scene_rect, view);

        shutdown(&runtime, &cmd, hub);
    }

    /// A UI inteira, sem janela, sobre o hub real com os mocks semeados:
    /// canvas, painel com terminal e navegador, foco de sessão, rascunho e
    /// diálogos.
    #[test]
    fn whole_ui_renders_over_the_mock_hub() {
        let runtime = runtime();
        let (ctx, mut app, shared, cmd, hub) = mock_app(&runtime, true);

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

        // A segunda sessão posta à direita da primeira fica lá; posta por
        // cima dela, desce só na exibição (a posição guardada não muda).
        let (a, b) = {
            let model = shared.model();
            (model.sessions()[0].id, model.sessions()[1].id)
        };
        let fa = app.canvas.session_frame(&shared.model(), a).unwrap();
        let right = pos2(fa.right() + 80.0, fa.top());
        app.canvas.set_position(b, right);
        frame(&ctx, &mut app);
        let fb = app.canvas.session_frame(&shared.model(), b).unwrap();
        assert_eq!(fb.min, right);
        let over = fa.min + vec2(100.0, 60.0);
        app.canvas.set_position(b, over);
        frame(&ctx, &mut app);
        let fb = app.canvas.session_frame(&shared.model(), b).unwrap();
        assert_eq!(fb.left(), over.x);
        assert_eq!(fb.top(), fa.bottom() + crate::layout::SESSION_GAP);
        assert_eq!(app.canvas.position(b), Some(over));
        app.canvas.request_fit();
        frame(&ctx, &mut app);

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

        // Foco numa sessão (cards detalhados).
        let first = shared.model().sessions()[0].id;
        app.st.focus = Some(first);
        app.canvas.request_fit();
        frame(&ctx, &mut app);
        app.st.selected = None;
        frame(&ctx, &mut app);

        // Rascunho no canvas, compacto e depois com todas as opções; fica na
        // área visível, legível, e a vista não se reajusta sozinha.
        app.canvas
            .open_draft_in(&app.config, Path::new("/src/loja"));
        frame(&ctx, &mut app);
        frame(&ctx, &mut app);
        let compact = app.canvas.draft().unwrap().rect();
        assert!(app.canvas.visible().contains_rect(compact));
        assert!(app.canvas.zoom >= 0.8);
        app.canvas.draft_mut().unwrap().more = true;
        frame(&ctx, &mut app);
        frame(&ctx, &mut app);
        let expanded = app.canvas.draft().unwrap().rect();
        assert_eq!(expanded.min, compact.min, "a janela cresce para baixo");
        assert!(expanded.height() > compact.height() + 150.0);
        // Uma sessão nova chega com o rascunho aberto: a vista fica onde está.
        app.st.focus = None;
        frame(&ctx, &mut app);
        let view = app.canvas.scene_rect;
        let api = SessionConfig::new("api", "/src/api", "documentar a API");
        let preset = &app.config.presets[0];
        let (_, _, commands) =
            actions::new_orchestration(&app.config, api, preset, TermSize::default());
        for c in commands {
            cmd.send(c).unwrap();
        }
        wait_sessions(&ctx, &mut app, &shared, 3);
        frame(&ctx, &mut app);
        assert_eq!(
            app.canvas.scene_rect, view,
            "a vista pulou com o rascunho aberto"
        );
        // Cancelar devolve o ajuste automático que havia antes.
        app.canvas.cancel_draft();
        frame(&ctx, &mut app);
        assert_ne!(app.canvas.scene_rect, view);

        app.dialogs.open_settings(&app.config);
        frame(&ctx, &mut app);
        assert!(app.dialogs.any_open());

        shutdown(&runtime, &cmd, hub);
    }
}
