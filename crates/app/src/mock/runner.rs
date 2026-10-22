//! Runner falso: cada agente é uma task que segue um roteiro de saída ANSI,
//! com `seq` correto, eco do input, `WaitingForInput` nas perguntas, saída
//! com código, replay e kill.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use orquestra_core::bytes::Bytes;
use orquestra_core::{
    AgentEvent, AgentId, AgentInfo, AgentRunner, AgentStatus, Error, EventBus, Result,
    SpawnRequest, TermSize, Timestamp, Workspace, WorktreeCleanup, WorktreeInfo,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::script::{self, BOLD, CLAUDE, GRAY, RESET, Step};
use crate::feed::lock;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

struct OutState {
    seq: u64,
    ring: VecDeque<u8>,
}

struct Agent {
    id: AgentId,
    bus: EventBus,
    out: Mutex<OutState>,
    replay_bytes: usize,
    input: mpsc::UnboundedSender<Bytes>,
    task: Mutex<Option<JoinHandle<()>>>,
    status: Mutex<AgentStatus>,
}

impl Agent {
    fn emit(&self, data: impl Into<Bytes>) {
        let data = data.into();
        if data.is_empty() {
            return;
        }
        // Publica com o lock: Output e Replay saem na ordem do seq.
        let mut out = lock(&self.out);
        let seq = out.seq;
        out.seq += 1;
        out.ring.extend(data.iter().copied());
        let excess = out.ring.len().saturating_sub(self.replay_bytes);
        out.ring.drain(..excess);
        self.bus.publish(AgentEvent::Output {
            id: self.id,
            seq,
            data,
        });
    }

    fn set_status(&self, status: AgentStatus) {
        let mut cur = lock(&self.status);
        if cur.is_terminal() || *cur == status {
            return;
        }
        *cur = status.clone();
        self.bus.publish(AgentEvent::StatusChanged {
            id: self.id,
            status,
        });
    }

    fn replay(&self) {
        let out = lock(&self.out);
        let data: Vec<u8> = out.ring.iter().copied().collect();
        self.bus.publish(AgentEvent::Replay {
            id: self.id,
            next_seq: out.seq,
            data: Bytes::from(data),
        });
    }

    fn is_alive(&self) -> bool {
        lock(&self.status).is_alive()
    }
}

/// O que o usuário digitou desde o último Enter.
#[derive(Default)]
struct LineEditor {
    line: String,
}

enum Typed {
    Nothing,
    Line(String),
    /// Escape sozinho: "não" em prompts de permissão.
    Escape,
    Interrupt,
}

impl LineEditor {
    /// Processa bytes do PTY: ecoa o que é imprimível e ignora respostas do
    /// terminal (`ESC [ ... R` etc.).
    fn feed(&mut self, data: &[u8], echo: &mut Vec<u8>) -> Typed {
        let text = String::from_utf8_lossy(data);
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => match chars.peek() {
                    Some('[') | Some('O') | Some(']') => {
                        chars.next();
                        // Pula até o byte final da sequência.
                        for c in chars.by_ref() {
                            if ('@'..='~').contains(&c) || c == '\x07' {
                                break;
                            }
                        }
                    }
                    None => return Typed::Escape,
                    Some(_) => {}
                },
                '\r' | '\n' => {
                    echo.extend(b"\r\n");
                    return Typed::Line(std::mem::take(&mut self.line));
                }
                '\x7f' | '\x08' => {
                    if self.line.pop().is_some() {
                        echo.extend(b"\x08 \x08");
                    }
                }
                '\x03' => {
                    echo.extend(b"^C\r\n");
                    self.line.clear();
                    return Typed::Interrupt;
                }
                c if !c.is_control() => {
                    self.line.push(c);
                    let mut buf = [0; 4];
                    echo.extend(c.encode_utf8(&mut buf).as_bytes());
                }
                _ => {}
            }
        }
        Typed::Nothing
    }
}

