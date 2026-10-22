//! Protocolo do socket do gateway: uma requisição JSON por linha,
//! `{"token": "...", "method": "...", "params": {...}}`, e uma resposta por
//! linha, `{"ok": ...}` ou `{"error": "..."}`.
//!
//! Os parâmetros são os mesmos das ferramentas MCP: a ponte stdio repassa os
//! argumentos como vieram, e o schema que o LLM vê sai destes tipos.
//!
//! O cliente não deve fechar a escrita antes da resposta: o gateway entende
//! o fim da conexão como desistência e abandona a requisição (um `wait_for`
//! ou `take_news` cancelado não consome nada).

use orquestra_core::{AgentKind, AgentStatus, QueueReason};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Métodos aceitos pelo gateway.
pub mod method {
    pub const LIST_AGENTS: &str = "list_agents";
    pub const SPAWN_AGENT: &str = "spawn_agent";
    pub const SEND_INPUT: &str = "send_input";
    pub const READ_OUTPUT: &str = "read_output";
    pub const WAIT_FOR: &str = "wait_for";
    pub const KILL_AGENT: &str = "kill_agent";
    /// Só o hook `Stop` usa; não vira ferramenta MCP.
    pub const TAKE_NEWS: &str = "take_news";
}

/// Uma linha enviada ao gateway.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub token: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Uma linha devolvida pelo gateway.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Ok(Value),
    Error(String),
}

impl Response {
    pub fn into_result(self) -> Result<Value, String> {
        match self {
            Self::Ok(value) => Ok(value),
            Self::Error(message) => Err(message),
        }
    }
}

impl From<Result<Value, String>> for Response {
    fn from(result: Result<Value, String>) -> Self {
        match result {
            Ok(value) => Self::Ok(value),
            Err(message) => Self::Error(message),
        }
    }
}

// ---------------------------------------------------------------------------
// Parâmetros (também são o schema das ferramentas MCP). Os doc comments dos
// campos viram a descrição no schema, que o modelo lê: por isso em inglês,
// como as instruções e as descrições das ferramentas.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ListAgentsParams {}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SpawnAgentParams {
    /// Short name, unique in the session (part of the branch name), e.g.
    /// "api-login". If taken, orquestra adds a suffix (-2, -3...).
    pub name: String,
    /// Full, self-contained task for the agent: it does not see this
    /// conversation. Say what to do, where, what done means and what not to
    /// touch.
    pub task: String,
    /// Agent kind: "claude" or "codex" (or a preset name). Default: the first
    /// kind allowed in the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Gives the agent its own Chromium, controllable via MCP. Default: the
    /// preset's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<bool>,
    /// Where the agent works: "worktree" (a git worktree and branch of its
    /// own: to change code without disturbing the others) or "folder"
    /// (directly in the folder: to research, read, organize files or work
    /// outside a repository; agents in the same folder can step on each
    /// other). Default: "worktree" if the folder is a git repository, else
    /// "folder".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Working folder, if not the session's: absolute, "~/..." or relative to
    /// the session folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SendInputParams {
    /// Agent name (or short id).
    pub agent: String,
    /// Text to type into the agent's terminal. Sent as is: for special keys
    /// use escapes (e.g. "\u001b" for Esc). Empty + submit = just Enter.
    pub text: String,
    /// Press Enter after the text.
    #[serde(default = "yes")]
    pub submit: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReadOutputParams {
    /// Agent name (or short id).
    pub agent: String,
    /// How many non-empty lines from the end of the screen to return (1 to
    /// 500).
    #[serde(default = "default_lines")]
    pub lines: usize,
}

