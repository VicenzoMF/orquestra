//! Traits que `runner`, `browser`, `store` e `mcp` implementam.
//!
//! Regras comuns:
//! - Os serviços recebem um `EventBus` no construtor e publicam eventos por
//!   ele; as traits não carregam a fiação.
//! - Métodos assíncronos retornam rápido. Trabalho longo (leitura do
//!   PTY, screencast) roda em tasks internas e reporta por evento. Exceções:
//!   `AgentRunner::spawn` e `BrowserManager::launch`, que esperam o recurso
//!   ficar pronto.
//! - O hub chama os métodos de um mesmo agente em sequência, nunca em paralelo.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::{
    AgentConfig, AgentEvent, AgentId, AgentInfo, AgentStatus, AppConfig, BrowserEndpoint,
    BrowserInput, Navigation, OrchestratorGrant, Result, SessionId, SessionRecord, SpawnRequest,
    TermSize, Timestamp, Viewport, WorktreeCleanup,
};

/// Processos de agentes em PTY e seus worktrees. Implementado por `runner`.
///
/// Publica: `StatusChanged` (Running/WaitingForInput/Done/Failed), `Output`,
/// `Replay`.
pub trait AgentRunner: Send + Sync + 'static {
    /// Cria o worktree (se `Workspace::Worktree`), registra `mcp_servers` e
    /// hooks na CLI do agente, sobe o processo no PTY e começa a ler a saída.
    /// Retorna quando o processo existe.
    fn spawn(&self, req: SpawnRequest) -> impl Future<Output = Result<AgentInfo>> + Send;

    /// Enfileira bytes para o PTY; não espera o processo ler.
    fn send_input(&self, id: AgentId, data: Bytes) -> impl Future<Output = Result<()>> + Send;

    fn resize(&self, id: AgentId, size: TermSize) -> impl Future<Output = Result<()>> + Send;

    /// SIGTERM no grupo do processo; SIGKILL após `kill_grace_ms`. O status
    /// final chega por `StatusChanged`. Ok se já tiver terminado.
    fn kill(&self, id: AgentId) -> impl Future<Output = Result<()>> + Send;

    /// Mata se vivo, aplica `cleanup` e esquece o agente.
    fn remove(
        &self,
        id: AgentId,
        cleanup: WorktreeCleanup,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Publica um `Replay` com o histórico de saída guardado.
    fn request_replay(&self, id: AgentId) -> impl Future<Output = Result<()>> + Send;

    /// Mata todos os agentes (worktrees ficam).
    fn shutdown(&self) -> impl Future<Output = ()> + Send;
}

/// Um Chromium por agente, controlado via CDP. Implementado por `browser`.
///
/// Publica: `BrowserStatus`, `BrowserFrame`, `BrowserNavigated`.
pub trait BrowserManager: Send + Sync + 'static {
    /// Garante um Chromium para o agente e retorna a porta de debug.
    /// Idempotente: se já existe, devolve o mesmo endpoint.
    fn launch(&self, id: AgentId) -> impl Future<Output = Result<BrowserEndpoint>> + Send;

    /// Mata o Chromium do agente. Ok se não houver.
    fn close(&self, id: AgentId) -> impl Future<Output = Result<()>> + Send;

    /// Liga/desliga o screencast da página ativa.
    fn set_streaming(&self, id: AgentId, on: bool) -> impl Future<Output = Result<()>> + Send;

    fn navigate(&self, id: AgentId, nav: Navigation) -> impl Future<Output = Result<()>> + Send;

    fn input(&self, id: AgentId, input: BrowserInput) -> impl Future<Output = Result<()>> + Send;

    fn resize(&self, id: AgentId, viewport: Viewport) -> impl Future<Output = Result<()>> + Send;

    /// Mata todos os Chromium.
    fn shutdown(&self) -> impl Future<Output = ()> + Send;
}

/// Persistência. Implementado por `store`.
///
/// Síncrona de propósito (rusqlite): o hub chama `record` numa thread
/// dedicada e as consultas via `spawn_blocking`, então pode bloquear.
pub trait Store: Send + Sync + 'static {
    /// Recebe todo evento do canal principal (nunca `BrowserFrame`), em ordem.
    /// Deve agrupar escritas de `Output` (log) em vez de um INSERT por chunk.
    fn record(&self, event: &AgentEvent) -> Result<()>;

    /// As `limit` sessões mais recentes e os agentes delas.
    fn history(&self, limit: usize) -> Result<History>;

    /// Últimos `max_bytes` de saída registrada de um agente.
    fn output_log(&self, id: AgentId, max_bytes: usize) -> Result<Vec<u8>>;

    /// Lida pelo `main` antes de montar o hub. Arquivo ausente = default.
    fn load_config(&self) -> Result<AppConfig>;

    fn save_config(&self, config: &AppConfig) -> Result<()>;

    /// Grava o que estiver em buffer. Chamado no shutdown.
    fn flush(&self) -> Result<()>;
}

/// Notificação de desktop. Implementado por `store` (notify-rust).
/// Chamado numa thread dedicada: pode bloquear.
pub trait Notifier: Send + Sync + 'static {
    fn notify(&self, notification: &Notification) -> Result<()>;
}

/// Ponte entre orquestradores e o hub: o servidor MCP `orquestra`.
/// Implementado por `mcp`.
///
/// O gateway assina o `EventBus` e manda `Command`s como qualquer cliente; esta
/// trait cobre só o que o hub precisa dele ao subir/remover um orquestrador.
/// Síncrona: só registra/apaga uma credencial.
pub trait OrchestratorGateway: Send + Sync + 'static {
    /// Credencial do orquestrador `agent` da sessão `session`. Com ela, o
    /// orquestrador só enxerga e controla agentes da própria sessão.
    fn grant(&self, session: SessionId, agent: AgentId) -> Result<OrchestratorGrant>;

    /// Invalida a credencial (o agente foi removido).
    fn revoke(&self, agent: AgentId);
}

/// Resposta de `Store::history`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    pub sessions: Vec<SessionRecord>,
    pub agents: Vec<AgentRecord>,
}

/// Linha do histórico de agentes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: AgentId,
    pub config: AgentConfig,
    /// `None` se o spawn falhou antes do processo existir.
    pub info: Option<AgentInfo>,
    pub status: AgentStatus,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NotificationKind {
    NeedsInput,
    NeedsApproval,
    Finished,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub agent: AgentId,
    pub kind: NotificationKind,
    pub title: String,
    pub body: String,
}
