//! `Runner`: processos de agentes em PTY.
//!
//! Por agente, três threads: uma lê o PTY (64 KiB por leitura) e publica
//! `Output` com `seq` contínuo; uma escreve a fila de `send_input`; uma espera
//! o processo e publica o status final depois que a leitura esvazia. Uma
//! thread do runner faz o `tick` da heurística de ociosidade de todos.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use orquestra_core::{
    AgentEvent, AgentId, AgentInfo, AgentKind, AgentRunner, AgentStatus, Error, EventBus, Result,
    RunnerConfig, SpawnRequest, TermSize, Timestamp, Workspace, WorktreeCleanup, WorktreeInfo,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::command::{CommandSpec, build_command};
use crate::process::{SIGKILL, SIGTERM, signal_tree};
use crate::replay::ReplayRing;
use crate::status::{DetectorConfig, ProcessExit, StatusDetector};
use crate::worktree;

const READ_BUF: usize = 64 * 1024;
/// Quanto o status final espera a leitura do PTY esvaziar (um filho em
/// background pode segurar o PTY aberto para sempre).
const DRAIN_WAIT: Duration = Duration::from_millis(500);
/// Até quanto o prompt inicial de Shell/Custom espera a primeira saída.
const STARTUP_WAIT: Duration = Duration::from_millis(500);
/// Espera pela morte depois do SIGKILL.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// Implementação de `AgentRunner` sobre `portable-pty`.
pub struct Runner {
    inner: Arc<Inner>,
}

struct Inner {
    bus: EventBus,
    config: RunnerConfig,
    agents: Mutex<HashMap<AgentId, Arc<Agent>>>,
}

struct Agent {
    pid: u32,
    master: Mutex<Box<dyn MasterPty + Send>>,
    shared: Arc<Shared>,
    worktree: Option<WorktreeInfo>,
}

/// O que as threads do agente compartilham com o runner.
struct Shared {
    id: AgentId,
    bus: EventBus,
    out: Mutex<OutputState>,
    /// Fila da thread de escrita; `None` depois que o processo sai.
    input: Mutex<Option<Sender<Bytes>>>,
    kill_requested: AtomicBool,
    /// O líder já foi colhido: o pid dele pode ser reusado, `killpg` não.
    reaped: AtomicBool,
    /// `true` depois que o status final foi publicado.
    exited: watch::Sender<bool>,
}

struct OutputState {
    next_seq: u64,
    ring: ReplayRing,
    detector: StatusDetector,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Runner {
    pub fn new(bus: EventBus, config: RunnerConfig) -> Self {
        let tick = Duration::from_millis((config.idle_ms / 5).clamp(20, 250));
        let inner = Arc::new(Inner {
            bus,
            config,
            agents: Mutex::new(HashMap::new()),
        });
        spawn_ticker(Arc::downgrade(&inner), tick);
        Self { inner }
    }
}

impl AgentRunner for Runner {
    async fn spawn(&self, req: SpawnRequest) -> Result<AgentInfo> {
        if lock(&self.inner.agents).contains_key(&req.id) {
            return Err(Error::AgentExists(req.id));
        }
        // git e fork/exec bloqueiam: fora das threads do runtime.
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.start(req))
            .await
            .map_err(|err| Error::Spawn(format!("tarefa de spawn falhou: {err}")))?
    }

    async fn send_input(&self, id: AgentId, data: Bytes) -> Result<()> {
        let agent = self.inner.get(id)?;
        lock(&agent.shared.out).detector.input(Instant::now());
        match lock(&agent.shared.input).as_ref() {
            Some(tx) => {
                let _ = tx.send(data);
            }
            None => debug!(agent = %id.short(), "input descartado: o processo já saiu"),
        }
        Ok(())
    }

    async fn resize(&self, id: AgentId, size: TermSize) -> Result<()> {
        let agent = self.inner.get(id)?;
        lock(&agent.master)
            .resize(pty_size(size))
            .map_err(|err| Error::Other(format!("resize: {err}")))
    }

    async fn kill(&self, id: AgentId) -> Result<()> {
        let agent = self.inner.get(id)?;
        if !agent.leader_alive() {
            return Ok(());
        }
        agent.shared.kill_requested.store(true, Ordering::SeqCst);
        signal_tree(agent.pid, SIGTERM, true);
        let grace = self.inner.grace();
        tokio::spawn(async move {
            if !agent.wait_exit(grace).await {
                signal_tree(agent.pid, SIGKILL, agent.leader_alive());
            }
        });
        Ok(())
    }

