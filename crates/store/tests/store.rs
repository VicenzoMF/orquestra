//! `SqliteStore` de ponta a ponta, sempre num diretório temporário.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use orquestra_core::bytes::Bytes;
use orquestra_core::{
    AgentConfig, AgentEvent, AgentId, AgentInfo, AgentKind, AgentRole, AgentStatus, AppConfig,
    BrowserEndpoint, Error, QueueReason, SessionConfig, SessionId, Store, TermSize, Timestamp,
    Workspace, WorktreeInfo,
};
use orquestra_store::{SqliteStore, StoreOptions};

struct Env {
    _dir: tempfile::TempDir,
    db: PathBuf,
    config: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("data").join("orquestra.db");
        let config = dir.path().join("config").join("config.toml");
        Self {
            _dir: dir,
            db,
            config,
        }
    }

    fn open(&self) -> SqliteStore {
        SqliteStore::open(&self.db, &self.config).unwrap()
    }

    fn open_with(&self, options: StoreOptions) -> SqliteStore {
        SqliteStore::open_with(&self.db, &self.config, options).unwrap()
    }

    /// Conexão crua, para olhar o que de fato está no banco.
    fn raw(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.db).unwrap()
    }

    fn count(&self, sql: &str) -> i64 {
        self.raw().query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn chunks(&self, agent: AgentId) -> i64 {
        self.raw()
            .query_row(
                "SELECT COUNT(*) FROM output_chunks WHERE agent_id = ?1",
                [agent.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn stored_bytes(&self, agent: AgentId) -> i64 {
        self.raw()
            .query_row(
                "SELECT COALESCE(SUM(length(data)), 0) FROM output_chunks WHERE agent_id = ?1",
                [agent.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    }
}

/// Sem gravação por tempo nem por tamanho: só `flush`/`Shutdown` gravam.
fn manual() -> StoreOptions {
    StoreOptions {
        flush_interval: Duration::from_secs(3600),
        flush_bytes: usize::MAX,
        ..StoreOptions::default()
    }
}

fn session_config(name: &str) -> SessionConfig {
    SessionConfig::new(name, "/src/loja", "login com e-mail")
}

fn agent_config(
    session: SessionId,
    name: &str,
    role: AgentRole,
    parent: Option<AgentId>,
) -> AgentConfig {
    AgentConfig {
        name: name.to_owned(),
        kind: AgentKind::ClaudeCode,
        program: "claude".to_owned(),
        args: vec!["--verbose".to_owned()],
        env: BTreeMap::from([("CHAVE".to_owned(), "valor".to_owned())]),
        session,
        role,
        parent,
        workspace: Workspace::Worktree {
            repo: "/src/loja".into(),
            base: Some("main".to_owned()),
            branch: None,
        },
        initial_prompt: Some("faça o login".to_owned()),
        browser: true,
        size: TermSize::default(),
    }
}

fn agent_info(id: AgentId, config: &AgentConfig) -> AgentInfo {
    AgentInfo {
        id,
        config: config.clone(),
        workdir: "/wt/loja/abc".into(),
        worktree: Some(WorktreeInfo {
            repo: "/src/loja".into(),
            path: "/wt/loja/abc".into(),
            branch: "orquestra/login-abc".to_owned(),
            base_ref: "main".to_owned(),
            base_commit: "0123456789abcdef".to_owned(),
        }),
        pid: Some(4242),
        browser: Some(BrowserEndpoint {
            port: 9222,
            ws_url: "ws://127.0.0.1:9222/devtools/browser/x".to_owned(),
        }),
        started_at: Timestamp(1_700_000_000_000),
    }
}

fn session_created(store: &SqliteStore, name: &str) -> SessionId {
    let id = SessionId::generate();
    store
        .record(&AgentEvent::SessionCreated {
            id,
            config: Arc::new(session_config(name)),
        })
        .unwrap();
    id
}

fn agent_created(store: &SqliteStore, config: &AgentConfig) -> AgentId {
    let id = AgentId::generate();
    store
        .record(&AgentEvent::Created {
            id,
            config: Arc::new(config.clone()),
        })
        .unwrap();
    id
}

fn status(store: &SqliteStore, id: AgentId, status: AgentStatus) {
    store
        .record(&AgentEvent::StatusChanged { id, status })
        .unwrap();
}

fn output(store: &SqliteStore, id: AgentId, seq: u64, data: &[u8]) {
    store
        .record(&AgentEvent::Output {
            id,
            seq,
            data: Bytes::copy_from_slice(data),
        })
        .unwrap();
}

/// Espera algo que a thread de gravação faz sozinha.
fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "não aconteceu: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn full_flow_round_trips_through_history() {
    let env = Env::new();
    let store = env.open_with(manual());

    let session = session_created(&store, "loja · login");
    let orch_cfg = agent_config(session, "orquestrador", AgentRole::Orchestrator, None);
    let orch = agent_created(&store, &orch_cfg);
    status(&store, orch, AgentStatus::Starting);
    let orch_info = agent_info(orch, &orch_cfg);
    store
        .record(&AgentEvent::Spawned {
            info: Arc::new(orch_info.clone()),
        })
        .unwrap();
    status(&store, orch, AgentStatus::Running);

    let worker_cfg = agent_config(session, "login-ui", AgentRole::Worker, Some(orch));
    let worker = agent_created(&store, &worker_cfg);
    status(
        &store,
        worker,
        AgentStatus::Queued {
            reason: QueueReason::Approval,
        },
    );
    status(&store, worker, AgentStatus::Starting);
    status(&store, worker, AgentStatus::Running);
    output(&store, worker, 0, b"ola ");
    output(&store, worker, 1, b"mundo\r\n");
    output(&store, orch, 0, b"plano");
    status(&store, worker, AgentStatus::WaitingForInput);
    status(&store, worker, AgentStatus::Done { exit_code: Some(0) });

    // Eventos que a store ignora.
    store
        .record(&AgentEvent::Error {
            id: Some(worker),
            message: "x".to_owned(),
        })
        .unwrap();
    store
        .record(&AgentEvent::BrowserNavigated {
            id: worker,
            url: "http://localhost".to_owned(),
            title: "t".to_owned(),
        })
        .unwrap();

    store.record(&AgentEvent::Shutdown).unwrap();
    store.flush().unwrap();
    drop(store);

    // Tudo volta numa store nova sobre o mesmo arquivo.
    let store = env.open();
    let history = store.history(10).unwrap();
    assert_eq!(history.sessions.len(), 1);
    let s = &history.sessions[0];
    assert_eq!(s.id, session);
    assert_eq!(s.config, session_config("loja · login"));
    assert_eq!(s.closed_at, None, "o Shutdown deixa a sessão para reabrir");

    assert_eq!(history.agents.len(), 2);
    let (o, w) = (&history.agents[0], &history.agents[1]);
    assert_eq!(o.id, orch);
    assert_eq!(o.config, orch_cfg);
    assert_eq!(o.info.as_ref(), Some(&orch_info));
    // Ainda rodando no Shutdown: fica assim, para a próxima abertura reabrir.
    assert_eq!(o.status, AgentStatus::Running);
    assert_eq!(w.id, worker);
    assert_eq!(w.config, worker_cfg);
    assert_eq!(w.config.parent, Some(orch));
    assert_eq!(w.info, None);
    assert_eq!(w.status, AgentStatus::Done { exit_code: Some(0) });
    assert!(w.created_at <= w.updated_at);

    assert_eq!(store.output_log(worker, 1 << 20).unwrap(), b"ola mundo\r\n");
    assert_eq!(store.output_log(orch, 1 << 20).unwrap(), b"plano");
    assert_eq!(store.output_log(worker, 6).unwrap(), b"undo\r\n");
    assert_eq!(store.output_log(worker, 0).unwrap(), b"");
    assert_eq!(
        store.output_log(AgentId::generate(), 100).unwrap(),
        b"",
        "agente sem saída"
    );

    // status_log guarda só o que foi publicado: 5 do worker, 2 do orquestrador.
    assert_eq!(env.count("SELECT COUNT(*) FROM status_log"), 7);
    let kinds: Vec<(String, String)> = env
        .raw()
        .prepare("SELECT kind, role FROM agents ORDER BY created_at, rowid")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        kinds,
        [
            ("claude_code".to_owned(), "orchestrator".to_owned()),
            ("claude_code".to_owned(), "worker".to_owned())
        ]
    );
}

#[test]
fn history_is_newest_first_and_limited() {
    let env = Env::new();
    let store = env.open();
    let mut sessions = Vec::new();
    for name in ["a", "b", "c"] {
        let id = session_created(&store, name);
        agent_created(&store, &agent_config(id, name, AgentRole::Worker, None));
        sessions.push(id);
    }
    let history = store.history(2).unwrap();
    let ids: Vec<_> = history.sessions.iter().map(|s| s.id).collect();
    assert_eq!(ids, [sessions[2], sessions[1]]);
    let names: Vec<_> = history
        .agents
        .iter()
        .map(|a| a.config.name.as_str())
        .collect();
    assert_eq!(names, ["c", "b"]);
    assert!(history.sessions.iter().all(|s| s.closed_at.is_none()));
    // Sem status gravado ainda.
    assert!(
        history
            .agents
            .iter()
            .all(|a| a.status == AgentStatus::Starting)
    );

    assert_eq!(store.history(0).unwrap(), Default::default());
    assert_eq!(store.history(usize::MAX).unwrap().sessions.len(), 3);
}

#[test]
fn session_closed_and_removed_keep_history() {
    let env = Env::new();
    let store = env.open();
    let session = session_created(&store, "s");
    let a = agent_created(&store, &agent_config(session, "a", AgentRole::Worker, None));
    let b = agent_created(&store, &agent_config(session, "b", AgentRole::Worker, None));
    status(&store, a, AgentStatus::Running);
    status(
        &store,
        b,
        AgentStatus::Failed {
            reason: "exit 1".to_owned(),
        },
    );
    output(&store, a, 0, b"saida de a");

    store
        .record(&AgentEvent::SessionClosed { id: session })
        .unwrap();
    store.record(&AgentEvent::Removed { id: a }).unwrap();
    store.record(&AgentEvent::Removed { id: b }).unwrap();

    let history = store.history(10).unwrap();
    assert!(history.sessions[0].closed_at.is_some());
    assert_eq!(history.agents.len(), 2);
    assert_eq!(
        history.agents[0].status,
        AgentStatus::Done { exit_code: None }
    );
    assert_eq!(
        history.agents[1].status,
        AgentStatus::Failed {
            reason: "exit 1".to_owned()
        },
        "status final não é sobrescrito"
    );
    assert_eq!(store.output_log(a, 100).unwrap(), b"saida de a");
    assert_eq!(
        env.count("SELECT COUNT(*) FROM agents WHERE removed_at IS NOT NULL"),
        2
    );

    // Um status final que chega depois do Removed ainda vale.
    status(&store, a, AgentStatus::Done { exit_code: Some(0) });
    let history = store.history(10).unwrap();
    assert_eq!(
        history.agents[0].status,
        AgentStatus::Done { exit_code: Some(0) }
    );
}

#[test]
fn output_is_buffered_and_written_in_one_batch() {
    let env = Env::new();
    let store = env.open_with(manual());
    let session = session_created(&store, "s");
    let a = agent_created(&store, &agent_config(session, "a", AgentRole::Worker, None));
    let b = agent_created(&store, &agent_config(session, "b", AgentRole::Worker, None));

    let mut expected = Vec::new();
    for seq in 0..100u64 {
        let line = format!("linha {seq}\n");
        output(&store, a, seq, line.as_bytes());
        expected.extend_from_slice(line.as_bytes());
    }
    output(&store, b, 0, b"b");
    assert_eq!(env.chunks(a), 0, "nada gravado antes do flush");
    // output_log enxerga o buffer.
    assert_eq!(store.output_log(a, usize::MAX).unwrap(), expected);
    assert_eq!(store.output_log(a, 9).unwrap(), b"linha 99\n");

    store.flush().unwrap();
    assert_eq!(env.chunks(a), 1, "um chunk por agente por lote");
    assert_eq!(env.chunks(b), 1);
    let first_seq: i64 = env
        .raw()
        .query_row(
            "SELECT first_seq FROM output_chunks WHERE agent_id = ?1",
            [a.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(first_seq, 0);
    assert_eq!(store.output_log(a, usize::MAX).unwrap(), expected);

    // Banco + buffer juntos, na ordem.
    output(&store, a, 100, b"fim");
    expected.extend_from_slice(b"fim");
    assert_eq!(store.output_log(a, usize::MAX).unwrap(), expected);
    assert_eq!(store.output_log(a, 13).unwrap(), b"\nlinha 99\nfim");
    store.flush().unwrap();
    assert_eq!(env.chunks(a), 2);
    assert_eq!(store.output_log(a, 13).unwrap(), b"\nlinha 99\nfim");
}

#[test]
fn full_buffer_is_written_without_flush() {
    let env = Env::new();
    let store = env.open_with(StoreOptions {
        flush_bytes: 1024,
        ..manual()
    });
    let session = session_created(&store, "s");
    let a = agent_created(&store, &agent_config(session, "a", AgentRole::Worker, None));
    for seq in 0..15u64 {
        output(&store, a, seq, &[b'x'; 64]);
    }
    assert_eq!(env.chunks(a), 0);
    output(&store, a, 15, &[b'x'; 64]);
    assert_eq!(env.chunks(a), 1, "16 × 64 = 1024 bytes dispara a gravação");
}

#[test]
fn timer_writes_buffer_without_new_events() {
    let env = Env::new();
    let store = env.open_with(StoreOptions {
        flush_interval: Duration::from_millis(30),
        flush_bytes: usize::MAX,
        ..StoreOptions::default()
    });
    let session = session_created(&store, "s");
    let a = agent_created(&store, &agent_config(session, "a", AgentRole::Worker, None));
    output(&store, a, 0, b"sozinho");
    eventually("thread grava o buffer", || env.chunks(a) == 1);
    assert_eq!(store.output_log(a, 100).unwrap(), b"sozinho");
}

#[test]
fn drop_writes_pending_output() {
    let env = Env::new();
    let store = env.open_with(manual());
    let session = session_created(&store, "s");
    let a = agent_created(&store, &agent_config(session, "a", AgentRole::Worker, None));
    output(&store, a, 0, b"ultimo");
    drop(store);
    assert_eq!(env.open().output_log(a, 100).unwrap(), b"ultimo");
}

#[test]
fn retention_drops_oldest_chunks() {
    let env = Env::new();
    let cap = 4096;
    let store = env.open_with(StoreOptions {
        flush_bytes: 512,
        retention_bytes: cap,
        ..manual()
    });
    let session = session_created(&store, "s");
    let a = agent_created(&store, &agent_config(session, "a", AgentRole::Worker, None));
    let other = agent_created(&store, &agent_config(session, "b", AgentRole::Worker, None));
    output(&store, other, 0, b"intocado");

    let mut stream = Vec::new();
    for seq in 0..200u64 {
        let data: Vec<u8> = (0..100).map(|i| ((seq * 100 + i) % 251) as u8).collect();
        output(&store, a, seq, &data);
        stream.extend_from_slice(&data);
    }
    store.flush().unwrap();

    let stored = env.stored_bytes(a);
    assert!(stored > 0 && stored <= cap as i64, "guardado: {stored}");
    let log = store.output_log(a, usize::MAX).unwrap();
    assert_eq!(log.len() as i64, stored);
    assert!(stream.ends_with(&log), "o log é o fim da saída");
    assert_eq!(store.output_log(other, 100).unwrap(), b"intocado");

    // Reabrindo, o teto continua valendo (o total vem do banco).
    drop(store);
    let store = env.open_with(StoreOptions {
        flush_bytes: 512,
        retention_bytes: cap,
        ..manual()
    });
    for seq in 200..260u64 {
        output(&store, a, seq, &[b'z'; 100]);
    }
    store.flush().unwrap();
    assert!(env.stored_bytes(a) <= cap as i64);
    assert!(
        store
            .output_log(a, usize::MAX)
            .unwrap()
            .ends_with(&[b'z'; 100])
    );
}

#[test]
fn single_huge_output_is_capped() {
    let env = Env::new();
    let store = env.open_with(StoreOptions {
        retention_bytes: 1000,
        ..manual()
    });
    let session = session_created(&store, "s");
    let a = agent_created(&store, &agent_config(session, "a", AgentRole::Worker, None));
    let data: Vec<u8> = (0..5000u32).map(|i| (i % 256) as u8).collect();
    output(&store, a, 0, &data);
    assert_eq!(store.output_log(a, usize::MAX).unwrap(), &data[4000..]);
    store.flush().unwrap();
    assert_eq!(store.output_log(a, usize::MAX).unwrap(), &data[4000..]);
    assert_eq!(env.stored_bytes(a), 1000);
}

#[test]
fn output_of_unknown_agent_does_not_spoil_the_batch() {
    let env = Env::new();
    let store = env.open_with(manual());
    let session = session_created(&store, "s");
    let a = agent_created(&store, &agent_config(session, "a", AgentRole::Worker, None));
    let ghost = AgentId::generate();
    output(&store, ghost, 0, b"fantasma");
    output(&store, a, 0, b"real");
    status(&store, ghost, AgentStatus::Running);
    store.record(&AgentEvent::Removed { id: ghost }).unwrap();
    store.flush().unwrap();
    assert_eq!(env.chunks(ghost), 0);
    assert_eq!(store.output_log(a, 100).unwrap(), b"real");
    assert_eq!(env.count("SELECT COUNT(*) FROM status_log"), 0);
}

#[test]
fn agent_of_unknown_session_is_an_error() {
    let env = Env::new();
    let store = env.open();
    let config = agent_config(SessionId::generate(), "a", AgentRole::Worker, None);
    let err = store
        .record(&AgentEvent::Created {
            id: AgentId::generate(),
            config: Arc::new(config),
        })
        .unwrap_err();
    assert!(
        matches!(&err, Error::Store(msg) if msg.starts_with("Created")),
        "{err}"
    );
}

#[test]
fn migrations_are_idempotent() {
    let env = Env::new();
    let store = env.open();
    let session = session_created(&store, "s");
    drop(store);
    let store = env.open();
    drop(store);
    let store = env.open();
    assert_eq!(store.history(10).unwrap().sessions[0].id, session);

    let raw = env.raw();
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 1);
    let mode: String = raw
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let tables: Vec<String> = raw
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        tables,
        ["agents", "output_chunks", "sessions", "status_log"]
    );
}

#[test]
fn newer_schema_is_rejected() {
    let env = Env::new();
    drop(env.open());
    env.raw().pragma_update(None, "user_version", 99).unwrap();
    let err = SqliteStore::open(&env.db, &env.config).err().unwrap();
    assert!(
        matches!(&err, Error::Store(msg) if msg.contains("versão 99")),
        "{err}"
    );
}

#[cfg(unix)]
#[test]
fn new_database_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    drop(env.open());
    let mode = std::fs::metadata(&env.db).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn close_interrupted_fixes_sessions_left_open_by_a_crash() {
    let env = Env::new();
    let (old, running, done) = {
        let store = env.open();
        let old = session_created(&store, "antiga");
        let running = agent_created(&store, &agent_config(old, "a", AgentRole::Worker, None));
        let done = agent_created(&store, &agent_config(old, "b", AgentRole::Worker, None));
        status(&store, running, AgentStatus::Running);
        status(&store, done, AgentStatus::Done { exit_code: Some(0) });
        (old, running, done)
        // drop sem Shutdown = crash
    };

    let store = env.open();
    let current = session_created(&store, "atual");
    assert_eq!(store.close_interrupted().unwrap(), 1);
    assert_eq!(store.close_interrupted().unwrap(), 0, "idempotente");

    let history = store.history(10).unwrap();
    let by_id = |id| history.sessions.iter().find(|s| s.id == id).unwrap();
    assert!(by_id(old).closed_at.is_some());
    assert!(
        by_id(current).closed_at.is_none(),
        "sessão deste processo fica"
    );
    let agent = |id| history.agents.iter().find(|a| a.id == id).unwrap();
    assert!(matches!(agent(running).status, AgentStatus::Failed { .. }));
    assert_eq!(agent(done).status, AgentStatus::Done { exit_code: Some(0) });
}

/// Fechar o app e cair dão no mesmo: o que estava aberto volta na próxima
/// abertura, com o último status de cada agente. Sessão encerrada e agente
/// removido não voltam.
#[test]
fn open_sessions_lists_what_to_reopen_after_shutdown_or_crash() {
    let env = Env::new();
    let (crashed, live) = {
        let store = env.open();
        let crashed = session_created(&store, "caiu");
        let live = agent_created(&store, &agent_config(crashed, "a", AgentRole::Worker, None));
        status(&store, live, AgentStatus::Running);
        (crashed, live)
        // drop sem Shutdown = queda
    };
    let (closed, kept, finished, removed) = {
        let store = env.open();
        let closed = session_created(&store, "encerrada");
        store
            .record(&AgentEvent::SessionClosed { id: closed })
            .unwrap();
        let kept = session_created(&store, "fechou o app");
        let finished = agent_created(&store, &agent_config(kept, "b", AgentRole::Worker, None));
        status(&store, finished, AgentStatus::Done { exit_code: Some(0) });
        let removed = agent_created(&store, &agent_config(kept, "c", AgentRole::Worker, None));
        store.record(&AgentEvent::Removed { id: removed }).unwrap();
        store.record(&AgentEvent::Shutdown).unwrap();
        (closed, kept, finished, removed)
    };

    let store = env.open();
    let reopen = store.open_sessions().unwrap();
    let sessions: Vec<_> = reopen.sessions.iter().map(|s| s.id).collect();
    assert_eq!(sessions, [crashed, kept], "em ordem de criação");
    assert!(!sessions.contains(&closed));
    let agents: Vec<_> = reopen
        .agents
        .iter()
        .map(|a| (a.id, a.status.clone()))
        .collect();
    assert_eq!(
        agents,
        [
            (live, AgentStatus::Running),
            (finished, AgentStatus::Done { exit_code: Some(0) }),
        ]
    );
    assert!(!reopen.agents.iter().any(|a| a.id == removed));
}

#[test]
fn config_goes_through_the_store() {
    let env = Env::new();
    let store = env.open();
    assert_eq!(store.config_path(), env.config.as_path());
    assert_eq!(store.load_config().unwrap(), AppConfig::default());
    assert!(!env.config.exists());

    let mut cfg = AppConfig::default();
    cfg.notifications.on_done = false;
    store.save_config(&cfg).unwrap();
    assert_eq!(store.load_config().unwrap(), cfg);

    std::fs::write(&env.config, "[notifications\n").unwrap();
    assert!(matches!(store.load_config(), Err(Error::Config(_))));
}

#[test]
fn store_is_send_sync_and_works_from_many_threads() {
    fn assert_service<T: Store + Send + Sync + 'static>() {}
    assert_service::<SqliteStore>();

    let env = Env::new();
    let store = Arc::new(env.open_with(StoreOptions {
        flush_interval: Duration::from_millis(5),
        flush_bytes: 256,
        ..StoreOptions::default()
    }));
    let session = session_created(&store, "s");
    let handles: Vec<_> = (0..4)
        .map(|t| {
            let store = store.clone();
            std::thread::spawn(move || {
                let id = agent_created(
                    &store,
                    &agent_config(session, &format!("t{t}"), AgentRole::Worker, None),
                );
                let mut expected = Vec::new();
                for seq in 0..200u64 {
                    let data = format!("{t}:{seq};");
                    output(&store, id, seq, data.as_bytes());
                    expected.extend_from_slice(data.as_bytes());
                    if seq % 50 == 0 {
                        status(&store, id, AgentStatus::Running);
                        store.history(5).unwrap();
                    }
                }
                assert_eq!(store.output_log(id, usize::MAX).unwrap(), expected);
                (id, expected)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    store.save_config(&AppConfig::default()).unwrap();
    store.flush().unwrap();
    for (id, expected) in &results {
        assert_eq!(&store.output_log(*id, usize::MAX).unwrap(), expected);
    }
    assert_eq!(store.history(1).unwrap().agents.len(), 4);
}

#[test]
fn default_paths_end_in_orquestra() {
    // Só calcula caminhos; não cria nada no home real.
    if let Some((db, config)) = SqliteStore::default_paths() {
        assert!(db.ends_with(Path::new("orquestra").join("orquestra.db")));
        assert!(config.ends_with(Path::new("orquestra").join("config.toml")));
    }
}
