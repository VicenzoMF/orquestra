//! `orquestra mcp`: servidor MCP em stdio (rmcp) que o CLI do orquestrador
//! sobe. Cada ferramenta vira uma requisição ao socket do gateway.
//!
//! O stdout é do protocolo MCP: nada de log nele (use stderr).

use orquestra_core::{Error, Result};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Serialize;
use serde_json::{Value, json};

use crate::client::GatewayClient;
use crate::protocol::{
    KillAgentParams, ReadOutputParams, SendInputParams, SpawnAgentParams, WaitForParams, method,
};

/// Servidor MCP `orquestra` (lado do orquestrador).
#[derive(Clone)]
pub struct OrquestraBridge {
    /// `Err` quando o ambiente não tem a credencial: as ferramentas existem,
    /// mas respondem com o motivo, legível para o LLM.
    client: std::result::Result<GatewayClient, String>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl OrquestraBridge {
    pub fn new(client: GatewayClient) -> Self {
        Self {
            client: Ok(client),
            tool_router: Self::tool_router(),
        }
    }

    /// Credencial de `ORQUESTRA_SOCKET`/`ORQUESTRA_TOKEN`.
    pub fn from_env() -> Self {
        Self {
            client: GatewayClient::from_env(),
            tool_router: Self::tool_router(),
        }
    }

    async fn forward(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> std::result::Result<Value, String> {
        let client = self.client.as_ref().map_err(Clone::clone)?;
        let params = serde_json::to_value(params).map_err(|e| e.to_string())?;
        client.call(method, params).await
    }

    #[tool(
        description = "Lists the agents in this session (including you): name, short id, kind, \
        role, status, who created it (parent), browser port and a summary of the task. \
        Status: pending/starting (starting up), queued (waiting for a free slot), \
        awaiting_approval (waiting for the human to approve), running (working), \
        waiting_for_input (stopped and waiting for an answer: usually done with the task or \
        has a question), done, failed."
    )]
    async fn list_agents(&self) -> std::result::Result<String, String> {
        self.forward(method::LIST_AGENTS, json!({}))
            .await
            .map(|v| pretty(&v))
    }

    #[tool(
        description = "Creates an agent (Claude Code or Codex) with a task. It works in its own \
        git worktree and branch (workspace \"worktree\", the default in a git folder) or \
        directly in a folder (workspace \"folder\"; dir picks a folder other than the \
        session's). It starts working on its own; follow it with wait_for and read_output. \
        Write the task in full and self-contained: the agent does not see this conversation. \
        Returns name, id, folder, workspace and the initial status: starting; queued if the \
        limit of simultaneous agents was reached (it starts when a slot frees up); \
        awaiting_approval if the session requires the human to approve."
    )]
    async fn spawn_agent(
        &self,
        Parameters(params): Parameters<SpawnAgentParams>,
    ) -> std::result::Result<String, String> {
        self.forward(method::SPAWN_AGENT, params)
            .await
            .map(|v| pretty(&v))
    }

    #[tool(
        description = "Types text into the terminal of one of your agents and, by default, \
        presses Enter. Use it to answer a question, pick an option of a permission prompt \
        (e.g. \"1\" or \"y\") or give the next instruction. Read the screen with read_output \
        before answering."
    )]
    async fn send_input(
        &self,
        Parameters(params): Parameters<SendInputParams>,
    ) -> std::result::Result<String, String> {
        self.forward(method::SEND_INPUT, params)
            .await
            .map(|v| pretty(&v))
    }

    #[tool(
        description = "Shows the last lines of an agent's terminal screen, as plain text. Use it \
        to see what it is doing, what it asked or its final summary."
    )]
    async fn read_output(
        &self,
        Parameters(params): Parameters<ReadOutputParams>,
    ) -> std::result::Result<String, String> {
        self.forward(method::READ_OUTPUT, params)
            .await
            .map(|v| screen_text(&v))
    }

    #[tool(
        description = "Blocks until your agents meet a condition, or until the timeout; use it \
        instead of calling list_agents in a loop. until: \"idle\" (stopped: waiting for \
        input, done or failed; the usual way to know someone finished), \"done\" (process \
        ended), \"attention\" (needs you: input, approval or failure; or finished), \
        \"change\" (any status change). Without `agents`, it watches all your children and \
        returns on the next event (those already in the condition only count if they change \
        again). With all=true, it waits for all of them. Returns triggered (who fired), each \
        one's status and timed_out."
    )]
    async fn wait_for(
        &self,
        Parameters(params): Parameters<WaitForParams>,
    ) -> std::result::Result<String, String> {
        self.forward(method::WAIT_FOR, params)
            .await
            .map(|v| pretty(&v))
    }

    #[tool(
        description = "Ends the process of one of your agents (SIGTERM, then SIGKILL). The \
        worktree and branch stay. Use it when the task was abandoned or the agent is stuck."
    )]
    async fn kill_agent(
        &self,
        Parameters(params): Parameters<KillAgentParams>,
    ) -> std::result::Result<String, String> {
        self.forward(method::KILL_AGENT, params)
            .await
            .map(|v| pretty(&v))
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "orquestra",
    instructions = "orquestra tools to create and coordinate other coding agents from this \
    session. Typical flow: split the work into independent tasks; spawn_agent for each one; \
    wait_for(until=\"idle\") to know who stopped; read_output to see the result or the \
    question; send_input to answer or give the next instruction; kill_agent to stop one. \
    Each agent works in its own git worktree and branch (git folders) or directly in a \
    folder, as chosen in spawn_agent."
)]
impl ServerHandler for OrquestraBridge {}

