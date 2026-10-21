//! Agentes em PTY: spawn, I/O, detecção de status e worktree por agente.
//! Implementa `orquestra_core::AgentRunner`. Ver `docs/ARCHITECTURE.md`.
//!
//! - [`Runner`]: o serviço (PTY via `portable-pty`, worktrees via `git`).
//! - [`build_command`]: como cada tipo de agente é chamado (MCP, hooks,
//!   prompt inicial, ambiente). Puro.
//! - [`StatusDetector`]: status a partir da saída, em camadas. Puro.

mod command;
mod process;
mod replay;
mod runner;
mod status;
mod worktree;

pub use command::{CommandSpec, build_command};
pub use runner::Runner;
pub use status::{DetectorConfig, ProcessExit, StatusDetector};