struct Session {
    agent: Arc<Agent>,
    input: mpsc::UnboundedReceiver<Bytes>,
    editor: LineEditor,
    scale: f64,
    is_shell: bool,
    cwd: String,
}

impl Session {
    fn ms(&self, ms: u64) -> Duration {
        Duration::from_secs_f64(ms as f64 / 1000.0 * self.scale)
    }

    /// Trata um chunk de input fora de pergunta. Devolve a linha, se houve Enter.
    fn typed(&mut self, data: &[u8]) -> Option<String> {
        let mut echo = Vec::new();
        let typed = self.editor.feed(data, &mut echo);
        self.agent.emit(echo);
        match typed {
            Typed::Line(line) => Some(line),
            _ => None,
        }
    }

    /// Resposta a uma mensagem digitada enquanto o agente trabalha.
    fn ack(&self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        if self.is_shell {
            self.agent.emit(format!(
                "{GRAY}mock: `{line}` não foi executado{RESET}\r\n\x1b[32mmock\x1b[0m:\x1b[36m{}\x1b[0m$ ",
                self.cwd
            ));
        } else {
            self.agent.emit(format!(
                "{BOLD}●{RESET} Anotado: “{line}”. Sigo com isso em mente.\r\n"
            ));
        }
    }

    async fn sleep(&mut self, ms: u64) {
        self.sleep_for(self.ms(ms)).await;
    }

    async fn spin(&mut self, label: &str, ms: u64) {
        let started = Instant::now();
        let total = self.ms(ms);
        let tick = self.ms(250).max(Duration::from_millis(1));
        let mut i = 0;
        while started.elapsed() < total {
            let secs = started.elapsed().as_secs();
            self.agent.emit(format!(
                "\r\x1b[2K{CLAUDE}{}{RESET} {label}… {GRAY}({secs}s · esc para interromper){RESET}",
                SPINNER[i % SPINNER.len()]
            ));
            i += 1;
            let wait = tick.min(total.saturating_sub(started.elapsed()));
            self.sleep_for(wait).await;
        }
        self.agent.emit("\r\x1b[2K");
    }

