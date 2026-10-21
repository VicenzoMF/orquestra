//! Controlador de um Chromium: segue a aba ativa, faz o screencast e repassa
//! input, navegação e resize. É um ator (uma task por agente); os métodos do
//! `BrowserManager` falam com ele por canal e esperam a resposta.
//!
//! **Aba ativa.** A mais recente criada (aba nova ou popup aberto pelo agente
//! via Playwright MCP). Se ela é fechada, vale a mais recente que sobrou.
//! Foco não tem evento no CDP; o que temos é o
//! `Page.screencastVisibilityChanged`: quando a aba transmitida fica oculta
//! (outra veio para a frente com `Page.bringToFront`), procuramos a aba com
//! `document.visibilityState == "visible"` e passamos para ela.
//!
//! Limites: com o streaming desligado essa troca só acontece quando ele volta
//! a ligar; e páginas controladas pelo Playwright ficam sempre "visíveis"
//! (emulação de foco), então lá um `bringToFront` entre abas que já existem
//! não é percebido: seguimos só criação e fechamento.
//!
//! **Sessões.** Só a aba ativa tem sessão nossa (`Target.attachToTarget`,
//! `flatten`), com `Page.enable` e nada mais: o resto das abas fica só para o
//! agente. URL e título vêm de `Target.getTargetInfo`, relido a cada sinal de
//! mudança.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use chromiumoxide::cdp::browser_protocol::browser::{
    Bounds, GetWindowForTargetParams, SetWindowBoundsParams, WindowId, WindowState,
};
use chromiumoxide::cdp::browser_protocol::emulation::{
    ClearDeviceMetricsOverrideParams, SetDeviceMetricsOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
use chromiumoxide::cdp::browser_protocol::page::{
    self, EventScreencastFrame, EventScreencastVisibilityChanged, GetNavigationHistoryParams,
    NavigateParams, NavigateToHistoryEntryParams, ReloadParams, ScreencastFrameAckParams,
    StartScreencastFormat, StartScreencastParams, StopScreencastParams,
};
use chromiumoxide::cdp::browser_protocol::target::{
    AttachToTargetParams, CreateTargetParams, DetachFromTargetParams, GetTargetInfoParams,
    GetTargetsParams, SetDiscoverTargetsParams, TargetId,
};
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use orquestra_core::{
    AgentEvent, AgentId, BrowserFrame, BrowserInput, Error, EventBus, ImageFormat, Navigation,
    Result, Viewport,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, trace, warn};

use crate::cdp::{self, Cdp, RawEvent};
use crate::dispatch::{self, MouseState};

/// Prazo das sondagens em abas que não são nossas (podem estar pausadas
/// esperando o depurador do agente).
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// Prazo para uma navegação responder antes de desistirmos de reportar erro.
const NAVIGATION_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) enum Request {
    Streaming(bool),
    Navigate(Navigation),
    Input(BrowserInput),
    Resize(Viewport),
}

type Reply = oneshot::Sender<Result<()>>;

/// Ponta para mandar pedidos ao controlador.
#[derive(Clone, Debug)]
pub(crate) struct Handle {
    tx: mpsc::UnboundedSender<(Request, Reply)>,
}

impl Handle {
    pub async fn request(&self, req: Request) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx.send((req, tx)).map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }
}

fn gone() -> Error {
    Error::Browser("conexão com o Chromium encerrada".to_owned())
}

/// O que o controlador precisa saber da config.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings {
    pub quality: u8,
    pub every_nth_frame: u32,
    pub viewport: Viewport,
}

/// Fios com o resto da instância.
pub(crate) struct Wiring {
    pub id: AgentId,
    pub bus: EventBus,
    pub cdp: Cdp,
    pub events: mpsc::UnboundedReceiver<RawEvent>,
    /// Fechamento pedido: a queda da conexão é esperada.
    pub closing: Arc<AtomicBool>,
    /// Acionado se a conexão cair sozinha: o monitor mata o processo.
    pub kill: Arc<Notify>,
}

#[derive(Debug)]
struct Tab {
    target_id: String,
    url: String,
    title: String,
}

#[derive(Debug)]
struct Active {
    target_id: String,
    session: String,
}