/// Condição de `wait_for`. No schema vira um `enum` de strings simples (sem
/// `$ref`/`oneOf`, que nem todo cliente MCP entende); por isso os variantes
/// não têm doc comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum WaitUntil {
    // Parou: aguardando input, terminou ou falhou.
    Idle,
    // Terminou ou falhou (processo encerrado).
    Done,
    // Precisa de atenção (aguardando input, aprovação, falhou) ou terminou.
    Attention,
    // Qualquer mudança de status depois da chamada.
    Change,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WaitForParams {
    /// Agents to watch (names or short ids). Default: all your children; then
    /// those already in the condition only count if they change again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<String>>,
    /// "idle" (stopped: waiting for input, done or failed), "done" (process
    /// ended), "attention" (needs you, or finished) or "change" (any status
    /// change).
    pub until: WaitUntil,
    /// Maximum wait in seconds (0 to 3600).
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// true: wait until all agents meet the condition; false: the first one.
    #[serde(default)]
    pub all: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct KillAgentParams {
    /// Agent name (or short id).
    pub agent: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct TakeNewsParams {
    /// Sem novidades e com filhos trabalhando, espera até este tempo por uma.
    #[serde(default)]
    pub wait_secs: u64,
}

fn yes() -> bool {
    true
}

pub const DEFAULT_LINES: usize = 80;
pub const MAX_LINES: usize = 500;
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;
pub const MAX_TIMEOUT_SECS: u64 = 3600;

fn default_lines() -> usize {
    DEFAULT_LINES
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

// ---------------------------------------------------------------------------
// Respostas
// ---------------------------------------------------------------------------

/// Um agente da sessão, como `list_agents` mostra.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentSummary {
    pub name: String,
    pub id: String,
    pub kind: String,
    pub role: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
}

/// Nome, id curto e status de um agente.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBrief {
    pub agent: String,
    pub id: String,
    /// Código de `status_code` (`running`, `waiting_for_input`...).
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl AgentBrief {
    /// Ainda trabalha sozinho: vivo e sem esperar ninguém.
    pub fn is_working(&self) -> bool {
        matches!(
            self.status.as_str(),
            "pending" | "queued" | "starting" | "running"
        )
    }
}

/// Uma novidade de um filho para o orquestrador.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewsItem {
    pub agent: String,
    pub id: String,
    /// `waiting_for_input`, `awaiting_approval`, `done`, `failed` ou `removed`.
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Quando aconteceu (ms desde a época Unix).
    pub at: i64,
}

/// Resposta de `take_news`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewsReport {
    pub news: Vec<NewsItem>,
    /// Todos os filhos do orquestrador, com o status atual.
    pub children: Vec<AgentBrief>,
}

impl NewsReport {
    pub fn working(&self) -> impl Iterator<Item = &AgentBrief> {
        self.children.iter().filter(|c| c.is_working())
    }
}

// ---------------------------------------------------------------------------
// Códigos estáveis
// ---------------------------------------------------------------------------

/// Código estável do status (snake_case), usado no JSON.
pub fn status_code(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Queued {
            reason: QueueReason::Capacity,
        } => "queued",
        AgentStatus::Queued {
            reason: QueueReason::Approval,
        } => "awaiting_approval",
        AgentStatus::Starting => "starting",
        AgentStatus::Running => "running",
        AgentStatus::WaitingForInput => "waiting_for_input",
        AgentStatus::Done { .. } => "done",
        AgentStatus::Failed { .. } => "failed",
    }
}

/// Código de saída ou motivo da falha, quando houver.
pub fn status_detail(status: &AgentStatus) -> Option<String> {
    match status {
        AgentStatus::Done {
            exit_code: Some(code),
        } => Some(format!("código de saída {code}")),
        AgentStatus::Done { exit_code: None } => Some("encerrado a pedido".to_owned()),
        AgentStatus::Failed { reason } => Some(reason.clone()),
        _ => None,
    }
}

/// Rótulo em português de um código de status ou de novidade.
pub fn code_label(code: &str) -> &str {
    match code {
        "pending" => "aguardando o hub",
        "queued" => "na fila",
        "awaiting_approval" => "aguardando aprovação do humano",
        "starting" => "iniciando",
        "running" => "trabalhando",
        "waiting_for_input" => "aguardando input",
        "done" => "terminou",
        "failed" => "falhou",
        "removed" => "removido",
        other => other,
    }
}

pub fn kind_code(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::ClaudeCode => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Shell => "shell",
        AgentKind::Custom => "custom",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn response_wire_format() {
        let ok = serde_json::to_value(Response::Ok(json!({"a": 1}))).unwrap();
        assert_eq!(ok, json!({"ok": {"a": 1}}));
        let err = serde_json::to_value(Response::Error("x".into())).unwrap();
        assert_eq!(err, json!({"error": "x"}));
        let back: Response = serde_json::from_value(json!({"error": "y"})).unwrap();
        assert_eq!(back.into_result(), Err("y".to_owned()));
    }

    #[test]
    fn until_schema_is_a_plain_string_enum() {
        let schema = serde_json::to_value(schemars::schema_for!(WaitForParams)).unwrap();
        assert!(schema.get("$defs").is_none(), "{schema}");
        assert_eq!(schema["properties"]["until"]["type"], "string");
        assert_eq!(
            schema["properties"]["until"]["enum"],
            json!(["idle", "done", "attention", "change"])
        );
        assert!(schema["properties"]["until"]["description"].is_string());
    }

    #[test]
    fn params_fill_defaults() {
        let p: SendInputParams =
            serde_json::from_value(json!({"agent": "a", "text": "t"})).unwrap();
        assert!(p.submit);
        let p: ReadOutputParams = serde_json::from_value(json!({"agent": "a"})).unwrap();
        assert_eq!(p.lines, DEFAULT_LINES);
        let p: WaitForParams = serde_json::from_value(json!({"until": "attention"})).unwrap();
        assert_eq!(p.until, WaitUntil::Attention);
        assert_eq!(p.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert!(p.agents.is_none() && !p.all);
        assert!(serde_json::from_value::<WaitForParams>(json!({"until": "nunca"})).is_err());
    }
}