    /// Pausa atendendo o input (eco e resposta a mensagens).
    async fn sleep_for(&mut self, d: Duration) {
        let deadline = Instant::now() + d;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return,
                data = self.input.recv() => match data {
                    Some(data) => {
                        if let Some(line) = self.typed(&data) {
                            self.ack(&line);
                        }
                    }
                    None => return tokio::time::sleep_until(deadline).await,
                },
            }
        }
    }

    /// Espera uma linha inteira (ou Escape). `true` = resposta afirmativa.
    async fn ask(&mut self) -> bool {
        self.agent.set_status(AgentStatus::WaitingForInput);
        let answer = loop {
            let Some(data) = self.input.recv().await else {
                // Ninguém mais escreve: fica parado para sempre.
                return std::future::pending().await;
            };
            let mut echo = Vec::new();
            let typed = self.editor.feed(&data, &mut echo);
            self.agent.emit(echo);
            match typed {
                Typed::Line(line) => break !line.trim().to_lowercase().starts_with('n'),
                Typed::Escape | Typed::Interrupt => {
                    self.agent.emit("\r\n");
                    break false;
                }
                Typed::Nothing => {}
            }
        };
        self.agent.set_status(AgentStatus::Running);
        answer
    }

    /// Espera uma opção do menu: número, Enter (a primeira) ou Esc. `true`
    /// = sim (qualquer opção menos a última).
    async fn choose(&mut self, options: u8) -> bool {
        self.agent.set_status(AgentStatus::WaitingForInput);
        let answer = loop {
            let Some(data) = self.input.recv().await else {
                return std::future::pending().await;
            };
            match data.as_ref() {
                b"\r" => break true,
                b"\x1b" | b"\x03" => break false,
                [d @ b'1'..=b'9'] if d - b'0' <= options => break d - b'0' < options,
                _ => {}
            }
        };
        self.agent.set_status(AgentStatus::Running);
        answer
    }

    async fn run(mut self, steps: Vec<Step>) {
        self.agent.set_status(AgentStatus::Running);
        for step in steps {
            match step {
                Step::Out(text) => self.agent.emit(text),
                Step::Sleep(ms) => self.sleep(ms).await,
                Step::Spin(label, ms) => self.spin(label, ms).await,
                Step::Ask { prompt, on_no } => {
                    self.agent.emit(prompt);
                    if !self.ask().await {
                        self.agent.emit(format!("{BOLD}●{RESET} {on_no}\r\n"));
                    }
                }
                Step::Menu {
                    text,
                    options,
                    on_no,
                } => {
                    self.agent.emit(text.clone());
                    let yes = self.choose(options).await;
                    self.agent.emit(script::erase(&text));
                    if !yes {
                        self.agent.emit(format!("{BOLD}●{RESET} {on_no}\r\n"));
                    }
                }
                Step::Hold => loop {
                    let Some(data) = self.input.recv().await else {
                        return std::future::pending().await;
                    };
                    if let Some(line) = self.typed(&data) {
                        self.ack(&line);
                    }
                },
                Step::Exit(code) => {
                    let status = if code == 0 {
                        AgentStatus::Done { exit_code: Some(0) }
                    } else {
                        AgentStatus::Failed {
                            reason: format!("saiu com código {code}"),
                        }
                    };
                    self.agent.set_status(status);
                    return;
                }
            }
        }
        // Sem `Exit`: fica no prompt, como uma CLI interativa.
        self.agent.set_status(AgentStatus::WaitingForInput);
        while let Some(data) = self.input.recv().await {
            let Some(line) = self.typed(&data) else {
                continue;
            };
            if line.trim().is_empty() {
                continue;
            }
            if self.is_shell {
                self.ack(&line);
                continue;
            }
            self.agent.set_status(AgentStatus::Running);
            self.spin("Pensando", 1500).await;
            self.agent.emit(format!(
                "{BOLD}●{RESET} Entendido: “{line}”. Vou cuidar disso.\r\n"
            ));
            self.agent.set_status(AgentStatus::WaitingForInput);
        }
    }
}

pub struct MockRunner {
    bus: EventBus,
    agents: Mutex<HashMap<AgentId, Arc<Agent>>>,
    sizes: Mutex<HashMap<AgentId, TermSize>>,
    next_pid: AtomicU32,
    replay_bytes: usize,
    /// Multiplica todos os tempos dos roteiros.
    scale: f64,
}

impl MockRunner {
    pub fn new(bus: EventBus, replay_bytes: usize, scale: f64) -> Self {
        Self {
            bus,
            agents: Mutex::new(HashMap::new()),
            sizes: Mutex::new(HashMap::new()),
            next_pid: AtomicU32::new(41_000),
            replay_bytes: replay_bytes.clamp(4096, 1 << 20),
            scale,
        }
    }

    fn agent(&self, id: AgentId) -> Result<Arc<Agent>> {
        lock(&self.agents)
            .get(&id)
            .cloned()
            .ok_or(Error::UnknownAgent(id))
    }

    /// Tamanho do PTY falso (o último `resize`).
    pub fn size_of(&self, id: AgentId) -> Option<TermSize> {
        lock(&self.sizes).get(&id).copied()
    }
}

fn worktree_for(repo: &Path, req: &SpawnRequest) -> (PathBuf, Option<WorktreeInfo>) {
    let repo_name = repo
        .file_name()
        .map_or_else(|| "repo".to_owned(), |n| n.to_string_lossy().into_owned());
    let short = req.id.short();
    let path = PathBuf::from(format!("/tmp/orquestra-mock/worktrees/{repo_name}/{short}"));
    let branch = format!("orquestra/{}-{short}", req.config.name);
    let info = WorktreeInfo {
        repo: repo.to_owned(),
        path: path.clone(),
        branch,
        base_ref: "HEAD".to_owned(),
        base_commit: "4f2c9e1".to_owned(),
    };
    (path, Some(info))
}

