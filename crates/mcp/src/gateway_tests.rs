//! Gateway contra um hub falso: consome os `Command`s e publica no bus o que
//! o hub publicaria.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use orquestra_core::{
    AgentConfig, AgentEvent, AgentId, AgentKind, AgentRole, AgentStatus, AppConfig, Command,
    CommandReceiver, EventBus, OrchestratorGateway, QueueReason, SessionConfig, SessionId,
    Workspace, command_channel, conventions,
};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::client::GatewayClient;
use crate::gateway::McpGateway;
use crate::protocol::{NewsReport, method};
use crate::state::tests::config;

type Log = Arc<Mutex<Vec<Command>>>;

/// Status que o hub falso publica logo depois do `Created`.
type Initial = Arc<Mutex<AgentStatus>>;

fn fake_hub(bus: EventBus, mut rx: CommandReceiver, initial: Initial) -> Log {
    let log: Log = Arc::default();
    let seen = log.clone();
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            match &cmd {
                Command::Spawn { id, config } => {
                    bus.publish(AgentEvent::Created {
                        id: *id,
                        config: Arc::new((**config).clone()),
                    });
                    let status = initial.lock().unwrap().clone();
                    bus.publish(AgentEvent::StatusChanged { id: *id, status });
                }
                Command::Kill { id } => bus.publish(AgentEvent::StatusChanged {
                    id: *id,
                    status: AgentStatus::Done { exit_code: None },
                }),
                _ => {}
            }
            seen.lock().unwrap().push(cmd);
        }
    });
    log
}

struct Harness {
    gw: McpGateway,
    bus: EventBus,
    log: Log,
    initial: Initial,
    session: SessionId,
    orq: AgentId,
    token: String,
    socket: PathBuf,
    _dir: TempDir,
}

impl Harness {
    async fn new() -> Self {
        Self::with_session(SessionConfig::new("loja", "/src/loja", "login com e-mail")).await
    }

    async fn with_session(session_config: SessionConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("gw.sock");
        let bus = EventBus::new();
        let (tx, rx) = command_channel();
        let gw = McpGateway::start_at(tx, bus.clone(), AppConfig::default(), &socket).unwrap();
        let initial: Initial = Arc::new(Mutex::new(AgentStatus::Starting));
        let log = fake_hub(bus.clone(), rx, initial.clone());

        let session = SessionId::generate();
        bus.publish(AgentEvent::SessionCreated {
            id: session,
            config: Arc::new(session_config),
        });
        let orq = AgentId::generate();
        bus.publish(AgentEvent::Created {
            id: orq,
            config: Arc::new(config(
                session,
                "orquestrador",
                AgentRole::Orchestrator,
                None,
            )),
        });
        bus.publish(AgentEvent::StatusChanged {
            id: orq,
            status: AgentStatus::Running,
        });
        let grant = gw.grant(session, orq).unwrap();
        let token = grant.env[conventions::env::TOKEN].clone();
        let h = Self {
            gw,
            bus,
            log,
            initial,
            session,
            orq,
            token,
            socket,
            _dir: dir,
        };
        h.eventually(|st| st.agents.contains_key(&orq) && st.sessions.contains_key(&session))
            .await;
        h
    }

    async fn call_as(&self, token: &str, method: &str, params: Value) -> Result<Value, String> {
        let line = json!({"token": token, "method": method, "params": params}).to_string();
        self.gw.shared().handle_line(&line).await.into_result()
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.call_as(&self.token, method, params).await
    }

