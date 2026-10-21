use bytes::Bytes;

use crate::{
    AgentConfig, AgentId, AppConfig, BrowserInput, Navigation, SessionConfig, SessionId, TermSize,
    Viewport, WorktreeCleanup,
};

/// Pedidos ao sistema, vindos da UI ou do MCP `orquestra`. Não têm resposta
/// direta: o resultado chega como `AgentEvent` (inclusive `AgentEvent::Error`).
///
/// O hub processa os comandos de um mesmo agente em ordem; agentes diferentes
/// rodam em paralelo.
#[derive(Clone, Debug)]
pub enum Command {
    /// Cria uma orquestração vazia → `SessionCreated`. O orquestrador é um
    /// `Spawn` normal com `role: Orchestrator` nessa sessão.
    CreateSession {
        id: SessionId,
        config: Box<SessionConfig>,
    },
    /// Remove todos os agentes da sessão (com `cleanup`) → `Removed`… e
    /// `SessionClosed`.
    CloseSession {
        id: SessionId,
        cleanup: WorktreeCleanup,
    },

    /// `id` é gerado por quem pede (`AgentId::generate()`). A sessão de
    /// `config.session` precisa existir. Workers podem ir para a fila
    /// (`Queued`) por limite ou aprovação.
    Spawn {
        id: AgentId,
        config: Box<AgentConfig>,
    },
    /// Bytes já codificados para o terminal (teclas, colar com bracketed paste...).
    SendInput {
        id: AgentId,
        data: Bytes,
    },
    Resize {
        id: AgentId,
        size: TermSize,
    },
    /// Encerra o processo (SIGTERM → SIGKILL). Worktree e navegador ficam.
    /// Na fila: sai da fila como `Done`.
    Kill {
        id: AgentId,
    },
    /// Encerra se vivo, aplica `cleanup`, fecha o navegador e esquece o agente.
    Remove {
        id: AgentId,
        cleanup: WorktreeCleanup,
    },
    /// Libera um spawn em `Queued { Approval }`.
    ApproveSpawn {
        id: AgentId,
    },
    /// Descarta um spawn em `Queued { Approval }` → `Removed`.
    RejectSpawn {
        id: AgentId,
    },
    /// Pede um `AgentEvent::Replay` (ao abrir um terminal ou após lag).
    RequestReplay {
        id: AgentId,
    },

    /// Lança o Chromium do agente se preciso, liga o screencast e navega para
    /// `url` se houver.
    OpenBrowser {
        id: AgentId,
        url: Option<String>,
    },
    /// Mata o Chromium do agente.
    CloseBrowser {
        id: AgentId,
    },
    /// Liga/desliga o screencast (painel visível ou não); o Chromium continua.
    SetBrowserVisible {
        id: AgentId,
        visible: bool,
    },
    BrowserInput {
        id: AgentId,
        input: BrowserInput,
    },
    BrowserNavigate {
        id: AgentId,
        nav: Navigation,
    },
    BrowserResize {
        id: AgentId,
        viewport: Viewport,
    },

    /// Pede um `AgentEvent::History` com as `limit` sessões mais recentes.
    LoadHistory {
        limit: usize,
    },
    /// Persiste a config (e aplica a parte de notificações na hora).
    SaveConfig(Box<AppConfig>),
    /// Encerra tudo: agentes, navegadores, store. Termina com `AgentEvent::Shutdown`.
    Shutdown,
}

impl Command {
    /// Agente alvo; `None` para comandos globais.
    pub fn agent_id(&self) -> Option<AgentId> {
        match self {
            Self::Spawn { id, .. }
            | Self::SendInput { id, .. }
            | Self::Resize { id, .. }
            | Self::Kill { id }
            | Self::Remove { id, .. }
            | Self::ApproveSpawn { id }
            | Self::RejectSpawn { id }
            | Self::RequestReplay { id }
            | Self::OpenBrowser { id, .. }
            | Self::CloseBrowser { id }
            | Self::SetBrowserVisible { id, .. }
            | Self::BrowserInput { id, .. }
            | Self::BrowserNavigate { id, .. }
            | Self::BrowserResize { id, .. } => Some(*id),
            Self::CreateSession { .. }
            | Self::CloseSession { .. }
            | Self::LoadHistory { .. }
            | Self::SaveConfig(_)
            | Self::Shutdown => None,
        }
    }

    /// Nome curto para logs e mensagens de erro.
    pub fn name(&self) -> &'static str {
        match self {
            Self::CreateSession { .. } => "create_session",
            Self::CloseSession { .. } => "close_session",
            Self::Spawn { .. } => "spawn",
            Self::SendInput { .. } => "send_input",
            Self::Resize { .. } => "resize",
            Self::Kill { .. } => "kill",
            Self::Remove { .. } => "remove",
            Self::ApproveSpawn { .. } => "approve_spawn",
            Self::RejectSpawn { .. } => "reject_spawn",
            Self::RequestReplay { .. } => "request_replay",
            Self::OpenBrowser { .. } => "open_browser",
            Self::CloseBrowser { .. } => "close_browser",
            Self::SetBrowserVisible { .. } => "set_browser_visible",
            Self::BrowserInput { .. } => "browser_input",
            Self::BrowserNavigate { .. } => "browser_navigate",
            Self::BrowserResize { .. } => "browser_resize",
            Self::LoadHistory { .. } => "load_history",
            Self::SaveConfig(_) => "save_config",
            Self::Shutdown => "shutdown",
        }
    }
}
