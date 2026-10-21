//! Erro interno do SQLite, convertido para `orquestra_core::Error` na
//! fronteira das traits.

use std::fmt;

use orquestra_core::Error;

pub(crate) type DbResult<T> = std::result::Result<T, DbError>;

#[derive(Debug)]
pub(crate) enum DbError {
    Sql(rusqlite::Error),
    Json(serde_json::Error),
    /// Banco criado por uma versão mais nova do app.
    Schema(String),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(err) => write!(f, "SQLite: {err}"),
            Self::Json(err) => write!(f, "JSON: {err}"),
            Self::Schema(msg) => f.write_str(msg),
        }
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sql(err)
    }
}

impl From<serde_json::Error> for DbError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

impl From<DbError> for Error {
    fn from(err: DbError) -> Self {
        Error::Store(err.to_string())
    }
}
