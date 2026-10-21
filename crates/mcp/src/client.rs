//! Cliente do socket do gateway: uma conexão por chamada. Usado pela ponte
//! stdio (async) e pelo hook `Stop` (síncrono, sem runtime).
//!
//! Não feche a escrita antes de ler a resposta: o gateway entende o fim da
//! conexão como desistência e abandona a requisição.

use std::io::{BufRead, BufReader as StdBufReader, Write};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::time::Duration;

use orquestra_core::conventions;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::protocol::{Request, Response};

#[derive(Clone, Debug)]
pub struct GatewayClient {
    socket: PathBuf,
    token: String,
}

impl GatewayClient {
    pub fn new(socket: impl Into<PathBuf>, token: impl Into<String>) -> Self {
        Self {
            socket: socket.into(),
            token: token.into(),
        }
    }

    /// `ORQUESTRA_SOCKET` e `ORQUESTRA_TOKEN` do ambiente.
    pub fn from_env() -> Result<Self, String> {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    format!("{name} não definido: este processo não foi iniciado pelo orquestra")
                })
        };
        Ok(Self::new(
            var(conventions::env::SOCKET)?,
            var(conventions::env::TOKEN)?,
        ))
    }

    fn request_line(&self, method: &str, params: Value) -> Result<String, String> {
        let request = Request {
            token: self.token.clone(),
            method: method.to_owned(),
            params,
        };
        let mut line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        line.push('\n');
        Ok(line)
    }

    fn unreachable(&self, err: std::io::Error) -> String {
        format!(
            "gateway do orquestra inacessível ({}): {err}",
            self.socket.display()
        )
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let line = self.request_line(method, params)?;
        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| self.unreachable(e))?;
        let (read, mut write) = stream.into_split();
        write
            .write_all(line.as_bytes())
            .await
            .map_err(|e| self.unreachable(e))?;
        let mut reply = String::new();
        BufReader::new(read)
            .read_line(&mut reply)
            .await
            .map_err(|e| self.unreachable(e))?;
        parse_reply(&reply)
    }

    /// Versão síncrona; `timeout` limita a espera pela resposta.
    pub fn call_blocking(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let line = self.request_line(method, params)?;
        let mut stream = StdUnixStream::connect(&self.socket).map_err(|e| self.unreachable(e))?;
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(timeout)))
            .and_then(|()| stream.write_all(line.as_bytes()))
            .map_err(|e| self.unreachable(e))?;
        let mut reply = String::new();
        StdBufReader::new(stream)
            .read_line(&mut reply)
            .map_err(|e| self.unreachable(e))?;
        parse_reply(&reply)
    }
}

fn parse_reply(reply: &str) -> Result<Value, String> {
    if reply.is_empty() {
        return Err("o gateway do orquestra fechou a conexão sem responder".to_owned());
    }
    serde_json::from_str::<Response>(reply)
        .map_err(|e| format!("resposta inválida do gateway: {e}"))?
        .into_result()
}