    async fn remove(&self, id: AgentId, cleanup: WorktreeCleanup) -> Result<()> {
        let agent = self.inner.get(id)?;
        self.inner.terminate(&agent).await;
        // Sobras em background na sessão (o líder já saiu).
        signal_tree(agent.pid, SIGKILL, false);
        lock(&self.inner.agents).remove(&id);
        lock(&agent.shared.input).take();
        let Some(info) = agent.worktree.clone() else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || worktree::cleanup(&info, cleanup))
            .await
            .map_err(|err| Error::Git(format!("tarefa de limpeza falhou: {err}")))?
    }

    async fn request_replay(&self, id: AgentId) -> Result<()> {
        let agent = self.inner.get(id)?;
        let out = lock(&agent.shared.out);
        let mut data = out.detector.mode_prefix();
        data.extend(out.ring.snapshot());
        // Publicado com a trava: nenhum `Output` entra entre o corte e o evento.
        self.inner.bus.publish(AgentEvent::Replay {
            id,
            next_seq: out.next_seq,
            data: Bytes::from(data),
        });
        Ok(())
    }

    async fn shutdown(&self) {
        let alive: Vec<Arc<Agent>> = lock(&self.inner.agents)
            .values()
            .filter(|agent| agent.leader_alive())
            .cloned()
            .collect();
        for agent in &alive {
            agent.shared.kill_requested.store(true, Ordering::SeqCst);
            signal_tree(agent.pid, SIGTERM, true);
        }
        let deadline = Instant::now() + self.inner.grace();
        for agent in &alive {
            agent
                .wait_exit(deadline.saturating_duration_since(Instant::now()))
                .await;
        }
        let stubborn: Vec<&Arc<Agent>> = alive.iter().filter(|a| !a.has_exited()).collect();
        for agent in &stubborn {
            signal_tree(agent.pid, SIGKILL, agent.leader_alive());
        }
        let deadline = Instant::now() + KILL_WAIT;
        for agent in stubborn {
            agent
                .wait_exit(deadline.saturating_duration_since(Instant::now()))
                .await;
        }
    }
}

impl Inner {
    fn get(&self, id: AgentId) -> Result<Arc<Agent>> {
        lock(&self.agents)
            .get(&id)
            .cloned()
            .ok_or(Error::UnknownAgent(id))
    }

    fn grace(&self) -> Duration {
        Duration::from_millis(self.config.kill_grace_ms)
    }

    /// SIGTERM, espera `kill_grace_ms`, SIGKILL; volta depois do status final.
    async fn terminate(&self, agent: &Agent) {
        if agent.leader_alive() {
            agent.shared.kill_requested.store(true, Ordering::SeqCst);
            signal_tree(agent.pid, SIGTERM, true);
            if !agent.wait_exit(self.grace()).await {
                signal_tree(agent.pid, SIGKILL, agent.leader_alive());
            }
        }
        if !agent.wait_exit(KILL_WAIT).await {
            warn!(agent = %agent.shared.id.short(), "o processo não morreu nem com SIGKILL");
        }
    }

    /// Prepara o diretório e sobe o processo (bloqueante).
    fn start(self: &Arc<Self>, req: SpawnRequest) -> Result<AgentInfo> {
        match &req.config.workspace {
            Workspace::Directory { path } => {
                if !path.is_dir() {
                    return Err(Error::Spawn(format!(
                        "diretório não existe: {}",
                        path.display()
                    )));
                }
                self.launch(&req, path, None)
            }
            Workspace::Worktree { repo, base, branch } => {
                let root = worktree::root_dir(&self.config)?;
                let created = worktree::create(
                    &root,
                    &self.config.branch_prefix,
                    &worktree::Request {
                        repo,
                        base: base.as_deref(),
                        branch: branch.as_deref(),
                        agent_name: &req.config.name,
                        id: req.id,
                    },
                )?;
                let path = created.info.path.clone();
                self.launch(&req, &path, Some(created.info.clone()))
                    .inspect_err(|_| worktree::discard(&created))
            }
        }
    }

