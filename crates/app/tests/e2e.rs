//! Ponta a ponta com os serviços reais (PTY, git, Chromium, SQLite e gateway
//! MCP) atrás do mesmo `backend::start` que a UI usa. Sem LLM: os agentes são
//! scripts `sh`, e o orquestrador é um script que expõe a credencial que o hub
//! lhe deu, para o teste falar com o gateway como o CLI falaria.
//!
//! `cargo test -p orquestra-app --test e2e -- --ignored --nocapture`

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use orquestra_app::backend::{self, Backend};
use orquestra_app::mock::LogNotifier;
use orquestra_browser::{Chromium, find_executable};
use orquestra_core::{
    AgentConfig, AgentEvent, AgentId, AgentInfo, AgentKind, AgentPreset, AgentRole, AgentStatus,
    AppConfig, BrowserInput, Command, CommandSender, EventReceiver, MouseAction, MouseButton,
    MouseInput, Services, SessionConfig, SessionId, TermSize, Workspace,
};
use orquestra_mcp::{GatewayClient, McpGateway};
use orquestra_runner::Runner;
use orquestra_store::SqliteStore;
use serde_json::{Value, json};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(30);

/// Worker "da UI": diz onde está, grava um arquivo no worktree e pede input.
const WORKER: &str = r#"echo "agente $AGENTE em $(pwd)"; echo "cdp=${ORQUESTRA_CDP_URL:-nenhum}"; date > "feito-$AGENTE.txt"; printf 'continuar? '; read r; echo "resposta: $r""#;

/// Preset "eco" usado pelo `spawn_agent` do gateway: lê a tarefa do PTY.
const ECO: &str = r#"read tarefa; echo "tarefa recebida: $tarefa"; echo "$tarefa" > tarefa.txt; printf 'mais alguma coisa? '; read r; echo "fim: $r""#;

/// Orquestrador falso: mostra a credencial que recebeu e fica esperando.
const ORQ: &str = r#"echo "SOCK=$ORQUESTRA_SOCKET"; echo "TOK=$ORQUESTRA_TOKEN"; printf 'orquestrador> '; read x"#;

struct World {
    cmd: CommandSender,
    events: EventReceiver,
    frames: EventReceiver,
    hub: JoinHandle<()>,
    log: Vec<AgentEvent>,
    started: Instant,
}

impl World {
    fn send(&self, cmd: Command) {
        self.cmd.send(cmd).expect("hub parou");
    }

    /// Lê eventos (guardando todos) até `done` valer sobre o que já chegou.
    /// Olha o histórico antes: a ordem entre agentes não é garantida.
    async fn wait(&mut self, what: &str, done: impl Fn(&[AgentEvent]) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done(&self.log) {
            let event = tokio::time::timeout_at(deadline, self.events.recv())
                .await
                .unwrap_or_else(|_| panic!("timeout esperando: {what}"))
                .expect("bus fechado");
            if let AgentEvent::Error { message, .. } = &event {
                eprintln!("  [erro do hub] {message}");
            }
            self.log.push(event);
        }
        eprintln!("  {:>6} ms  {what}", self.started.elapsed().as_millis());
    }

    async fn until(&mut self, what: &str, pred: impl Fn(&AgentEvent) -> bool) -> AgentEvent {
        self.wait(what, |log| log.iter().any(&pred)).await;
        self.log.iter().find(|e| pred(e)).cloned().unwrap()
    }

    /// Espera o último status conhecido de `id` satisfazer `pred`.
    async fn status(&mut self, id: AgentId, what: &str, pred: impl Fn(&AgentStatus) -> bool) {
        self.wait(what, |log| {
            log.iter()
                .rev()
                .find_map(|e| match e {
                    AgentEvent::StatusChanged { id: i, status } if *i == id => Some(status),
                    _ => None,
                })
                .is_some_and(&pred)
        })
        .await;
    }

    async fn spawned(&mut self, id: AgentId) -> AgentInfo {
        match self
            .until(
                "spawned",
                |e| matches!(e, AgentEvent::Spawned { info } if info.id == id),
            )
            .await
        {
            AgentEvent::Spawned { info } => (*info).clone(),
            _ => unreachable!(),
        }
    }

    /// Saída do PTY do agente até agora, sem `\r`.
    fn output(&self, id: AgentId) -> String {
        let mut out = String::new();
        for e in &self.log {
            if let AgentEvent::Output { id: i, data, .. } = e
                && *i == id
            {
                out.push_str(&String::from_utf8_lossy(data));
            }
        }
        out.replace('\r', "")
    }

