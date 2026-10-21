//! Testes do `Runner` com processos reais no PTY (`sh -c '…'`) e worktrees num
//! repositório git temporário. Sem rede e sem `claude`/`codex`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use bytes::Bytes;
use orquestra_core::{
    AgentConfig, AgentEvent, AgentId, AgentKind, AgentRole, AgentRunner, AgentStatus,
    BrowserEndpoint, Error, EventBus, EventReceiver, RunnerConfig, SessionId, SpawnRequest,
    TermSize, Workspace, WorktreeCleanup,
};
use orquestra_runner::Runner;

const TIMEOUT: Duration = Duration::from_secs(10);
/// O mesmo comando que os hooks injetados rodam (ver `command.rs`).
const WAITING_HOOK: &str =
    r"{ printf '\033]777;orquestra;waiting\007' > /dev/tty; } 2>/dev/null || true";
const RUNNING_HOOK: &str =
    r"{ printf '\033]777;orquestra;running\007' > /dev/tty; } 2>/dev/null || true";

fn config() -> RunnerConfig {
    RunnerConfig {
        // Longe o bastante para a heurística não interferir, salvo onde testada.
        idle_ms: 60_000,
        kill_grace_ms: 300,
        ..RunnerConfig::default()
    }
}

fn request(kind: AgentKind, program: &str, args: &[&str], workspace: Workspace) -> SpawnRequest {
    SpawnRequest {
        id: AgentId::generate(),
        config: AgentConfig {
            name: "Meu Agente".to_owned(),
            kind,
            program: program.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            env: BTreeMap::new(),
            session: SessionId::generate(),
            role: AgentRole::Worker,
            parent: None,
            workspace,
            initial_prompt: None,
            browser: false,
            size: TermSize::default(),
        },
        browser: None,
        mcp_servers: Vec::new(),
        env: BTreeMap::new(),
        stop_hook: None,
    }
}

fn here() -> Workspace {
    Workspace::Directory {
        path: std::env::temp_dir(),
    }
}

fn sh(script: &str) -> SpawnRequest {
    request(AgentKind::Custom, "sh", &["-c", script], here())
}

/// Eventos de um agente, acumulando saída e status pelo caminho.
struct Watch {
    rx: EventReceiver,
    id: AgentId,
    out: Vec<u8>,
    seqs: Vec<u64>,
    statuses: Vec<AgentStatus>,
}

impl Watch {
    fn new(bus: &EventBus, id: AgentId) -> Self {
        Self {
            rx: bus.subscribe(),
            id,
            out: Vec::new(),
            seqs: Vec::new(),
            statuses: Vec::new(),
        }
    }

    async fn next_within(&mut self, timeout: Duration) -> Option<AgentEvent> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let event = tokio::time::timeout_at(deadline, self.rx.recv())
                .await
                .ok()?
                .expect("bus fechado ou atrasado");
            if event.agent_id() != Some(self.id) {
                continue;
            }
            match &event {
                AgentEvent::Output { seq, data, .. } => {
                    self.seqs.push(*seq);
                    self.out.extend_from_slice(data);
                }
                AgentEvent::StatusChanged { status, .. } => self.statuses.push(status.clone()),
                _ => {}
            }
            return Some(event);
        }
    }

    async fn next(&mut self) -> AgentEvent {
        let text = self.text();
        self.next_within(TIMEOUT)
            .await
            .unwrap_or_else(|| panic!("nenhum evento em {TIMEOUT:?}; saída até aqui: {text:?}"))
    }

    async fn until(&mut self, pred: impl Fn(&AgentStatus) -> bool) -> AgentStatus {
        loop {
            if let AgentEvent::StatusChanged { status, .. } = self.next().await
                && pred(&status)
            {
                return status;
            }
        }
    }

    async fn terminal(&mut self) -> AgentStatus {
        self.until(AgentStatus::is_terminal).await
    }

    async fn output_contains(&mut self, needle: &str) {
        while !self.text().contains(needle) {
            self.next().await;
        }
    }

    /// Nenhuma mudança de status por `dur`.
    async fn no_status_change(&mut self, dur: Duration) {
        let deadline = Instant::now() + dur;
        while let Some(event) = self
            .next_within(deadline.saturating_duration_since(Instant::now()))
            .await
        {
            if let AgentEvent::StatusChanged { status, .. } = event {
                panic!("status mudou para {status:?}");
            }
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }
}