/// Ponto de entrada de `orquestra mcp`: serve em stdin/stdout até o CLI fechar.
pub async fn run_stdio_bridge() -> Result<()> {
    let service = OrquestraBridge::from_env()
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| Error::Other(format!("mcp: {e}")))?;
    service
        .waiting()
        .await
        .map_err(|e| Error::Other(format!("mcp: {e}")))?;
    Ok(())
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// `read_output` em texto: cabeçalho curto e a tela como está.
fn screen_text(value: &Value) -> String {
    let field = |k: &str| value.get(k).and_then(Value::as_str).unwrap_or_default();
    let mut header = format!("{} [{}]", field("agent"), field("status"));
    if let Some(detail) = value.get("detail").and_then(Value::as_str) {
        header.push_str(&format!(" ({detail})"));
    }
    if let Some(note) = value.get("note").and_then(Value::as_str) {
        return format!("{header}: {note}");
    }
    let lines = value.get("lines").and_then(Value::as_u64).unwrap_or(0);
    format!("{header}, últimas {lines} linhas:\n{}", field("text"))
}

#[cfg(test)]
mod tests {
    use rmcp::model::CallToolRequestParams;

    use super::*;

    async fn connect(
        bridge: OrquestraBridge,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            if let Ok(server) = bridge.serve(server_io).await {
                let _ = server.waiting().await;
            }
        });
        ().serve(client_io).await.unwrap()
    }

    #[tokio::test]
    async fn exposes_the_six_tools_with_schemas() {
        let client = connect(OrquestraBridge::new(GatewayClient::new(
            "/nao/existe.sock",
            "t",
        )))
        .await;
        let info = client.peer_info().unwrap();
        assert_eq!(info.server_info.as_ref().unwrap().name, "orquestra");
        assert!(
            info.instructions
                .as_deref()
                .unwrap()
                .contains("spawn_agent")
        );

        let tools = client.list_all_tools().await.unwrap();
        let mut names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "kill_agent",
                "list_agents",
                "read_output",
                "send_input",
                "spawn_agent",
                "wait_for"
            ]
        );
        let spawn = tools.iter().find(|t| t.name == "spawn_agent").unwrap();
        let schema = serde_json::to_value(&*spawn.input_schema).unwrap();
        for prop in ["name", "task", "kind", "browser"] {
            assert!(
                schema["properties"][prop]["description"].is_string(),
                "{prop}"
            );
        }
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("name")) && required.contains(&json!("task")));
        let wait = tools.iter().find(|t| t.name == "wait_for").unwrap();
        let schema = serde_json::to_value(&*wait.input_schema).unwrap();
        assert!(schema.to_string().contains("attention"));
        assert!(spawn.description.as_deref().unwrap().contains("worktree"));
        client.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn gateway_errors_become_readable_tool_errors() {
        let client = connect(OrquestraBridge::new(GatewayClient::new(
            "/nao/existe.sock",
            "t",
        )))
        .await;
        let result = client
            .call_tool(CallToolRequestParams::new("list_agents"))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.contains("gateway do orquestra inacessível"), "{text}");

        let unconfigured = OrquestraBridge {
            client: Err("ORQUESTRA_SOCKET não definido".to_owned()),
            tool_router: OrquestraBridge::tool_router(),
        };
        let client = connect(unconfigured).await;
        let result = client
            .call_tool(CallToolRequestParams::new("list_agents"))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(
            result.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("não definido")
        );
    }

    #[test]
    fn read_output_is_rendered_as_plain_text() {
        let value = json!({"agent": "api", "status": "running", "lines": 2, "text": "a\nb"});
        assert_eq!(
            screen_text(&value),
            "api [running], últimas 2 linhas:\na\nb"
        );
        let empty = json!({"agent": "api", "status": "starting", "note": "sem saída", "text": ""});
        assert_eq!(screen_text(&empty), "api [starting]: sem saída");
    }
}