    async fn output_contains(&mut self, id: AgentId, needle: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.output(id).contains(needle) {
            assert!(
                Instant::now() < deadline,
                "saída sem {needle:?}:\n{}",
                self.output(id)
            );
            let _ = tokio::time::timeout(Duration::from_millis(200), async {
                if let Ok(e) = self.events.recv().await {
                    self.log.push(e);
                }
            })
            .await;
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Process::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=e2e", "-c", "user.email=e2e@exemplo"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Repositório com uma página cujo botão (canto superior esquerdo, 400×300)
/// navega para `clicado.html`.
fn make_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("loja");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(
        repo.join("index.html"),
        r#"<!doctype html><title>Início</title><body style="margin:0">
<button style="position:absolute;left:0;top:0;width:400px;height:300px"
 onclick="location.href='clicado.html'">Entrar</button></body>"#,
    )
    .unwrap();
    std::fs::write(
        repo.join("clicado.html"),
        "<!doctype html><title>Clicado</title>ok",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "página"]);
    repo
}

fn script_agent(session: SessionId, name: &str, script: &str, browser: bool) -> AgentConfig {
    AgentConfig {
        name: name.to_owned(),
        kind: AgentKind::Custom,
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), script.to_owned()],
        env: BTreeMap::from([("AGENTE".to_owned(), name.to_owned())]),
        session,
        role: AgentRole::Worker,
        parent: None,
        workspace: Workspace::Worktree {
            repo: PathBuf::new(),
            base: None,
            branch: None,
        },
        initial_prompt: None,
        browser,
        size: TermSize::default(),
    }
}

fn click(x: f64, y: f64, action: MouseAction) -> BrowserInput {
    BrowserInput::Mouse(MouseInput {
        action,
        x,
        y,
        button: MouseButton::Left,
        click_count: 1,
        modifiers: Default::default(),
    })
}

