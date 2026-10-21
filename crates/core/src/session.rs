use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{AgentKind, SessionId, Timestamp};

/// Uma orquestração: um grupo no canvas com um objetivo, um repositório e,
/// normalmente, um orquestrador que cria os agentes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfig {
    /// Nome de exibição ("loja · login + produtos").
    pub name: String,
    /// Repositório onde os worktrees dos agentes são criados.
    pub repo: PathBuf,
    /// Objetivo dado ao orquestrador.
    pub goal: String,
    /// Máximo de workers vivos desta sessão. `None` = só o limite global vale.
    #[serde(default)]
    pub max_workers: Option<u32>,
    /// Tipos que o orquestrador pode criar (o MCP `orquestra` aplica).
    #[serde(default = "all_worker_kinds")]
    pub allowed_kinds: Vec<AgentKind>,
    /// Agentes criados pelo orquestrador podem pedir navegador (o MCP aplica).
    #[serde(default = "yes")]
    pub workers_may_browse: bool,
    /// Spawns com `parent` ficam em `Queued { Approval }` até o humano decidir.
    #[serde(default)]
    pub approve_spawns: bool,
}

fn all_worker_kinds() -> Vec<AgentKind> {
    vec![AgentKind::ClaudeCode, AgentKind::Codex]
}

fn yes() -> bool {
    true
}

impl SessionConfig {
    pub fn new(name: impl Into<String>, repo: impl Into<PathBuf>, goal: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            repo: repo.into(),
            goal: goal.into(),
            max_workers: None,
            allowed_kinds: all_worker_kinds(),
            workers_may_browse: true,
            approve_spawns: false,
        }
    }
}

/// Linha do histórico de sessões.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: SessionId,
    pub config: SessionConfig,
    pub created_at: Timestamp,
    pub closed_at: Option<Timestamp>,
}
