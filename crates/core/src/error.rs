use crate::AgentId;

/// Erro comum aos serviços. Cada crate converte seus erros internos para cá,
/// na fronteira das traits.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("agente desconhecido: {0}")]
    UnknownAgent(AgentId),
    #[error("agente já existe: {0}")]
    AgentExists(AgentId),
    #[error("falha ao iniciar agente: {0}")]
    Spawn(String),
    #[error("git: {0}")]
    Git(String),
    #[error("navegador: {0}")]
    Browser(String),
    #[error("armazenamento: {0}")]
    Store(String),
    #[error("configuração: {0}")]
    Config(String),
    #[error("E/S: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
