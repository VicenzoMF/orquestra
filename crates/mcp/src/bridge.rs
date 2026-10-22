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
        description = "Lista os agentes desta orquestração (inclusive você): nome, id curto, tipo, \
        papel, status, quem criou (parent), porta do navegador e um resumo da tarefa. Status: \
        pending/starting (subindo), queued (fila por limite de agentes simultâneos), \
        awaiting_approval (esperando o humano aprovar), running (trabalhando), waiting_for_input \
        (parou e espera resposta: em geral terminou a tarefa ou tem uma pergunta), done, failed."
    )]
    async fn list_agents(&self) -> std::result::Result<String, String> {
        self.forward(method::LIST_AGENTS, json!({}))
            .await
            .map(|v| pretty(&v))
    }

    #[tool(
        description = "Cria um agente (Claude Code ou Codex) já com uma tarefa. Ele trabalha num \
        git worktree e branch próprios (workspace \"worktree\", o padrão numa pasta git) ou \
        direto numa pasta (workspace \"pasta\"; dir escolhe outra pasta que não a da sessão). \
        Ele começa a trabalhar sozinho; acompanhe com wait_for e read_output. Escreva a tarefa \
        completa e autocontida: o agente não vê esta conversa. Retorna nome, id, pasta, \
        workspace e status inicial: starting; queued se o limite de agentes simultâneos foi \
        atingido (sobe sozinho quando abrir vaga); awaiting_approval se a sessão exige que o \
        humano aprove."
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
        description = "Digita texto no terminal de um agente seu e, por padrão, aperta Enter. Use \
        para responder uma pergunta, escolher uma opção de um pedido de permissão (ex.: \"1\" ou \
        \"y\") ou dar a próxima instrução. Leia a tela com read_output antes de responder."
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
        description = "Mostra as últimas linhas da tela do terminal de um agente, em texto puro. \
        Use para ver o que ele está fazendo, o que perguntou ou o resumo final do trabalho."
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
        description = "Bloqueia até agentes seus satisfazerem uma condição, ou até o timeout; use \
        em vez de chamar list_agents em loop. until: \"idle\" (parou: aguardando input, terminou \
        ou falhou; o caso comum para saber que alguém acabou a tarefa), \"done\" (processo \
        encerrado), \"attention\" (precisa de você: input, aprovação ou falha; ou terminou), \
        \"change\" (qualquer mudança de status). Sem `agents`, observa todos os seus filhos e \
        volta no próximo evento (quem já estava na condição só conta se mudar de novo). Com \
        all=true, espera todos. Retorna triggered (quem disparou), o status de cada um e \
        timed_out."
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
        description = "Encerra o processo de um agente seu (SIGTERM, depois SIGKILL). O worktree \
        e a branch ficam. Use quando a tarefa foi abandonada ou o agente travou."
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
    instructions = "Ferramentas do orquestra para coordenar os agentes de código desta \
    orquestração. Fluxo típico: divida o objetivo em tarefas independentes; spawn_agent para \
    cada uma; wait_for(until=\"idle\") para saber quem parou; read_output para ver o resultado \
    ou a pergunta; send_input para responder ou dar a próxima instrução; kill_agent para \
    encerrar. Cada agente trabalha num worktree e branch próprios (pastas git) ou direto \
    numa pasta, conforme o spawn_agent."
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