/// Fala JSON-RPC com o binário `orquestra mcp` real, como o CLI faria.
fn bridge_call(exe: &str, env: &[(&str, &str)], tool: &str, args: Value) -> Value {
    let mut child = Process::new(exe)
        .arg("mcp")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut send = |v: Value| writeln!(stdin, "{v}").unwrap();
    send(
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},
        "clientInfo":{"name":"e2e","version":"0"}}}),
    );
    send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":tool,"arguments":args}}));
    let mut line = String::new();
    let result = loop {
        line.clear();
        assert!(
            stdout.read_line(&mut line).unwrap() > 0,
            "ponte fechou o stdout"
        );
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["id"] == 2 {
            break v;
        }
    };
    drop(stdin);
    let _ = child.wait();
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "sobe PTYs, git e Chromium de verdade"]
async fn tres_agentes_navegador_e_orquestrador_via_mcp() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_repo(tmp.path());
    let chromium_ok = find_executable(&Default::default()).is_some();

    let mut config = AppConfig::default();
    config.runner.worktree_root = Some(tmp.path().join("worktrees"));
    config.runner.idle_ms = 400;
    config.presets.push(AgentPreset {
        name: "eco".to_owned(),
        kind: AgentKind::Custom,
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), ECO.to_owned()],
        env: BTreeMap::new(),
        browser: false,
    });
    let socket = tmp.path().join("gw.sock");
    let store = SqliteStore::open(
        &tmp.path().join("db.sqlite"),
        &tmp.path().join("config.toml"),
    )
    .unwrap();

    let started = Instant::now();
    let Backend {
        cmd,
        events,
        frames,
        hub,
        ..
    } = backend::start(&Handle::current(), &config, |bus, cmd| {
        Ok(Services {
            runner: Runner::new(bus.clone(), config.runner.clone()),
            browser: Chromium::new(bus.clone(), config.browser.clone()),
            store,
            notifier: LogNotifier,
            gateway: McpGateway::start_at(cmd.clone(), bus.clone(), config.clone(), &socket)?,
        })
    })
    .unwrap();
    let mut w = World {
        cmd,
        events,
        frames,
        hub,
        log: Vec::new(),
        started,
    };

    // --- Orquestração com 3 agentes ao mesmo tempo, cada um no seu worktree.
    eprintln!("== 3 agentes em paralelo");
    let sid = SessionId::generate();
    let mut session = SessionConfig::new("e2e", &repo, "testar o orquestra");
    session.allowed_kinds.push(AgentKind::Custom);
    w.send(Command::CreateSession {
        id: sid,
        config: Box::new(session),
    });
    w.until("sessão criada", |e| {
        matches!(e, AgentEvent::SessionCreated { .. })
    })
    .await;

    let names = ["front", "qa", "api"];
    let ids: Vec<AgentId> = names.iter().map(|_| AgentId::generate()).collect();
    for (id, name) in ids.iter().zip(names) {
        let mut cfg = script_agent(sid, name, WORKER, name == "qa" && chromium_ok);
        cfg.workspace = Workspace::Worktree {
            repo: repo.clone(),
            base: None,
            branch: None,
        };
        w.send(Command::Spawn {
            id: *id,
            config: Box::new(cfg),
        });
    }
    let mut infos = HashMap::new();
    for (id, name) in ids.iter().zip(names) {
        let info = w.spawned(*id).await;
        infos.insert(name, info);
    }
    for id in &ids {
        w.status(*id, "aguardando input", |s| {
            *s == AgentStatus::WaitingForInput
        })
        .await;
    }
    // Os três estão vivos ao mesmo tempo, cada um no próprio worktree e branch.
    let worktrees = git(&repo, &["worktree", "list"]);
    let mut branches: Vec<&str> = Vec::new();
    for name in names {
        let wt = infos[name].worktree.as_ref().expect("sem worktree");
        assert!(wt.path.join(format!("feito-{name}.txt")).exists());
        for other in names.iter().filter(|o| **o != name) {
            assert!(!wt.path.join(format!("feito-{other}.txt")).exists());
        }
        assert!(worktrees.contains(&*wt.path.to_string_lossy()));
        branches.push(&wt.branch);
        let id = infos[name].id;
        w.output_contains(id, &format!("agente {name} em {}", wt.path.display()))
            .await;
    }
    branches.sort();
    branches.dedup();
    assert_eq!(branches.len(), 3, "branches repetidas: {branches:?}");
    eprintln!("  worktrees:\n{worktrees}");

    // --- Navegador do "qa": abrir, ver quadros, clicar e navegar.
    if chromium_ok {
        eprintln!("== navegador do qa");
        let qa = infos["qa"].clone();
        let endpoint = qa.browser.clone().expect("qa sem navegador");
        w.output_contains(qa.id, &format!("cdp={}", endpoint.http_url()))
            .await;
        let page = qa.worktree.as_ref().unwrap().path.join("index.html");
        w.send(Command::OpenBrowser {
            id: qa.id,
            url: Some(format!("file://{}", page.display())),
        });
        w.until("navegou para index.html", |e| {
            matches!(e, AgentEvent::BrowserNavigated { id, url, .. } if *id == qa.id && url.ends_with("index.html"))
        })
        .await;
        let frame = loop {
            match tokio::time::timeout(WAIT, w.frames.recv()).await {
                Ok(Ok(AgentEvent::BrowserFrame { id, frame })) if id == qa.id => break frame,
                Ok(Ok(_)) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                other => panic!("sem quadro do navegador: {other:?}"),
            }
        };
        assert_eq!(&frame.data[..2], &[0xFF, 0xD8], "quadro não é JPEG");
        eprintln!(
            "  quadro {} B, viewport {}×{}",
            frame.data.len(),
            frame.css_width,
            frame.css_height
        );
        for action in [MouseAction::Move, MouseAction::Down, MouseAction::Up] {
            w.send(Command::BrowserInput {
                id: qa.id,
                input: click(100.0, 100.0, action),
            });
        }
        w.until("clique navegou para clicado.html", |e| {
            matches!(e, AgentEvent::BrowserNavigated { id, url, .. } if *id == qa.id && url.ends_with("clicado.html"))
        })
        .await;
    } else {
        eprintln!("== navegador PULADO: nenhum Chromium encontrado");
    }

    // --- Input: cada um responde e termina.
    eprintln!("== input e término");
    for id in &ids {
        w.send(Command::SendInput {
            id: *id,
            data: "sim\r".into(),
        });
    }
    for id in &ids {
        w.status(*id, "terminou com 0", |s| {
            *s == AgentStatus::Done { exit_code: Some(0) }
        })
        .await;
        w.output_contains(*id, "resposta: sim").await;
    }

    // --- Orquestrador cria um agente pelo gateway MCP.
    eprintln!("== orquestrador via gateway MCP");
    let orq = AgentId::generate();
    let mut orq_cfg = script_agent(sid, "orquestrador", ORQ, false);
    orq_cfg.role = AgentRole::Orchestrator;
    orq_cfg.workspace = Workspace::Directory { path: repo.clone() };
    w.send(Command::Spawn {
        id: orq,
        config: Box::new(orq_cfg),
    });
    w.output_contains(orq, "orquestrador>").await;
    let out = w.output(orq);
    let field = |key: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or_else(|| panic!("sem {key} na saída do orquestrador:\n{out}"))
            .trim()
            .to_owned()
    };
    let (sock, token) = (field("SOCK="), field("TOK="));
    assert_eq!(Path::new(&sock), socket);
    assert!(token.len() >= 32, "token curto: {token}");

    let client = GatewayClient::new(&sock, &token);
    let spawned = client
        .call(
            "spawn_agent",
            json!({"name": "via-mcp", "task": "ola do orquestrador", "kind": "eco"}),
        )
        .await
        .expect("spawn_agent");
    eprintln!("  spawn_agent → {spawned}");
    let child = match w
        .until(
            "filho criado pelo orquestrador",
            |e| matches!(e, AgentEvent::Created { config, .. } if config.parent == Some(orq)),
        )
        .await
    {
        AgentEvent::Created { id, config } => {
            assert_eq!(config.name, "via-mcp");
            assert_eq!(config.session, sid);
            assert!(matches!(config.workspace, Workspace::Worktree { .. }));
            id
        }
        _ => unreachable!(),
    };
    let child_info = w.spawned(child).await;
    w.output_contains(child, "tarefa recebida: ola do orquestrador")
        .await;

    let waited = client
        .call(
            "wait_for",
            json!({"agents": ["via-mcp"], "until": "idle", "timeout_secs": 20}),
        )
        .await
        .expect("wait_for");
    eprintln!("  wait_for → {waited}");
    let read = client
        .call("read_output", json!({"agent": "via-mcp", "lines": 20}))
        .await
        .expect("read_output");
    assert!(
        read.to_string()
            .contains("tarefa recebida: ola do orquestrador"),
        "read_output: {read}"
    );
    let file = child_info
        .worktree
        .as_ref()
        .unwrap()
        .path
        .join("tarefa.txt");
    assert_eq!(
        std::fs::read_to_string(file).unwrap().trim(),
        "ola do orquestrador"
    );

    // A ponte stdio real (`orq mcp`) enxerga o mesmo estado.
    let exe = env!("CARGO_BIN_EXE_orq");
    let env = [
        ("ORQUESTRA_SOCKET", sock.as_str()),
        ("ORQUESTRA_TOKEN", token.as_str()),
    ];
    let listed = bridge_call(exe, &env, "list_agents", json!({}));
    assert!(
        listed.to_string().contains("via-mcp"),
        "list_agents: {listed}"
    );
    eprintln!("  `orquestra mcp` list_agents ok");

    // O hook Stop sai 0 e, se decidir, decide em JSON.
    let mut hook = Process::new(exe)
        .args(["hook", "stop"])
        .envs(env)
        .env("ORQUESTRA_HOOK_WAIT_SECS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(br#"{"hook_event_name":"Stop","stop_hook_active":false}"#)
        .unwrap();
    let hook = hook.wait_with_output().unwrap();
    assert!(hook.status.success());
    let hook_out = String::from_utf8_lossy(&hook.stdout);
    if !hook_out.trim().is_empty() {
        let v: Value = serde_json::from_str(hook_out.trim()).unwrap();
        assert_eq!(v["decision"], "block");
    }
    eprintln!("  `orquestra hook stop` → {}", hook_out.trim());

    client
        .call("kill_agent", json!({"agent": "via-mcp"}))
        .await
        .expect("kill_agent");
    w.status(child, "filho morto pelo orquestrador", |s| {
        *s == AgentStatus::Done { exit_code: None }
    })
    .await;

    // --- Histórico no SQLite e shutdown.
    eprintln!("== histórico e shutdown");
    w.send(Command::LoadHistory { limit: 5 });
    match w
        .until("histórico", |e| matches!(e, AgentEvent::History(_)))
        .await
    {
        AgentEvent::History(h) => {
            assert_eq!(h.sessions.len(), 1);
            assert_eq!(h.agents.len(), 5, "3 workers + orquestrador + filho");
        }
        _ => unreachable!(),
    }
    w.send(Command::Shutdown);
    w.until("shutdown", |e| matches!(e, AgentEvent::Shutdown))
        .await;
    tokio::time::timeout(WAIT, w.hub).await.unwrap().unwrap();
    // Worktrees ficam (cleanup padrão = Keep).
    assert!(infos["front"].worktree.as_ref().unwrap().path.exists());
    eprintln!("== ok em {} ms", started.elapsed().as_millis());
}
