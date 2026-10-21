//! SQLite (sessões, agentes, logs), config TOML e notificações de desktop.
//! Implementa `orquestra_core::Store` e `orquestra_core::Notifier`.
//! Ver `docs/ARCHITECTURE.md`.
//!
//! - [`SqliteStore`]: `<data_dir>/orquestra.db` + `<config_dir>/config.toml`.
//! - [`DesktopNotifier`]: `notify-rust` (zbus), com rajadas agrupadas.

mod config;
mod error;
mod notifier;
mod output;
mod schema;
mod sqlite;

pub use config::{load_config_file, save_config_file};
pub use notifier::DesktopNotifier;
pub use sqlite::{SqliteStore, StoreOptions};