struct Controller {
    id: AgentId,
    bus: EventBus,
    cdp: Cdp,
    settings: Settings,
    /// Fator de escala do lançamento (`--force-device-scale-factor`).
    launch_scale: f64,
    /// Em ordem de criação; a última é a mais recente.
    tabs: Vec<Tab>,
    active: Option<Active>,
    streaming: bool,
    /// Screencast rodando na sessão ativa.
    casting: bool,
    /// Escala emulada na sessão ativa (resize com outro `device_scale_factor`).
    emulating: bool,
    /// Moldura da janela (janela − viewport), medida uma vez.
    frame_size: Option<(i64, i64)>,
    seq: u64,
    last_nav: Option<(String, String)>,
    mouse: MouseState,
}

/// Descobre as abas, ativa a inicial e põe o controlador para rodar.
pub(crate) async fn start(wiring: Wiring, settings: Settings) -> Result<(Handle, JoinHandle<()>)> {
    let Wiring {
        id,
        bus,
        cdp,
        events,
        closing,
        kill,
    } = wiring;
    let mut ctl = Controller {
        id,
        bus,
        cdp,
        settings,
        launch_scale: settings.viewport.device_scale_factor,
        tabs: Vec::new(),
        active: None,
        streaming: false,
        casting: false,
        emulating: false,
        frame_size: None,
        seq: 0,
        last_nav: None,
        mouse: MouseState::default(),
    };
    ctl.init().await?;
    let (tx, requests) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        ctl.run(events, requests).await;
        if !closing.load(Ordering::SeqCst) {
            warn!(agent = %id.short(), "conexão CDP caiu; encerrando o Chromium");
            kill.notify_one();
        }
    });
    Ok((Handle { tx }, task))
}

impl Controller {
    async fn init(&mut self) -> Result<()> {
        self.cdp
            .call(None, SetDiscoverTargetsParams::new(true))
            .await?;
        let targets = self.cdp.call(None, GetTargetsParams::default()).await?;
        let infos: Vec<TargetInfo> =
            serde_json::from_value(targets["targetInfos"].clone()).unwrap_or_default();
        for info in infos {
            self.add_tab(info);
        }
        self.ensure_active().await;
        Ok(())
    }

