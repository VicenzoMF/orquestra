//! Um Chromium por agente via CDP: lançamento, screencast e repasse de input.
//! Implementa `orquestra_core::BrowserManager`. Ver `docs/ARCHITECTURE.md`.
//!
//! - **Processo.** Cada agente tem o seu: `--remote-debugging-port=0` (a porta
//!   real vem do stderr ou do `DevToolsActivePort`), `--user-data-dir`
//!   temporário apagado no `close`, headless por padrão. Cookies, storage e
//!   abas de um agente não aparecem para outro.
//! - **CDP.** Uma conexão nossa no WebSocket do navegador, ao lado da do
//!   agente (Playwright MCP). Só a aba ativa ganha sessão nossa; ver
//!   `controller`.
//! - **Resize.** `Browser.setWindowBounds` na janela da aba ativa (funciona
//!   em headless), somando a moldura da janela para o viewport bater. Outro
//!   `device_scale_factor` só por emulação, na aba ativa.
//! - **Status.** `Launching` → `Ready` no `launch`, `Closed` no `close` (ou se
//!   o Chromium sair com sucesso sozinho) e `Failed` se ele não subir ou morrer.
//! - **Encerramento.** `close`/`shutdown`: `Browser.close`, espera, mata se
//!   preciso e apaga o perfil. Descartar o `Chromium` sem `shutdown` ainda mata
//!   os processos (`kill_on_drop`), mas sem esperar, e o perfil pode sobrar; o
//!   primeiro `launch` apaga perfis largados assim (mais de 10 min, sem
//!   Chromium vivo).

mod cdp;
mod controller;
mod discovery;
mod dispatch;
mod process;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::browser::CloseParams;
use orquestra_core::{
    AgentEvent, AgentId, BrowserConfig, BrowserEndpoint, BrowserInput, BrowserManager,
    BrowserStatus, Error, EventBus, Navigation, Result, Viewport,
};
use tempfile::TempDir;
use tokio::process::Child;
use tokio::sync::{Notify, oneshot};
use tokio::task::{AbortHandle, JoinHandle};
use tracing::{info, warn};

use crate::cdp::Cdp;
use crate::controller::{Request, Settings, Wiring};

pub use crate::discovery::{ENV_EXECUTABLE, find_executable};

/// Prazo para a conexão CDP e a descoberta da aba inicial.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// `Browser.close` educado antes de matar.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// Espera pela saída do processo depois do `Browser.close`.
const EXIT_GRACE: Duration = Duration::from_secs(3);
/// Perfis largados por execuções que morreram são apagados depois disso.
const STALE_PROFILE_AGE: Duration = Duration::from_secs(10 * 60);

/// Gerente dos Chromium dos agentes. `Sync`: o hub chama agentes diferentes
/// em paralelo; cada agente tem seu controlador, então um não espera o outro.
pub struct Chromium {
    shared: Arc<Shared>,
}

/// Diagnóstico de um Chromium aberto (testes, painel de recursos).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserProcess {
    pub endpoint: BrowserEndpoint,
    pub pid: u32,
    /// Perfil temporário (`--user-data-dir`), apagado no `close`.
    pub profile_dir: PathBuf,
}

struct Shared {
    bus: EventBus,
    config: BrowserConfig,
    agents: Mutex<HashMap<AgentId, Instance>>,
    generation: AtomicU64,
    /// A faxina de perfis largados já rodou.
    swept: AtomicBool,
}

/// Um Chromium vivo.
struct Instance {
    /// Distingue relançamentos do mesmo agente.
    generation: u64,
    endpoint: BrowserEndpoint,
    pid: u32,
    cdp: Cdp,
    ctl: controller::Handle,
    closing: Arc<AtomicBool>,
    kill: Arc<Notify>,
    /// Termina quando o processo sai.
    monitor: JoinHandle<()>,
    /// Abortadas ao descartar a instância (o monitor junto: o `Child` cai e
    /// o `kill_on_drop` mata o processo).
    tasks: Tasks,
    profile: TempDir,
}

struct Tasks(Vec<AbortHandle>);

impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

impl Chromium {
    pub fn new(bus: EventBus, config: BrowserConfig) -> Self {
        Self {
            shared: Arc::new(Shared {
                bus,
                config,
                agents: Mutex::new(HashMap::new()),
                generation: AtomicU64::new(0),
                swept: AtomicBool::new(false),
            }),
        }
    }

