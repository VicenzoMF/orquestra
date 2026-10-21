//! MCP `orquestra`: dá a orquestradores as ferramentas para criar e acompanhar
//! agentes da própria sessão. Implementa `orquestra_core::OrchestratorGateway`.
//! Ver `docs/ARCHITECTURE.md`.

mod client;
mod gateway;
mod methods;
pub mod protocol;
mod screen;
mod state;

#[cfg(test)]
mod gateway_tests;

pub use client::GatewayClient;
pub use gateway::McpGateway;
