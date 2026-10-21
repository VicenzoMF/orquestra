//! Agentes em PTY: spawn, I/O, detecção de status e worktree por agente.
//! Implementa `orquestra_core::AgentRunner`. Ver `docs/ARCHITECTURE.md`.
//!
//! - [`build_command`]: como cada tipo de agente é chamado (MCP, hooks,
//!   prompt inicial, ambiente). Puro.

mod command;

pub use command::{CommandSpec, build_command};
