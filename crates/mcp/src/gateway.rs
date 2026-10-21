//! `McpGateway`: roda dentro do app. Assina o bus, mantém o estado e atende
//! `orquestra mcp` e `orquestra hook stop` num socket Unix local.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use orquestra_core::{
    AgentId, AppConfig, CommandSender, Error, EventBus, EventReceiver, McpServerSpec,
    OrchestratorGateway, OrchestratorGrant, Result, SessionId, conventions,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::protocol::{Request, Response};
use crate::state::{Grant, State};

/// Maior linha aceita no socket.
const MAX_LINE: u64 = 1 << 20;

pub(crate) const SHUTTING_DOWN: &str = "o orquestra está encerrando";

/// Gateway do MCP `orquestra`. Criado pelo `main` com `start`, entregue ao hub
/// em `Services.gateway`. Clonar é barato (o `main` pode guardar um clone para
/// `update_config`). Quando o último clone cai (ou no `Shutdown` do hub), para
/// de atender e apaga o socket.
#[derive(Clone)]
pub struct McpGateway {
    handle: Arc<Handle>,
}

struct Handle {
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

pub(crate) struct Shared {
    state: Mutex<State>,
    /// Sobe a cada evento que `wait_for`/`take_news` observam.
    pub(crate) changes: watch::Sender<u64>,
    pub(crate) commands: CommandSender,
    config: Mutex<AppConfig>,
    socket: SocketFile,
    /// Caminho deste binário: vira `orquestra mcp` / `orquestra hook stop`.
    exe: String,
    stop: watch::Sender<bool>,
}

struct SocketFile {
    path: PathBuf,
    /// Diretório criado só para o socket (fallback fora do `XDG_RUNTIME_DIR`).
    owned_dir: Option<PathBuf>,
}

impl SocketFile {
    fn remove(&self) {
        let _ = fs::remove_file(&self.path);
        if let Some(dir) = &self.owned_dir {
            let _ = fs::remove_dir(dir);
        }
    }
}

impl McpGateway {
    /// Sobe o gateway em `$XDG_RUNTIME_DIR/orquestra/<pid>.sock` (ou num
    /// diretório temporário privado). Precisa rodar dentro do runtime tokio.
    pub fn start(commands: CommandSender, bus: EventBus, config: AppConfig) -> Result<Self> {
        let events = bus.subscribe();
        let (path, owned_dir) = socket_location()?;
        Self::launch(commands, events, config, SocketFile { path, owned_dir })
    }

    /// Como `start`, mas num caminho escolhido (testes, várias instâncias).
    /// O diretório precisa existir.
    pub fn start_at(
        commands: CommandSender,
        bus: EventBus,
        config: AppConfig,
        socket: impl Into<PathBuf>,
    ) -> Result<Self> {
        let events = bus.subscribe();
        let socket = SocketFile {
            path: socket.into(),
            owned_dir: None,
        };
        Self::launch(commands, events, config, socket)
    }

