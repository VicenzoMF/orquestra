//! Cliente CDP mínimo sobre a `Connection` do chromiumoxide.
//!
//! O `Handler` do chromiumoxide não serve aqui: ele se anexa a toda aba e a
//! inicializa (Network, Runtime, mundo isolado, emulação) e, se essa
//! inicialização expira, fecha a aba. O navegador é do agente, que o controla
//! pelo Playwright MCP; nós só olhamos a aba ativa. Então usamos só o
//! transporte (`Connection`) e os tipos gerados (`chromiumoxide::cdp`).

use std::collections::HashMap;
use std::time::Duration;

use chromiumoxide::Connection;
use chromiumoxide::cdp::browser_protocol::target::SessionId;
use chromiumoxide::error::CdpError;
use chromiumoxide::types::{CallId, Command, EventMessage, Message, Method, MethodId};
use futures::StreamExt;
use orquestra_core::{Error, Result};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, trace};

/// Prazo padrão de um comando.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Evento cru: o `params` é decodificado por quem consome.
#[derive(Debug, Deserialize)]
pub(crate) struct RawEvent {
    pub method: MethodId,
    #[serde(rename = "sessionId", default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub params: Value,
}

impl Method for RawEvent {
    fn identifier(&self) -> MethodId {
        self.method.clone()
    }
}

impl EventMessage for RawEvent {
    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

type Reply = oneshot::Sender<Result<Value, String>>;

struct Call {
    method: MethodId,
    session: Option<String>,
    params: Value,
    reply: Reply,
}

/// Resposta pendente de um comando enviado com [`Cdp::send`].
pub(crate) type Pending = oneshot::Receiver<Result<Value, String>>;

/// Ponta de envio, clonável. A conexão fecha quando o navegador cai ou
/// quando todas as pontas somem.
#[derive(Clone, Debug)]
pub(crate) struct Cdp {
    tx: mpsc::UnboundedSender<Call>,
}

impl Cdp {
    /// Conecta no WebSocket do navegador. Devolve a ponta de envio, os
    /// eventos (de todas as sessões) e a task que move a conexão.
    pub async fn connect(
        ws_url: &str,
    ) -> Result<(Self, mpsc::UnboundedReceiver<RawEvent>, JoinHandle<()>), String> {
        let conn = Connection::<RawEvent>::connect(ws_url)
            .await
            .map_err(|e| format!("conexão CDP em {ws_url}: {e}"))?;
        let (tx, calls) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::unbounded_channel();
        let driver = tokio::spawn(drive(conn, calls, events_tx));
        Ok((Self { tx }, events, driver))
    }

    /// Envia sem esperar; descarte o retorno para "dispare e esqueça".
    pub fn send<C: Command>(&self, session: Option<&str>, cmd: C) -> Pending {
        let (reply, rx) = oneshot::channel();
        let method = cmd.identifier();
        match serde_json::to_value(&cmd) {
            Ok(params) => {
                let call = Call {
                    method,
                    session: session.map(str::to_owned),
                    params,
                    reply,
                };
                if let Err(mpsc::error::SendError(call)) = self.tx.send(call) {
                    let _ = call.reply.send(Err("conexão CDP fechada".to_owned()));
                }
            }
            Err(e) => {
                let _ = reply.send(Err(format!("{method}: {e}")));
            }
        }
        rx
    }

    /// Envia e espera a resposta (até [`CALL_TIMEOUT`]).
    pub async fn call<C: Command>(&self, session: Option<&str>, cmd: C) -> Result<Value> {
        self.call_within(CALL_TIMEOUT, session, cmd).await
    }

    pub async fn call_within<C: Command>(
        &self,
        limit: Duration,
        session: Option<&str>,
        cmd: C,
    ) -> Result<Value> {
        let method = cmd.identifier();
        let pending = self.send(session, cmd);
        wait(limit, &method, pending).await
    }
}

/// Espera uma resposta pendente, convertendo para o erro do contrato.
pub(crate) async fn wait(limit: Duration, method: &str, pending: Pending) -> Result<Value> {
    match tokio::time::timeout(limit, pending).await {
        Ok(Ok(Ok(value))) => Ok(value),
        Ok(Ok(Err(e))) => Err(Error::Browser(format!("{method}: {e}"))),
        Ok(Err(_)) => Err(Error::Browser(format!("{method}: conexão CDP fechada"))),
        Err(_) => Err(Error::Browser(format!(
            "{method}: sem resposta em {limit:?}"
        ))),
    }
}

/// Move a conexão: manda os comandos da fila e distribui respostas e eventos.
async fn drive(
    mut conn: Connection<RawEvent>,
    mut calls: mpsc::UnboundedReceiver<Call>,
    events: mpsc::UnboundedSender<RawEvent>,
) {
    let mut pending: HashMap<CallId, Reply> = HashMap::new();
    loop {
        tokio::select! {
            call = calls.recv() => {
                let Some(call) = call else { break };
                trace!(method = %call.method, "CDP →");
                let session = call.session.map(SessionId::from);
                match conn.submit_command(call.method, session, call.params) {
                    Ok(id) => {
                        pending.insert(id, call.reply);
                    }
                    Err(e) => {
                        let _ = call.reply.send(Err(e.to_string()));
                    }
                }
            }
            msg = conn.next() => match msg {
                Some(Ok(Message::Response(resp))) => {
                    if let Some(reply) = pending.remove(&resp.id) {
                        let result = match resp.error {
                            Some(e) => Err(format!("{} ({})", e.message, e.code)),
                            None => Ok(resp.result.unwrap_or(Value::Null)),
                        };
                        let _ = reply.send(result);
                    }
                }
                Some(Ok(Message::Event(event))) => {
                    let _ = events.send(event);
                }
                Some(Err(CdpError::Ws(e))) => {
                    debug!(%e, "WebSocket CDP caiu");
                    break;
                }
                Some(Err(e)) => debug!(%e, "mensagem CDP ignorada"),
                None => break,
            },
        }
    }
    // As respostas pendentes caem junto: quem espera recebe "conexão fechada".
}
