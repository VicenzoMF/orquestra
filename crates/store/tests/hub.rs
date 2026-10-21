//! A `SqliteStore` de verdade dentro do `Hub` do core, com runner, navegador
//! e gateway falsos: a mesma fiação da Fase 2.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use orquestra_core::bytes::Bytes;
use orquestra_core::{
    AgentConfig, AgentEvent, AgentId, AgentInfo, AgentKind, AgentRole, AgentRunner, AgentStatus,
    AppConfig, BrowserEndpoint, BrowserInput, BrowserManager, Command, Error, EventBus,
    EventReceiver, Hub, HubConfig, McpServerSpec, Navigation, Notification, NotificationKind,
    Notifier, OrchestratorGateway, OrchestratorGrant, Result, Services, SessionConfig, SessionId,
    SpawnRequest, Store, TermSize, Timestamp, Viewport, Workspace, WorktreeCleanup,
    command_channel,
};
use orquestra_store::SqliteStore;

/// Sobe "o processo": Running, duas saídas e espera input.
struct ScriptedRunner {
    bus: EventBus,
}

impl AgentRunner for ScriptedRunner {
    async fn spawn(&self, req: SpawnRequest) -> Result<AgentInfo> {
        let id = req.id;
        let bus = self.bus.clone();
        tokio::spawn(async move {
            bus.publish(AgentEvent::StatusChanged {
                id,
                status: AgentStatus::Running,
            });
            for (seq, data) in [(0, "ola "), (1, "mundo")] {
                bus.publish(AgentEvent::Output {
                    id,
                    seq,
                    data: Bytes::from_static(data.as_bytes()),
                });
            }
            bus.publish(AgentEvent::StatusChanged {
                id,
                status: AgentStatus::WaitingForInput,
            });
        });
        Ok(AgentInfo {
            id,
            config: req.config,
            workdir: "/tmp".into(),
            worktree: None,
            pid: Some(7),
            browser: None,
            started_at: Timestamp::now(),
        })
    }
    async fn send_input(&self, _id: AgentId, _data: Bytes) -> Result<()> {
        Ok(())
    }
    async fn resize(&self, _id: AgentId, _size: TermSize) -> Result<()> {
        Ok(())
    }
    async fn kill(&self, _id: AgentId) -> Result<()> {
        Ok(())
    }
    async fn remove(&self, _id: AgentId, _cleanup: WorktreeCleanup) -> Result<()> {
        Ok(())
    }
    async fn request_replay(&self, _id: AgentId) -> Result<()> {
        Ok(())
    }
    async fn shutdown(&self) {}
}

struct NoBrowser;

impl BrowserManager for NoBrowser {
    async fn launch(&self, _id: AgentId) -> Result<BrowserEndpoint> {
        Err(Error::Browser("sem navegador no teste".to_owned()))
    }
    async fn close(&self, _id: AgentId) -> Result<()> {
        Ok(())
    }
    async fn set_streaming(&self, _id: AgentId, _on: bool) -> Result<()> {
        Ok(())
    }
    async fn navigate(&self, _id: AgentId, _nav: Navigation) -> Result<()> {
        Ok(())
    }
    async fn input(&self, _id: AgentId, _input: BrowserInput) -> Result<()> {
        Ok(())
    }
    async fn resize(&self, _id: AgentId, _viewport: Viewport) -> Result<()> {
        Ok(())
    }
    async fn shutdown(&self) {}
}

struct NoGateway;

impl OrchestratorGateway for NoGateway {
    fn grant(&self, _session: SessionId, _agent: AgentId) -> Result<OrchestratorGrant> {
        Ok(OrchestratorGrant {
            mcp: McpServerSpec {
                name: "orquestra".to_owned(),
                command: "orquestra".to_owned(),
                args: vec!["mcp".to_owned()],
                env: BTreeMap::new(),
            },
            env: BTreeMap::new(),
            stop_hook: None,
        })
    }
    fn revoke(&self, _agent: AgentId) {}
}

#[derive(Clone, Default)]
struct Sent(Arc<Mutex<Vec<Notification>>>);

impl Notifier for Sent {
    fn notify(&self, notification: &Notification) -> Result<()> {
        self.0.lock().unwrap().push(notification.clone());
        Ok(())
    }
}

async fn next(events: &mut EventReceiver) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("evento não chegou")
        .expect("bus fechou")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hub_records_into_sqlite_and_flushes_on_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("orquestra.db");
    let config_path = dir.path().join("config.toml");
    let store = SqliteStore::open(&db, &config_path).unwrap();
    let app_config = store.load_config().unwrap();

    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let sent = Sent::default();
    let services = Services {
        runner: ScriptedRunner { bus: bus.clone() },
        browser: NoBrowser,
        store,
        notifier: sent.clone(),
        gateway: NoGateway,
    };
    let (tx, rx) = command_channel();
    let hub =
        tokio::spawn(Hub::new(services, bus.clone(), HubConfig::from_app(&app_config)).run(rx));

    let session = SessionId::generate();
    let session_config = SessionConfig::new("loja", "/src/loja", "login");
    tx.send(Command::CreateSession {
        id: session,
        config: Box::new(session_config.clone()),
    })
    .unwrap();
    let agent = AgentId::generate();
    let agent_config = AgentConfig {
        name: "api".to_owned(),
        kind: AgentKind::Shell,
        program: "sh".to_owned(),
        args: Vec::new(),
        env: BTreeMap::new(),
        session,
        role: AgentRole::Worker,
        parent: None,
        workspace: Workspace::Directory {
            path: "/tmp".into(),
        },
        initial_prompt: None,
        browser: false,
        size: TermSize::default(),
    };
    tx.send(Command::Spawn {
        id: agent,
        config: Box::new(agent_config.clone()),
    })
    .unwrap();

    // O histórico pelo hub (spawn_blocking) acaba refletindo o que o recorder gravou.
    let mut seen = None;
    for _ in 0..100 {
        tx.send(Command::LoadHistory { limit: 5 }).unwrap();
        let history = loop {
            if let AgentEvent::History(history) = next(&mut events).await {
                break history;
            }
        };
        if history
            .agents
            .iter()
            .any(|a| a.id == agent && a.status == AgentStatus::WaitingForInput && a.info.is_some())
        {
            seen = Some(history);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let history = seen.expect("o histórico não chegou a ter o agente esperando input");
    assert_eq!(history.sessions[0].id, session);
    assert_eq!(history.sessions[0].config, session_config);

    let mut saved = AppConfig::default();
    saved.ui.font_size = 16.0;
    tx.send(Command::SaveConfig(Box::new(saved.clone())))
        .unwrap();
    tx.send(Command::Shutdown).unwrap();
    tokio::time::timeout(Duration::from_secs(10), hub)
        .await
        .expect("hub não parou")
        .unwrap();

    // Outra store sobre o mesmo arquivo vê tudo gravado.
    let store = SqliteStore::open(&db, &config_path).unwrap();
    let history = store.history(5).unwrap();
    assert!(history.sessions[0].closed_at.is_some());
    let record = &history.agents[0];
    assert_eq!(record.config, agent_config);
    assert_eq!(record.info.as_ref().unwrap().pid, Some(7));
    assert_eq!(record.status, AgentStatus::Done { exit_code: None });
    assert_eq!(store.output_log(agent, 1024).unwrap(), b"ola mundo");
    assert_eq!(store.load_config().unwrap(), saved);

    let kinds: Vec<_> = sent.0.lock().unwrap().iter().map(|n| n.kind).collect();
    assert_eq!(kinds, [NotificationKind::NeedsInput]);
}
