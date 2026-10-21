use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{AgentId, BrowserEndpoint, McpServerSpec, SessionId, Timestamp};

/// Família do agente. Define a heurística de status e como o runner injeta
/// prompt inicial e MCP do navegador.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    Shell,
    #[default]
    Custom,
}

/// Papel do agente na orquestração.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Coordena a sessão: recebe o MCP `orquestra` e cria/acompanha os
    /// outros agentes. No máximo um por sessão (a UI garante).
    Orchestrator,
    /// Executa uma tarefa. Conta no limite de agentes simultâneos.
    #[default]
    Worker,
}

/// Onde o processo do agente roda.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Workspace {
    /// Cria um git worktree novo para o agente (isolamento padrão).
    Worktree {
        /// Repositório de origem (qualquer caminho dentro dele).
        repo: PathBuf,
        /// Ref de partida; `None` = HEAD do repositório.
        #[serde(default)]
        base: Option<String>,
        /// Branch a criar; `None` = `<branch_prefix><nome>-<id curto>`.
        #[serde(default)]
        branch: Option<String>,
    },
    /// Roda direto num diretório existente, sem isolamento.
    Directory { path: PathBuf },
}

/// Tamanho do terminal. A UI é a dona do tamanho: redimensiona o próprio
/// `Term` e manda `Command::Resize` para o PTY acompanhar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
    /// Tamanho da célula em pixels (vai para o `TIOCSWINSZ`; 0 se desconhecido).
    pub cell_width: u16,
    pub cell_height: u16,
}

impl Default for TermSize {
    fn default() -> Self {
        Self {
            cols: 120,
            rows: 32,
            cell_width: 0,
            cell_height: 0,
        }
    }
}

/// Tudo que é preciso para subir um agente. Serializável para presets e store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Nome de exibição (também entra no nome da branch).
    pub name: String,
    pub kind: AgentKind,
    /// Executável (`claude`, `codex`, `bash`...), resolvido pelo PATH.
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Variáveis extras; o ambiente do app é herdado.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Orquestração a que o agente pertence (precisa existir no hub).
    pub session: SessionId,
    #[serde(default)]
    pub role: AgentRole,
    /// Quem criou este agente: a seta no canvas. `None` = criado pela UI
    /// (avulso). Spawns com `parent` passam pela aprovação da sessão, se ligada.
    #[serde(default)]
    pub parent: Option<AgentId>,
    pub workspace: Workspace,
    /// Primeira tarefa. O runner a passa como argumento (Claude Code/Codex) ou
    /// a escreve no PTY (demais tipos).
    #[serde(default)]
    pub initial_prompt: Option<String>,
    /// Dá ao agente um Chromium próprio, já lançado no spawn, com o endpoint
    /// CDP injetado no ambiente e no MCP do agente.
    #[serde(default)]
    pub browser: bool,
    /// Tamanho inicial do PTY.
    #[serde(default)]
    pub size: TermSize,
}

/// Por que um spawn está na fila.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueReason {
    /// Limite global ou da sessão atingido; sai quando uma vaga abrir.
    Capacity,
    /// A sessão pede aprovação humana (`Command::ApproveSpawn`/`RejectSpawn`).
    Approval,
}

/// Estado do ciclo de vida de um agente.
///
/// Quem publica: o hub publica `Queued`, `Starting` e o `Failed` de um spawn
/// que falhou; depois do spawn, só o runner muda o status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentStatus {
    /// Spawn aceito mas esperando vaga ou aprovação; ainda não há processo.
    Queued { reason: QueueReason },
    /// Saiu da fila; worktree/processo subindo.
    Starting,
    /// Processo vivo e trabalhando (produzindo saída).
    Running,
    /// Processo vivo esperando o humano (prompt ocioso, pedido de permissão...).
    WaitingForInput,
    /// Terminou com código 0, ou foi encerrado pelo usuário (`exit_code: None`
    /// quando morto por sinal a pedido do usuário).
    Done { exit_code: Option<i32> },
    /// Falha no spawn, código de saída ≠ 0 ou morte por sinal inesperado.
    Failed { reason: String },
}

impl AgentStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Failed { .. })
    }

    pub fn is_alive(&self) -> bool {
        !self.is_terminal()
    }

    /// Ocupa uma vaga do limite de agentes simultâneos.
    pub fn occupies_slot(&self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::WaitingForInput)
    }

    /// Estados que pedem atenção humana (destacados na UI, geram notificação).
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            Self::WaitingForInput
                | Self::Failed { .. }
                | Self::Queued {
                    reason: QueueReason::Approval
                }
        )
    }

    /// Mesmo estado, ignorando o payload (`Failed("a")` ~ `Failed("b")`).
    pub fn same_state(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Queued {
                reason: QueueReason::Capacity,
            } => "queued",
            Self::Queued {
                reason: QueueReason::Approval,
            } => "awaiting approval",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::WaitingForInput => "waiting for input",
            Self::Done { .. } => "done",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Worktree criado para um agente.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    /// Raiz do repositório de origem.
    pub repo: PathBuf,
    /// Diretório do worktree (é o `workdir` do agente).
    pub path: PathBuf,
    pub branch: String,
    /// Ref pedida (`HEAD`, `main`...).
    pub base_ref: String,
    /// SHA resolvido de `base_ref` na criação.
    pub base_commit: String,
}

/// Fatos de runtime de um agente já iniciado (devolvido por `AgentRunner::spawn`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: AgentId,
    pub config: AgentConfig,
    /// Diretório onde o processo roda.
    pub workdir: PathBuf,
    pub worktree: Option<WorktreeInfo>,
    pub pid: Option<u32>,
    pub browser: Option<BrowserEndpoint>,
    pub started_at: Timestamp,
}

/// O que fazer com o worktree ao remover um agente.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeCleanup {
    /// Mantém worktree e branch (padrão: nada se perde).
    #[default]
    Keep,
    /// Remove o diretório do worktree; a branch fica.
    RemoveWorktree,
    /// Remove worktree e branch.
    RemoveWorktreeAndBranch,
}

/// Entrada de `AgentRunner::spawn`. Montada pelo hub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnRequest {
    pub id: AgentId,
    pub config: AgentConfig,
    /// Presente se o agente tem navegador: o runner injeta o endpoint no
    /// ambiente (`ORQUESTRA_CDP_URL`, `ORQUESTRA_CDP_PORT`).
    pub browser: Option<BrowserEndpoint>,
    /// Servidores MCP a registrar no agente (navegador, `orquestra`). O runner
    /// traduz para o formato de cada CLI (`--mcp-config`, `-c mcp_servers...`).
    pub mcp_servers: Vec<McpServerSpec>,
    /// Variáveis extras definidas pelo hub (credencial do gateway etc.).
    pub env: BTreeMap<String, String>,
    /// Só orquestradores Claude Code: comando de shell para o hook `Stop`, no
    /// lugar do hook de status padrão. É por ele que as novidades dos agentes
    /// chegam ao orquestrador quando ele termina um turno.
    pub stop_hook: Option<String>,
}