async fn start(runner: &Runner, bus: &EventBus, req: SpawnRequest) -> Watch {
    let mut watch = Watch::new(bus, req.id);
    runner.spawn(req).await.expect("spawn");
    assert_eq!(watch.until(|_| true).await, AgentStatus::Running);
    watch
}

fn process_gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_arrives_in_order_with_contiguous_seq() {
    let bus = EventBus::new();
    let runner = Runner::new(bus.clone(), config());
    let req = sh("i=1; while [ $i -le 3000 ]; do echo \"linha $i\"; i=$((i+1)); done");
    let mut watch = start(&runner, &bus, req).await;

    assert_eq!(
        watch.terminal().await,
        AgentStatus::Done { exit_code: Some(0) }
    );
    let expected: Vec<u64> = (0..watch.seqs.len() as u64).collect();
    assert_eq!(watch.seqs, expected);
    let text = watch.text();
    let mut at = 0;
    for i in 1..=3000 {
        let line = format!("linha {i}\r\n");
        let found = text[at..]
            .find(&line)
            .unwrap_or_else(|| panic!("falta {line:?}"));
        at += found + line.len();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_reaches_the_process() {
    let bus = EventBus::new();
    let runner = Runner::new(bus.clone(), config());
    let req = sh("read linha; echo \"recebi:$linha\"");
    let id = req.id;
    let mut watch = start(&runner, &bus, req).await;

    runner
        .send_input(id, Bytes::from_static(b"ola mundo\r"))
        .await
        .unwrap();
    watch.output_contains("recebi:ola mundo").await;
    assert_eq!(
        watch.terminal().await,
        AgentStatus::Done { exit_code: Some(0) }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_codes_become_done_or_failed() {
    let bus = EventBus::new();
    let runner = Runner::new(bus.clone(), config());

    let mut ok = start(&runner, &bus, sh("exit 0")).await;
    assert_eq!(
        ok.terminal().await,
        AgentStatus::Done { exit_code: Some(0) }
    );

    let mut bad = start(&runner, &bus, sh("exit 3")).await;
    match bad.terminal().await {
        AgentStatus::Failed { reason } => assert!(reason.contains('3'), "{reason}"),
        other => panic!("esperava Failed, veio {other:?}"),
    }

    let mut signaled = start(&runner, &bus, sh("kill -9 $$")).await;
    assert!(matches!(
        signaled.terminal().await,
        AgentStatus::Failed { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_takes_down_the_whole_group() {
    let bus = EventBus::new();
    let runner = Runner::new(bus.clone(), config());
    let req = sh("sleep 30 & echo \"bg:$!:\"; wait");
    let id = req.id;
    let mut watch = start(&runner, &bus, req).await;

    watch.output_contains(":\r\n").await;
    let text = watch.text();
    let pid: u32 = text
        .split("bg:")
        .nth(1)
        .and_then(|rest| rest.split(':').next())
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("sem pid em {text:?}"));
    assert!(!process_gone(pid));

    runner.kill(id).await.unwrap();
    assert_eq!(
        watch.terminal().await,
        AgentStatus::Done { exit_code: None }
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while !process_gone(pid) {
        assert!(
            Instant::now() < deadline,
            "o filho em background sobreviveu"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Já morto: kill continua Ok.
    runner.kill(id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_escalates_to_sigkill_after_grace() {
    let bus = EventBus::new();
    let runner = Runner::new(bus.clone(), config());
    let req = sh("trap '' TERM; echo pronto; sleep 30");
    let id = req.id;
    let mut watch = start(&runner, &bus, req).await;
    watch.output_contains("pronto").await;

    let t0 = Instant::now();
    runner.kill(id).await.unwrap();
    assert!(
        t0.elapsed() < Duration::from_millis(200),
        "kill deve voltar rápido"
    );
    assert_eq!(
        watch.terminal().await,
        AgentStatus::Done { exit_code: None }
    );
    assert!(t0.elapsed() >= Duration::from_millis(300), "antes do grace");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_returns_the_tail_of_the_output() {
    let bus = EventBus::new();
    let runner = Runner::new(
        bus.clone(),
        RunnerConfig {
            replay_bytes: 16,
            ..config()
        },
    );
    let req = sh("printf 'AAAAAAAAAAAAAAAAAAAA'; sleep 0.3; printf 'fim-da-saida'; sleep 30");
    let id = req.id;
    let mut watch = start(&runner, &bus, req).await;
    watch.output_contains("fim-da-saida").await;

    runner.request_replay(id).await.unwrap();
    loop {
        if let AgentEvent::Replay { next_seq, data, .. } = watch.next().await {
            assert_eq!(next_seq, watch.seqs.len() as u64);
            assert_eq!(&data[..], b"AAAAfim-da-saida");
            break;
        }
    }
    runner.kill(id).await.unwrap();
    watch.terminal().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn osc_from_hooks_drives_the_status() {
    let bus = EventBus::new();
    let runner = Runner::new(bus.clone(), config());
    let script = format!("{WAITING_HOOK}; sleep 0.4; {RUNNING_HOOK}; sleep 0.4");
    let mut watch = start(&runner, &bus, sh(&script)).await;

    watch.terminal().await;
    assert_eq!(
        watch.statuses,
        [
            AgentStatus::Running,
            AgentStatus::WaitingForInput,
            AgentStatus::Running,
            AgentStatus::Done { exit_code: Some(0) },
        ]
    );
    // Passou por /dev/tty e chega crua no `Output` (o terminal da UI a ignora).
    assert!(watch.text().contains("\x1b]777;orquestra;waiting\x07"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_shell_at_a_prompt_waits_for_input() {
    let bus = EventBus::new();
    let runner = Runner::new(
        bus.clone(),
        RunnerConfig {
            idle_ms: 200,
            ..config()
        },
    );
    let req = request(
        AgentKind::Shell,
        "sh",
        &["-c", "printf 'meu-prompt$ '; sleep 30"],
        here(),
    );
    let id = req.id;
    let mut watch = start(&runner, &bus, req).await;

    assert_eq!(watch.until(|_| true).await, AgentStatus::WaitingForInput);
    // O eco da digitação não acorda o agente.
    runner
        .send_input(id, Bytes::from_static(b"abc"))
        .await
        .unwrap();
    watch.no_status_change(Duration::from_millis(600)).await;
    assert!(watch.text().contains("abc"));
    runner.kill(id).await.unwrap();
    watch.terminal().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_shell_without_prompt_keeps_running() {
    let bus = EventBus::new();
    let runner = Runner::new(
        bus.clone(),
        RunnerConfig {
            idle_ms: 200,
            ..config()
        },
    );
    let req = request(
        AgentKind::Shell,
        "sh",
        &["-c", "echo trabalhando...; sleep 30"],
        here(),
    );
    let id = req.id;
    let mut watch = start(&runner, &bus, req).await;
    watch.no_status_change(Duration::from_millis(800)).await;
    runner.kill(id).await.unwrap();
    watch.terminal().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_receives_initial_prompt_on_the_pty() {
    let bus = EventBus::new();
    let runner = Runner::new(bus.clone(), config());
    let mut req = request(AgentKind::Shell, "sh", &[], here());
    req.config.initial_prompt = Some("echo inicial-$((40+2))".to_owned());
    let id = req.id;
    let mut watch = start(&runner, &bus, req).await;

    watch.output_contains("inicial-42").await;
    runner
        .send_input(id, Bytes::from_static(b"exit\r"))
        .await
        .unwrap();
    assert_eq!(
        watch.terminal().await,
        AgentStatus::Done { exit_code: Some(0) }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resize_reaches_the_pty() {
    let bus = EventBus::new();
    let runner = Runner::new(bus.clone(), config());
    let req = sh("read l; stty size");
    let id = req.id;
    let mut watch = start(&runner, &bus, req).await;

    let size = TermSize {
        cols: 200,
        rows: 50,
        cell_width: 8,
        cell_height: 16,
    };
    runner.resize(id, size).await.unwrap();
    runner
        .send_input(id, Bytes::from_static(b"\r"))
        .await
        .unwrap();
    watch.output_contains("50 200").await;
    watch.terminal().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environment_is_layered_and_cleaned() {
    let bus = EventBus::new();
    let runner = Runner::new(bus.clone(), config());
    let mut req = sh(
        "echo \"id=$ORQUESTRA_AGENT_ID sess=$ORQUESTRA_SESSION_ID term=$TERM \
         color=$COLORTERM cdp=$ORQUESTRA_CDP_URL x=$X_TESTE cc=${CLAUDECODE:-nenhum}\"",
    );
    req.config
        .env
        .insert("X_TESTE".to_owned(), "config".to_owned());
    req.env.insert("X_TESTE".to_owned(), "hub".to_owned());
    req.browser = Some(BrowserEndpoint {
        port: 9555,
        ws_url: "ws://127.0.0.1:9555/devtools/browser/x".to_owned(),
    });
    let (id, session) = (req.id, req.config.session);
    let mut watch = start(&runner, &bus, req).await;
    watch.terminal().await;

    let text = watch.text();
    assert!(text.contains(&format!("id={id} sess={session}")), "{text}");
    assert!(
        text.contains("term=xterm-256color color=truecolor"),
        "{text}"
    );
    assert!(text.contains("cdp=http://127.0.0.1:9555"), "{text}");
    assert!(text.contains("x=hub"), "{text}");
    assert!(text.contains("cc=nenhum"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_kills_and_forgets() {
    let bus = EventBus::new();
    let runner = Runner::new(bus.clone(), config());
    let req = sh("sleep 30");
    let id = req.id;
    let mut watch = start(&runner, &bus, req).await;

    runner.remove(id, WorktreeCleanup::Keep).await.unwrap();
    assert_eq!(
        watch.terminal().await,
        AgentStatus::Done { exit_code: None }
    );
    assert!(matches!(
        runner.request_replay(id).await,
        Err(Error::UnknownAgent(_))
    ));
    assert!(matches!(
        runner.remove(id, WorktreeCleanup::Keep).await,
        Err(Error::UnknownAgent(_))
    ));
    assert!(matches!(
        runner.send_input(AgentId::generate(), Bytes::new()).await,
        Err(Error::UnknownAgent(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_errors_are_reported() {
    let bus = EventBus::new();
    let runner = Runner::new(bus.clone(), config());

    let missing = request(AgentKind::Custom, "/nao/existe/prog", &[], here());
    assert!(matches!(runner.spawn(missing).await, Err(Error::Spawn(_))));

    let nowhere = request(
        AgentKind::Custom,
        "sh",
        &[],
        Workspace::Directory {
            path: PathBuf::from("/nao/existe/dir"),
        },
    );
    assert!(matches!(runner.spawn(nowhere).await, Err(Error::Spawn(_))));

    let req = sh("sleep 30");
    let id = req.id;
    let mut watch = start(&runner, &bus, req.clone()).await;
    assert!(matches!(
        runner.spawn(req).await,
        Err(Error::AgentExists(_))
    ));
    runner.kill(id).await.unwrap();
    watch.terminal().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_kills_everyone() {
    let bus = EventBus::new();
    let runner = Runner::new(bus.clone(), config());
    let mut a = start(&runner, &bus, sh("sleep 30")).await;
    let mut b = start(&runner, &bus, sh("trap '' TERM; sleep 30")).await;

    runner.shutdown().await;
    assert_eq!(a.terminal().await, AgentStatus::Done { exit_code: None });
    assert_eq!(b.terminal().await, AgentStatus::Done { exit_code: None });
}

// ---- worktrees ----

fn git(dir: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=teste",
            "-c",
            "user.email=teste@exemplo",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("git");
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]).unwrap();
    git(
        dir.path(),
        &["commit", "-q", "--allow-empty", "-m", "inicio"],
    )
    .unwrap();
    dir
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

fn worktree_runner(bus: &EventBus, root: &Path) -> Runner {
    Runner::new(
        bus.clone(),
        RunnerConfig {
            worktree_root: Some(root.to_owned()),
            ..config()
        },
    )
}

fn worktree(repo: &Path) -> Workspace {
    Workspace::Worktree {
        repo: repo.to_owned(),
        base: None,
        branch: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_is_created_and_removed_with_its_branch() {
    let repo = init_repo();
    let root = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let runner = worktree_runner(&bus, root.path());
    let req = request(
        AgentKind::Custom,
        "sh",
        &["-c", "git rev-parse --abbrev-ref HEAD"],
        worktree(repo.path()),
    );
    let id = req.id;
    let mut watch = Watch::new(&bus, id);
    let info = runner.spawn(req).await.unwrap();

    let wt = info.worktree.clone().expect("worktree");
    let branch = format!("orquestra/meu-agente-{}", id.short());
    assert_eq!(wt.branch, branch);
    assert_eq!(wt.base_ref, "HEAD");
    assert_eq!(
        wt.base_commit,
        git(repo.path(), &["rev-parse", "HEAD"]).unwrap()
    );
    assert_eq!(wt.repo, repo.path().canonicalize().unwrap());
    assert_eq!(info.workdir, wt.path);
    assert!(wt.path.is_dir());
    assert_eq!(wt.path.file_name().unwrap(), id.short().as_str());
    let repo_dir = wt.path.parent().unwrap();
    assert_eq!(repo_dir.parent().unwrap(), root.path());
    let dir_name = repo_dir.file_name().unwrap().to_string_lossy().into_owned();
    let (_, hash) = dir_name.rsplit_once('-').expect("<repo>-<hash8>");
    assert!(
        hash.len() == 8 && hash.chars().all(|c| c.is_ascii_hexdigit()),
        "{dir_name}"
    );
    assert!(branch_exists(repo.path(), &branch));

    watch.output_contains(&branch).await;
    assert_eq!(
        watch.terminal().await,
        AgentStatus::Done { exit_code: Some(0) }
    );

    runner
        .remove(id, WorktreeCleanup::RemoveWorktreeAndBranch)
        .await
        .unwrap();
    assert!(!wt.path.exists());
    assert!(!branch_exists(repo.path(), &branch));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_modes_keep_what_they_promise() {
    let repo = init_repo();
    let root = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let runner = worktree_runner(&bus, root.path());

    let mut infos = Vec::new();
    for _ in 0..2 {
        let req = request(
            AgentKind::Custom,
            "sh",
            &["-c", "sleep 30"],
            worktree(repo.path()),
        );
        infos.push(runner.spawn(req).await.unwrap());
    }
    let keep = infos[0].worktree.clone().unwrap();
    let only_dir = infos[1].worktree.clone().unwrap();

    runner
        .remove(infos[0].id, WorktreeCleanup::Keep)
        .await
        .unwrap();
    assert!(keep.path.is_dir());
    assert!(branch_exists(repo.path(), &keep.branch));

    runner
        .remove(infos[1].id, WorktreeCleanup::RemoveWorktree)
        .await
        .unwrap();
    assert!(!only_dir.path.exists());
    assert!(branch_exists(repo.path(), &only_dir.branch));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_honors_base_and_branch() {
    let repo = init_repo();
    let first = git(repo.path(), &["rev-parse", "HEAD"]).unwrap();
    git(
        repo.path(),
        &["commit", "-q", "--allow-empty", "-m", "segundo"],
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let runner = worktree_runner(&bus, root.path());

    let req = request(
        AgentKind::Custom,
        "sh",
        &["-c", "git rev-parse HEAD"],
        Workspace::Worktree {
            repo: repo.path().to_owned(),
            base: Some(first.clone()),
            branch: Some("feature/x".to_owned()),
        },
    );
    let id = req.id;
    let mut watch = Watch::new(&bus, id);
    let info = runner.spawn(req).await.unwrap();
    let wt = info.worktree.unwrap();
    assert_eq!(wt.branch, "feature/x");
    assert_eq!(wt.base_commit, first);
    watch.output_contains(&first).await;
    watch.terminal().await;
    runner
        .remove(id, WorktreeCleanup::RemoveWorktreeAndBranch)
        .await
        .unwrap();
    assert!(!branch_exists(repo.path(), "feature/x"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_spawn_undoes_the_worktree() {
    let repo = init_repo();
    let root = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let runner = worktree_runner(&bus, root.path());

    let req = request(
        AgentKind::Custom,
        "/nao/existe/prog",
        &[],
        worktree(repo.path()),
    );
    let id = req.id;
    assert!(matches!(runner.spawn(req).await, Err(Error::Spawn(_))));
    let branch = format!("orquestra/meu-agente-{}", id.short());
    assert!(!branch_exists(repo.path(), &branch));
    let leftovers: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .flat_map(|repo_dir| std::fs::read_dir(repo_dir.unwrap().path()).unwrap())
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert_eq!(
        git(repo.path(), &["worktree", "list"])
            .unwrap()
            .lines()
            .count(),
        1
    );

    let not_a_repo = tempfile::tempdir().unwrap();
    let req = request(AgentKind::Custom, "sh", &[], worktree(not_a_repo.path()));
    assert!(matches!(runner.spawn(req).await, Err(Error::Git(_))));
}
