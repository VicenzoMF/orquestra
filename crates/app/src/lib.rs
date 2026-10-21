//! UI do orquestra (egui). Só envia `Command` e consome `AgentEvent`; ver
//! `docs/ARCHITECTURE.md`, seções 6 (app) e 7 (layout).

pub mod actions;
pub mod backend;
pub mod browser;
pub mod feed;
pub mod fonts;
pub mod layout;
pub mod mock;
pub mod model;
pub mod term;
pub mod theme;
