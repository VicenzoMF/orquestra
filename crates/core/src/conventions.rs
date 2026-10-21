//! Convenções que cruzam processos (agente ↔ runner ↔ gateway) e por isso não
//! passam pelo `EventBus`. Mudar qualquer valor aqui quebra agentes já rodando.

/// Variáveis de ambiente injetadas nos agentes.
pub mod env {
    pub const AGENT_ID: &str = "ORQUESTRA_AGENT_ID";
    pub const SESSION_ID: &str = "ORQUESTRA_SESSION_ID";
    /// `http://127.0.0.1:<porta>` do Chromium do agente.
    pub const CDP_URL: &str = "ORQUESTRA_CDP_URL";
    pub const CDP_PORT: &str = "ORQUESTRA_CDP_PORT";
    /// Socket local do gateway MCP (só orquestradores).
    pub const SOCKET: &str = "ORQUESTRA_SOCKET";
    /// Credencial do orquestrador no gateway (só orquestradores).
    pub const TOKEN: &str = "ORQUESTRA_TOKEN";
}

/// Sequências OSC invisíveis que os hooks dos agentes escrevem no próprio TTY
/// (`> /dev/tty`) para o runner saber o status sem adivinhar pela tela.
/// Formato: `ESC ] 777 ; orquestra ; <estado> BEL`.
pub mod osc {
    /// Terminou o turno e espera o humano (hooks `Stop` e `Notification`).
    pub const WAITING: &str = "\x1b]777;orquestra;waiting\x07";
    /// Recebeu um prompt e voltou a trabalhar (hook `UserPromptSubmit`).
    pub const RUNNING: &str = "\x1b]777;orquestra;running\x07";
    /// Parâmetros que identificam a sequência num parser `vte` (`osc_dispatch`).
    pub const PARAMS: [&[u8]; 2] = [b"777", b"orquestra"];
}

/// Nome do servidor MCP do gateway. No Claude Code as ferramentas aparecem
/// como `mcp__orquestra__<ferramenta>`.
pub const MCP_SERVER_NAME: &str = "orquestra";
