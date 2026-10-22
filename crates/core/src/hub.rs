//! Roteador entre a fila de comandos e os serviços.
//!
//! - **Registro**: o hub conhece as sessões e o status de cada agente (assina
//!   o próprio `EventBus`). Com isso aplica o limite de workers simultâneos e
//!   a aprovação de spawns: o que não pode subir vai para a fila (`Queued`) e
//!   sai quando abre vaga ou o humano aprova.
//! - **Lanes**: cada agente tem uma task com fila própria que executa seus
//!   comandos em ordem (teclas nunca trocam de ordem; um `Kill` logo após o
//!   `Spawn` espera o spawn terminar). Agentes diferentes rodam em paralelo.
//! - **Recorder**: thread dedicada que assina o canal principal, repassa tudo
//!   ao `Store` e transforma transições de status em notificações.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::{Arc, PoisonError, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinSet;
use tracing::warn;

use crate::{
    AgentConfig, AgentEvent, AgentId, AgentInfo, AgentRecord, AgentRole, AgentRunner, AgentStatus,
    AppConfig, BrowserManager, Command, CommandReceiver, Error, EventBus, EventReceiver,
    McpServerTemplate, Navigation, NotificationConfig, Notifier, OrchestrationConfig,
    OrchestratorGateway, QueueReason, Result, SessionConfig, SessionId, SpawnRequest, Store,
    Workspace, WorktreeCleanup, notification_for,
};

/// Janela em que a primeira espera de um agente reaberto não notifica.
const RESUME_QUIET: Duration = Duration::from_secs(30);

/// Quanto da saída gravada volta ao terminal de um agente reaberto sem processo.
const RESTORED_REPLAY_BYTES: usize = 512 * 1024;

/// Tempo que as lanes têm para esvaziar a fila no shutdown antes de serem abortadas.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

pub struct Services<R, B, S, N, G> {
    pub runner: R,
    pub browser: B,
    pub store: S,
    pub notifier: N,
    pub gateway: G,
}

/// A parte da config que o hub usa.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HubConfig {
    pub notifications: NotificationConfig,
    pub orchestration: OrchestrationConfig,
    /// MCP registrado em todo agente que tem navegador.
    pub browser_mcp: Option<McpServerTemplate>,
}

impl HubConfig {
    pub fn from_app(config: &AppConfig) -> Self {
        Self {
            notifications: config.notifications.clone(),
            orchestration: config.orchestration.clone(),
            browser_mcp: config.browser.agent_mcp.clone(),
        }
    }
}

pub struct Hub<R, B, S, N, G> {
    inner: Arc<Inner<R, B, S, N, G>>,
    recorder_rx: EventReceiver,
    events_rx: EventReceiver,
    orchestration: OrchestrationConfig,
}

struct Inner<R, B, S, N, G> {
    services: Services<R, B, S, N, G>,
    bus: EventBus,
    notifications: RwLock<NotificationConfig>,
    browser_mcp: Option<McpServerTemplate>,
}