    /// Processo, porta e perfil do Chromium do agente, se aberto.
    pub fn process(&self, id: AgentId) -> Option<BrowserProcess> {
        self.shared.agents().get(&id).map(|i| BrowserProcess {
            endpoint: i.endpoint.clone(),
            pid: i.pid,
            profile_dir: i.profile.path().to_owned(),
        })
    }

    /// Agentes com Chromium aberto.
    pub fn open(&self) -> Vec<AgentId> {
        self.shared.agents().keys().copied().collect()
    }

    fn handle(&self, id: AgentId) -> Result<controller::Handle> {
        self.shared
            .agents()
            .get(&id)
            .map(|i| i.ctl.clone())
            .ok_or_else(|| Error::Browser("nenhum navegador aberto para este agente".to_owned()))
    }

    async fn request(&self, id: AgentId, req: Request) -> Result<()> {
        self.handle(id)?.request(req).await
    }
}

impl Shared {
    fn agents(&self) -> MutexGuard<'_, HashMap<AgentId, Instance>> {
        // Ninguém entra em pânico segurando o lock; se acontecer, o mapa
        // continua consistente (só inserções e remoções inteiras).
        self.agents.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self, id: AgentId, status: BrowserStatus) {
        self.bus.publish(AgentEvent::BrowserStatus { id, status });
    }
}

impl BrowserManager for Chromium {
    async fn launch(&self, id: AgentId) -> Result<BrowserEndpoint> {
        if let Some(i) = self.shared.agents().get(&id) {
            return Ok(i.endpoint.clone());
        }
        self.shared.publish(id, BrowserStatus::Launching);
        if !self.shared.swept.swap(true, Ordering::Relaxed) {
            tokio::task::spawn_blocking(|| {
                process::sweep_stale_profiles(&std::env::temp_dir(), STALE_PROFILE_AGE)
            });
        }
        match start(&self.shared, id).await {
            Ok((instance, inserted)) => {
                let endpoint = instance.endpoint.clone();
                info!(agent = %id.short(), port = endpoint.port, pid = instance.pid, "Chromium pronto");
                self.shared.agents().insert(id, instance);
                let _ = inserted.send(());
                self.shared.publish(
                    id,
                    BrowserStatus::Ready {
                        endpoint: endpoint.clone(),
                    },
                );
                Ok(endpoint)
            }
            Err(reason) => {
                warn!(agent = %id.short(), %reason, "Chromium não subiu");
                self.shared.publish(
                    id,
                    BrowserStatus::Failed {
                        reason: reason.clone(),
                    },
                );
                Err(Error::Browser(reason))
            }
        }
    }

    async fn close(&self, id: AgentId) -> Result<()> {
        let Some(instance) = self.shared.agents().remove(&id) else {
            return Ok(());
        };
        stop(instance).await;
        self.shared.publish(id, BrowserStatus::Closed);
        Ok(())
    }