impl AgentRunner for MockRunner {
    async fn spawn(&self, req: SpawnRequest) -> Result<AgentInfo> {
        if lock(&self.agents).contains_key(&req.id) {
            return Err(Error::AgentExists(req.id));
        }
        tokio::time::sleep(Duration::from_secs_f64(0.3 * self.scale)).await;
        let (workdir, worktree) = match &req.config.workspace {
            Workspace::Worktree { repo, .. } => worktree_for(repo, &req),
            Workspace::Directory { path } => (path.clone(), None),
            Workspace::Restore { path, worktree } => (path.clone(), worktree.clone()),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let agent = Arc::new(Agent {
            id: req.id,
            bus: self.bus.clone(),
            out: Mutex::new(OutState {
                seq: 0,
                ring: VecDeque::new(),
            }),
            replay_bytes: self.replay_bytes,
            input: tx,
            task: Mutex::new(None),
            status: Mutex::new(AgentStatus::Starting),
        });
        let cwd = workdir.display().to_string();
        let steps = script::script_for(&req.config, &cwd);
        let session = Session {
            agent: agent.clone(),
            input: rx,
            editor: LineEditor::default(),
            scale: self.scale,
            is_shell: matches!(
                req.config.kind,
                orquestra_core::AgentKind::Shell | orquestra_core::AgentKind::Custom
            ),
            cwd,
        };
        let task = tokio::spawn(session.run(steps));
        *lock(&agent.task) = Some(task);
        lock(&self.sizes).insert(req.id, req.config.size);
        lock(&self.agents).insert(req.id, agent);
        Ok(AgentInfo {
            id: req.id,
            config: req.config,
            workdir,
            worktree,
            pid: Some(self.next_pid.fetch_add(1, Ordering::Relaxed)),
            browser: req.browser,
            started_at: Timestamp::now(),
        })
    }

    async fn send_input(&self, id: AgentId, data: Bytes) -> Result<()> {
        let agent = self.agent(id)?;
        if agent.is_alive() {
            let _ = agent.input.send(data);
        }
        Ok(())
    }

    async fn resize(&self, id: AgentId, size: TermSize) -> Result<()> {
        self.agent(id)?;
        lock(&self.sizes).insert(id, size);
        Ok(())
    }

    async fn kill(&self, id: AgentId) -> Result<()> {
        let agent = self.agent(id)?;
        if let Some(task) = lock(&agent.task).take() {
            task.abort();
        }
        if agent.is_alive() {
            agent.emit(format!("\r\n{GRAY}[processo encerrado]{RESET}\r\n"));
            agent.set_status(AgentStatus::Done { exit_code: None });
        }
        Ok(())
    }

    async fn remove(&self, id: AgentId, _cleanup: WorktreeCleanup) -> Result<()> {
        let agent = lock(&self.agents)
            .remove(&id)
            .ok_or(Error::UnknownAgent(id))?;
        lock(&self.sizes).remove(&id);
        if let Some(task) = lock(&agent.task).take() {
            task.abort();
        }
        Ok(())
    }

    async fn request_replay(&self, id: AgentId) -> Result<()> {
        self.agent(id)?.replay();
        Ok(())
    }

    async fn shutdown(&self) {
        let agents: Vec<_> = lock(&self.agents).values().cloned().collect();
        for agent in agents {
            if let Some(task) = lock(&agent.task).take() {
                task.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use orquestra_core::{AgentConfig, AgentKind, AgentRole, SessionId};

    use super::*;

    fn request(name: &str, kind: AgentKind) -> SpawnRequest {
        SpawnRequest {
            id: AgentId::generate(),
            config: AgentConfig {
                name: name.to_owned(),
                kind,
                program: "claude".to_owned(),
                args: Vec::new(),
                env: BTreeMap::new(),
                session: SessionId::generate(),
                role: AgentRole::Worker,
                parent: None,
                workspace: Workspace::Worktree {
                    repo: "/src/loja".into(),
                    base: None,
                    branch: None,
                },
                initial_prompt: Some("tarefa".to_owned()),
                browser: false,
                size: TermSize::default(),
            },
            browser: None,
            mcp_servers: Vec::new(),
            env: BTreeMap::new(),
            stop_hook: None,
            instructions: None,
        }
    }

    async fn status_until(
        rx: &mut orquestra_core::EventReceiver,
        id: AgentId,
        pred: impl Fn(&AgentStatus) -> bool,
    ) -> AgentStatus {
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("status não chegou")
                .unwrap();
            if let AgentEvent::StatusChanged { id: i, status } = ev
                && i == id
                && pred(&status)
            {
                return status;
            }
        }
    }

    #[test]
    fn line_editor_echoes_and_skips_terminal_replies() {
        let mut ed = LineEditor::default();
        let mut echo = Vec::new();
        assert!(matches!(ed.feed(b"\x1b[3;1Rsi", &mut echo), Typed::Nothing));
        assert!(matches!(ed.feed(b"\x7fm\r", &mut echo), Typed::Line(l) if l == "sm"));
        assert_eq!(echo, b"si\x08 \x08m\r\n");
        assert!(matches!(ed.feed(b"\x1b", &mut echo), Typed::Escape));
    }

    #[tokio::test]
    async fn worker_asks_waits_for_input_and_finishes() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let runner = MockRunner::new(bus.clone(), 1 << 16, 0.001);
        let req = request("login-ui", AgentKind::ClaudeCode);
        let id = req.id;
        let info = runner.spawn(req).await.unwrap();
        assert!(
            info.worktree
                .unwrap()
                .branch
                .starts_with("orquestra/login-ui-")
        );

        status_until(&mut rx, id, |s| *s == AgentStatus::WaitingForInput).await;
        runner
            .send_input(id, Bytes::from_static(b"1"))
            .await
            .unwrap();
        let done = status_until(&mut rx, id, AgentStatus::is_terminal).await;
        assert_eq!(done, AgentStatus::Done { exit_code: Some(0) });

        // Replay devolve tudo e o próximo seq.
        runner.request_replay(id).await.unwrap();
        let mut seqs = 0;
        loop {
            match rx.recv().await.unwrap() {
                AgentEvent::Output { .. } => seqs += 1,
                AgentEvent::Replay { next_seq, data, .. } => {
                    assert!(next_seq > 0);
                    assert!(String::from_utf8_lossy(&data).contains("Claude Code"));
                    break;
                }
                _ => {}
            }
        }
        let _ = seqs;
        runner.remove(id, WorktreeCleanup::Keep).await.unwrap();
        assert!(matches!(
            runner.remove(id, WorktreeCleanup::Keep).await,
            Err(Error::UnknownAgent(_))
        ));
    }

    #[tokio::test]
    async fn failing_worker_and_kill() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let runner = MockRunner::new(bus.clone(), 1 << 16, 0.001);
        let req = request("estilos", AgentKind::ClaudeCode);
        let id = req.id;
        runner.spawn(req).await.unwrap();
        let st = status_until(&mut rx, id, AgentStatus::is_terminal).await;
        assert!(matches!(st, AgentStatus::Failed { .. }));

        let req = request("qa-navegador", AgentKind::ClaudeCode);
        let id = req.id;
        runner.spawn(req).await.unwrap();
        status_until(&mut rx, id, |s| *s == AgentStatus::Running).await;
        runner.kill(id).await.unwrap();
        let st = status_until(&mut rx, id, AgentStatus::is_terminal).await;
        assert_eq!(st, AgentStatus::Done { exit_code: None });
    }
}