    async fn run(
        &mut self,
        mut events: mpsc::UnboundedReceiver<RawEvent>,
        mut requests: mpsc::UnboundedReceiver<(Request, Reply)>,
    ) {
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Some(event) => self.on_event(event).await,
                    None => break,
                },
                req = requests.recv() => match req {
                    Some((req, reply)) => {
                        let result = self.on_request(req).await;
                        let _ = reply.send(result);
                    }
                    None => break,
                },
            }
        }
        debug!(agent = %self.id.short(), "controlador do navegador terminou");
    }

    // ---- eventos ----

    async fn on_event(&mut self, ev: RawEvent) {
        if ev.method != EventScreencastFrame::IDENTIFIER {
            trace!(method = %ev.method, session = ?ev.session_id, params = %ev.params, "CDP ←");
        }
        let from_active = ev.session_id.is_some()
            && ev.session_id.as_deref() == self.active.as_ref().map(|a| a.session.as_str());
        match ev.method.as_ref() {
            "Target.targetCreated" => {
                let Some(info) = parse::<TargetEvent>(ev.params).map(|e| e.target_info) else {
                    return;
                };
                let target_id = info.target_id.clone();
                if self.add_tab(info) {
                    self.activate(&target_id).await;
                    self.ensure_active().await;
                }
            }
            "Target.targetInfoChanged" => {
                let Some(info) = parse::<TargetEvent>(ev.params).map(|e| e.target_info) else {
                    return;
                };
                let active = self.is_active(&info.target_id);
                if let Some(tab) = self.tab_mut(&info.target_id) {
                    tab.url = info.url;
                    tab.title = info.title;
                }
                if active {
                    self.refresh_active_info().await;
                }
            }
            "Target.targetDestroyed" => {
                let Some(TargetIdEvent { target_id }) = parse(ev.params) else {
                    return;
                };
                self.tabs.retain(|t| t.target_id != target_id);
                if self.is_active(&target_id) {
                    // A sessão morreu junto com a aba.
                    self.active = None;
                    self.casting = false;
                    self.emulating = false;
                }
                self.ensure_active().await;
            }
            "Target.detachedFromTarget" => {
                let Some(DetachedEvent { session_id }) = parse(ev.params) else {
                    return;
                };
                let lost = self
                    .active
                    .as_ref()
                    .is_some_and(|a| a.session == session_id);
                if lost {
                    // A aba fechou (o `targetDestroyed` vem depois) ou alguém
                    // nos desanexou: tenta de novo nela e, se não der, na mais
                    // recente que aceitar.
                    let target = self.active.take().map(|a| a.target_id);
                    self.casting = false;
                    self.emulating = false;
                    if let Some(target) = target.filter(|t| self.tab_mut(t).is_some()) {
                        self.activate(&target).await;
                    }
                    self.ensure_active().await;
                }
            }
            EventScreencastFrame::IDENTIFIER => self.on_frame(ev, from_active),
            EventScreencastVisibilityChanged::IDENTIFIER if from_active => {
                let visible =
                    parse::<EventScreencastVisibilityChanged>(ev.params).is_some_and(|e| e.visible);
                if !visible {
                    self.follow_visible_tab().await;
                }
            }
            "Page.frameNavigated" if from_active => {
                let Some(FrameNavigated { frame }) = parse(ev.params) else {
                    return;
                };
                if frame.parent_id.is_none() {
                    self.refresh_active_info().await;
                }
            }
            "Page.navigatedWithinDocument" if from_active => {
                let Some(WithinDocument { frame_id }) = parse(ev.params) else {
                    return;
                };
                // O frame principal tem o id da aba.
                if self.is_active(&frame_id) {
                    self.refresh_active_info().await;
                }
            }
            // O `<title>` lido no carregamento não gera `targetInfoChanged`
            // (só mudanças posteriores de `document.title` geram).
            "Page.domContentEventFired" | "Page.loadEventFired" if from_active => {
                self.refresh_active_info().await;
            }
            _ => {}
        }
    }

    /// Lê URL e título atuais da aba ativa (`Target.getTargetInfo`) e publica
    /// se mudaram. Sempre consulta em vez de confiar no evento: eventos
    /// enfileirados durante uma ativação chegam depois e estariam velhos.
    async fn refresh_active_info(&mut self) {
        let Some(target) = self.active.as_ref().map(|a| a.target_id.clone()) else {
            return;
        };
        let params = GetTargetInfoParams {
            target_id: Some(TargetId::from(target.clone())),
        };
        // Se a consulta falhar, publica o que já se sabe.
        if let Ok(resp) = self.cdp.call(None, params).await
            && let Some(TargetEvent { target_info: info }) = parse(resp)
            && let Some(tab) = self.tab_mut(&target)
        {
            tab.url = info.url;
            tab.title = info.title;
        }
        if self.is_active(&target) {
            self.publish_active_nav();
        }
    }

    fn on_frame(&mut self, ev: RawEvent, from_active: bool) {
        let Some(frame) = parse::<EventScreencastFrame>(ev.params) else {
            return;
        };
        // Sem ack o Chromium para de mandar quadros; vale também para quadros
        // atrasados de uma sessão antiga.
        if let Some(session) = ev.session_id.as_deref() {
            drop(self.cdp.send(
                Some(session),
                ScreencastFrameAckParams::new(frame.session_id),
            ));
        }
        if !self.streaming || !from_active {
            return;
        }
        let b64: &[u8] = frame.data.as_ref();
        let data = match BASE64.decode(b64) {
            Ok(data) => Bytes::from(data),
            Err(e) => {
                debug!(%e, "quadro com base64 inválido");
                return;
            }
        };
        self.seq += 1;
        let m = frame.metadata;
        self.bus.publish(AgentEvent::BrowserFrame {
            id: self.id,
            frame: Arc::new(BrowserFrame {
                seq: self.seq,
                format: ImageFormat::Jpeg,
                data,
                css_width: m.device_width,
                css_height: m.device_height,
                page_scale: m.page_scale_factor,
                offset_top: m.offset_top,
                scroll_x: m.scroll_offset_x,
                scroll_y: m.scroll_offset_y,
            }),
        });
    }

    fn publish_active_nav(&mut self) {
        let Some(active) = &self.active else { return };
        let Some(tab) = self.tabs.iter().find(|t| t.target_id == active.target_id) else {
            return;
        };
        let nav = (tab.url.clone(), tab.title.clone());
        let same_url = self.last_nav.as_ref().is_some_and(|(url, _)| *url == nav.0);
        // Título vazio na mesma URL é só o meio de um recarregamento.
        if self.last_nav.as_ref() == Some(&nav) || (same_url && nav.1.is_empty()) {
            return;
        }
        self.last_nav = Some(nav.clone());
        let (url, title) = nav;
        self.bus.publish(AgentEvent::BrowserNavigated {
            id: self.id,
            url,
            title,
        });
    }

    // ---- abas ----

    /// Registra uma aba nova; `false` se já era conhecida ou não é aba.
    fn add_tab(&mut self, info: TargetInfo) -> bool {
        let is_tab = info.kind == "page"
            && !info.url.starts_with("devtools://")
            && !info.url.starts_with("chrome-extension://");
        if !is_tab || self.tabs.iter().any(|t| t.target_id == info.target_id) {
            return false;
        }
        self.tabs.push(Tab {
            target_id: info.target_id,
            url: info.url,
            title: info.title,
        });
        true
    }

    fn tab_mut(&mut self, target_id: &str) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.target_id == target_id)
    }

    fn is_active(&self, target_id: &str) -> bool {
        self.active
            .as_ref()
            .is_some_and(|a| a.target_id == target_id)
    }

    fn session(&self) -> Result<String> {
        self.active
            .as_ref()
            .map(|a| a.session.clone())
            .ok_or_else(|| Error::Browser("nenhuma aba aberta".to_owned()))
    }

    /// Passa a acompanhar `target_id`: solta a sessão anterior, anexa, liga
    /// `Page`, ajusta a janela e, se for o caso, o screencast.
    async fn activate(&mut self, target_id: &str) {
        if self.is_active(target_id) {
            return;
        }
        self.release_active();
        let session = match self.attach(target_id).await {
            Ok(session) => session,
            Err(e) => {
                debug!(%e, "não consegui anexar à aba");
                return;
            }
        };
        debug!(agent = %self.id.short(), target = target_id, "aba ativa");
        self.active = Some(Active {
            target_id: target_id.to_owned(),
            session: session.clone(),
        });
        if let Err(e) = self
            .cdp
            .call(Some(&session), page::EnableParams::default())
            .await
        {
            debug!(%e, "Page.enable falhou");
        }
        if let Err(e) = self.fit().await {
            debug!(%e, "não consegui ajustar a janela");
        }
        if self.streaming
            && let Err(e) = self.start_cast().await
        {
            warn!(%e, "não consegui iniciar o screencast");
        }
        // A aba pode ter carregado antes do `Page.enable`: lê URL e título agora.
        self.refresh_active_info().await;
    }

    fn release_active(&mut self) {
        let Some(old) = self.active.take() else {
            return;
        };
        if self.casting {
            drop(
                self.cdp
                    .send(Some(&old.session), StopScreencastParams::default()),
            );
        }
        let detach = DetachFromTargetParams {
            session_id: Some(old.session.into()),
        };
        drop(self.cdp.send(None, detach));
        self.casting = false;
        self.emulating = false;
    }

    async fn attach(&self, target_id: &str) -> Result<String> {
        let mut params = AttachToTargetParams::new(TargetId::from(target_id.to_owned()));
        params.flatten = Some(true);
        let resp = self.cdp.call(None, params).await?;
        resp["sessionId"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::Browser("Target.attachToTarget sem sessionId".to_owned()))
    }

    /// Sem aba ativa: pega a mais recente que aceitar sessão (uma aba que
    /// está fechando ainda aparece na lista até o `targetDestroyed`).
    async fn ensure_active(&mut self) {
        let candidates: Vec<String> = self
            .tabs
            .iter()
            .rev()
            .map(|t| t.target_id.clone())
            .collect();
        for target in candidates {
            if self.active.is_some() {
                return;
            }
            self.activate(&target).await;
        }
    }

    /// A aba ativa ficou oculta: acha a que está visível (a mais recente
    /// primeiro) e passa para ela.
    async fn follow_visible_tab(&mut self) {
        // O aviso pode ser transitório (troca de processo na navegação):
        // confirma antes de trocar.
        if let Ok(session) = self.session()
            && self
                .evaluate(&session, "document.visibilityState", PROBE_TIMEOUT)
                .await
                .is_ok_and(|v| v == "visible")
        {
            return;
        }
        let current = self.active.as_ref().map(|a| a.target_id.clone());
        let candidates: Vec<String> = self
            .tabs
            .iter()
            .rev()
            .map(|t| t.target_id.clone())
            .filter(|t| Some(t) != current.as_ref())
            .collect();
        for target in candidates {
            if self.is_visible(&target).await {
                self.activate(&target).await;
                break;
            }
        }
        self.ensure_active().await;
    }

    async fn is_visible(&self, target_id: &str) -> bool {
        let mut params = AttachToTargetParams::new(TargetId::from(target_id.to_owned()));
        params.flatten = Some(true);
        let Ok(resp) = self.cdp.call_within(PROBE_TIMEOUT, None, params).await else {
            return false;
        };
        let Some(session) = resp["sessionId"].as_str() else {
            return false;
        };
        let visible = self
            .evaluate(session, "document.visibilityState", PROBE_TIMEOUT)
            .await
            .is_ok_and(|v| v == "visible");
        let detach = DetachFromTargetParams {
            session_id: Some(session.to_owned().into()),
        };
        drop(self.cdp.send(None, detach));
        visible
    }

    async fn evaluate(&self, session: &str, expression: &str, limit: Duration) -> Result<Value> {
        let mut params = EvaluateParams::new(expression);
        params.return_by_value = Some(true);
        let resp = self.cdp.call_within(limit, Some(session), params).await?;
        Ok(resp["result"]["value"].clone())
    }

    // ---- screencast e tamanho ----

    async fn start_cast(&mut self) -> Result<()> {
        let session = self.session()?;
        let vp = self.settings.viewport;
        let scale = if vp.device_scale_factor > 0.0 {
            vp.device_scale_factor
        } else {
            1.0
        };
        let params = StartScreencastParams {
            format: Some(StartScreencastFormat::Jpeg),
            quality: Some(i64::from(self.settings.quality.min(100))),
            max_width: Some((f64::from(vp.width) * scale).round() as i64),
            max_height: Some((f64::from(vp.height) * scale).round() as i64),
            every_nth_frame: Some(i64::from(self.settings.every_nth_frame.max(1))),
        };
        self.cdp.call(Some(&session), params).await?;
        self.casting = true;
        Ok(())
    }

    async fn stop_cast(&mut self) -> Result<()> {
        if !self.casting {
            return Ok(());
        }
        self.casting = false;
        let session = self.session()?;
        self.cdp
            .call(Some(&session), StopScreencastParams::default())
            .await
            .map(drop)
    }

    /// Ajusta a janela da aba ativa para o viewport pedido.
    ///
    /// `Browser.setWindowBounds` mede a janela, não o viewport: mesmo em
    /// headless há uma moldura (87 px de altura no Chrome 155). Medimos a
    /// moldura uma vez (janela − `innerWidth/innerHeight` da aba inicial) e
    /// somamos. Um `device_scale_factor` diferente do lançamento só dá para
    /// emular (`Emulation.setDeviceMetricsOverride`), e só na aba ativa.
    async fn fit(&mut self) -> Result<()> {
        let session = self.session()?;
        let target = self
            .active
            .as_ref()
            .map(|a| a.target_id.clone())
            .unwrap_or_default();
        let window = self
            .cdp
            .call(
                None,
                GetWindowForTargetParams {
                    target_id: Some(TargetId::from(target)),
                },
            )
            .await?;
        let window_id = window["windowId"].as_i64();
        let bounds = &window["bounds"];
        let (Some(window_id), Some(w), Some(h)) = (
            window_id,
            bounds["width"].as_i64(),
            bounds["height"].as_i64(),
        ) else {
            return Err(Error::Browser(
                "Browser.getWindowForTarget inesperado".to_owned(),
            ));
        };

        let frame = match self.frame_size {
            Some(frame) => frame,
            None => {
                let inner = self
                    .evaluate(&session, "[innerWidth, innerHeight]", PROBE_TIMEOUT)
                    .await?;
                let (Some(iw), Some(ih)) = (inner[0].as_i64(), inner[1].as_i64()) else {
                    return Err(Error::Browser("viewport ilegível".to_owned()));
                };
                let frame = ((w - iw).max(0), (h - ih).max(0));
                self.frame_size = Some(frame);
                frame
            }
        };
        let vp = self.settings.viewport;
        let want = (
            i64::from(vp.width) + frame.0,
            i64::from(vp.height) + frame.1,
        );
        if (w, h) != want {
            let bounds = Bounds {
                left: None,
                top: None,
                width: Some(want.0),
                height: Some(want.1),
                window_state: Some(WindowState::Normal),
            };
            self.cdp
                .call(
                    None,
                    SetWindowBoundsParams::new(WindowId::new(window_id), bounds),
                )
                .await?;
        }

        let scaled = (vp.device_scale_factor - self.launch_scale).abs() > 1e-3
            && vp.device_scale_factor > 0.0;
        if scaled {
            // Largura/altura 0: só a escala; o tamanho continua vindo da janela.
            let params = SetDeviceMetricsOverrideParams::new(0, 0, vp.device_scale_factor, false);
            self.cdp.call(Some(&session), params).await?;
            self.emulating = true;
        } else if self.emulating {
            self.cdp
                .call(Some(&session), ClearDeviceMetricsOverrideParams::default())
                .await?;
            self.emulating = false;
        }
        Ok(())
    }

    // ---- pedidos ----

    async fn on_request(&mut self, req: Request) -> Result<()> {
        // Recupera a aba ativa se uma troca anterior falhou.
        self.ensure_active().await;
        match req {
            Request::Streaming(on) => self.set_streaming(on).await,
            Request::Navigate(nav) => self.navigate(nav).await,
            Request::Input(input) => self.input(input).await,
            Request::Resize(viewport) => self.resize(viewport).await,
        }
    }

    async fn set_streaming(&mut self, on: bool) -> Result<()> {
        self.streaming = on;
        if !on {
            return self.stop_cast().await;
        }
        if self.active.is_some() && !self.casting {
            self.start_cast().await?;
        }
        Ok(())
    }

    async fn navigate(&mut self, nav: Navigation) -> Result<()> {
        let url = match nav {
            Navigation::Url(url) => normalize_url(&url),
            Navigation::Reload => {
                let session = self.session()?;
                return self
                    .cdp
                    .call(Some(&session), ReloadParams::default())
                    .await
                    .map(drop);
            }
            Navigation::Back => return self.go_history(-1).await,
            Navigation::Forward => return self.go_history(1).await,
        };
        let Ok(session) = self.session() else {
            // Sem aba: abre uma; ela vira a ativa pelo `targetCreated`.
            return self
                .cdp
                .call(None, CreateTargetParams::new(url))
                .await
                .map(drop);
        };
        // `Page.navigate` só responde no commit (servidor lento = segundos):
        // não segura a fila do agente; um erro chega como `AgentEvent::Error`.
        let pending = self
            .cdp
            .send(Some(&session), NavigateParams::new(url.clone()));
        let (bus, id) = (self.bus.clone(), self.id);
        tokio::spawn(async move {
            let message = match cdp::wait(NAVIGATION_TIMEOUT, "Page.navigate", pending).await {
                Ok(resp) => match resp["errorText"].as_str() {
                    // Abortada por outra navegação ou download: não é erro.
                    Some(err) if !err.is_empty() && err != "net::ERR_ABORTED" => {
                        format!("navegar para {url}: {err}")
                    }
                    _ => return,
                },
                Err(e) => format!("navegar para {url}: {e}"),
            };
            bus.publish(AgentEvent::Error {
                id: Some(id),
                message,
            });
        });
        Ok(())
    }

    async fn go_history(&mut self, delta: i64) -> Result<()> {
        let session = self.session()?;
        let history = self
            .cdp
            .call(Some(&session), GetNavigationHistoryParams::default())
            .await?;
        let entries = history["entries"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let target = history["currentIndex"].as_i64().map(|i| i + delta);
        let entry = target
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| entries.get(i))
            .and_then(|e| e["id"].as_i64());
        match entry {
            Some(entry_id) => self
                .cdp
                .call(Some(&session), NavigateToHistoryEntryParams::new(entry_id))
                .await
                .map(drop),
            // Sem para onde ir: como o botão desabilitado de um navegador.
            None => Ok(()),
        }
    }

    async fn input(&mut self, input: BrowserInput) -> Result<()> {
        let session = self.session()?;
        let session = Some(session.as_str());
        match input {
            BrowserInput::Mouse(m) => {
                let params = self.mouse.mouse(&m);
                self.cdp.call(session, params).await
            }
            BrowserInput::Wheel(w) => self.cdp.call(session, self.mouse.wheel(&w)).await,
            BrowserInput::Key(k) => self.cdp.call(session, dispatch::key_event(&k)).await,
            BrowserInput::Text(text) => self.cdp.call(session, InsertTextParams::new(text)).await,
        }
        .map(drop)
    }

    async fn resize(&mut self, viewport: Viewport) -> Result<()> {
        if viewport.width == 0 || viewport.height == 0 {
            return Err(Error::Browser(format!(
                "viewport inválido: {}x{}",
                viewport.width, viewport.height
            )));
        }
        self.settings.viewport = viewport;
        if self.active.is_none() {
            return Ok(());
        }
        self.fit().await?;
        if self.casting {
            // Novo tamanho máximo dos quadros.
            self.stop_cast().await?;
            self.start_cast().await?;
        }
        Ok(())
    }
}