    async fn set_streaming(&self, id: AgentId, on: bool) -> Result<()> {
        match self.handle(id) {
            Ok(handle) => handle.request(Request::Streaming(on)).await,
            // Desligar o que não existe não é erro (painel fechando).
            Err(_) if !on => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn navigate(&self, id: AgentId, nav: Navigation) -> Result<()> {
        self.request(id, Request::Navigate(nav)).await
    }

    async fn input(&self, id: AgentId, input: BrowserInput) -> Result<()> {
        self.request(id, Request::Input(input)).await
    }

    async fn resize(&self, id: AgentId, viewport: Viewport) -> Result<()> {
        self.request(id, Request::Resize(viewport)).await
    }

    async fn shutdown(&self) {
        let instances: Vec<_> = self.shared.agents().drain().collect();
        futures::future::join_all(instances.into_iter().map(|(id, instance)| async move {
            stop(instance).await;
            self.shared.publish(id, BrowserStatus::Closed);
        }))
        .await;
    }
}

/// Sobe processo, conexão e controlador. O `Sender` avisa o monitor de que a
/// instância já está no mapa (ou que o lançamento desistiu, se cair).
async fn start(
    shared: &Arc<Shared>,
    id: AgentId,
) -> Result<(Instance, oneshot::Sender<()>), String> {
    let config = &shared.config;
    let exe = find_executable(config).ok_or_else(|| {
        format!(
            "Chromium não encontrado: configure browser.executable ou ${ENV_EXECUTABLE}, \
             ou instale chromium/google-chrome"
        )
    })?;
    let process::Process {
        mut child,
        ws_url,
        port,
        profile,
    } = process::spawn(&exe, config).await?;
    let pid = child.id().unwrap_or_default();

    let closing = Arc::new(AtomicBool::new(false));
    let kill = Arc::new(Notify::new());
    let connected = tokio::time::timeout(CONNECT_TIMEOUT, async {
        let (cdp, events, driver) = Cdp::connect(&ws_url).await?;
        let wiring = Wiring {
            id,
            bus: shared.bus.clone(),
            cdp: cdp.clone(),
            events,
            closing: closing.clone(),
            kill: kill.clone(),
        };
        let settings = Settings {
            quality: config.screencast_quality,
            every_nth_frame: config.screencast_every_nth_frame,
            viewport: config.viewport,
        };
        match controller::start(wiring, settings).await {
            Ok((ctl, task)) => Ok((cdp, ctl, driver, task)),
            Err(e) => {
                driver.abort();
                Err(e.to_string())
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        Err(format!(
            "Chromium não respondeu ao CDP em {CONNECT_TIMEOUT:?}"
        ))
    });

    let (cdp, ctl, driver, task) = match connected {
        Ok(parts) => parts,
        Err(reason) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            tokio::task::spawn_blocking(move || process::remove_profile(profile));
            return Err(reason);
        }
    };

    let (inserted_tx, inserted_rx) = oneshot::channel();
    let generation = shared.generation.fetch_add(1, Ordering::Relaxed);
    let monitor = tokio::spawn(monitor(
        Arc::downgrade(shared),
        id,
        generation,
        child,
        kill.clone(),
        closing.clone(),
        inserted_rx,
    ));
    let tasks = Tasks(vec![
        driver.abort_handle(),
        task.abort_handle(),
        monitor.abort_handle(),
    ]);
    let instance = Instance {
        generation,
        endpoint: BrowserEndpoint { port, ws_url },
        pid,
        cdp,
        ctl,
        closing,
        kill,
        monitor,
        tasks,
        profile,
    };
    Ok((instance, inserted_tx))
}

/// Fecha um Chromium: `Browser.close`, espera sair (ou mata) e apaga o perfil.
async fn stop(instance: Instance) {
    let Instance {
        cdp,
        closing,
        kill,
        mut monitor,
        tasks,
        profile,
        ..
    } = instance;
    closing.store(true, Ordering::SeqCst);
    let _ = cdp
        .call_within(CLOSE_GRACE, None, CloseParams::default())
        .await;
    if tokio::time::timeout(EXIT_GRACE, &mut monitor)
        .await
        .is_err()
    {
        kill.notify_one();
        let _ = monitor.await;
    }
    drop(tasks);
    let _ = tokio::task::spawn_blocking(move || process::remove_profile(profile)).await;
}

/// Espera o processo sair. Saída não pedida: tira a instância do mapa,
/// publica `Closed` (código 0) ou `Failed` e apaga o perfil.
async fn monitor(
    shared: Weak<Shared>,
    id: AgentId,
    generation: u64,
    mut child: Child,
    kill: Arc<Notify>,
    closing: Arc<AtomicBool>,
    inserted: oneshot::Receiver<()>,
) {
    let status = tokio::select! {
        status = child.wait() => status,
        () = kill.notified() => {
            let _ = child.start_kill();
            child.wait().await
        }
    };
    if closing.load(Ordering::SeqCst) {
        return;
    }
    // Se o lançamento ainda não pôs a instância no mapa, espera.
    let _ = inserted.await;
    let Some(shared) = shared.upgrade() else {
        return;
    };
    let instance = {
        let mut agents = shared.agents();
        match agents.get(&id) {
            Some(i) if i.generation == generation && !i.closing.load(Ordering::SeqCst) => {
                agents.remove(&id)
            }
            _ => None,
        }
    };
    let Some(instance) = instance else { return };
    let status = match status {
        Ok(s) if s.success() => BrowserStatus::Closed,
        Ok(s) => BrowserStatus::Failed {
            reason: format!("Chromium saiu: {s}"),
        },
        Err(e) => BrowserStatus::Failed {
            reason: format!("Chromium: {e}"),
        },
    };
    warn!(agent = %id.short(), ?status, "Chromium saiu sem pedido");
    shared.publish(id, status);
    let Instance { profile, tasks, .. } = instance;
    tokio::task::spawn_blocking(move || process::remove_profile(profile));
    // Por último: aborta as tasks da instância, inclusive esta.
    drop(tasks);
}
