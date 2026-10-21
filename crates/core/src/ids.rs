use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// UUIDv7: ordenável pela hora de criação.
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }

            /// 8 dígitos hex para nomes de branch, diretórios e logs.
            ///
            /// Vem do fim do UUID (parte aleatória): o começo de um v7 é o
            /// timestamp e colidiria entre ids criados no mesmo minuto.
            pub fn short(&self) -> String {
                let hex = self.0.simple().to_string();
                hex[hex.len() - 8..].to_owned()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.short())
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self)
            }
        }
    };
}

uuid_id!(
    /// Identificador estável de um agente.
    ///
    /// Quem pede o spawn (UI ou MCP) gera o id, para correlacionar eventos
    /// desde o primeiro instante, antes de o processo existir.
    AgentId
);

uuid_id!(
    /// Identificador de uma orquestração (sessão): um grupo no canvas com no
    /// máximo um orquestrador e os agentes que pertencem a ele.
    SessionId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ids_of_consecutive_agents_differ() {
        let a = AgentId::generate();
        let b = AgentId::generate();
        assert_ne!(a.short(), b.short());
        assert_eq!(a.short().len(), 8);
    }

    #[test]
    fn display_round_trips() {
        let id = SessionId::generate();
        assert_eq!(id.to_string().parse::<SessionId>().unwrap(), id);
        assert!(format!("{id:?}").starts_with("SessionId("));
    }
}