    fn launch(
        commands: CommandSender,
        events: EventReceiver,
        config: AppConfig,
        socket: SocketFile,
    ) -> Result<Self> {
        // Um socket velho no mesmo caminho é de um processo morto; outro tipo
        // de arquivo fica (e o bind falha).
        if fs::symlink_metadata(&socket.path).is_ok_and(|m| m.file_type().is_socket()) {
            let _ = fs::remove_file(&socket.path);
        }
        let listener = match UnixListener::bind(&socket.path) {
            Ok(listener) => listener,
            Err(err) => {
                if let Some(dir) = &socket.owned_dir {
                    let _ = fs::remove_dir(dir);
                }
                return Err(err.into());
            }
        };
        if let Err(err) = fs::set_permissions(&socket.path, Permissions::from_mode(0o600)) {
            socket.remove();
            return Err(err.into());
        }
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "orquestra".to_owned());
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changes: watch::Sender::new(0),
            commands,
            config: Mutex::new(config),
            socket,
            exe,
            stop: watch::Sender::new(false),
        });
        let tasks = vec![
            tokio::spawn(run_events(shared.clone(), events)),
            tokio::spawn(run_accept(shared.clone(), listener)),
        ];
        Ok(Self {
            handle: Arc::new(Handle { shared, tasks }),
        })
    }

    fn inner(&self) -> &Arc<Shared> {
        &self.handle.shared
    }

    /// Caminho do socket (o mesmo que vai em `ORQUESTRA_SOCKET`).
    pub fn socket_path(&self) -> &Path {
        &self.inner().socket.path
    }

    /// Troca a config usada pelo `spawn_agent` (presets). Chame depois de um
    /// `SaveConfig`: o gateway não vê comandos.
    pub fn update_config(&self, config: AppConfig) {
        *self
            .inner()
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = config;
    }

    #[cfg(test)]
    pub(crate) fn shared(&self) -> &Arc<Shared> {
        self.inner()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.shared.close();
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl OrchestratorGateway for McpGateway {
    fn grant(&self, session: SessionId, agent: AgentId) -> Result<OrchestratorGrant> {
        let token = new_token().map_err(|err| Error::Other(format!("gerar token: {err}")))?;
        let shared = self.inner();
        shared.lock().grant(
            token.clone(),
            Grant {
                session,
                orchestrator: agent,
            },
        );
        let exe = &shared.exe;
        let socket = shared.socket.path.to_string_lossy().into_owned();
        let env = BTreeMap::from([
            (conventions::env::SOCKET.to_owned(), socket),
            (conventions::env::TOKEN.to_owned(), token),
        ]);
        Ok(OrchestratorGrant {
            mcp: McpServerSpec {
                name: conventions::MCP_SERVER_NAME.to_owned(),
                command: exe.clone(),
                args: vec!["mcp".to_owned()],
                // Também no spec: o Codex não repassa o ambiente do agente
                // aos servidores MCP, só o que estiver aqui.
                env: env.clone(),
            },
            env,
            stop_hook: Some(format!("{} hook stop", shell_quote(exe))),
        })
    }

    fn revoke(&self, agent: AgentId) {
        self.inner().lock().revoke(agent);
    }
}

impl Shared {
    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn config(&self) -> AppConfig {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn bump(&self) {
        self.changes.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Para de atender e apaga o socket. Idempotente.
    fn close(&self) {
        self.lock().shutdown = true;
        self.bump();
        self.stop.send_replace(true);
        self.socket.remove();
    }

    fn apply_effects(&self, fx: crate::state::Effects) {
        for id in fx.replay {
            let _ = self
                .commands
                .send(orquestra_core::Command::RequestReplay { id });
        }
        if fx.changed {
            self.bump();
        }
    }

    pub(crate) async fn handle_line(self: &Arc<Self>, line: &str) -> Response {
        let req: Request = match serde_json::from_str(line.trim()) {
            Ok(req) => req,
            Err(err) => return Response::Error(format!("requisição inválida: {err}")),
        };
        let Some(grant) = self.lock().check(&req.token) else {
            return Response::Error("credencial inválida ou revogada".to_owned());
        };
        debug!(method = %req.method, orchestrator = %grant.orchestrator.short(), "gateway");
        self.dispatch(grant, &req.method, req.params).await.into()
    }
}

async fn run_events(shared: Arc<Shared>, mut events: EventReceiver) {
    loop {
        match events.recv().await {
            Ok(event) => {
                let shutdown = matches!(event, orquestra_core::AgentEvent::Shutdown);
                let fx = shared.lock().apply(&event);
                shared.apply_effects(fx);
                if shutdown {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(
                    n,
                    "gateway atrasou na leitura de eventos; pedindo replay das telas"
                );
                let fx = shared.lock().on_lag();
                shared.apply_effects(fx);
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    shared.close();
}

async fn run_accept(shared: Arc<Shared>, listener: UnixListener) {
    let mut stop = shared.stop.subscribe();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(serve_connection(shared.clone(), stream));
                }
                Err(err) => {
                    warn!(%err, "gateway: accept falhou");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            () = stopped(&mut stop) => break,
        }
    }
}

/// Uma requisição por vez por conexão. Se o cliente desconectar no meio (o
/// CLI cancelou a ferramenta, o hook morreu), a requisição é abandonada antes
/// de consumir qualquer coisa.
async fn serve_connection(shared: Arc<Shared>, stream: UnixStream) {
    let mut stop = shared.stop.subscribe();
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    loop {
        let line = tokio::select! {
            line = read_line(&mut reader) => line,
            () = stopped(&mut stop) => return,
        };
        let Some(line) = line else {
            return;
        };
        let response = tokio::select! {
            response = shared.handle_line(&line) => response,
            () = peer_closed(&mut reader) => return,
            () = stopped(&mut stop) => Response::Error(SHUTTING_DOWN.to_owned()),
        };
        let mut out = serde_json::to_string(&response)
            .unwrap_or_else(|_| r#"{"error":"resposta inválida"}"#.to_owned());
        out.push('\n');
        if write.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Resolve quando o gateway fecha.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|s| *s).await;
}

/// `None` no fim da conexão, em erro ou em linha grande demais.
async fn read_line(reader: &mut BufReader<OwnedReadHalf>) -> Option<String> {
    let mut buf = Vec::new();
    let n = (&mut *reader)
        .take(MAX_LINE)
        .read_until(b'\n', &mut buf)
        .await
        .ok()?;
    if n == 0 || (buf.last() != Some(&b'\n') && n as u64 >= MAX_LINE) {
        return None;
    }
    String::from_utf8(buf).ok()
}

/// Resolve quando o cliente fecha a conexão. Se ele mandar outra requisição
/// antes da resposta, deixa para a próxima volta e não resolve mais.
async fn peer_closed(reader: &mut BufReader<OwnedReadHalf>) {
    match reader.fill_buf().await {
        Ok(buf) if !buf.is_empty() => std::future::pending().await,
        _ => {}
    }
}

/// `$XDG_RUNTIME_DIR/orquestra/<pid>.sock`, ou `<tmp>/orquestra-<aleatório>/<pid>.sock`.
fn socket_location() -> io::Result<(PathBuf, Option<PathBuf>)> {
    let file = format!("{}.sock", std::process::id());
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(runtime).join("orquestra");
        if DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .is_ok()
        {
            return Ok((dir.join(file), None));
        }
    }
    let suffix = random_hex(6).map_err(io::Error::other)?;
    let dir = std::env::temp_dir().join(format!("orquestra-{suffix}"));
    // Não recursivo: falha se alguém já criou esse diretório.
    DirBuilder::new().mode(0o700).create(&dir)?;
    Ok((dir.join(file), Some(dir)))
}

fn random_hex(bytes: usize) -> std::result::Result<String, getrandom::Error> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// 256 bits do CSPRNG do sistema, em hex.
fn new_token() -> std::result::Result<String, getrandom::Error> {
    random_hex(32)
}

/// Quoting POSIX: deixa como está se só tiver caracteres seguros; senão,
/// aspas simples (com `'` virando `'\''`).
pub(crate) fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+,:@%=".contains(c));
    if safe {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(shell_quote("/usr/bin/orquestra"), "/usr/bin/orquestra");
        assert_eq!(
            shell_quote("/home/me/Meus Apps/orquestra"),
            "'/home/me/Meus Apps/orquestra'"
        );
        assert_eq!(shell_quote("/tmp/it's"), r"'/tmp/it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[tokio::test]
    async fn never_deletes_a_file_that_is_not_a_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("importante.txt");
        fs::write(&path, "dados").unwrap();
        let (tx, _rx) = orquestra_core::command_channel();
        let bus = EventBus::new();
        assert!(
            McpGateway::start_at(tx.clone(), bus.clone(), AppConfig::default(), &path).is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "dados");

        // Socket velho (processo morto) é substituído.
        let sock = dir.path().join("velho.sock");
        drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
        let gw = McpGateway::start_at(tx, bus, AppConfig::default(), &sock).unwrap();
        assert_eq!(gw.socket_path(), sock);
    }

    #[test]
    fn tokens_are_long_and_distinct() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