    async fn eventually(&self, cond: impl Fn(&crate::state::State) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !cond(&self.gw.shared().lock()) {
            assert!(Instant::now() < deadline, "condição não ficou verdadeira");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn status(&self, id: AgentId, status: AgentStatus) {
        self.bus.publish(AgentEvent::StatusChanged { id, status });
    }

    async fn status_settled(&self, id: AgentId, status: AgentStatus) {
        self.status(id, status.clone());
        self.eventually(|st| st.agents.get(&id).and_then(|a| a.status.as_ref()) == Some(&status))
            .await;
    }

    /// Cria um filho pelo `spawn_agent` e devolve o id completo.
    async fn spawn(&self, name: &str) -> AgentId {
        let reply = self
            .call(method::SPAWN_AGENT, json!({"name": name, "task": "tarefa"}))
            .await
            .unwrap();
        let short = reply["id"].as_str().unwrap().to_owned();
        let st = self.gw.shared().lock();
        st.resolve(self.session, &short).unwrap()
    }

    /// Espera o hub falso registrar um comando que satisfaça `pred`.
    async fn command_seen(&self, pred: impl Fn(&Command) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.commands().iter().any(&pred) {
            assert!(Instant::now() < deadline, "comando não chegou ao hub");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn commands(&self) -> Vec<Command> {
        self.log.lock().unwrap().clone()
    }

    fn spawned_configs(&self) -> Vec<AgentConfig> {
        self.commands()
            .into_iter()
            .filter_map(|c| match c {
                Command::Spawn { config, .. } => Some(*config),
                _ => None,
            })
            .collect()
    }

    fn inputs(&self, id: AgentId) -> Vec<String> {
        self.commands()
            .into_iter()
            .filter_map(|c| match c {
                Command::SendInput { id: i, data } if i == id => {
                    Some(String::from_utf8_lossy(&data).into_owned())
                }
                _ => None,
            })
            .collect()
    }

    /// Outra sessão com orquestrador e um worker "estranho".
    async fn other_session(&self) -> (SessionId, AgentId, AgentId) {
        let session = SessionId::generate();
        self.bus.publish(AgentEvent::SessionCreated {
            id: session,
            config: Arc::new(SessionConfig::new("blog", "/src/blog", "astro")),
        });
        let orq = AgentId::generate();
        self.bus.publish(AgentEvent::Created {
            id: orq,
            config: Arc::new(config(session, "orq-blog", AgentRole::Orchestrator, None)),
        });
        let worker = AgentId::generate();
        self.bus.publish(AgentEvent::Created {
            id: worker,
            config: Arc::new(config(session, "estranho", AgentRole::Worker, Some(orq))),
        });
        self.status_settled(worker, AgentStatus::Running).await;
        (session, orq, worker)
    }
}

#[tokio::test]
async fn grant_gives_spec_env_and_hook_and_revoke_invalidates() {
    let h = Harness::new().await;
    let grant = h.gw.grant(h.session, h.orq).unwrap();
    assert_eq!(grant.mcp.name, conventions::MCP_SERVER_NAME);
    assert_eq!(grant.mcp.args, ["mcp"]);
    assert_eq!(grant.mcp.env, grant.env);
    let exe = std::env::current_exe().unwrap();
    assert_eq!(grant.mcp.command, exe.to_string_lossy());
    assert_eq!(
        grant.env[conventions::env::SOCKET],
        h.socket.to_string_lossy()
    );
    assert!(grant.stop_hook.as_deref().unwrap().ends_with(" hook stop"));

    // Um novo grant troca o token: o anterior deixa de valer.
    let new_token = grant.env[conventions::env::TOKEN].clone();
    assert_ne!(new_token, h.token);
    let old = h.call(method::LIST_AGENTS, json!({})).await;
    assert!(old.unwrap_err().contains("credencial"));
    assert!(
        h.call_as(&new_token, method::LIST_AGENTS, json!({}))
            .await
            .is_ok()
    );

    assert!(
        h.call_as("x", method::LIST_AGENTS, json!({}))
            .await
            .is_err()
    );
    h.gw.revoke(h.orq);
    let revoked = h.call_as(&new_token, method::LIST_AGENTS, json!({})).await;
    assert_eq!(revoked.unwrap_err(), "credencial inválida ou revogada");
}

#[tokio::test]
async fn removing_the_orchestrator_revokes_its_token() {
    let h = Harness::new().await;
    assert!(h.call(method::LIST_AGENTS, json!({})).await.is_ok());
    h.bus.publish(AgentEvent::Removed { id: h.orq });
    h.eventually(|st| !st.agents.contains_key(&h.orq)).await;
    assert!(h.call(method::LIST_AGENTS, json!({})).await.is_err());
}

#[tokio::test]
async fn bad_requests_are_readable_errors() {
    let h = Harness::new().await;
    let err = h.gw.shared().handle_line("não é json").await.into_result();
    assert!(err.unwrap_err().starts_with("requisição inválida"));
    let err = h.call("format_disk", json!({})).await.unwrap_err();
    assert_eq!(err, "método desconhecido: format_disk");
    let err = h
        .call(method::SPAWN_AGENT, json!({"name": "x"}))
        .await
        .unwrap_err();
    assert!(err.starts_with("parâmetros inválidos"), "{err}");
}

#[tokio::test]
async fn list_agents_shows_the_session_only() {
    let h = Harness::new().await;
    let api = h.spawn("api").await;
    h.bus.publish(AgentEvent::Spawned {
        info: Arc::new(orquestra_core::AgentInfo {
            id: api,
            config: h.spawned_configs()[0].clone(),
            workdir: "/tmp/w".into(),
            worktree: None,
            pid: Some(1),
            browser: Some(orquestra_core::BrowserEndpoint {
                port: 9333,
                ws_url: "ws://x".into(),
            }),
            started_at: orquestra_core::Timestamp::now(),
        }),
    });
    h.status_settled(api, AgentStatus::Running).await;
    h.eventually(|st| st.agents[&api].browser_port.is_some())
        .await;
    h.other_session().await;

    let reply = h.call(method::LIST_AGENTS, json!({})).await.unwrap();
    assert_eq!(reply["session"]["goal"], "login com e-mail");
    assert_eq!(reply["you"], "orquestrador");
    let agents = reply["agents"].as_array().unwrap();
    let names: Vec<_> = agents.iter().map(|a| a["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["orquestrador", "api"]);
    let api_row = &agents[1];
    assert_eq!(api_row["id"], api.short());
    assert_eq!(api_row["kind"], "claude");
    assert_eq!(api_row["role"], "worker");
    assert_eq!(api_row["status"], "running");
    assert_eq!(api_row["parent"], "orquestrador");
    assert_eq!(api_row["browser_port"], 9333);
    assert_eq!(api_row["task"], "tarefa");
    assert_eq!(agents[0]["role"], "orchestrator");
}

#[tokio::test]
async fn other_sessions_are_invisible() {
    let h = Harness::new().await;
    let (_, _, stranger) = h.other_session().await;
    for reference in [
        "estranho".to_owned(),
        stranger.short(),
        stranger.to_string(),
    ] {
        for (m, params) in [
            (method::KILL_AGENT, json!({"agent": reference})),
            (method::READ_OUTPUT, json!({"agent": reference})),
            (
                method::SEND_INPUT,
                json!({"agent": reference, "text": "oi"}),
            ),
            (
                method::WAIT_FOR,
                json!({"agents": [reference], "until": "change"}),
            ),
        ] {
            let err = h.call(m, params).await.unwrap_err();
            assert_eq!(err, format!("agente desconhecido: {reference}"), "{m}");
        }
    }
    assert!(
        h.commands()
            .iter()
            .all(|c| !matches!(c, Command::Kill { .. } | Command::SendInput { .. }))
    );
}

#[tokio::test]
async fn spawn_agent_builds_the_worker_config() {
    let h = Harness::new().await;
    let reply = h
        .call(
            method::SPAWN_AGENT,
            json!({"name": "api login", "task": "  Implemente o login  ", "kind": "codex", "browser": true}),
        )
        .await
        .unwrap();
    assert_eq!(reply["agent"], "api-login");
    assert_eq!(reply["status"], "starting");

    let configs = h.spawned_configs();
    let cfg = &configs[0];
    assert_eq!(cfg.name, "api-login");
    assert_eq!(cfg.kind, AgentKind::Codex);
    assert_eq!(cfg.program, "codex");
    assert_eq!(cfg.session, h.session);
    assert_eq!(cfg.role, AgentRole::Worker);
    assert_eq!(cfg.parent, Some(h.orq));
    assert_eq!(
        cfg.workspace,
        Workspace::Worktree {
            repo: "/src/loja".into(),
            base: None,
            branch: None
        }
    );
    assert_eq!(cfg.initial_prompt.as_deref(), Some("Implemente o login"));
    assert!(cfg.browser);

    // Nome repetido ganha sufixo; sem `kind`, o primeiro tipo permitido.
    let again = h
        .call(
            method::SPAWN_AGENT,
            json!({"name": "API-login", "task": "outra"}),
        )
        .await
        .unwrap();
    assert_eq!(again["agent"], "API-login-2");
    let configs = h.spawned_configs();
    assert_eq!(configs[1].kind, AgentKind::ClaudeCode);
    assert!(!configs[1].browser, "o preset padrão não pede navegador");
}

#[tokio::test]
async fn spawn_agent_respects_session_rules() {
    let h = Harness::with_session(SessionConfig {
        allowed_kinds: vec![AgentKind::ClaudeCode],
        workers_may_browse: false,
        ..SessionConfig::new("loja", "/src/loja", "login")
    })
    .await;
    let err = h
        .call(
            method::SPAWN_AGENT,
            json!({"name": "a", "task": "t", "kind": "codex"}),
        )
        .await
        .unwrap_err();
    assert!(err.contains("não é permitido"), "{err}");
    let err = h
        .call(
            method::SPAWN_AGENT,
            json!({"name": "a", "task": "t", "browser": true}),
        )
        .await
        .unwrap_err();
    assert!(err.contains("navegador"), "{err}");
    let err = h
        .call(method::SPAWN_AGENT, json!({"name": "a", "task": "   "}))
        .await
        .unwrap_err();
    assert!(err.contains("task vazia"), "{err}");
    assert!(h.spawned_configs().is_empty());
}

#[tokio::test]
async fn spawn_agent_reports_queue_and_approval() {
    let h = Harness::new().await;
    *h.initial.lock().unwrap() = AgentStatus::Queued {
        reason: QueueReason::Capacity,
    };
    let reply = h
        .call(method::SPAWN_AGENT, json!({"name": "a", "task": "t"}))
        .await
        .unwrap();
    assert_eq!(reply["status"], "queued");
    assert!(reply["note"].as_str().unwrap().contains("limite"));

    *h.initial.lock().unwrap() = AgentStatus::Queued {
        reason: QueueReason::Approval,
    };
    let reply = h
        .call(method::SPAWN_AGENT, json!({"name": "b", "task": "t"}))
        .await
        .unwrap();
    assert_eq!(reply["status"], "awaiting_approval");
}

#[tokio::test]
async fn read_output_returns_clean_text() {
    let h = Harness::new().await;
    let a = h.spawn("a").await;
    let empty = h
        .call(method::READ_OUTPUT, json!({"agent": "a"}))
        .await
        .unwrap();
    assert_eq!(empty["text"], "");
    assert!(empty["note"].is_string());

    let chunks: [&[u8]; 3] = [
        b"\x1b[?25l\x1b[1;34m\xe2\x97\x8f\x1b[0m Lendo arquivos\r\n",
        b"\x1b[2K\r\x1b[32mTestes: 12 ok\x1b[0m\r\n\r\n\r\n",
        b"\x1b]0;claude\x07> pronto",
    ];
    for (seq, data) in chunks.iter().enumerate() {
        h.bus.publish(AgentEvent::Output {
            id: a,
            seq: seq as u64,
            data: Bytes::from_static(data),
        });
    }
    h.eventually(|st| st.screen_lines(a, 5).is_some_and(|l| l.len() == 3))
        .await;
    let reply = h
        .call(method::READ_OUTPUT, json!({"agent": "a"}))
        .await
        .unwrap();
    assert_eq!(reply["text"], "● Lendo arquivos\nTestes: 12 ok\n> pronto");
    assert_eq!(reply["lines"], 3);
    let tail = h
        .call(method::READ_OUTPUT, json!({"agent": a.short(), "lines": 1}))
        .await
        .unwrap();
    assert_eq!(tail["text"], "> pronto");
    assert!(
        !h.commands()
            .iter()
            .any(|c| matches!(c, Command::RequestReplay { .. })),
        "seq contínuo não pede replay"
    );
}

#[tokio::test]
async fn send_input_types_then_presses_enter() {
    let h = Harness::new().await;
    let a = h.spawn("a").await;
    h.status_settled(a, AgentStatus::WaitingForInput).await;

    let reply = h
        .call(method::SEND_INPUT, json!({"agent": "a", "text": "sim"}))
        .await
        .unwrap();
    assert_eq!(reply["submitted"], true);
    h.call(
        method::SEND_INPUT,
        json!({"agent": "a", "text": "", "submit": true}),
    )
    .await
    .unwrap();
    h.call(
        method::SEND_INPUT,
        json!({"agent": "a", "text": "só", "submit": false}),
    )
    .await
    .unwrap();

    // Programa com bracketed paste: texto de várias linhas vai colado.
    h.bus.publish(AgentEvent::Output {
        id: a,
        seq: 0,
        data: Bytes::from_static(b"\x1b[?2004h> "),
    });
    h.eventually(|st| st.bracketed_paste(a)).await;
    h.call(method::SEND_INPUT, json!({"agent": "a", "text": "l1\nl2"}))
        .await
        .unwrap();
    h.command_seen(|_| h.inputs(a).len() == 6).await;
    let report: NewsReport =
        serde_json::from_value(h.call(method::TAKE_NEWS, json!({})).await.unwrap()).unwrap();
    assert!(report.news.is_empty(), "responder consome a novidade");
    let inputs = h.inputs(a);
    assert_eq!(
        inputs,
        ["sim", "\r", "\r", "só", "\x1b[200~l1\nl2\x1b[201~", "\r"]
    );

    let err = h
        .call(
            method::SEND_INPUT,
            json!({"agent": "orquestrador", "text": "x"}),
        )
        .await
        .unwrap_err();
    assert!(err.contains("você mesmo"));
    h.status_settled(a, AgentStatus::Done { exit_code: Some(0) })
        .await;
    let err = h
        .call(method::SEND_INPUT, json!({"agent": "a", "text": "x"}))
        .await
        .unwrap_err();
    assert!(err.contains("já terminou"));

    *h.initial.lock().unwrap() = AgentStatus::Queued {
        reason: QueueReason::Capacity,
    };
    h.spawn("b").await;
    let err = h
        .call(method::SEND_INPUT, json!({"agent": "b", "text": "x"}))
        .await
        .unwrap_err();
    assert!(err.contains("fila"));
}

#[tokio::test]
async fn kill_agent_sends_kill_but_not_to_self() {
    let h = Harness::new().await;
    let a = h.spawn("a").await;
    let reply = h
        .call(method::KILL_AGENT, json!({"agent": "a"}))
        .await
        .unwrap();
    assert_eq!(reply["killed"], true);
    h.command_seen(|c| matches!(c, Command::Kill { id } if *id == a))
        .await;
    h.eventually(|st| {
        st.agents[&a]
            .status
            .as_ref()
            .is_some_and(AgentStatus::is_terminal)
    })
    .await;
    let again = h
        .call(method::KILL_AGENT, json!({"agent": "a"}))
        .await
        .unwrap();
    assert_eq!(again["killed"], false);
    let report: NewsReport =
        serde_json::from_value(h.call(method::TAKE_NEWS, json!({})).await.unwrap()).unwrap();
    assert!(
        report.news.is_empty(),
        "o fim pedido pelo orquestrador não é novidade"
    );
    let err = h
        .call(method::KILL_AGENT, json!({"agent": "orquestrador"}))
        .await
        .unwrap_err();
    assert!(err.contains("próprio orquestrador"));
}

#[tokio::test]
async fn wait_for_resolves_on_the_event() {
    let h = Arc::new(Harness::new().await);
    let (a, b) = (h.spawn("a").await, h.spawn("b").await);
    h.status_settled(a, AgentStatus::Running).await;
    h.status_settled(b, AgentStatus::Running).await;

    let waiter = {
        let h = h.clone();
        tokio::spawn(async move { h.call(method::WAIT_FOR, json!({"until": "idle"})).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiter.is_finished());
    h.status(b, AgentStatus::WaitingForInput);
    let reply = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply["timed_out"], false);
    assert_eq!(reply["triggered"], json!(["b"]));
    let statuses: Vec<_> = reply["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["status"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(statuses, ["running", "waiting_for_input"]);

    // Sem `agents`, b (já parado) não dispara de novo; com `agents`, dispara.
    let quick = h
        .call(
            method::WAIT_FOR,
            json!({"until": "idle", "timeout_secs": 0}),
        )
        .await
        .unwrap();
    assert_eq!(quick["timed_out"], true);
    let explicit = h
        .call(
            method::WAIT_FOR,
            json!({"agents": ["b"], "until": "idle", "timeout_secs": 0}),
        )
        .await
        .unwrap();
    assert_eq!(explicit["triggered"], json!(["b"]));
}

#[tokio::test]
async fn wait_for_times_out_and_supports_all_and_change() {
    let h = Arc::new(Harness::new().await);
    let (a, b) = (h.spawn("a").await, h.spawn("b").await);
    h.status_settled(a, AgentStatus::Running).await;

    let started = Instant::now();
    let reply = h
        .call(
            method::WAIT_FOR,
            json!({"until": "done", "timeout_secs": 1}),
        )
        .await
        .unwrap();
    assert_eq!(reply["timed_out"], true);
    assert_eq!(reply["triggered"], json!([]));
    assert!(started.elapsed() >= Duration::from_millis(900));

    let all = {
        let h = h.clone();
        tokio::spawn(async move {
            h.call(method::WAIT_FOR, json!({"until": "done", "all": true}))
                .await
        })
    };
    h.status_settled(a, AgentStatus::Done { exit_code: Some(0) })
        .await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!all.is_finished(), "falta o b");
    h.status(
        b,
        AgentStatus::Failed {
            reason: "boom".into(),
        },
    );
    let reply = all.await.unwrap().unwrap();
    assert_eq!(reply["triggered"], json!(["a", "b"]));

    // Todos terminados: nada a esperar.
    let reply = h
        .call(method::WAIT_FOR, json!({"until": "change"}))
        .await
        .unwrap();
    assert!(reply["note"].as_str().unwrap().contains("nada a esperar"));
}

#[tokio::test]
async fn wait_for_change_sees_any_transition() {
    let h = Arc::new(Harness::new().await);
    let a = h.spawn("a").await;
    let waiter = {
        let h = h.clone();
        tokio::spawn(async move {
            h.call(
                method::WAIT_FOR,
                json!({"agents": ["a"], "until": "change"}),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    h.status(a, AgentStatus::Running);
    let reply = waiter.await.unwrap().unwrap();
    assert_eq!(reply["triggered"], json!(["a"]));
}

#[tokio::test]
async fn wait_for_consumes_the_news_it_reports() {
    let h = Harness::new().await;
    let a = h.spawn("a").await;
    h.status_settled(a, AgentStatus::Running).await;
    h.status_settled(a, AgentStatus::WaitingForInput).await;
    h.call(method::WAIT_FOR, json!({"agents": ["a"], "until": "idle"}))
        .await
        .unwrap();
    let report: NewsReport =
        serde_json::from_value(h.call(method::TAKE_NEWS, json!({})).await.unwrap()).unwrap();
    assert!(report.news.is_empty());
}

#[tokio::test]
async fn take_news_drains_and_can_wait() {
    let h = Arc::new(Harness::new().await);
    let (a, b) = (h.spawn("a").await, h.spawn("b").await);
    h.status_settled(a, AgentStatus::Running).await;
    h.status_settled(b, AgentStatus::Done { exit_code: Some(0) })
        .await;

    let report: NewsReport = serde_json::from_value(
        h.call(method::TAKE_NEWS, json!({"wait_secs": 0}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(report.news.len(), 1);
    assert_eq!(report.news[0].agent, "b");
    assert_eq!(report.news[0].event, "done");
    assert_eq!(report.children.len(), 2);
    assert_eq!(
        report
            .working()
            .map(|c| c.agent.as_str())
            .collect::<Vec<_>>(),
        ["a"]
    );

    // Vazio + filho trabalhando: espera a próxima.
    let waiter = {
        let h = h.clone();
        tokio::spawn(async move { h.call(method::TAKE_NEWS, json!({"wait_secs": 5})).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiter.is_finished());
    h.status(a, AgentStatus::WaitingForInput);
    let report: NewsReport = serde_json::from_value(
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(report.news[0].event, "waiting_for_input");

    // Ninguém trabalhando: não espera.
    let started = Instant::now();
    h.call(method::TAKE_NEWS, json!({"wait_secs": 5}))
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn socket_round_trip_with_private_permissions() {
    let h = Harness::new().await;
    let mode = std::fs::metadata(&h.socket).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    let client = GatewayClient::new(&h.socket, &h.token);
    let reply = client.call(method::LIST_AGENTS, json!({})).await.unwrap();
    assert_eq!(reply["you"], "orquestrador");
    let reply = client
        .call(
            method::SPAWN_AGENT,
            json!({"name": "via-socket", "task": "t"}),
        )
        .await
        .unwrap();
    assert_eq!(reply["agent"], "via-socket");

    let intruder = GatewayClient::new(&h.socket, "errado");
    let err = intruder
        .call(method::LIST_AGENTS, json!({}))
        .await
        .unwrap_err();
    assert_eq!(err, "credencial inválida ou revogada");

    // Cliente síncrono (o do hook), fora do runtime.
    let sync = client.clone();
    let reply = tokio::task::spawn_blocking(move || {
        sync.call_blocking(method::TAKE_NEWS, json!({}), Duration::from_secs(2))
    })
    .await
    .unwrap()
    .unwrap();
    assert!(reply["children"].is_array());

    let socket = h.socket.clone();
    let Harness { gw, _dir: dir, .. } = h;
    let keep = gw.clone();
    drop(gw);
    assert!(socket.exists(), "um clone ainda vivo mantém o gateway");
    drop(keep);
    assert!(!socket.exists(), "o socket some com o último clone");
    drop(dir);
    let gone = client
        .call(method::LIST_AGENTS, json!({}))
        .await
        .unwrap_err();
    assert!(gone.contains("inacessível"), "{gone}");
}

#[tokio::test]
async fn abandoned_take_news_does_not_eat_the_news() {
    use tokio::io::AsyncWriteExt;

    let h = Harness::new().await;
    let a = h.spawn("a").await;
    h.status_settled(a, AgentStatus::Running).await;

    let mut raw = tokio::net::UnixStream::connect(&h.socket).await.unwrap();
    let line = json!({"token": h.token, "method": "take_news", "params": {"wait_secs": 30}});
    raw.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(raw); // o hook morreu no meio da espera
    tokio::time::sleep(Duration::from_millis(50)).await;

    h.status_settled(a, AgentStatus::Done { exit_code: Some(0) })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let report: NewsReport =
        serde_json::from_value(h.call(method::TAKE_NEWS, json!({})).await.unwrap()).unwrap();
    assert_eq!(report.news.len(), 1, "a novidade continua lá");
}

#[tokio::test]
async fn hub_shutdown_closes_the_socket() {
    let h = Harness::new().await;
    h.bus.publish(AgentEvent::Shutdown);
    h.eventually(|st| st.shutdown).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!h.socket.exists());
    let err = h.call(method::LIST_AGENTS, json!({})).await.unwrap_err();
    assert_eq!(err, crate::gateway::SHUTTING_DOWN);
}

#[tokio::test]
async fn gap_in_output_requests_replay() {
    let h = Harness::new().await;
    let a = h.spawn("a").await;
    h.bus.publish(AgentEvent::Output {
        id: a,
        seq: 4,
        data: Bytes::from_static(b"meio"),
    });
    h.command_seen(|c| matches!(c, Command::RequestReplay { id } if *id == a))
        .await;
}

#[tokio::test]
async fn mcp_client_through_bridge_and_socket() {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;

    let h = Harness::new().await;
    let bridge = crate::bridge::OrquestraBridge::new(GatewayClient::new(&h.socket, &h.token));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        if let Ok(server) = bridge.serve(server_io).await {
            let _ = server.waiting().await;
        }
    });
    let client = ().serve(client_io).await.unwrap();
    let args = |v: Value| v.as_object().unwrap().clone();

    let spawned = client
        .call_tool(
            CallToolRequestParams::new("spawn_agent")
                .with_arguments(args(json!({"name": "web", "task": "faça o front"}))),
        )
        .await
        .unwrap();
    assert_ne!(spawned.is_error, Some(true));
    let text = &spawned.content[0].as_text().unwrap().text;
    let reply: Value = serde_json::from_str(text).unwrap();
    assert_eq!(reply["agent"], "web");
    assert_eq!(
        h.spawned_configs()[0].initial_prompt.as_deref(),
        Some("faça o front")
    );

    let web = h.gw.shared().lock().resolve(h.session, "web").unwrap();
    h.bus.publish(AgentEvent::Output {
        id: web,
        seq: 0,
        data: Bytes::from_static(b"\x1b[33mok\x1b[0m\r\n"),
    });
    h.eventually(|st| st.screen_lines(web, 1).is_some()).await;
    let out = client
        .call_tool(
            CallToolRequestParams::new("read_output").with_arguments(args(json!({"agent": "web"}))),
        )
        .await
        .unwrap();
    assert_eq!(
        out.content[0].as_text().unwrap().text,
        "web [starting], últimas 1 linhas:\nok"
    );

    let err = client
        .call_tool(
            CallToolRequestParams::new("kill_agent")
                .with_arguments(args(json!({"agent": "ninguem"}))),
        )
        .await
        .unwrap();
    assert_eq!(err.is_error, Some(true));
    assert_eq!(
        err.content[0].as_text().unwrap().text,
        "agente desconhecido: ninguem"
    );
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_hook_against_the_gateway() {
    use crate::hook::{StopDecision, decide_stop};

    let h = Harness::new().await;
    let a = h.spawn("a").await;
    h.status_settled(a, AgentStatus::Running).await;
    let client = GatewayClient::new(&h.socket, &h.token);
    let hook = move |input: &'static str, wait: u64| {
        let client = client.clone();
        tokio::task::spawn_blocking(move || {
            decide_stop(input, wait, |secs| {
                let reply = client.call_blocking(
                    method::TAKE_NEWS,
                    json!({"wait_secs": secs}),
                    Duration::from_secs(secs + 5),
                )?;
                serde_json::from_value(reply).map_err(|e| e.to_string())
            })
        })
    };

    let first = hook(r#"{"stop_hook_active":false}"#, 1).await.unwrap();
    assert!(matches!(first, StopDecision::Block(r) if r.contains("1 agente ainda trabalhando: a")));

    // Continuando e sem novidade em 1 s: deixa parar.
    let again = hook(r#"{"stop_hook_active":true}"#, 1).await.unwrap();
    assert_eq!(again, StopDecision::AllowStop);

    // A novidade chega durante a espera do hook.
    let pending = hook(r#"{"stop_hook_active":true}"#, 5);
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.status(a, AgentStatus::Done { exit_code: Some(0) });
    let late = pending.await.unwrap();
    assert!(matches!(late, StopDecision::Block(r) if r.contains("a (") && r.contains("terminou")));

    // Tudo entregue e ninguém trabalhando: deixa parar.
    let done = hook(r#"{"stop_hook_active":false}"#, 1).await.unwrap();
    assert_eq!(done, StopDecision::AllowStop);
}