impl<R, B, S, N, G> Hub<R, B, S, N, G>
where
    R: AgentRunner,
    B: BrowserManager,
    S: Store,
    N: Notifier,
    G: OrchestratorGateway,
{
    /// `bus` deve ser o mesmo entregue aos serviços. O hub e o recorder
    /// assinam aqui, então nada publicado depois do `new` lhes escapa.
    pub fn new(services: Services<R, B, S, N, G>, bus: EventBus, config: HubConfig) -> Self {
        let recorder_rx = bus.subscribe();
        let events_rx = bus.subscribe();
        let inner = Inner {
            services,
            bus,
            notifications: RwLock::new(config.notifications),
            browser_mcp: config.browser_mcp,
        };
        Self {
            inner: Arc::new(inner),
            recorder_rx,
            events_rx,
            orchestration: config.orchestration,
        }
    }

    /// Processa comandos até `Command::Shutdown` ou até todos os
    /// `CommandSender` caírem; então encerra os serviços e retorna.
    pub async fn run(self, mut commands: CommandReceiver) {
        let Self {
            inner,
            recorder_rx,
            mut events_rx,
            orchestration,
        } = self;
        let recorder = inner.clone().spawn_recorder(recorder_rx);
        let mut router = Router::new(inner.clone(), orchestration);

        loop {
            tokio::select! {
                event = events_rx.recv() => match event {
                    Ok(event) => router.on_event(&event),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(n, "hub atrasou na leitura de eventos; vagas podem ficar defasadas");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                cmd = commands.recv() => match cmd {
                    Some(cmd) => {
                        if router.on_command(cmd).is_break() {
                            break;
                        }
                    }
                    None => break,
                },
            }
            router.reap();
        }

        router.drain().await;
        // O store para de gravar antes de os agentes morrerem: o banco fica
        // com cada um como estava, e a próxima abertura os reabre (como
        // depois de uma queda). Sem o recorder, o fim deles também não vira
        // notificação.
        inner.bus.publish(AgentEvent::Shutdown);
        let joined = tokio::time::timeout(
            SHUTDOWN_GRACE,
            tokio::task::spawn_blocking(move || recorder.join()),
        )
        .await;
        if joined.is_err() {
            warn!("recorder não terminou a tempo");
        }
        inner.services.runner.shutdown().await;
        inner.services.browser.shutdown().await;
    }
}

enum LaneCmd {
    /// Com as instruções escondidas de quem pode criar agentes.
    Spawn(Box<AgentConfig>, Option<String>),
    Run(Command),
}

struct Entry {
    config: Arc<AgentConfig>,
    status: AgentStatus,
    /// `Some` enquanto o spawn está na fila.
    queued: Option<Pending>,
}

#[derive(Clone, Copy)]
struct Pending {
    needs_approval: bool,
}

/// Estado do hub; vive na task de `Hub::run`, sem locks.
struct Router<R, B, S, N, G> {
    inner: Arc<Inner<R, B, S, N, G>>,
    lanes: HashMap<AgentId, mpsc::UnboundedSender<LaneCmd>>,
    tasks: JoinSet<()>,
    sessions: HashMap<SessionId, Arc<SessionConfig>>,
    agents: HashMap<AgentId, Entry>,
    /// Ordem de chegada dos spawns na fila.
    queue: Vec<AgentId>,
    /// Limite de vagas e as instruções de quem pode criar agentes.
    orchestration: OrchestrationConfig,
}

impl<R, B, S, N, G> Router<R, B, S, N, G>
where
    R: AgentRunner,
    B: BrowserManager,
    S: Store,
    N: Notifier,
    G: OrchestratorGateway,
{
    fn new(inner: Arc<Inner<R, B, S, N, G>>, orchestration: OrchestrationConfig) -> Self {
        Self {
            inner,
            lanes: HashMap::new(),
            tasks: JoinSet::new(),
            sessions: HashMap::new(),
            agents: HashMap::new(),
            queue: Vec::new(),
            orchestration,
        }
    }

    fn on_command(&mut self, cmd: Command) -> ControlFlow<()> {
        match cmd {
            Command::Shutdown => return ControlFlow::Break(()),
            Command::LoadHistory { limit } => {
                self.tasks.spawn(self.inner.clone().load_history(limit));
            }
            Command::SaveConfig(config) => {
                self.orchestration = config.orchestration.clone();
                self.tasks.spawn(self.inner.clone().save_config(config));
                self.try_dequeue();
            }
            Command::CreateSession { id, config } => self.create_session(id, *config),
            Command::CloseSession { id, cleanup } => self.close_session(id, cleanup),
            Command::Spawn { id, config } => self.spawn(id, *config),
            Command::Restore { record } => self.restore(*record),
            Command::ApproveSpawn { id } => self.approve(id),
            Command::RejectSpawn { id } => self.reject(id),
            Command::Remove { id, cleanup } => self.remove(id, cleanup),
            cmd => {
                let Some(id) = cmd.agent_id() else {
                    return ControlFlow::Continue(());
                };
                if !self.is_queued(id) {
                    self.send_lane(id, LaneCmd::Run(cmd));
                } else if matches!(cmd, Command::Kill { .. }) {
                    self.unqueue(id);
                    self.set_status(id, AgentStatus::Done { exit_code: None });
                } else {
                    self.error(
                        Some(id),
                        format!("{}: o agente ainda está na fila", cmd.name()),
                    );
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn on_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::StatusChanged { id, status } => {
                // `Queued`/`Starting` saem do próprio hub, que já atualizou o
                // registro; reaplicar aqui atrasado desfaria um dequeue recente.
                if matches!(status, AgentStatus::Queued { .. } | AgentStatus::Starting) {
                    return;
                }
                let Some(entry) = self.agents.get_mut(id) else {
                    return;
                };
                if entry.queued.is_some() {
                    return;
                }
                entry.status = status.clone();
                if status.is_terminal() {
                    self.try_dequeue();
                }
            }
            AgentEvent::Removed { id } => {
                self.agents.remove(id);
                self.queue.retain(|q| q != id);
                self.try_dequeue();
            }
            _ => {}
        }
    }

    fn create_session(&mut self, id: SessionId, config: SessionConfig) {
        if self.sessions.contains_key(&id) {
            return self.error(None, format!("create_session: sessão já existe: {id}"));
        }
        let config = Arc::new(config);
        self.sessions.insert(id, config.clone());
        self.publish(AgentEvent::SessionCreated { id, config });
    }

    /// `SessionClosed` sai na hora; os `Removed` chegam conforme cada agente
    /// termina de ser removido.
    fn close_session(&mut self, id: SessionId, cleanup: WorktreeCleanup) {
        if self.sessions.remove(&id).is_none() {
            return self.error(None, format!("close_session: sessão desconhecida: {id}"));
        }
        let members: Vec<AgentId> = self
            .agents
            .iter()
            .filter(|(_, e)| e.config.session == id)
            .map(|(agent, _)| *agent)
            .collect();
        for agent in members {
            self.remove(agent, cleanup);
        }
        self.publish(AgentEvent::SessionClosed { id });
    }

    fn spawn(&mut self, id: AgentId, config: AgentConfig) {
        if self.agents.contains_key(&id) {
            return self.error(Some(id), format!("spawn: {}", Error::AgentExists(id)));
        }
        let Some(session) = self.sessions.get(&config.session) else {
            return self.error(
                Some(id),
                format!("spawn: sessão desconhecida: {}", config.session),
            );
        };
        // Reabrir não é criar: já foi aprovado na execução anterior.
        let needs_approval = config.role == AgentRole::Worker
            && config.parent.is_some()
            && session.approve_spawns
            && !config.workspace.resumes();
        let reason = if needs_approval {
            QueueReason::Approval
        } else {
            QueueReason::Capacity
        };

        let config = Arc::new(config);
        self.publish(AgentEvent::Created {
            id,
            config: config.clone(),
        });
        self.agents.insert(
            id,
            Entry {
                config,
                status: AgentStatus::Queued { reason },
                queued: Some(Pending { needs_approval }),
            },
        );
        self.queue.push(id);
        if !self.try_start(id) {
            self.publish(AgentEvent::StatusChanged {
                id,
                status: AgentStatus::Queued { reason },
            });
        }
    }

    fn restore(&mut self, record: AgentRecord) {
        let AgentRecord {
            id,
            mut config,
            info,
            status,
            ..
        } = record;
        if status.is_terminal() {
            return self.restore_finished(id, config, info, status);
        }
        // Já tinha processo: volta onde estava, com a conversa. Sem `info`
        // (na fila, ou caiu antes de subir), começa como da primeira vez.
        if let Some(info) = info {
            config.workspace = Workspace::Restore {
                path: info.workdir,
                worktree: info.worktree,
            };
        }
        self.spawn(id, config);
    }

    fn restore_finished(
        &mut self,
        id: AgentId,
        config: AgentConfig,
        info: Option<AgentInfo>,
        status: AgentStatus,
    ) {
        if self.agents.contains_key(&id) {
            return self.error(Some(id), format!("restore: {}", Error::AgentExists(id)));
        }
        if !self.sessions.contains_key(&config.session) {
            return self.error(
                Some(id),
                format!("restore: sessão desconhecida: {}", config.session),
            );
        }
        let config = Arc::new(config);
        self.publish(AgentEvent::Created {
            id,
            config: config.clone(),
        });
        if let Some(info) = info {
            self.publish(AgentEvent::Spawned {
                info: Arc::new(info),
            });
        }
        self.agents.insert(
            id,
            Entry {
                config,
                status: status.clone(),
                queued: None,
            },
        );
        self.publish(AgentEvent::StatusChanged { id, status });
        self.tasks.spawn(self.inner.clone().replay_stored(id));
    }

    fn approve(&mut self, id: AgentId) {
        match self.agents.get_mut(&id).and_then(|e| e.queued.as_mut()) {
            Some(pending) if pending.needs_approval => {
                pending.needs_approval = false;
                if !self.try_start(id) {
                    self.set_status(
                        id,
                        AgentStatus::Queued {
                            reason: QueueReason::Capacity,
                        },
                    );
                }
            }
            _ => self.error(
                Some(id),
                "approve_spawn: o agente não está aguardando aprovação".to_owned(),
            ),
        }
    }

    fn reject(&mut self, id: AgentId) {
        let awaiting = self
            .agents
            .get(&id)
            .and_then(|e| e.queued)
            .is_some_and(|p| p.needs_approval);
        if !awaiting {
            return self.error(
                Some(id),
                "reject_spawn: o agente não está aguardando aprovação".to_owned(),
            );
        }
        self.unqueue(id);
        self.agents.remove(&id);
        self.publish(AgentEvent::Removed { id });
    }

    fn remove(&mut self, id: AgentId, cleanup: WorktreeCleanup) {
        if self.is_queued(id) {
            self.unqueue(id);
            self.agents.remove(&id);
            self.publish(AgentEvent::Removed { id });
            return;
        }
        self.send_lane(id, LaneCmd::Run(Command::Remove { id, cleanup }));
        // A lane esvazia a fila (Remove por último) e termina.
        self.lanes.remove(&id);
    }

    /// Tira da fila e manda subir, se puder. `true` se saiu da fila.
    fn try_start(&mut self, id: AgentId) -> bool {
        let Some(entry) = self.agents.get(&id) else {
            return false;
        };
        let Some(pending) = entry.queued else {
            return false;
        };
        let worker = entry.config.role == AgentRole::Worker;
        let session = entry.config.session;
        if pending.needs_approval || (worker && !self.has_capacity(session)) {
            return false;
        }

        self.queue.retain(|q| *q != id);
        let Some(entry) = self.agents.get_mut(&id) else {
            return false;
        };
        entry.queued = None;
        entry.status = AgentStatus::Starting;
        let config = Box::new((*entry.config).clone());
        let instructions = (config.role == AgentRole::Orchestrator)
            .then(|| self.sessions.get(&session))
            .flatten()
            .map(|s| self.orchestration.render_delegation(s));
        self.publish(AgentEvent::StatusChanged {
            id,
            status: AgentStatus::Starting,
        });
        self.send_lane(id, LaneCmd::Spawn(config, instructions));
        true
    }

    fn try_dequeue(&mut self) {
        for id in self.queue.clone() {
            self.try_start(id);
        }
    }

    /// Há vaga para mais um worker na sessão (limite global e da sessão).
    fn has_capacity(&self, session: SessionId) -> bool {
        let session_max = self.sessions.get(&session).and_then(|s| s.max_workers);
        let (mut total, mut in_session) = (0u32, 0u32);
        for e in self.agents.values() {
            if e.config.role == AgentRole::Worker && e.queued.is_none() && e.status.occupies_slot()
            {
                total += 1;
                if e.config.session == session {
                    in_session += 1;
                }
            }
        }
        total < self.orchestration.max_concurrent_workers
            && session_max.is_none_or(|max| in_session < max)
    }

    fn is_queued(&self, id: AgentId) -> bool {
        self.agents.get(&id).is_some_and(|e| e.queued.is_some())
    }

    fn unqueue(&mut self, id: AgentId) {
        self.queue.retain(|q| *q != id);
        if let Some(entry) = self.agents.get_mut(&id) {
            entry.queued = None;
        }
    }

    fn set_status(&mut self, id: AgentId, status: AgentStatus) {
        if let Some(entry) = self.agents.get_mut(&id) {
            entry.status = status.clone();
        }
        self.publish(AgentEvent::StatusChanged { id, status });
    }

    fn send_lane(&mut self, id: AgentId, cmd: LaneCmd) {
        let Self {
            inner,
            lanes,
            tasks,
            ..
        } = self;
        let lane = lanes.entry(id).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tasks.spawn(inner.clone().run_lane(id, rx));
            tx
        });
        let _ = lane.send(cmd);
    }

    fn publish(&self, event: AgentEvent) {
        self.inner.bus.publish(event);
    }

    fn error(&self, id: Option<AgentId>, message: String) {
        self.inner.publish_error(id, message);
    }

    fn reap(&mut self) {
        while self.tasks.try_join_next().is_some() {}
    }

    async fn drain(mut self) {
        self.lanes.clear();
        let tasks = &mut self.tasks;
        let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            warn!("lanes não terminaram a tempo; abortando");
            self.tasks.shutdown().await;
        }
    }
}

