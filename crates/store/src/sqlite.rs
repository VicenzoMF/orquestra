//! `SqliteStore`: o `Store` do orquestra sobre SQLite.
//!
//! - Eventos de sessão/agente/status viram um UPDATE/INSERT na hora.
//! - `Output` vai para um buffer em memória; uma thread grava o buffer numa
//!   transação a cada `flush_interval`, e o próprio `record` grava quando ele
//!   passa de `flush_bytes`. Cada agente guarda até `retention_bytes`.
//! - `Removed` não apaga nada: marca `removed_at` e o agente segue no histórico.
//! - `Shutdown` grava o buffer, fecha as sessões criadas por este processo e
//!   marca como `Done { exit_code: None }` os agentes delas ainda vivos.
//!
//! Travas: sempre `db` antes de `buffer`. Quem grava o buffer segura `db`
//! do esvaziamento ao commit, então `output_log` nunca vê o buraco.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use directories::ProjectDirs;
use orquestra_core::{
    AgentConfig, AgentEvent, AgentId, AgentInfo, AgentRecord, AgentStatus, AppConfig, Error,
    History, Result, SessionConfig, SessionId, SessionRecord, Store, Timestamp,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use tracing::{debug, warn};

use crate::config::{load_config_file, save_config_file};
use crate::error::DbResult;
use crate::output::{Chunk, OutputBuffer};
use crate::schema;

/// Ajustes do log de saída. O default segue a arquitetura.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreOptions {
    /// Intervalo máximo entre gravações do buffer de saída.
    pub flush_interval: Duration,
    /// Bytes em buffer (somando todos os agentes) que forçam a gravação.
    pub flush_bytes: usize,
    /// Saída guardada por agente; acima disso os chunks mais antigos saem.
    pub retention_bytes: usize,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            flush_interval: Duration::from_millis(250),
            flush_bytes: 64 << 10,
            retention_bytes: 8 << 20,
        }
    }
}

/// `Store` sobre SQLite (WAL) e `config.toml`. `Send + Sync`: uma conexão
/// atrás de um `Mutex`.
pub struct SqliteStore {
    shared: Arc<Shared>,
    db_path: PathBuf,
    config_path: PathBuf,
    /// Serializa `save_config`.
    config_lock: Mutex<()>,
    flusher: Option<JoinHandle<()>>,
}

impl SqliteStore {
    /// `(banco, config)` padrão: `<data_dir>/orquestra.db` e
    /// `<config_dir>/config.toml`. `None` se não há diretório home.
    pub fn default_paths() -> Option<(PathBuf, PathBuf)> {
        let dirs = ProjectDirs::from("", "", "orquestra")?;
        Some((
            dirs.data_dir().join("orquestra.db"),
            dirs.config_dir().join("config.toml"),
        ))
    }

    /// Abre nos caminhos de `default_paths`.
    pub fn open_default() -> Result<Self> {
        let (db, config) = Self::default_paths()
            .ok_or_else(|| Error::Store("diretório home não encontrado".to_owned()))?;
        Self::open(&db, &config)
    }

    /// Abre (ou cria) o banco e aplica as migrações. A config só é lida em
    /// `load_config`.
    pub fn open(db_path: &Path, config_path: &Path) -> Result<Self> {
        Self::open_with(db_path, config_path, StoreOptions::default())
    }

    pub fn open_with(db_path: &Path, config_path: &Path, options: StoreOptions) -> Result<Self> {
        if let Some(dir) = db_path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(|err| {
                Error::Store(format!("não foi possível criar {}: {err}", dir.display()))
            })?;
        }
        let existed = db_path.exists();
        let mut conn = Connection::open(db_path).map_err(|err| {
            Error::Store(format!(
                "não foi possível abrir {}: {err}",
                db_path.display()
            ))
        })?;
        if !existed {
            restrict_permissions(db_path);
        }
        schema::configure(&conn)?;
        schema::migrate(&mut conn)?;

