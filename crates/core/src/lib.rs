//! Contratos do orquestra, sem dependência de UI.
//!
//! - **Comandos** (`Command`): UI e MCP → hub, por um `mpsc` sem limite.
//! - **Eventos** (`AgentEvent`): serviços → todos, pelo `EventBus` (broadcast).
//! - **Serviços** (`AgentRunner`, `BrowserManager`, `Store`, `Notifier`,
//!   `OrchestratorGateway`): implementados em `runner`, `browser`, `store` e `mcp`.
//! - **Hub**: liga comandos aos serviços, mantém sessões e a fila de spawns.
//!   A UI nunca chama um serviço direto.
//!
//! Visão completa em `docs/ARCHITECTURE.md`.

mod agent;
mod browser;
mod bus;
mod command;
mod config;
pub mod conventions;
mod error;
mod event;
pub mod hub;
mod ids;
mod mcp;
mod notify;
mod services;
mod session;
mod time;

pub use agent::{
    AgentConfig, AgentInfo, AgentKind, AgentRole, AgentStatus, QueueReason, SpawnRequest, TermSize,
    Workspace, WorktreeCleanup, WorktreeInfo,
};
pub use browser::{
    BrowserEndpoint, BrowserFrame, BrowserInput, BrowserStatus, ImageFormat, KeyAction, KeyInput,
    Modifiers, MouseAction, MouseButton, MouseInput, Navigation, Viewport, WheelInput,
};
pub use bus::{
    CommandReceiver, CommandSender, EVENT_CAPACITY, EventBus, EventReceiver, FRAME_CAPACITY,
    command_channel,
};
pub use command::Command;
pub use config::{
    AgentPreset, AppConfig, BrowserConfig, NotificationConfig, OrchestrationConfig, RunnerConfig,
    Theme, UiConfig,
};
pub use error::{Error, Result};
pub use event::AgentEvent;
pub use hub::{Hub, HubConfig, Services};
pub use ids::{AgentId, SessionId};
pub use mcp::{McpServerSpec, McpServerTemplate, OrchestratorGrant};
pub use notify::notification_for;
pub use services::{
    AgentRecord, AgentRunner, BrowserManager, History, Notification, NotificationKind, Notifier,
    OrchestratorGateway, Store,
};
pub use session::{SessionConfig, SessionRecord};
pub use time::Timestamp;

/// Reexport para os crates usarem a mesma versão de `bytes` do contrato.
pub use bytes;
