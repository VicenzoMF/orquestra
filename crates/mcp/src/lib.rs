//! MCP `orquestra`: dá a orquestradores as ferramentas para criar e acompanhar
//! agentes da própria sessão. Implementa `orquestra_core::OrchestratorGateway`.
//! Ver `docs/ARCHITECTURE.md`.

mod bridge;
mod client;
mod gateway;
mod hook;
mod methods;
pub mod protocol;
mod screen;
mod state;

#[cfg(test)]
mod gateway_tests;

pub use bridge::{OrquestraBridge, run_stdio_bridge};
pub use client::GatewayClient;
pub use gateway::McpGateway;
pub use hook::{DEFAULT_HOOK_WAIT_SECS, HOOK_WAIT_ENV, StopDecision, decide_stop, run_stop_hook};