        let options = StoreOptions {
            flush_interval: options.flush_interval.max(Duration::from_millis(1)),
            ..options
        };
        let shared = Arc::new(Shared {
            db: Mutex::new(Db {
                conn,
                own_sessions: HashSet::new(),
                stored: HashMap::new(),
            }),
            buffer: Mutex::new(OutputBuffer::default()),
            options,
            stopping: Mutex::new(false),
            wake: Condvar::new(),
        });
        let flusher = std::thread::Builder::new()
            .name("orquestra-store".to_owned())
            .spawn({
                let shared = shared.clone();
                move || shared.flush_loop()
            })?;
        Ok(Self {
            shared,
            db_path: db_path.to_owned(),
            config_path: config_path.to_owned(),
            config_lock: Mutex::new(()),
            flusher: Some(flusher),
        })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Fecha sessões que ficaram abertas de uma execução anterior que não
    /// chegou ao `Shutdown` (crash, kill -9). Os agentes vivos delas viram
    /// `Failed`. Ignora as sessões deste processo. Chame na abertura do app,
    /// antes do hub; devolve quantas sessões fechou.
    ///
    /// Não é automático no `open`: outro processo pode estar usando o banco.
    pub fn close_interrupted(&self) -> Result<usize> {
        Ok(self.shared.db().close_interrupted()?)
    }
}

impl Store for SqliteStore {
    fn record(&self, event: &AgentEvent) -> Result<()> {
        let now = Timestamp::now();
        let shared = &*self.shared;
        let result = match event {
            AgentEvent::Output { id, seq, data } => {
                return shared.push_output(*id, *seq, data, now);
            }
            AgentEvent::SessionCreated { id, config } => {
                shared.db().session_created(*id, config, now)
            }
            AgentEvent::SessionClosed { id } => shared.db().session_closed(*id, now),
            AgentEvent::Created { id, config } => shared.db().agent_created(*id, config, now),
            AgentEvent::Spawned { info } => shared.db().agent_spawned(info, now),
            AgentEvent::StatusChanged { id, status } => {
                shared.db().status_changed(*id, status, now)
            }
            AgentEvent::Removed { id } => shared.db().agent_removed(*id, now),
            AgentEvent::Shutdown => shared.shutdown(now),
            AgentEvent::Replay { .. }
            | AgentEvent::BrowserStatus { .. }
            | AgentEvent::BrowserFrame { .. }
            | AgentEvent::BrowserNavigated { .. }
            | AgentEvent::History(_)
            | AgentEvent::Error { .. } => Ok(()),
        };
        result.map_err(|err| Error::Store(format!("{}: {err}", event_name(event))))
    }

    fn history(&self, limit: usize) -> Result<History> {
        Ok(self.shared.db().history(limit)?)
    }

    fn output_log(&self, id: AgentId, max_bytes: usize) -> Result<Vec<u8>> {
        Ok(self.shared.output_log(id, max_bytes)?)
    }

    fn load_config(&self) -> Result<AppConfig> {
        load_config_file(&self.config_path)
    }

    fn save_config(&self, config: &AppConfig) -> Result<()> {
        let _guard = self
            .config_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        save_config_file(&self.config_path, config)
    }

    fn flush(&self) -> Result<()> {
        Ok(self.shared.flush()?)
    }
}

impl Drop for SqliteStore {
    /// Para a thread de gravação, que grava o buffer antes de sair.
    fn drop(&mut self) {
        *self
            .shared
            .stopping
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        self.shared.wake.notify_all();
        if let Some(flusher) = self.flusher.take() {
            let _ = flusher.join();
        }
    }
}

struct Shared {
    db: Mutex<Db>,
    buffer: Mutex<OutputBuffer>,
    options: StoreOptions,
    stopping: Mutex<bool>,
    wake: Condvar,
}

impl Shared {
    fn db(&self) -> MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn buffer(&self) -> MutexGuard<'_, OutputBuffer> {
        self.buffer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Caminho quente: só toca o buffer, salvo quando ele enche.
    fn push_output(&self, id: AgentId, seq: u64, data: &[u8], now: Timestamp) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let buffered = self
            .buffer()
            .push(id, seq, data, now, self.options.retention_bytes);
        if buffered >= self.options.flush_bytes {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&self) -> DbResult<()> {
        let mut db = self.db();
        self.flush_locked(&mut db)
    }

