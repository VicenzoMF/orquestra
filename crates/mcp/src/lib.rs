//! MCP `orquestra`: dá a orquestradores as ferramentas para criar e acompanhar
//! agentes da própria sessão. Ver `docs/ARCHITECTURE.md` (§5 e §6 "mcp").
//!
//! Três partes, uma por modo do binário:
//!
//! - **Gateway** (`McpGateway`, dentro do app): assina o `EventBus`, mantém
//!   sessões, agentes, a tela de cada worker e a fila de novidades de cada
//!   orquestrador, e atende num socket Unix local (`protocol`). Implementa
//!   `OrchestratorGateway`: o hub pede um `grant` ao subir um orquestrador.
//! - **Ponte stdio** (`run_stdio_bridge`, `orquestra mcp`): servidor MCP
//!   (rmcp) que o CLI do orquestrador sobe; repassa as ferramentas ao socket.
//! - **Hook** (`run_stop_hook`, `orquestra hook stop`): o hook `Stop` do
//!   orquestrador Claude Code; traz as novidades dos filhos ao fim do turno.
//!
//! Montagem no `main` (Fase 2):
//!
//! ```ignore
//! match args {
//!     // runtime current_thread com `enable_all()` basta
//!     ["mcp"] => runtime.block_on(orquestra_mcp::run_stdio_bridge())?,
//!     // síncrono, sem runtime
//!     ["hook", "stop"] => std::process::exit(orquestra_mcp::run_stop_hook()),
//!     _ => {
//!         let _guard = runtime.enter(); // `start` sobe tasks e o socket no runtime
//!         let gateway = McpGateway::start(cmd_tx.clone(), bus.clone(), config.clone())?;
//!         let gateway_handle = gateway.clone(); // p/ `update_config` após SaveConfig
//!         // ... Services { gateway, .. }
//!     }
//! }
//! ```
//!
//! Nos modos `mcp` e `hook` o stdout é do protocolo: log só em stderr.

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
