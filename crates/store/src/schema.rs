//! Schema do SQLite e migrações por `PRAGMA user_version`.

use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};
use tracing::warn;

use crate::error::{DbError, DbResult};

/// A posição `i` leva o banco da versão `i` para `i + 1`. Só acrescente no
/// fim: uma migração já publicada nunca muda.
const MIGRATIONS: &[&str] = &[V1];

/// Versão do schema que este código entende.
pub(crate) const VERSION: i64 = MIGRATIONS.len() as i64;

/// v1: sessões, agentes, log de status e log de saída.
///
/// Ids são o UUID em texto; datas, `Timestamp` (ms). Os `*_json` guardam os
/// tipos do core inteiros; as colunas soltas (`name`, `kind`...) servem só
/// para consulta. `agents.removed_at` marca o `Removed` sem apagar nada.
const V1: &str = "
CREATE TABLE sessions (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    config_json TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    closed_at   INTEGER
);
CREATE INDEX sessions_by_created ON sessions(created_at);

CREATE TABLE agents (
    id          TEXT PRIMARY KEY NOT NULL,
    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL,
    role        TEXT NOT NULL,
    parent_id   TEXT,
    config_json TEXT NOT NULL,
    info_json   TEXT,
    status_json TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    removed_at  INTEGER
);
CREATE INDEX agents_by_session ON agents(session_id, created_at);

CREATE TABLE status_log (
    agent_id    TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    at          INTEGER NOT NULL,
    status_json TEXT NOT NULL
);
CREATE INDEX status_log_by_agent ON status_log(agent_id, at);

CREATE TABLE output_chunks (
    id        INTEGER PRIMARY KEY,
    agent_id  TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    first_seq INTEGER NOT NULL,
    at        INTEGER NOT NULL,
    data      BLOB NOT NULL
);
CREATE INDEX output_chunks_by_agent ON output_chunks(agent_id, id);
";

/// Pragmas de cada conexão. Fora de transação: `journal_mode` não muda
/// dentro de uma.
pub(crate) fn configure(conn: &Connection) -> DbResult<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    let mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "wal", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        warn!(%mode, "SQLite não aceitou WAL");
    }
    // Com WAL, NORMAL não perde integridade; só os últimos commits num crash do SO.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

/// Aplica as migrações que faltam, numa transação `IMMEDIATE` (dois processos
/// abrindo ao mesmo tempo não migram duas vezes). Idempotente.
pub(crate) fn migrate(conn: &mut Connection) -> DbResult<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current > VERSION {
        return Err(DbError::Schema(format!(
            "o banco está na versão {current} do schema, mais nova que a {VERSION} deste orquestra"
        )));
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current.max(0) as usize) {
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
    }
    tx.commit()?;
    Ok(())
}