impl<R, B, S, N, G> Inner<R, B, S, N, G>
where
    R: AgentRunner,
    B: BrowserManager,
    S: Store,
    N: Notifier,
    G: OrchestratorGateway,
{
    async fn run_lane(self: Arc<Self>, id: AgentId, mut rx: mpsc::UnboundedReceiver<LaneCmd>) {
        while let Some(cmd) = rx.recv().await {
            let (name, result) = match cmd {
                LaneCmd::Spawn(config, instructions) => {
                    ("spawn", self.spawn_agent(id, *config, instructions).await)
                }
                LaneCmd::Run(cmd) => (cmd.name(), self.handle(id, cmd).await),
            };
            if let Err(err) = result {
                warn!(agent = %id.short(), command = name, %err, "comando falhou");
                self.publish_error(Some(id), format!("{name}: {err}"));
            }
        }
    }

    async fn handle(&self, id: AgentId, cmd: Command) -> Result<()> {
        let Services {
            runner,
            browser,
            gateway,
            ..
        } = &self.services;
        match cmd {
            Command::SendInput { data, .. } => runner.send_input(id, data).await,
            Command::Resize { size, .. } => runner.resize(id, size).await,
            Command::Kill { .. } => runner.kill(id).await,
            Command::Remove { cleanup, .. } => {
                let removed = ignore_unknown(runner.remove(id, cleanup).await);
                let closed = browser.close(id).await;
                gateway.revoke(id);
                self.bus.publish(AgentEvent::Removed { id });
                removed.and(closed)
            }
            Command::RequestReplay { .. } => runner.request_replay(id).await,
            Command::OpenBrowser { url, .. } => {
                browser.launch(id).await?;
                browser.set_streaming(id, true).await?;
                match url {
                    Some(url) => browser.navigate(id, Navigation::Url(url)).await,
                    None => Ok(()),
                }
            }
            Command::CloseBrowser { .. } => browser.close(id).await,
            Command::SetBrowserVisible { visible, .. } => browser.set_streaming(id, visible).await,
            Command::BrowserInput { input, .. } => browser.input(id, input).await,
            Command::BrowserNavigate { nav, .. } => browser.navigate(id, nav).await,
            Command::BrowserResize { viewport, .. } => browser.resize(id, viewport).await,
            // Tratados pelo `Router`; nunca chegam a uma lane.
            Command::CreateSession { .. }
            | Command::CloseSession { .. }
            | Command::Spawn { .. }
            | Command::Restore { .. }
            | Command::ApproveSpawn { .. }
            | Command::RejectSpawn { .. }
            | Command::LoadHistory { .. }
            | Command::SaveConfig(_)
            | Command::Shutdown => Ok(()),
        }
    }

    /// [navegador] → [credencial do gateway] → runner.spawn → `Spawned`.
    /// O runner pode publicar `StatusChanged`/`Output` antes do `Spawned`.
    async fn spawn_agent(
        &self,
        id: AgentId,
        config: AgentConfig,
        instructions: Option<String>,
    ) -> Result<()> {
        let Services {
            runner,
            browser,
            gateway,
            ..
        } = &self.services;

        let endpoint = if config.browser {
            match browser.launch(id).await {
                Ok(endpoint) => Some(endpoint),
                Err(err) => {
                    // O agente sobe mesmo assim, só sem navegador.
                    self.publish_error(Some(id), format!("spawn: seguindo sem navegador ({err})"));
                    None
                }
            }
        } else {
            None
        };
        let had_browser = endpoint.is_some();
        let orchestrator = config.role == AgentRole::Orchestrator;

        let mut req = SpawnRequest {
            id,
            config,
            browser: endpoint,
            mcp_servers: Vec::new(),
            env: BTreeMap::new(),
            stop_hook: None,
            instructions,
        };
        if let (Some(endpoint), Some(template)) = (&req.browser, &self.browser_mcp) {
            req.mcp_servers.push(template.render(endpoint));
        }

        let result = async move {
            if orchestrator {
                let grant = gateway.grant(req.config.session, id)?;
                req.mcp_servers.push(grant.mcp);
                req.env.extend(grant.env);
                req.stop_hook = grant.stop_hook;
            }
            runner.spawn(req).await
        }
        .await;

        match result {
            Ok(info) => {
                self.bus.publish(AgentEvent::Spawned {
                    info: Arc::new(info),
                });
                Ok(())
            }
            Err(err) => {
                if had_browser {
                    let _ = browser.close(id).await;
                }
                if orchestrator {
                    gateway.revoke(id);
                }
                self.bus.publish(AgentEvent::StatusChanged {
                    id,
                    status: AgentStatus::Failed {
                        reason: err.to_string(),
                    },
                });
                Err(err)
            }
        }
    }

    async fn load_history(self: Arc<Self>, limit: usize) {
        let inner = self.clone();
        match tokio::task::spawn_blocking(move || inner.services.store.history(limit)).await {
            Ok(Ok(history)) => self.bus.publish(AgentEvent::History(Arc::new(history))),
            Ok(Err(err)) => self.publish_error(None, format!("load_history: {err}")),
            Err(err) => self.publish_error(None, format!("load_history: {err}")),
        }
    }

    /// O terminal de um agente reaberto sem processo: o fim da saída gravada.
    async fn replay_stored(self: Arc<Self>, id: AgentId) {
        let inner = self.clone();
        let read = tokio::task::spawn_blocking(move || {
            inner.services.store.output_log(id, RESTORED_REPLAY_BYTES)
        })
        .await;
        match read {
            Ok(Ok(data)) if !data.is_empty() => self.bus.publish(AgentEvent::Replay {
                id,
                next_seq: 0,
                data: data.into(),
            }),
            Ok(Ok(_)) => {}
            Ok(Err(err)) => self.publish_error(Some(id), format!("restore: {err}")),
            Err(err) => self.publish_error(Some(id), format!("restore: {err}")),
        }
    }

    async fn save_config(self: Arc<Self>, config: Box<AppConfig>) {
        *self
            .notifications
            .write()
            .unwrap_or_else(PoisonError::into_inner) = config.notifications.clone();
        let inner = self.clone();
        match tokio::task::spawn_blocking(move || inner.services.store.save_config(&config)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => self.publish_error(None, format!("save_config: {err}")),
            Err(err) => self.publish_error(None, format!("save_config: {err}")),
        }
    }

    fn spawn_recorder(self: Arc<Self>, rx: EventReceiver) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("orquestra-recorder".to_owned())
            .spawn(move || self.record_loop(rx))
            .expect("não foi possível criar a thread do recorder")
    }

    fn record_loop(&self, mut rx: EventReceiver) {
        let mut names: HashMap<AgentId, String> = HashMap::new();
        let mut last: HashMap<AgentId, AgentStatus> = HashMap::new();
        // Pai de cada agente criado por outro, e quem é orquestrador.
        let mut parents: HashMap<AgentId, AgentId> = HashMap::new();
        let mut orchestrators: HashSet<AgentId> = HashSet::new();
        // Reabertos, desde quando: voltam todos parados, e a primeira espera
        // logo depois de reabrir não é novidade para notificar.
        let mut resumed: HashMap<AgentId, std::time::Instant> = HashMap::new();
        loop {
            let event = match rx.blocking_recv() {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(n, "recorder atrasou; eventos não gravados");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if let Err(err) = self.services.store.record(&event) {
                warn!(%err, "store.record falhou");
            }
            match &event {
                AgentEvent::Created { id, config } => {
                    names.insert(*id, config.name.clone());
                    if let Some(parent) = config.parent {
                        parents.insert(*id, parent);
                    }
                    if config.role == AgentRole::Orchestrator {
                        orchestrators.insert(*id);
                    }
                    if config.workspace.resumes() {
                        resumed.insert(*id, std::time::Instant::now());
                    }
                }
                AgentEvent::StatusChanged { id, status } => {
                    let prev = last.insert(*id, status.clone());
                    // Orquestrador que parou com filhos trabalhando espera os
                    // agentes, não o humano: quem precisar avisa por si.
                    let watching = *status == AgentStatus::WaitingForInput
                        && orchestrators.contains(id)
                        && parents.iter().any(|(child, parent)| {
                            parent == id && last.get(child).is_none_or(AgentStatus::is_alive)
                        });
                    // Um agente que aparece já terminado foi reaberto assim.
                    let restored_finished = prev.is_none() && status.is_terminal();
                    let restored_idle = *status == AgentStatus::WaitingForInput
                        && resumed
                            .remove(id)
                            .is_some_and(|at| at.elapsed() < RESUME_QUIET);
                    if watching || restored_finished || restored_idle {
                        continue;
                    }
                    let name = names.get(id).map_or("agente", String::as_str);
                    let cfg = self
                        .notifications
                        .read()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone();
                    if let Some(n) = notification_for(*id, name, prev.as_ref(), status, &cfg)
                        && let Err(err) = self.services.notifier.notify(&n)
                    {
                        warn!(%err, "notificação falhou");
                    }
                }
                AgentEvent::Removed { id } => {
                    names.remove(id);
                    last.remove(id);
                    parents.remove(id);
                    orchestrators.remove(id);
                    resumed.remove(id);
                }
                AgentEvent::Shutdown => break,
                _ => {}
            }
        }
        if let Err(err) = self.services.store.flush() {
            warn!(%err, "store.flush falhou");
        }
    }

    fn publish_error(&self, id: Option<AgentId>, message: String) {
        self.bus.publish(AgentEvent::Error { id, message });
    }
}

