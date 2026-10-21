//! Um Chromium por agente via CDP: lançamento, screencast e repasse de input.
//! Implementa `orquestra_core::BrowserManager`. Ver `docs/ARCHITECTURE.md`.

mod cdp;
mod discovery;
mod dispatch;
mod process;

pub use crate::discovery::{ENV_EXECUTABLE, find_executable};