    fn flush_locked(&self, db: &mut Db) -> DbResult<()> {
        let chunks = self.buffer().take();
        if chunks.is_empty() {
            return Ok(());
        }
        db.write_output(chunks, self.options.retention_bytes as u64)
    }

    fn shutdown(&self, now: Timestamp) -> DbResult<()> {
        let mut db = self.db();
        let flushed = self.flush_locked(&mut db);
        db.shutdown(now)?;
        flushed
    }

    fn output_log(&self, id: AgentId, max_bytes: usize) -> DbResult<Vec<u8>> {
        if max_bytes == 0 {
            return Ok(Vec::new());
        }
        // `db` primeiro: enquanto ela está presa, nada sai do buffer para o banco.
        let db = self.db();
        let tail = self.buffer().tail(id, max_bytes).to_vec();
        let mut missing = max_bytes - tail.len();
        let mut parts = Vec::new();
        if missing > 0 {
            let mut stmt = db.conn.prepare_cached(
                "SELECT data FROM output_chunks WHERE agent_id = ?1 ORDER BY id DESC",
            )?;
            let mut rows = stmt.query([id.to_string()])?;
            while missing > 0 {
                let Some(row) = rows.next()? else { break };
                let mut data: Vec<u8> = row.get(0)?;
                if data.len() > missing {
                    data.drain(..data.len() - missing);
                }
                missing -= data.len();
                parts.push(data);
            }
        }
        let mut out = Vec::with_capacity(max_bytes - missing);
        for part in parts.iter().rev() {
            out.extend_from_slice(part);
        }
        out.extend_from_slice(&tail);
        Ok(out)
    }

    /// Thread de gravação: acorda a cada `flush_interval` e grava o que houver.
    fn flush_loop(&self) {
        let mut stopping = self.stopping.lock().unwrap_or_else(PoisonError::into_inner);
        while !*stopping {
            stopping = self
                .wake
                .wait_timeout(stopping, self.options.flush_interval)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            if *stopping {
                break;
            }
            drop(stopping);
            // Solta a trava do buffer antes do flush, que a pega de novo.
            let pending = !self.buffer().is_empty();
            if pending && let Err(err) = self.flush() {
                warn!(%err, "gravação do log de saída falhou");
            }
            stopping = self.stopping.lock().unwrap_or_else(PoisonError::into_inner);
        }
        drop(stopping);
        if let Err(err) = self.flush() {
            warn!(%err, "gravação final do log de saída falhou");
        }
    }
}

struct Db {
    conn: Connection,
    /// Sessões criadas por este processo: só elas fecham no `Shutdown`.
    own_sessions: HashSet<SessionId>,
    /// Bytes de saída no banco por agente (cache do `SUM`).
    stored: HashMap<AgentId, u64>,
}

