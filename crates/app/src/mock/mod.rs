//! Serviços falsos para rodar a UI sobre o `Hub` real sem PTY, Chromium,
//! SQLite nem MCP (modo `--mock`).

mod browser;
mod director;
mod font;
mod runner;
mod script;

use std::collections::BTreeMap;

pub use browser::MockBrowser;
pub use director::{run_director, seed};
pub use runner::MockRunner;

use orquestra_core::{
    AgentEvent, AgentId, AppConfig, History, McpServerSpec, Notification, Notifier,
    OrchestratorGateway, OrchestratorGrant, Result, SessionId, Store, conventions,
};
use tracing::info;

/// Store que não guarda nada: config padrão e histórico vazio.
pub struct NullStore;

impl Store for NullStore {
    fn record(&self, _event: &AgentEvent) -> Result<()> {
        Ok(())
    }
    fn history(&self, _limit: usize) -> Result<History> {
        Ok(History::default())
    }
    fn output_log(&self, _id: AgentId, _max_bytes: usize) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn load_config(&self) -> Result<AppConfig> {
        Ok(AppConfig::default())
    }
    fn save_config(&self, _config: &AppConfig) -> Result<()> {
        Ok(())
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// Notificações só no log.
pub struct LogNotifier;

impl Notifier for LogNotifier {
    fn notify(&self, n: &Notification) -> Result<()> {
        info!(agent = %n.agent.short(), kind = ?n.kind, "{}: {}", n.title, n.body);
        Ok(())
    }
}

/// Gateway que concede uma credencial fictícia.
pub struct MockGateway;

impl OrchestratorGateway for MockGateway {
    fn grant(&self, _session: SessionId, agent: AgentId) -> Result<OrchestratorGrant> {
        Ok(OrchestratorGrant {
            mcp: McpServerSpec {
                name: conventions::MCP_SERVER_NAME.to_owned(),
                command: "orquestra".to_owned(),
                args: vec!["mcp".to_owned()],
                env: BTreeMap::new(),
            },
            env: BTreeMap::from([
                (
                    conventions::env::SOCKET.to_owned(),
                    "/tmp/orquestra-mock.sock".to_owned(),
                ),
                (
                    conventions::env::TOKEN.to_owned(),
                    format!("mock-{}", agent.short()),
                ),
            ]),
            stop_hook: Some("orquestra hook stop".to_owned()),
        })
    }

    fn revoke(&self, _agent: AgentId) {}
}
