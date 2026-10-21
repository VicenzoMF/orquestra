//! SQLite (sessões, agentes, logs), config TOML e notificações de desktop.
//! Implementa `orquestra_core::Store` e `orquestra_core::Notifier`.
//! Ver `docs/ARCHITECTURE.md`.

mod config;

pub use config::{load_config_file, save_config_file};