impl Db {
    fn session_created(
        &mut self,
        id: SessionId,
        config: &SessionConfig,
        now: Timestamp,
    ) -> DbResult<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO sessions (id, name, config_json, created_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                    name = excluded.name, config_json = excluded.config_json, closed_at = NULL",
            )?
            .execute(params![id.to_string(), config.name, json(config)?, now.0])?;
        self.own_sessions.insert(id);
        Ok(())
    }

    fn session_closed(&mut self, id: SessionId, now: Timestamp) -> DbResult<()> {
        self.conn
            .prepare_cached(
                "UPDATE sessions SET closed_at = ?2 WHERE id = ?1 AND closed_at IS NULL",
            )?
            .execute(params![id.to_string(), now.0])?;
        Ok(())
    }

    fn agent_created(&mut self, id: AgentId, config: &AgentConfig, now: Timestamp) -> DbResult<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO agents (id, session_id, name, kind, role, parent_id, config_json,
                                     created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                    session_id = excluded.session_id, name = excluded.name,
                    kind = excluded.kind, role = excluded.role, parent_id = excluded.parent_id,
                    config_json = excluded.config_json, updated_at = excluded.updated_at,
                    removed_at = NULL",
            )?
            .execute(params![
                id.to_string(),
                config.session.to_string(),
                config.name,
                tag(&config.kind)?,
                tag(&config.role)?,
                config.parent.map(|p| p.to_string()),
                json(config)?,
                now.0,
            ])?;
        Ok(())
    }

    fn agent_spawned(&mut self, info: &AgentInfo, now: Timestamp) -> DbResult<()> {
        let changed = self
            .conn
            .prepare_cached("UPDATE agents SET info_json = ?2, updated_at = ?3 WHERE id = ?1")?
            .execute(params![info.id.to_string(), json(info)?, now.0])?;
        if changed == 0 {
            debug!(agent = %info.id, "Spawned de agente desconhecido ignorado");
        }
        Ok(())
    }

    fn status_changed(
        &mut self,
        id: AgentId,
        status: &AgentStatus,
        now: Timestamp,
    ) -> DbResult<()> {
        let key = id.to_string();
        let status_json = json(status)?;
        let tx = self.conn.transaction()?;
        if set_status(&tx, &key, &status_json, now)? {
            tx.prepare_cached(
                "INSERT INTO status_log (agent_id, at, status_json) VALUES (?1, ?2, ?3)",
            )?
            .execute(params![key, now.0, status_json])?;
        } else {
            debug!(agent = %id, "status de agente desconhecido ignorado");
        }
        tx.commit()?;
        Ok(())
    }

    /// Mantém o agente e os logs. Se ele ainda constava vivo, fecha com
    /// `Done { exit_code: None }` (removido a pedido); um status final que
    /// chegue depois sobrescreve.
    fn agent_removed(&mut self, id: AgentId, now: Timestamp) -> DbResult<()> {
        let key = id.to_string();
        let tx = self.conn.transaction()?;
        let changed = tx
            .prepare_cached("UPDATE agents SET removed_at = ?2, updated_at = ?2 WHERE id = ?1")?
            .execute(params![key, now.0])?;
        if changed > 0 {
            let done = json(&AgentStatus::Done { exit_code: None })?;
            finish_if_alive(&tx, &key, &done, Some(now))?;
        }
        tx.commit()?;
        Ok(())
    }

    fn shutdown(&mut self, now: Timestamp) -> DbResult<()> {
        let done = json(&AgentStatus::Done { exit_code: None })?;
        let tx = self.conn.transaction()?;
        for session in &self.own_sessions {
            let key = session.to_string();
            tx.prepare_cached(
                "UPDATE sessions SET closed_at = ?2 WHERE id = ?1 AND closed_at IS NULL",
            )?
            .execute(params![key, now.0])?;
            for agent in session_agents(&tx, &key)? {
                finish_if_alive(&tx, &agent, &done, Some(now))?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn close_interrupted(&mut self) -> DbResult<usize> {
        let failed = json(&AgentStatus::Failed {
            reason: "interrompido: o orquestra fechou sem encerrar o agente".to_owned(),
        })?;
        let tx = self.conn.transaction()?;
        let open: Vec<(String, i64)> = tx
            .prepare("SELECT id, created_at FROM sessions WHERE closed_at IS NULL")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut closed = 0;
        for (key, created_at) in open {
            if key
                .parse::<SessionId>()
                .is_ok_and(|id| self.own_sessions.contains(&id))
            {
                continue;
            }
            // Fecha na última atividade conhecida, não agora.
            let last: Option<i64> = tx.query_row(
                "SELECT MAX(updated_at) FROM agents WHERE session_id = ?1",
                [&key],
                |row| row.get(0),
            )?;
            for agent in session_agents(&tx, &key)? {
                finish_if_alive(&tx, &agent, &failed, None)?;
            }
            tx.execute(
                "UPDATE sessions SET closed_at = ?2 WHERE id = ?1",
                params![key, last.unwrap_or(created_at).max(created_at)],
            )?;
            closed += 1;
        }
        tx.commit()?;
        Ok(closed)
    }

    /// Uma transação para o lote todo; saída de agente desconhecido é
    /// descartada. Depois aplica o teto por agente.
    fn write_output(&mut self, chunks: Vec<Chunk>, cap: u64) -> DbResult<()> {
        let Self { conn, stored, .. } = self;
        let tx = conn.transaction()?;
        let mut totals = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let key = chunk.agent.to_string();
            let inserted = tx
                .prepare_cached(
                    "INSERT INTO output_chunks (agent_id, first_seq, at, data)
                     SELECT ?1, ?2, ?3, ?4 WHERE EXISTS (SELECT 1 FROM agents WHERE id = ?1)",
                )?
                .execute(params![
                    key,
                    i64::try_from(chunk.first_seq).unwrap_or(i64::MAX),
                    chunk.at.0,
                    chunk.data,
                ])?;
            if inserted == 0 {
                debug!(agent = %chunk.agent, bytes = chunk.data.len(), "saída de agente desconhecido descartada");
                continue;
            }
            let mut total = match stored.get(&chunk.agent) {
                Some(before) => before + chunk.data.len() as u64,
                None => stored_bytes(&tx, &key)?,
            };
            if total > cap {
                total = prune(&tx, &key, total, cap)?;
            }
            totals.push((chunk.agent, total));
        }
        tx.commit()?;
        // Só depois do commit: num rollback o cache seguiria certo.
        stored.extend(totals);
        Ok(())
    }

    fn history(&self, limit: usize) -> DbResult<History> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut sessions = Vec::new();
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, config_json, created_at, closed_at FROM sessions
             ORDER BY created_at DESC, rowid DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        for row in rows {
            let (id, config, created_at, closed_at): (String, String, i64, Option<i64>) = row?;
            match decode_session(&id, &config, created_at, closed_at) {
                Ok(session) => sessions.push(session),
                Err(err) => warn!(session = %id, %err, "sessão ilegível no histórico; pulando"),
            }
        }

        let mut agents = Vec::new();
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, config_json, info_json, status_json, created_at, updated_at FROM agents
             WHERE session_id = ?1 ORDER BY created_at, rowid",
        )?;
        for session in &sessions {
            let rows = stmt.query_map([session.id.to_string()], |row| {
                Ok(AgentRow {
                    id: row.get(0)?,
                    config: row.get(1)?,
                    info: row.get(2)?,
                    status: row.get(3)?,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                })
            })?;
            for row in rows {
                let row = row?;
                match row.decode() {
                    Ok(agent) => agents.push(agent),
                    Err(err) => {
                        warn!(agent = %row.id, %err, "agente ilegível no histórico; pulando")
                    }
                }
            }
        }
        Ok(History { sessions, agents })
    }
}

