//! Uma janela por usuário, como o `code .`: o `orq` de outro terminal entrega
//! o pedido à instância aberta por um socket Unix (uma linha JSON por pedido,
//! resposta `ok`). Quem segura o lock é a instância; os outros são clientes.
//! Também impede duas instâncias no mesmo banco (a segunda marcaria as sessões
//! da primeira como interrompidas).

use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

const SOCKET: &str = "ui.sock";
const LOCK: &str = "ui.lock";
/// Tempo máximo para ler um pedido ou esperar o `ok`.
const IO_TIMEOUT: Duration = Duration::from_secs(3);
/// Maior pedido aceito (um caminho e um objetivo).
const MAX_REQUEST: u64 = 64 * 1024;

/// O que o `orq` pede à janela.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRequest {
    /// Raiz do repositório a abrir. Sem ela, só traz a janela para a frente.
    pub repo: Option<PathBuf>,
    /// Com objetivo a orquestração já começa; sem, abre o diálogo preenchido.
    pub goal: Option<String>,
}

/// Diretório do socket e do lock: `$XDG_RUNTIME_DIR/orquestra` ou, sem ele,
/// `<tmp>/orquestra-<uid>`, que precisa ser nosso e fechado para os outros.
pub fn default_dir() -> io::Result<PathBuf> {
    let uid = nix::unistd::getuid();
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(runtime) => PathBuf::from(runtime).join("orquestra"),
        None => std::env::temp_dir().join(format!("orquestra-{uid}")),
    };
    DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    let meta = std::fs::metadata(&dir)?;
    if meta.uid() != uid.as_raw() || meta.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} não é um diretório privado deste usuário", dir.display()),
        ));
    }
    Ok(dir)
}

/// Entrega o pedido à instância aberta. Erro: não há instância (ou ela não
/// respondeu a tempo).
pub fn send(dir: &Path, request: &OpenRequest) -> io::Result<()> {
    let mut stream = UnixStream::connect(dir.join(SOCKET))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut line = serde_json::to_string(request).map_err(io::Error::other)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    if reply.trim_end() == "ok" {
        Ok(())
    } else {
        Err(io::Error::other(format!("resposta inesperada: {reply:?}")))
    }
}

/// A instância: segura o lock e atende o socket. Ao sair, apaga o socket (o
/// lock solta junto com o arquivo).
pub struct Instance {
    _lock: File,
    socket: PathBuf,
    listener: Option<UnixListener>,
}

impl Instance {
    /// Tenta ser a instância. `None`: outra já é (ou está subindo agora).
    pub fn claim(dir: &Path) -> io::Result<Option<Self>> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(err)) => return Err(err),
        }
        let socket = dir.join(SOCKET);
        // Com o lock na mão, um socket que sobrou é de uma instância que morreu.
        match std::fs::remove_file(&socket) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let listener = UnixListener::bind(&socket)?;
        Ok(Some(Self {
            _lock: lock,
            socket,
            listener: Some(listener),
        }))
    }

    /// Atende os pedidos numa thread própria, entregando cada um a `on_request`.
    pub fn serve(&mut self, on_request: impl Fn(OpenRequest) + Send + 'static) -> io::Result<()> {
        let listener = self
            .listener
            .take()
            .ok_or_else(|| io::Error::other("a instância já está atendendo"))?;
        std::thread::Builder::new()
            .name("orquestra-instancia".to_owned())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    if let Some(request) = read_request(&stream) {
                        on_request(request);
                        let _ = (&stream).write_all(b"ok\n");
                    }
                }
            })?;
        Ok(())
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn read_request(stream: &UnixStream) -> Option<OpenRequest> {
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
    let mut line = String::new();
    BufReader::new(stream.take(MAX_REQUEST))
        .read_line(&mut line)
        .ok()?;
    serde_json::from_str(&line).ok()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn request(repo: &str, goal: Option<&str>) -> OpenRequest {
        OpenRequest {
            repo: Some(repo.into()),
            goal: goal.map(str::to_owned),
        }
    }

    #[test]
    fn requests_reach_the_instance() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            send(dir.path(), &OpenRequest::default()).is_err(),
            "sem instância"
        );

        let mut instance = Instance::claim(dir.path()).unwrap().unwrap();
        let (tx, rx) = mpsc::channel();
        instance.serve(move |r| tx.send(r).unwrap()).unwrap();

        let first = request("/src/loja", None);
        let second = request("/src/blog", Some("migrar\npara Astro"));
        send(dir.path(), &first).unwrap();
        send(dir.path(), &second).unwrap();
        assert_eq!(rx.recv().unwrap(), first);
        assert_eq!(rx.recv().unwrap(), second);
    }

    #[test]
    fn only_one_instance_and_the_socket_goes_away_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let instance = Instance::claim(dir.path()).unwrap().unwrap();
        assert!(Instance::claim(dir.path()).unwrap().is_none());
        drop(instance);
        assert!(!dir.path().join(SOCKET).exists());
        // Livre de novo.
        assert!(Instance::claim(dir.path()).unwrap().is_some());
    }

    #[test]
    fn stale_socket_from_a_dead_instance_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        // Socket sem ninguém atendendo, como depois de um crash.
        drop(UnixListener::bind(dir.path().join(SOCKET)).unwrap());
        assert!(send(dir.path(), &OpenRequest::default()).is_err());

        let mut instance = Instance::claim(dir.path()).unwrap().unwrap();
        let (tx, rx) = mpsc::channel();
        instance.serve(move |r| tx.send(r).unwrap()).unwrap();
        send(dir.path(), &OpenRequest::default()).unwrap();
        assert_eq!(rx.recv().unwrap(), OpenRequest::default());
    }

    #[test]
    fn garbage_gets_no_reply() {
        let dir = tempfile::tempdir().unwrap();
        let mut instance = Instance::claim(dir.path()).unwrap().unwrap();
        instance.serve(|_| panic!("não é pedido")).unwrap();
        let mut stream = UnixStream::connect(dir.path().join(SOCKET)).unwrap();
        stream.write_all(b"oi\n").unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert_eq!(reply, "");
    }
}