/// Endereço digitado → URL navegável: sem esquema vira `http://`.
pub(crate) fn normalize_url(input: &str) -> String {
    const SCHEMES: [&str; 8] = [
        "about:",
        "data:",
        "javascript:",
        "chrome:",
        "blob:",
        "view-source:",
        "file:",
        "mailto:",
    ];
    let s = input.trim();
    if s.is_empty() {
        return "about:blank".to_owned();
    }
    let lower = s.to_ascii_lowercase();
    if s.contains("://") || SCHEMES.iter().any(|p| lower.starts_with(p)) {
        s.to_owned()
    } else {
        format!("http://{s}")
    }
}

fn parse<T: for<'de> Deserialize<'de>>(params: Value) -> Option<T> {
    match serde_json::from_value(params) {
        Ok(v) => Some(v),
        Err(e) => {
            debug!(%e, "evento CDP ilegível");
            None
        }
    }
}

// Decodificação mínima dos eventos: só os campos usados, para não quebrar
// quando o Chromium muda o resto.

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetInfo {
    target_id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetEvent {
    target_info: TargetInfo,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetIdEvent {
    target_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetachedEvent {
    session_id: String,
}

#[derive(Debug, Deserialize)]
struct FrameNavigated {
    frame: NavigatedFrame,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NavigatedFrame {
    #[serde(default)]
    parent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WithinDocument {
    frame_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_without_scheme_get_http() {
        assert_eq!(normalize_url("localhost:3000"), "http://localhost:3000");
        assert_eq!(normalize_url(" example.com/a "), "http://example.com/a");
        assert_eq!(normalize_url("https://x.dev"), "https://x.dev");
        assert_eq!(normalize_url("about:blank"), "about:blank");
        assert_eq!(normalize_url("data:text/html,oi"), "data:text/html,oi");
        assert_eq!(normalize_url("file:///tmp/a.html"), "file:///tmp/a.html");
        assert_eq!(normalize_url(""), "about:blank");
    }

    #[test]
    fn target_info_tolerates_missing_fields() {
        let info: TargetEvent = serde_json::from_value(serde_json::json!({
            "targetInfo": {"targetId": "A", "type": "page", "attached": false, "extra": 1}
        }))
        .unwrap();
        assert_eq!(info.target_info.target_id, "A");
        assert_eq!(info.target_info.url, "");
    }
}