/// Atualiza o status atual; `false` se o agente não existe.
fn set_status(
    tx: &Transaction<'_>,
    agent: &str,
    status_json: &str,
    now: Timestamp,
) -> DbResult<bool> {
    let changed = tx
        .prepare_cached("UPDATE agents SET status_json = ?2, updated_at = ?3 WHERE id = ?1")?
        .execute(params![agent, status_json, now.0])?;
    Ok(changed > 0)
}

/// Troca o status por `final_json` se o atual ainda é vivo (ou ausente).
/// `at = None` mantém `updated_at`. Não entra no `status_log`, que só guarda
/// o que de fato foi publicado.
fn finish_if_alive(
    tx: &Transaction<'_>,
    agent: &str,
    final_json: &str,
    at: Option<Timestamp>,
) -> DbResult<()> {
    let current: Option<String> = tx
        .prepare_cached("SELECT status_json FROM agents WHERE id = ?1")?
        .query_row([agent], |row| row.get(0))
        .optional()?
        .flatten();
    let alive = match current {
        None => true,
        // Status que esta versão não entende fica como está.
        Some(json) => serde_json::from_str::<AgentStatus>(&json).is_ok_and(|s| s.is_alive()),
    };
    if !alive {
        return Ok(());
    }
    match at {
        Some(now) => {
            set_status(tx, agent, final_json, now)?;
        }
        None => {
            tx.prepare_cached("UPDATE agents SET status_json = ?2 WHERE id = ?1")?
                .execute(params![agent, final_json])?;
        }
    }
    Ok(())
}