    fn launch(
        self: &Arc<Self>,
        req: &SpawnRequest,
        workdir: &Path,
        worktree: Option<WorktreeInfo>,
    ) -> Result<AgentInfo> {
        let id = req.id;
        let spec = build_command(req, workdir);
        if spec.program.trim().is_empty() {
            return Err(Error::Spawn("programa vazio".to_owned()));
        }
        let pair = native_pty_system()
            .openpty(pty_size(req.config.size))
            .map_err(|err| Error::Spawn(format!("openpty: {err}")))?;
        let mut child = pair
            .slave
            .spawn_command(command_builder(&spec))
            .map_err(|err| Error::Spawn(format!("{}: {err}", spec.program)))?;
        // Sem a nossa cópia do escravo, a leitura recebe EOF quando o agente sai.
        drop(pair.slave);
        let master = pair.master;

        let Some(pid) = child.process_id() else {
            abandon(&mut child);
            return Err(Error::Spawn("processo sem pid".to_owned()));
        };
        let io = master
            .try_clone_reader()
            .and_then(|r| Ok((r, master.take_writer()?)));
        let (reader, writer) = match io {
            Ok(io) => io,
            Err(err) => {
                abandon(&mut child);
                return Err(Error::Spawn(format!("PTY: {err}")));
            }
        };

        let kind = req.config.kind;
        let (input_tx, input_rx) = mpsc::channel::<Bytes>();
        let shared = Arc::new(Shared {
            id,
            bus: self.bus.clone(),
            out: Mutex::new(OutputState {
                next_seq: 0,
                ring: ReplayRing::new(self.config.replay_bytes),
                detector: StatusDetector::new(
                    kind,
                    kind == AgentKind::ClaudeCode,
                    DetectorConfig::from_idle_ms(self.config.idle_ms),
                    Instant::now(),
                ),
            }),
            input: Mutex::new(Some(input_tx)),
            kill_requested: AtomicBool::new(false),
            reaped: AtomicBool::new(false),
            exited: watch::channel(false).0,
        });
        let agent = Arc::new(Agent {
            pid,
            master: Mutex::new(master),
            shared: shared.clone(),
            worktree: worktree.clone(),
        });
        {
            let mut agents = lock(&self.agents);
            if agents.contains_key(&id) {
                abandon(&mut child);
                return Err(Error::AgentExists(id));
            }
            agents.insert(id, agent);
        }
        // Antes de qualquer `Output`.
        self.bus.publish(AgentEvent::StatusChanged {
            id,
            status: AgentStatus::Running,
        });

        let (first_tx, first_rx) = mpsc::sync_channel::<()>(1);
        let (drained_tx, drained_rx) = mpsc::channel::<()>();
        let short = id.short();
        let threads = spawn_thread(format!("orquestra-wait-{short}"), {
            let shared = shared.clone();
            move || wait_loop(&shared, child, &drained_rx)
        })
        .and_then(|()| {
            spawn_thread(format!("orquestra-read-{short}"), {
                let shared = shared.clone();
                move || read_loop(&shared, reader, first_tx, drained_tx)
            })
        })
        .and_then(|()| {
            let startup = spec.startup_input.clone();
            spawn_thread(format!("orquestra-write-{short}"), move || {
                write_loop(writer, &input_rx, startup, &first_rx);
            })
        });
        if let Err(err) = threads {
            signal_tree(pid, SIGKILL, true);
            lock(&self.agents).remove(&id);
            return Err(err);
        }

        Ok(AgentInfo {
            id,
            config: req.config.clone(),
            workdir: workdir.to_owned(),
            worktree,
            pid: Some(pid),
            browser: req.browser.clone(),
            started_at: Timestamp::now(),
        })
    }
}

impl Agent {
    /// O status final já saiu.
    fn has_exited(&self) -> bool {
        *self.shared.exited.borrow()
    }

    fn leader_alive(&self) -> bool {
        !self.shared.reaped.load(Ordering::SeqCst)
    }

    /// `true` se o processo saiu (e o status final saiu) dentro de `timeout`.
    async fn wait_exit(&self, timeout: Duration) -> bool {
        let mut rx = self.shared.exited.subscribe();
        matches!(
            tokio::time::timeout(timeout, rx.wait_for(|exited| *exited)).await,
            Ok(Ok(_))
        )
    }
}

impl Shared {
    fn on_output(&self, chunk: &[u8]) {
        let now = Instant::now();
        let data = Bytes::copy_from_slice(chunk);
        let mut out = lock(&self.out);
        let seq = out.next_seq;
        out.next_seq += 1;
        out.ring.push(&data);
        let status = out.detector.feed(&data, now);
        // Publicado com a trava: `seq` sai em ordem e o `Replay` não fura a fila.
        self.bus.publish(AgentEvent::Output {
            id: self.id,
            seq,
            data,
        });
        if let Some(status) = status {
            self.publish_status(status);
        }
    }