/// `remove` de um agente que o runner nunca conheceu (spawn falhou) não é erro.
fn ignore_unknown(result: Result<()>) -> Result<()> {
    match result {
        Err(Error::UnknownAgent(_)) => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use bytes::Bytes;
    use tokio::task::JoinHandle as TaskHandle;

    use super::*;
    use crate::{
        AgentInfo, AgentKind, BrowserEndpoint, BrowserInput, CommandSender, History, McpServerSpec,
        Notification, NotificationKind, OrchestratorGrant, TermSize, Timestamp, Viewport,
        WorktreeInfo, command_channel,
    };

    #[derive(Default)]
    struct Log(Mutex<Vec<String>>);

    impl Log {
        fn push(&self, entry: impl Into<String>) {
            self.0.lock().unwrap().push(entry.into());
        }
        fn entries(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    struct FakeRunner {
        bus: EventBus,
        log: Arc<Log>,
        fail_spawn: bool,
        known: Mutex<HashSet<AgentId>>,
        requests: Arc<Mutex<Vec<SpawnRequest>>>,
    }

    impl AgentRunner for FakeRunner {
        async fn spawn(&self, req: SpawnRequest) -> Result<AgentInfo> {
            self.log.push(format!("spawn {}", req.config.name));
            self.requests.lock().unwrap().push(req.clone());
            if self.fail_spawn {
                return Err(Error::Spawn("boom".to_owned()));
            }
            self.known.lock().unwrap().insert(req.id);
            self.bus.publish(AgentEvent::StatusChanged {
                id: req.id,
                status: AgentStatus::Running,
            });
            Ok(AgentInfo {
                id: req.id,
                config: req.config,
                workdir: "/tmp".into(),
                worktree: None,
                pid: Some(1),
                browser: req.browser,
                started_at: Timestamp::now(),
            })
        }
        async fn send_input(&self, _id: AgentId, data: Bytes) -> Result<()> {
            self.log
                .push(format!("input {}", String::from_utf8_lossy(&data)));
            Ok(())
        }
        async fn resize(&self, _id: AgentId, _size: TermSize) -> Result<()> {
            Ok(())
        }
        async fn kill(&self, _id: AgentId) -> Result<()> {
            self.log.push("kill");
            Ok(())
        }
        async fn remove(&self, id: AgentId, _cleanup: WorktreeCleanup) -> Result<()> {
            self.log.push("remove");
            if self.known.lock().unwrap().remove(&id) {
                Ok(())
            } else {
                Err(Error::UnknownAgent(id))
            }
        }
        async fn request_replay(&self, _id: AgentId) -> Result<()> {
            Ok(())
        }
        async fn shutdown(&self) {
            self.log.push("runner.shutdown");
        }
    }

    struct FakeBrowser {
        log: Arc<Log>,
    }

    impl BrowserManager for FakeBrowser {
        async fn launch(&self, _id: AgentId) -> Result<BrowserEndpoint> {
            self.log.push("launch");
            Ok(BrowserEndpoint {
                port: 9333,
                ws_url: "ws://127.0.0.1:9333/devtools/browser/x".to_owned(),
            })
        }
        async fn close(&self, _id: AgentId) -> Result<()> {
            self.log.push("close");
            Ok(())
        }
        async fn set_streaming(&self, _id: AgentId, on: bool) -> Result<()> {
            self.log.push(format!("streaming {on}"));
            Ok(())
        }
        async fn navigate(&self, _id: AgentId, nav: Navigation) -> Result<()> {
            self.log.push(format!("navigate {nav:?}"));
            Ok(())
        }
        async fn input(&self, _id: AgentId, _input: BrowserInput) -> Result<()> {
            Ok(())
        }
        async fn resize(&self, _id: AgentId, _viewport: Viewport) -> Result<()> {
            Ok(())
        }
        async fn shutdown(&self) {
            self.log.push("browser.shutdown");
        }
    }

    struct MemStore {
        log: Arc<Log>,
    }

    impl Store for MemStore {
        fn record(&self, _event: &AgentEvent) -> Result<()> {
            Ok(())
        }
        fn history(&self, _limit: usize) -> Result<History> {
            Ok(History::default())
        }
        fn output_log(&self, _id: AgentId, _max_bytes: usize) -> Result<Vec<u8>> {
            Ok(b"saida gravada".to_vec())
        }
        fn load_config(&self) -> Result<AppConfig> {
            Ok(AppConfig::default())
        }
        fn save_config(&self, _config: &AppConfig) -> Result<()> {
            self.log.push("save_config");
            Ok(())
        }
        fn flush(&self) -> Result<()> {
            self.log.push("store.flush");
            Ok(())
        }
    }

    struct FakeNotifier {
        sent: Arc<Mutex<Vec<Notification>>>,
    }

    impl Notifier for FakeNotifier {
        fn notify(&self, notification: &Notification) -> Result<()> {
            self.sent.lock().unwrap().push(notification.clone());
            Ok(())
        }
    }

    struct FakeGateway {
        log: Arc<Log>,
    }

    impl OrchestratorGateway for FakeGateway {
        fn grant(&self, _session: SessionId, _agent: AgentId) -> Result<OrchestratorGrant> {
            self.log.push("grant");
            Ok(OrchestratorGrant {
                mcp: McpServerSpec {
                    name: "orquestra".to_owned(),
                    command: "orquestra".to_owned(),
                    args: vec!["mcp".to_owned()],
                    env: BTreeMap::new(),
                },
                env: BTreeMap::from([("ORQUESTRA_TOKEN".to_owned(), "t".to_owned())]),
                stop_hook: Some("orquestra hook stop".to_owned()),
            })
        }
        fn revoke(&self, _agent: AgentId) {
            self.log.push("revoke");
        }
    }

    #[derive(Default)]
    struct Opts {
        fail_spawn: bool,
        max_workers: Option<u32>,
    }

    struct Harness {
        tx: CommandSender,
        bus: EventBus,
        events: EventReceiver,
        log: Arc<Log>,
        requests: Arc<Mutex<Vec<SpawnRequest>>>,
        notifications: Arc<Mutex<Vec<Notification>>>,
        hub: TaskHandle<()>,
    }

    impl Harness {
        fn start(opts: Opts) -> Self {
            let bus = EventBus::new();
            let events = bus.subscribe();
            let log = Arc::new(Log::default());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let notifications = Arc::new(Mutex::new(Vec::new()));
            let services = Services {
                runner: FakeRunner {
                    bus: bus.clone(),
                    log: log.clone(),
                    fail_spawn: opts.fail_spawn,
                    known: Mutex::new(HashSet::new()),
                    requests: requests.clone(),
                },
                browser: FakeBrowser { log: log.clone() },
                store: MemStore { log: log.clone() },
                notifier: FakeNotifier {
                    sent: notifications.clone(),
                },
                gateway: FakeGateway { log: log.clone() },
            };
            let mut config = HubConfig {
                browser_mcp: Some(McpServerTemplate::playwright()),
                ..HubConfig::default()
            };
            if let Some(max) = opts.max_workers {
                config.orchestration.max_concurrent_workers = max;
            }
            let (tx, rx) = command_channel();
            let hub = tokio::spawn(Hub::new(services, bus.clone(), config).run(rx));
            Self {
                tx,
                bus,
                events,
                log,
                requests,
                notifications,
                hub,
            }
        }

        fn send(&self, cmd: Command) {
            self.tx.send(cmd).unwrap();
        }

        async fn next_event(&mut self) -> AgentEvent {
            tokio::time::timeout(Duration::from_secs(2), self.events.recv())
                .await
                .expect("evento não chegou")
                .unwrap()
        }

        /// Pula eventos até um que satisfaça `pred`.
        async fn until(&mut self, pred: impl Fn(&AgentEvent) -> bool) -> AgentEvent {
            loop {
                let event = self.next_event().await;
                if pred(&event) {
                    return event;
                }
            }
        }

        async fn status_of(&mut self, id: AgentId) -> AgentStatus {
            match self
                .until(|e| matches!(e, AgentEvent::StatusChanged { id: i, .. } if *i == id))
                .await
            {
                AgentEvent::StatusChanged { status, .. } => status,
                _ => unreachable!(),
            }
        }

        async fn session(&mut self, config: SessionConfig) -> SessionId {
            let id = SessionId::generate();
            self.send(Command::CreateSession {
                id,
                config: Box::new(config),
            });
            self.until(|e| matches!(e, AgentEvent::SessionCreated { id: i, .. } if *i == id))
                .await;
            id
        }

        fn spawned_names(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.config.name.clone())
                .collect()
        }

        async fn shutdown(self) -> Vec<String> {
            self.send(Command::Shutdown);
            self.hub.await.unwrap();
            self.log.entries()
        }
    }

    fn loja() -> SessionConfig {
        SessionConfig::new("loja", "/tmp/loja", "login com e-mail")
    }

    fn agent(session: SessionId, name: &str) -> AgentConfig {
        AgentConfig {
            name: name.to_owned(),
            kind: AgentKind::ClaudeCode,
            program: "claude".to_owned(),
            args: Vec::new(),
            env: Default::default(),
            session,
            role: AgentRole::Worker,
            parent: None,
            workspace: Workspace::Directory {
                path: "/tmp".into(),
            },
            initial_prompt: None,
            browser: false,
            size: TermSize::default(),
        }
    }

    fn spawn(id: AgentId, config: AgentConfig) -> Command {
        Command::Spawn {
            id,
            config: Box::new(config),
        }
    }

    #[tokio::test]
    async fn spawn_launches_browser_first_and_registers_its_mcp() {
        let mut h = Harness::start(Opts::default());
        let s = h.session(loja()).await;
        let id = AgentId::generate();
        h.send(spawn(
            id,
            AgentConfig {
                browser: true,
                ..agent(s, "qa")
            },
        ));

        assert!(matches!(h.next_event().await, AgentEvent::Created { id: i, .. } if i == id));
        assert_eq!(h.status_of(id).await, AgentStatus::Starting);
        assert_eq!(h.status_of(id).await, AgentStatus::Running);
        match h.until(|e| matches!(e, AgentEvent::Spawned { .. })).await {
            AgentEvent::Spawned { info } => assert_eq!(info.browser.as_ref().unwrap().port, 9333),
            _ => unreachable!(),
        }
        let req = h.requests.lock().unwrap()[0].clone();
        assert_eq!(req.mcp_servers.len(), 1);
        assert_eq!(
            req.mcp_servers[0].args.last().unwrap(),
            "http://127.0.0.1:9333"
        );
        assert!(req.stop_hook.is_none());
        let log = h.shutdown().await;
        assert_eq!(log[..2], ["launch", "spawn qa"]);
    }

    #[tokio::test]
    async fn commands_for_one_agent_keep_their_order() {
        let mut h = Harness::start(Opts::default());
        let s = h.session(loja()).await;
        let id = AgentId::generate();
        h.send(spawn(id, agent(s, "w")));
        for i in 0..50 {
            h.send(Command::SendInput {
                id,
                data: Bytes::from(i.to_string()),
            });
        }
        let log = h.shutdown().await;
        let inputs: Vec<_> = log
            .iter()
            .filter(|e| e.starts_with("input"))
            .cloned()
            .collect();
        let expected: Vec<_> = (0..50).map(|i| format!("input {i}")).collect();
        assert_eq!(inputs, expected);
        assert_eq!(log[0], "spawn w");
    }

    #[tokio::test]
    async fn failed_spawn_marks_failed_and_closes_browser() {
        let mut h = Harness::start(Opts {
            fail_spawn: true,
            ..Opts::default()
        });
        let s = h.session(loja()).await;
        let id = AgentId::generate();
        h.send(spawn(
            id,
            AgentConfig {
                browser: true,
                ..agent(s, "w")
            },
        ));

        assert_eq!(h.status_of(id).await, AgentStatus::Starting);
        assert!(matches!(h.status_of(id).await, AgentStatus::Failed { .. }));
        assert!(matches!(h.next_event().await, AgentEvent::Error { id: Some(i), .. } if i == id));
        let log = h.shutdown().await;
        assert_eq!(log[..3], ["launch", "spawn w", "close"]);
    }

    #[tokio::test]
    async fn duplicate_spawn_and_unknown_session_are_rejected() {
        let mut h = Harness::start(Opts::default());
        let s = h.session(loja()).await;
        let id = AgentId::generate();
        h.send(spawn(id, agent(s, "w")));
        h.send(spawn(id, agent(s, "w")));
        let err = h.until(|e| matches!(e, AgentEvent::Error { .. })).await;
        assert!(matches!(err, AgentEvent::Error { message, .. } if message.starts_with("spawn:")));

        h.send(spawn(
            AgentId::generate(),
            agent(SessionId::generate(), "x"),
        ));
        let err = h.until(|e| matches!(e, AgentEvent::Error { .. })).await;
        assert!(
            matches!(err, AgentEvent::Error { message, .. } if message.contains("sessão desconhecida"))
        );
        let log = h.shutdown().await;
        assert_eq!(log.iter().filter(|e| e.starts_with("spawn")).count(), 1);
    }

    #[tokio::test]
    async fn remove_of_failed_agent_is_not_an_error() {
        let mut h = Harness::start(Opts {
            fail_spawn: true,
            ..Opts::default()
        });
        let s = h.session(loja()).await;
        let id = AgentId::generate();
        h.send(spawn(id, agent(s, "w")));
        h.send(Command::Remove {
            id,
            cleanup: WorktreeCleanup::Keep,
        });
        let mut errors = 0;
        loop {
            match h.next_event().await {
                AgentEvent::Error { .. } => errors += 1,
                AgentEvent::Removed { id: i } if i == id => break,
                _ => {}
            }
        }
        assert_eq!(errors, 1, "só o erro do spawn");
        let log = h.shutdown().await;
        assert!(log.contains(&"remove".to_owned()) && log.contains(&"close".to_owned()));
    }

    #[tokio::test]
    async fn open_browser_launches_streams_and_navigates() {
        let h = Harness::start(Opts::default());
        let id = AgentId::generate();
        h.send(Command::OpenBrowser {
            id,
            url: Some("http://localhost:3000".to_owned()),
        });
        let log = h.shutdown().await;
        assert_eq!(
            log[..3],
            [
                "launch".to_owned(),
                "streaming true".to_owned(),
                format!(
                    "navigate {:?}",
                    Navigation::Url("http://localhost:3000".to_owned())
                ),
            ]
        );
    }

    #[tokio::test]
    async fn orchestrator_gets_a_grant_that_is_revoked_on_remove() {
        let mut h = Harness::start(Opts::default());
        let s = h.session(loja()).await;
        let id = AgentId::generate();
        h.send(spawn(
            id,
            AgentConfig {
                role: AgentRole::Orchestrator,
                ..agent(s, "orquestrador")
            },
        ));
        h.until(|e| matches!(e, AgentEvent::Spawned { .. })).await;

        let req = h.requests.lock().unwrap()[0].clone();
        assert_eq!(req.mcp_servers[0].name, "orquestra");
        assert_eq!(
            req.env.get("ORQUESTRA_TOKEN").map(String::as_str),
            Some("t")
        );
        assert_eq!(req.stop_hook.as_deref(), Some("orquestra hook stop"));

        h.send(Command::Remove {
            id,
            cleanup: WorktreeCleanup::Keep,
        });
        h.until(|e| matches!(e, AgentEvent::Removed { .. })).await;
        let log = h.shutdown().await;
        assert_eq!(log[0], "grant");
        assert!(log.contains(&"revoke".to_owned()));
    }

    #[tokio::test]
    async fn workers_wait_for_a_free_slot_but_orchestrators_do_not() {
        let mut h = Harness::start(Opts {
            max_workers: Some(1),
            ..Opts::default()
        });
        let s = h.session(loja()).await;
        let (w1, w2, orq) = (
            AgentId::generate(),
            AgentId::generate(),
            AgentId::generate(),
        );

        h.send(spawn(w1, agent(s, "w1")));
        assert_eq!(h.status_of(w1).await, AgentStatus::Starting);
        h.send(spawn(w2, agent(s, "w2")));
        assert_eq!(
            h.status_of(w2).await,
            AgentStatus::Queued {
                reason: QueueReason::Capacity
            }
        );
        h.send(spawn(
            orq,
            AgentConfig {
                role: AgentRole::Orchestrator,
                ..agent(s, "orq")
            },
        ));
        assert_eq!(h.status_of(orq).await, AgentStatus::Starting);

        h.bus.publish(AgentEvent::StatusChanged {
            id: w1,
            status: AgentStatus::Done { exit_code: Some(0) },
        });
        assert_eq!(h.status_of(w2).await, AgentStatus::Starting);
        h.until(|e| matches!(e, AgentEvent::Spawned { info } if info.id == w2))
            .await;
        let mut names = h.spawned_names();
        names.sort();
        assert_eq!(names, ["orq", "w1", "w2"]);
        h.shutdown().await;
    }

    #[tokio::test]
    async fn session_limit_applies_per_session() {
        let mut h = Harness::start(Opts::default());
        let a = h
            .session(SessionConfig {
                max_workers: Some(1),
                ..loja()
            })
            .await;
        let b = h
            .session(SessionConfig::new("blog", "/tmp/blog", "astro"))
            .await;
        let (a1, a2, b1) = (
            AgentId::generate(),
            AgentId::generate(),
            AgentId::generate(),
        );

        h.send(spawn(a1, agent(a, "a1")));
        assert_eq!(h.status_of(a1).await, AgentStatus::Starting);
        h.send(spawn(a2, agent(a, "a2")));
        assert!(matches!(h.status_of(a2).await, AgentStatus::Queued { .. }));
        h.send(spawn(b1, agent(b, "b1")));
        assert_eq!(h.status_of(b1).await, AgentStatus::Starting);
        h.shutdown().await;
    }

    #[tokio::test]
    async fn spawns_from_an_orchestrator_can_require_approval() {
        let mut h = Harness::start(Opts::default());
        let s = h
            .session(SessionConfig {
                approve_spawns: true,
                ..loja()
            })
            .await;
        let (orq, c1, c2, solo) = (
            AgentId::generate(),
            AgentId::generate(),
            AgentId::generate(),
            AgentId::generate(),
        );
        let child = |name: &str| AgentConfig {
            parent: Some(orq),
            ..agent(s, name)
        };
        h.send(spawn(
            orq,
            AgentConfig {
                role: AgentRole::Orchestrator,
                ..agent(s, "orq")
            },
        ));
        assert_eq!(h.status_of(orq).await, AgentStatus::Starting);

        let awaiting = AgentStatus::Queued {
            reason: QueueReason::Approval,
        };
        h.send(spawn(c1, child("c1")));
        assert_eq!(h.status_of(c1).await, awaiting);
        h.send(spawn(solo, agent(s, "solo")));
        assert_eq!(h.status_of(solo).await, AgentStatus::Starting);

        h.send(Command::ApproveSpawn { id: c1 });
        assert_eq!(h.status_of(c1).await, AgentStatus::Starting);

        h.send(spawn(c2, child("c2")));
        assert_eq!(h.status_of(c2).await, awaiting);
        h.send(Command::RejectSpawn { id: c2 });
        h.until(|e| matches!(e, AgentEvent::Removed { id } if *id == c2))
            .await;

        let notifications = h.notifications.clone();
        let requests = h.requests.clone();
        h.shutdown().await;
        let mut names: Vec<_> = requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.config.name.clone())
            .collect();
        names.sort();
        assert_eq!(names, ["c1", "orq", "solo"]);
        let approvals = notifications
            .lock()
            .unwrap()
            .iter()
            .filter(|n| n.kind == NotificationKind::NeedsApproval)
            .count();
        assert_eq!(approvals, 2);
    }

    #[tokio::test]
    async fn queued_agents_can_be_killed_or_removed_without_running() {
        let mut h = Harness::start(Opts {
            max_workers: Some(1),
            ..Opts::default()
        });
        let s = h.session(loja()).await;
        let (w1, w2, w3) = (
            AgentId::generate(),
            AgentId::generate(),
            AgentId::generate(),
        );
        h.send(spawn(w1, agent(s, "w1")));
        h.send(spawn(w2, agent(s, "w2")));
        h.send(spawn(w3, agent(s, "w3")));
        assert!(matches!(h.status_of(w3).await, AgentStatus::Queued { .. }));

        h.send(Command::SendInput {
            id: w2,
            data: Bytes::from_static(b"x"),
        });
        let err = h.until(|e| matches!(e, AgentEvent::Error { .. })).await;
        assert!(matches!(err, AgentEvent::Error { message, .. } if message.contains("fila")));

        h.send(Command::Kill { id: w2 });
        assert_eq!(h.status_of(w2).await, AgentStatus::Done { exit_code: None });
        h.send(Command::Remove {
            id: w3,
            cleanup: WorktreeCleanup::Keep,
        });
        h.until(|e| matches!(e, AgentEvent::Removed { id } if *id == w3))
            .await;

        let requests = h.requests.clone();
        h.shutdown().await;
        let names: Vec<_> = requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.config.name.clone())
            .collect();
        assert_eq!(names, ["w1"]);
    }

    #[tokio::test]
    async fn close_session_removes_every_agent_in_it() {
        let mut h = Harness::start(Opts::default());
        let s = h.session(loja()).await;
        let other = h
            .session(SessionConfig::new("blog", "/tmp/blog", "astro"))
            .await;
        let (a, b, c) = (
            AgentId::generate(),
            AgentId::generate(),
            AgentId::generate(),
        );
        h.send(spawn(a, agent(s, "a")));
        h.send(spawn(b, agent(s, "b")));
        h.send(spawn(c, agent(other, "c")));
        h.until(|e| matches!(e, AgentEvent::Spawned { info } if info.id == c))
            .await;

        h.send(Command::CloseSession {
            id: s,
            cleanup: WorktreeCleanup::RemoveWorktree,
        });
        let mut removed = HashSet::new();
        let mut closed = false;
        while removed.len() < 2 || !closed {
            match h.next_event().await {
                AgentEvent::Removed { id } => {
                    removed.insert(id);
                }
                AgentEvent::SessionClosed { id } => closed = id == s,
                _ => {}
            }
        }
        assert_eq!(removed, HashSet::from([a, b]));

        h.send(spawn(AgentId::generate(), agent(s, "late")));
        let err = h.until(|e| matches!(e, AgentEvent::Error { .. })).await;
        assert!(
            matches!(err, AgentEvent::Error { message, .. } if message.contains("sessão desconhecida"))
        );
        h.shutdown().await;
    }

    #[tokio::test]
    async fn notifies_on_entering_attention_states() {
        let mut h = Harness::start(Opts::default());
        let s = h.session(loja()).await;
        let id = AgentId::generate();
        h.send(spawn(id, agent(s, "claude-1")));
        h.until(|e| matches!(e, AgentEvent::Spawned { .. })).await;

        for status in [
            AgentStatus::WaitingForInput,
            AgentStatus::WaitingForInput,
            AgentStatus::Running,
            AgentStatus::WaitingForInput,
            AgentStatus::Done { exit_code: Some(0) },
        ] {
            h.bus.publish(AgentEvent::StatusChanged { id, status });
        }
        let notifications = h.notifications.clone();
        h.shutdown().await;

        let kinds: Vec<_> = notifications
            .lock()
            .unwrap()
            .iter()
            .map(|n| n.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                NotificationKind::NeedsInput,
                NotificationKind::NeedsInput,
                NotificationKind::Finished
            ]
        );
        assert_eq!(notifications.lock().unwrap()[0].title, "claude-1");
    }

    /// Só quem pode delegar recebe as instruções escondidas, com a pasta da
    /// sessão; uma config salva troca o texto dos próximos spawns.
    #[tokio::test]
    async fn delegation_instructions_go_only_to_delegating_agents() {
        let mut h = Harness::start(Opts::default());
        let s = h.session(loja()).await;
        let (lead, worker, later) = (
            AgentId::generate(),
            AgentId::generate(),
            AgentId::generate(),
        );
        h.send(spawn(
            lead,
            AgentConfig {
                role: AgentRole::Orchestrator,
                ..agent(s, "claude-code")
            },
        ));
        h.until(|e| matches!(e, AgentEvent::Spawned { info } if info.id == lead))
            .await;
        h.send(spawn(
            worker,
            AgentConfig {
                parent: Some(lead),
                ..agent(s, "api")
            },
        ));
        h.until(|e| matches!(e, AgentEvent::Spawned { info } if info.id == worker))
            .await;

        let mut config = AppConfig::default();
        config.orchestration.delegation_prompt = "Delegate from {folder}.".to_owned();
        h.send(Command::SaveConfig(Box::new(config)));
        h.send(spawn(
            later,
            AgentConfig {
                role: AgentRole::Orchestrator,
                ..agent(s, "codex")
            },
        ));
        h.until(|e| matches!(e, AgentEvent::Spawned { info } if info.id == later))
            .await;

        let instructions = |name: &str| {
            h.requests
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.config.name == name)
                .map(|r| r.instructions.clone())
                .unwrap()
        };
        let lead_text = instructions("claude-code").expect("quem delega recebe");
        assert!(
            lead_text.contains("whose folder is /tmp/loja."),
            "{lead_text}"
        );
        assert_eq!(instructions("api"), None, "quem foi criado não delega");
        assert_eq!(
            instructions("codex").as_deref(),
            Some("Delegate from /tmp/loja.")
        );
        h.shutdown().await;
    }

    /// Reabrir depois de um reinício: o vivo sobe onde estava e retoma a
    /// conversa, sem pedir aprovação de novo; o terminado volta sem processo,
    /// com a saída gravada; e nada disso vira notificação.
    #[tokio::test]
    async fn restore_reopens_live_agents_and_brings_back_finished_ones() {
        let mut h = Harness::start(Opts::default());
        let s = h
            .session(SessionConfig {
                approve_spawns: true,
                ..loja()
            })
            .await;
        let (orq, live, done) = (
            AgentId::generate(),
            AgentId::generate(),
            AgentId::generate(),
        );
        let worktree = WorktreeInfo {
            repo: "/src/loja".into(),
            path: "/wt/api".into(),
            branch: "orquestra/api".to_owned(),
            base_ref: "HEAD".to_owned(),
            base_commit: "abc".to_owned(),
        };
        let record = |id, config: AgentConfig, worktree: Option<WorktreeInfo>, status| {
            let info = AgentInfo {
                id,
                config: config.clone(),
                workdir: worktree
                    .as_ref()
                    .map_or("/src/loja".into(), |w| w.path.clone()),
                worktree,
                pid: Some(9),
                browser: None,
                started_at: Timestamp::now(),
            };
            Command::Restore {
                record: Box::new(AgentRecord {
                    id,
                    config,
                    info: Some(info),
                    status,
                    created_at: Timestamp::now(),
                    updated_at: Timestamp::now(),
                }),
            }
        };
        let orq_cfg = AgentConfig {
            role: AgentRole::Orchestrator,
            ..agent(s, "orquestrador")
        };
        let child = |name| AgentConfig {
            parent: Some(orq),
            ..agent(s, name)
        };
        h.send(record(orq, orq_cfg, None, AgentStatus::WaitingForInput));
        h.send(record(
            live,
            child("api"),
            Some(worktree.clone()),
            AgentStatus::Running,
        ));
        h.send(record(
            done,
            child("docs"),
            None,
            AgentStatus::Done { exit_code: Some(0) },
        ));

        // Sem aprovação: sai direto para `Starting`.
        assert_eq!(h.status_of(live).await, AgentStatus::Starting);
        // O replay do terminado e o spawn do vivo correm em paralelo.
        let (mut replay, mut spawned) = (None, false);
        while replay.is_none() || !spawned {
            match h.next_event().await {
                AgentEvent::Replay { id, next_seq, data } if id == done => {
                    replay = Some((next_seq, data));
                }
                AgentEvent::Spawned { info } if info.id == live => spawned = true,
                _ => {}
            }
        }
        assert_eq!(replay, Some((0, Bytes::from_static(b"saida gravada"))));
        let workspace = |name: &str| {
            h.requests
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.config.name == name)
                .map(|r| r.config.workspace.clone())
        };
        assert_eq!(
            workspace("api"),
            Some(Workspace::Restore {
                path: "/wt/api".into(),
                worktree: Some(worktree),
            })
        );
        assert_eq!(
            workspace("orquestrador"),
            Some(Workspace::Restore {
                path: "/src/loja".into(),
                worktree: None,
            })
        );
        assert_eq!(workspace("docs"), None, "terminado não sobe processo");

        // Todos voltam parados: não é novidade.
        for id in [orq, live] {
            h.bus.publish(AgentEvent::StatusChanged {
                id,
                status: AgentStatus::WaitingForInput,
            });
        }
        let notifications = h.notifications.clone();
        h.shutdown().await;
        assert!(notifications.lock().unwrap().is_empty());
    }

    /// O orquestrador para à espera dos filhos (`wait_for` em segundo plano):
    /// isso não é "aguardando sua resposta". Sem filho vivo, é.
    #[tokio::test]
    async fn orchestrator_waiting_on_its_children_does_not_notify() {
        let mut h = Harness::start(Opts::default());
        let s = h.session(loja()).await;
        let (orq, w) = (AgentId::generate(), AgentId::generate());
        h.send(spawn(
            orq,
            AgentConfig {
                role: AgentRole::Orchestrator,
                ..agent(s, "orquestrador")
            },
        ));
        h.until(|e| matches!(e, AgentEvent::Spawned { info } if info.id == orq))
            .await;
        h.send(spawn(
            w,
            AgentConfig {
                parent: Some(orq),
                ..agent(s, "api")
            },
        ));
        h.until(|e| matches!(e, AgentEvent::Spawned { info } if info.id == w))
            .await;

        let publish = |id, status| h.bus.publish(AgentEvent::StatusChanged { id, status });
        publish(orq, AgentStatus::WaitingForInput);
        publish(w, AgentStatus::Done { exit_code: Some(0) });
        publish(orq, AgentStatus::Running);
        publish(orq, AgentStatus::WaitingForInput);
        let notifications = h.notifications.clone();
        h.shutdown().await;

        let sent: Vec<_> = notifications
            .lock()
            .unwrap()
            .iter()
            .map(|n| (n.title.clone(), n.kind))
            .collect();
        assert_eq!(
            sent,
            [
                ("api".to_owned(), NotificationKind::Finished),
                ("orquestrador".to_owned(), NotificationKind::NeedsInput),
            ]
        );
    }

    #[tokio::test]
    async fn shutdown_stops_services_and_flushes_store() {
        let mut h = Harness::start(Opts::default());
        h.send(Command::LoadHistory { limit: 10 });
        assert!(matches!(h.next_event().await, AgentEvent::History(_)));
        h.send(Command::SaveConfig(Box::default()));

        let mut events = h.bus.subscribe();
        let log = h.shutdown().await;
        let tail = &log[log.len() - 3..];
        // O store grava tudo antes de os agentes morrerem (o banco os guarda
        // vivos, para reabrir).
        assert_eq!(tail, ["store.flush", "runner.shutdown", "browser.shutdown"]);
        assert!(log.contains(&"save_config".to_owned()));
        assert!(matches!(events.recv().await.unwrap(), AgentEvent::Shutdown));
    }
}
