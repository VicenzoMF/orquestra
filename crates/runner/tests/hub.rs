//! O `Runner` de verdade atrás do `Hub` do core, com serviços falsos no resto:
//! garante que o contrato fecha (tipos, `Send`) e que a sequência de eventos é
//! a documentada.

use std::time::Duration;

use bytes::Bytes;
use orquestra_core::{
    AgentConfig, AgentEvent, AgentId, AgentKind, AgentRole, AgentStatus, AppConfig,
    BrowserEndpoint, BrowserInput, BrowserManager, Command, Error, EventBus, History, Hub,
    HubConfig, Navigation, Notification, Notifier, OrchestratorGateway, OrchestratorGrant, Result,
    RunnerConfig, Services, SessionConfig, SessionId, Store, TermSize, Viewport, Workspace,
    WorktreeCleanup, command_channel,
};
use orquestra_runner::Runner;

struct NoBrowser;

impl BrowserManager for NoBrowser {
    async fn launch(&self, _: AgentId) -> Result<BrowserEndpoint> {
        Err(Error::Browser("sem navegador nos testes".to_owned()))
    }
    async fn close(&self, _: AgentId) -> Result<()> {
        Ok(())
    }
    async fn set_streaming(&self, _: AgentId, _: bool) -> Result<()> {
        Ok(())
    }
    async fn navigate(&self, _: AgentId, _: Navigation) -> Result<()> {
        Ok(())
    }
    async fn input(&self, _: AgentId, _: BrowserInput) -> Result<()> {
        Ok(())
    }
    async fn resize(&self, _: AgentId, _: Viewport) -> Result<()> {
        Ok(())
    }
    async fn shutdown(&self) {}
}

struct NoStore;

impl Store for NoStore {
    fn record(&self, _: &AgentEvent) -> Result<()> {
        Ok(())
    }
    fn history(&self, _: usize) -> Result<History> {
        Ok(History::default())
    }
    fn output_log(&self, _: AgentId, _: usize) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn load_config(&self) -> Result<AppConfig> {
        Ok(AppConfig::default())
    }
    fn save_config(&self, _: &AppConfig) -> Result<()> {
        Ok(())
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

struct NoNotifier;

impl Notifier for NoNotifier {
    fn notify(&self, _: &Notification) -> Result<()> {
        Ok(())
    }
}

struct NoGateway;

impl OrchestratorGateway for NoGateway {
    fn grant(&self, _: SessionId, _: AgentId) -> Result<OrchestratorGrant> {
        Err(Error::Other("sem gateway nos testes".to_owned()))
    }
    fn revoke(&self, _: AgentId) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hub_drives_the_real_runner() {
    let bus = EventBus::new();
    let mut rx = bus.subscribe();
    let (tx, cmds) = command_channel();
    let services = Services {
        runner: Runner::new(bus.clone(), RunnerConfig::default()),
        browser: NoBrowser,
        store: NoStore,
        notifier: NoNotifier,
        gateway: NoGateway,
    };
    let hub = tokio::spawn(Hub::new(services, bus.clone(), HubConfig::default()).run(cmds));

    let session = SessionId::generate();
    tx.send(Command::CreateSession {
        id: session,
        config: Box::new(SessionConfig::new("teste", std::env::temp_dir(), "eco")),
    })
    .unwrap();
    let id = AgentId::generate();
    tx.send(Command::Spawn {
        id,
        config: Box::new(AgentConfig {
            name: "eco".to_owned(),
            kind: AgentKind::Custom,
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), "read l; echo \"eco:$l\"".to_owned()],
            env: Default::default(),
            session,
            role: AgentRole::Worker,
            parent: None,
            workspace: Workspace::Directory {
                path: std::env::temp_dir(),
            },
            initial_prompt: None,
            browser: false,
            size: TermSize::default(),
        }),
    })
    .unwrap();
    tx.send(Command::SendInput {
        id,
        data: Bytes::from_static(b"oi\r"),
    })
    .unwrap();

    let mut statuses = Vec::new();
    let mut output = Vec::new();
    let mut spawned = false;
    while !statuses.last().is_some_and(AgentStatus::is_terminal) {
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timeout")
            .unwrap();
        match event {
            AgentEvent::StatusChanged { id: who, status } if who == id => statuses.push(status),
            AgentEvent::Output { id: who, data, .. } if who == id => {
                output.extend_from_slice(&data)
            }
            AgentEvent::Spawned { info } if info.id == id => {
                assert!(info.pid.is_some());
                spawned = true;
            }
            AgentEvent::Error { message, .. } => panic!("erro: {message}"),
            _ => {}
        }
    }
    assert!(spawned);
    assert_eq!(
        statuses,
        [
            AgentStatus::Starting,
            AgentStatus::Running,
            AgentStatus::Done { exit_code: Some(0) }
        ]
    );
    assert!(String::from_utf8_lossy(&output).contains("eco:oi"));

    tx.send(Command::Remove {
        id,
        cleanup: WorktreeCleanup::Keep,
    })
    .unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timeout")
            .unwrap();
        if matches!(event, AgentEvent::Removed { id: who } if who == id) {
            break;
        }
    }
    tx.send(Command::Shutdown).unwrap();
    tokio::time::timeout(Duration::from_secs(10), hub)
        .await
        .expect("hub não parou")
        .unwrap();
}