fn session_agents(tx: &Transaction<'_>, session: &str) -> DbResult<Vec<String>> {
    Ok(tx
        .prepare_cached("SELECT id FROM agents WHERE session_id = ?1")?
        .query_map([session], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn stored_bytes(tx: &Transaction<'_>, agent: &str) -> DbResult<u64> {
    let total: i64 = tx
        .prepare_cached(
            "SELECT COALESCE(SUM(length(data)), 0) FROM output_chunks WHERE agent_id = ?1",
        )?
        .query_row([agent], |row| row.get(0))?;
    Ok(total.max(0) as u64)
}

/// Apaga os chunks mais antigos até caber em `cap`; devolve o que sobrou.
fn prune(tx: &Transaction<'_>, agent: &str, total: u64, cap: u64) -> DbResult<u64> {
    let mut remaining = total;
    let mut cut = None;
    {
        let mut stmt = tx.prepare_cached(
            "SELECT id, length(data) FROM output_chunks WHERE agent_id = ?1 ORDER BY id",
        )?;
        let mut rows = stmt.query([agent])?;
        while remaining > cap {
            let Some(row) = rows.next()? else { break };
            let len: i64 = row.get(1)?;
            remaining = remaining.saturating_sub(len.max(0) as u64);
            cut = Some(row.get::<_, i64>(0)?);
        }
    }
    if let Some(cut) = cut {
        tx.prepare_cached("DELETE FROM output_chunks WHERE agent_id = ?1 AND id <= ?2")?
            .execute(params![agent, cut])?;
    }
    Ok(remaining)
}

struct AgentRow {
    id: String,
    config: String,
    info: Option<String>,
    status: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl AgentRow {
    fn decode(&self) -> std::result::Result<AgentRecord, String> {
        let id = self.id.parse::<AgentId>().map_err(|e| format!("id: {e}"))?;
        let config = serde_json::from_str(&self.config).map_err(|e| format!("config: {e}"))?;
        let info = self
            .info
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| format!("info: {e}"))?;
        // Sem status gravado (o primeiro se perdeu): o spawn tinha sido aceito.
        let status = match self.status.as_deref() {
            Some(json) => serde_json::from_str(json).map_err(|e| format!("status: {e}"))?,
            None => AgentStatus::Starting,
        };
        Ok(AgentRecord {
            id,
            config,
            info,
            status,
            created_at: Timestamp(self.created_at),
            updated_at: Timestamp(self.updated_at),
        })
    }
}

fn decode_session(
    id: &str,
    config: &str,
    created_at: i64,
    closed_at: Option<i64>,
) -> std::result::Result<SessionRecord, String> {
    Ok(SessionRecord {
        id: id.parse::<SessionId>().map_err(|e| format!("id: {e}"))?,
        config: serde_json::from_str(config).map_err(|e| format!("config: {e}"))?,
        created_at: Timestamp(created_at),
        closed_at: closed_at.map(Timestamp),
    })
}

fn json<T: Serialize + ?Sized>(value: &T) -> DbResult<String> {
    Ok(serde_json::to_string(value)?)
}

/// Nome serde de um enum unitário (`claude_code`, `worker`).
fn tag<T: Serialize>(value: &T) -> DbResult<String> {
    Ok(match serde_json::to_value(value)? {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    })
}

fn event_name(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::SessionCreated { .. } => "SessionCreated",
        AgentEvent::SessionClosed { .. } => "SessionClosed",
        AgentEvent::Created { .. } => "Created",
        AgentEvent::Spawned { .. } => "Spawned",
        AgentEvent::StatusChanged { .. } => "StatusChanged",
        AgentEvent::Removed { .. } => "Removed",
        AgentEvent::Shutdown => "Shutdown",
        _ => "evento",
    }
}

/// O log de saída pode ter segredos: banco novo só para o dono. O SQLite
/// cria `-wal`/`-shm` com a mesma permissão.
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        warn!(%err, path = %path.display(), "não foi possível restringir a permissão do banco");
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}