    fn tick(&self, now: Instant) {
        let mut out = lock(&self.out);
        if let Some(status) = out.detector.tick(now) {
            self.publish_status(status);
        }
    }

    fn publish_status(&self, status: AgentStatus) {
        self.bus.publish(AgentEvent::StatusChanged {
            id: self.id,
            status,
        });
    }
}

fn read_loop(
    shared: &Shared,
    mut reader: Box<dyn Read + Send>,
    first_output: SyncSender<()>,
    _drained: Sender<()>,
) {
    let mut buf = vec![0u8; READ_BUF];
    let mut first_output = Some(first_output);
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            // EIO: todos os lados escravos fecharam.
            Err(_) => break,
        };
        shared.on_output(&buf[..n]);
        if let Some(tx) = first_output.take() {
            let _ = tx.try_send(());
        }
    }
    // Soltar `_drained` avisa a thread de espera.
}

fn write_loop(
    mut writer: Box<dyn Write + Send>,
    input: &Receiver<Bytes>,
    startup: Option<Vec<u8>>,
    first_output: &Receiver<()>,
) {
    if let Some(bytes) = startup {
        // Deixa o programa subir antes de digitar por ele.
        let _ = first_output.recv_timeout(STARTUP_WAIT);
        if writer
            .write_all(&bytes)
            .and_then(|()| writer.flush())
            .is_err()
        {
            return;
        }
    }
    for data in input {
        if writer
            .write_all(&data)
            .and_then(|()| writer.flush())
            .is_err()
        {
            break;
        }
    }
}

fn wait_loop(shared: &Shared, mut child: Box<dyn Child + Send + Sync>, drained: &Receiver<()>) {
    let exit = match child.wait() {
        Ok(status) => match status.signal() {
            Some(signal) => ProcessExit::Signal(signal.to_owned()),
            None => ProcessExit::Code(status.exit_code() as i32),
        },
        Err(err) => ProcessExit::Unknown(format!("erro ao esperar o processo: {err}")),
    };
    shared.reaped.store(true, Ordering::SeqCst);
    // O status final vem depois da última saída.
    let _ = drained.recv_timeout(DRAIN_WAIT);
    lock(&shared.input).take();
    let kill_requested = shared.kill_requested.load(Ordering::SeqCst);
    {
        let mut out = lock(&shared.out);
        if let Some(status) = out.detector.exit(&exit, kill_requested) {
            shared.publish_status(status);
        }
    }
    shared.exited.send_replace(true);
}

fn spawn_ticker(inner: Weak<Inner>, every: Duration) {
    let spawned = thread::Builder::new()
        .name("orquestra-runner-tick".to_owned())
        .spawn(move || {
            loop {
                thread::sleep(every);
                let Some(inner) = inner.upgrade() else {
                    break;
                };
                let agents: Vec<Arc<Agent>> = lock(&inner.agents).values().cloned().collect();
                drop(inner);
                let now = Instant::now();
                for agent in agents {
                    agent.shared.tick(now);
                }
            }
        });
    if let Err(err) = spawned {
        warn!(%err, "sem thread de tick: a heurística de ociosidade fica desligada");
    }
}

fn spawn_thread(name: String, f: impl FnOnce() + Send + 'static) -> Result<()> {
    thread::Builder::new()
        .name(name)
        .spawn(f)
        .map(drop)
        .map_err(|err| Error::Spawn(format!("não foi possível criar thread: {err}")))
}

fn command_builder(spec: &CommandSpec) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(&spec.program);
    cmd.args(&spec.args);
    cmd.cwd(&spec.cwd);
    for key in &spec.env_remove {
        cmd.env_remove(key);
    }
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    cmd
}

/// Mata e colhe um processo que não chegou a ser registrado.
fn abandon(child: &mut Box<dyn Child + Send + Sync>) {
    let _ = child.kill();
    let _ = child.wait();
}

fn pty_size(size: TermSize) -> PtySize {
    let rows = size.rows.max(1);
    let cols = size.cols.max(1);
    PtySize {
        rows,
        cols,
        pixel_width: cols.saturating_mul(size.cell_width),
        pixel_height: rows.saturating_mul(size.cell_height),
    }
}
