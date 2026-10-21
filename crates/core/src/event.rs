use std::sync::Arc;

use bytes::Bytes;

use crate::{
    AgentConfig, AgentId, AgentInfo, AgentStatus, BrowserFrame, BrowserStatus, History,
    SessionConfig, SessionId,
};

/// Tudo que acontece no sistema, publicado no `EventBus`.
///
/// Payload grande vai em `Arc`/`Bytes`: o broadcast clona o evento para cada
/// assinante e pré-aloca `capacidade × size_of::<AgentEvent>()`.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// Orquestração criada (`Command::CreateSession`). Publicado pelo hub.
    SessionCreated {
        id: SessionId,
        config: Arc<SessionConfig>,
    },
    /// Orquestração encerrada; seus agentes recebem `Removed`. Publicado pelo hub.
    SessionClosed { id: SessionId },

    /// Spawn aceito: o agente existe para a UI. O status vem logo depois
    /// (`Queued` ou `Starting`). `config` traz sessão, papel e `parent`, que é
    /// a seta no canvas. Publicado pelo hub.
    Created {
        id: AgentId,
        config: Arc<AgentConfig>,
    },
    /// Processo no ar, com worktree/pid/endpoint. Publicado pelo hub.
    Spawned { info: Arc<AgentInfo> },
    /// Publicado pelo hub (`Queued`, `Starting`, `Failed` de spawn) e pelo runner.
    StatusChanged { id: AgentId, status: AgentStatus },
    /// Bytes crus do PTY, em ordem. `seq` cresce de 1 em 1 por agente a partir
    /// de 0; um buraco significa eventos perdidos (lag): peça `RequestReplay`.
    Output { id: AgentId, seq: u64, data: Bytes },
    /// Resposta a `RequestReplay`: o fim do histórico de saída, até
    /// `replay_bytes`. O consumidor reinicia o terminal, alimenta `data` e passa
    /// a esperar `Output` a partir de `next_seq`.
    Replay {
        id: AgentId,
        next_seq: u64,
        data: Bytes,
    },
    /// Agente esquecido pelo sistema (após `Command::Remove`). Publicado pelo hub.
    Removed { id: AgentId },

    /// Publicado pelo browser.
    BrowserStatus { id: AgentId, status: BrowserStatus },
    /// Vai para o canal de quadros (com perda), não para o principal; ver
    /// `EventBus`. Publicado pelo browser.
    BrowserFrame {
        id: AgentId,
        frame: Arc<BrowserFrame>,
    },
    /// A página principal navegou. Publicado pelo browser.
    BrowserNavigated {
        id: AgentId,
        url: String,
        title: String,
    },

    /// Resposta a `Command::LoadHistory`.
    History(Arc<History>),
    /// Um comando falhou. `id` é `None` se não se refere a um agente.
    Error {
        id: Option<AgentId>,
        message: String,
    },
    /// Último evento antes de o hub parar.
    Shutdown,
}

impl AgentEvent {
    pub fn agent_id(&self) -> Option<AgentId> {
        match self {
            Self::Created { id, .. }
            | Self::StatusChanged { id, .. }
            | Self::Output { id, .. }
            | Self::Replay { id, .. }
            | Self::Removed { id }
            | Self::BrowserStatus { id, .. }
            | Self::BrowserFrame { id, .. }
            | Self::BrowserNavigated { id, .. } => Some(*id),
            Self::Spawned { info } => Some(info.id),
            Self::Error { id, .. } => *id,
            Self::SessionCreated { .. }
            | Self::SessionClosed { .. }
            | Self::History(_)
            | Self::Shutdown => None,
        }
    }

    pub fn is_frame(&self) -> bool {
        matches!(self, Self::BrowserFrame { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O broadcast reserva um slot desse tamanho por posição da fila.
    #[test]
    fn event_stays_small() {
        assert!(
            std::mem::size_of::<AgentEvent>() <= 80,
            "AgentEvent tem {} bytes; mova o payload novo para Arc",
            std::mem::size_of::<AgentEvent>()
        );
    }
}
