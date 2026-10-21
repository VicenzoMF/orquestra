//! `orquestra hook stop`: o hook `Stop` do orquestrador Claude Code.
//!
//! Ao fim de cada turno pergunta ao gateway se os filhos têm novidades. Com
//! novidade (ou filhos ainda trabalhando), devolve `{"decision":"block"}` e o
//! Claude continua já sabendo; sem nada, escreve a OSC `waiting` no TTY e
//! deixa parar. Também deixa parar se um `wait_for` já roda em segundo plano
//! (o Claude Code acorda o orquestrador quando ele voltar). Gateway
//! inacessível nunca trava o orquestrador: deixa parar.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::time::Duration;

use orquestra_core::conventions;
use serde::Deserialize;
use serde_json::json;

use crate::client::GatewayClient;
use crate::protocol::{AgentBrief, NewsItem, NewsReport, code_label, method};

/// Segundos que o hook espera por novidade quando o Claude já está
/// continuando por causa dele (`stop_hook_active`).
pub const HOOK_WAIT_ENV: &str = "ORQUESTRA_HOOK_WAIT_SECS";
pub const DEFAULT_HOOK_WAIT_SECS: u64 = 45;

/// Folga além da espera pedida antes de desistir do gateway.
const REPLY_MARGIN: Duration = Duration::from_secs(10);
const MAX_INPUT: u64 = 1 << 20;

#[derive(Debug, Default, Deserialize)]
struct StopInput {
    #[serde(default)]
    stop_hook_active: bool,
    /// Tarefas em segundo plano do Claude Code. Uma chamada MCP longa (um
    /// `wait_for` passando de 2 min) vira uma delas e acorda o Claude quando
    /// termina.
    #[serde(default)]
    background_tasks: Vec<BackgroundTask>,
}

#[derive(Debug, Default, Deserialize)]
struct BackgroundTask {
    #[serde(default)]
    server: Option<String>,
}

impl StopInput {
    /// Já há uma chamada nossa (`wait_for`) rodando em segundo plano.
    fn waiting_in_background(&self) -> bool {
        self.background_tasks
            .iter()
            .any(|t| t.server.as_deref() == Some(conventions::MCP_SERVER_NAME))
    }
}

/// O que o hook decide.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopDecision {
    /// O Claude continua, com este motivo.
    Block(String),
    /// O turno termina (o hook escreve a OSC `waiting`).
    AllowStop,
}

impl StopDecision {
    /// O que vai no stdout do hook: o JSON do `block`, ou nada.
    pub fn stdout(&self) -> Option<String> {
        match self {
            Self::Block(reason) => Some(json!({"decision": "block", "reason": reason}).to_string()),
            Self::AllowStop => None,
        }
    }
}

/// Lógica do hook, sem I/O: `input` é o JSON recebido no stdin e
/// `take_news(wait_secs)` consulta o gateway.
pub fn decide_stop(
    input: &str,
    wait_secs: u64,
    mut take_news: impl FnMut(u64) -> Result<NewsReport, String>,
) -> StopDecision {
    let input: StopInput = serde_json::from_str(input).unwrap_or_default();
    let Ok(report) = take_news(0) else {
        return StopDecision::AllowStop;
    };
    if !report.news.is_empty() {
        return StopDecision::Block(news_reason(&report));
    }
    // Ninguém trabalhando, ou um `wait_for` em segundo plano vai acordar o
    // Claude quando algo mudar.
    if report.working().next().is_none() || input.waiting_in_background() {
        return StopDecision::AllowStop;
    }
    if !input.stop_hook_active {
        return StopDecision::Block(working_reason(&report));
    }
    // Já continuamos uma vez: espera um pouco por novidade em vez de insistir.
    if wait_secs == 0 {
        return StopDecision::AllowStop;
    }
    match take_news(wait_secs) {
        Ok(report) if !report.news.is_empty() => StopDecision::Block(news_reason(&report)),
        _ => StopDecision::AllowStop,
    }
}

/// Ponto de entrada de `orquestra hook stop`. Devolve o código de saída
/// (sempre 0: a decisão vai no stdout).
pub fn run_stop_hook() -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin().take(MAX_INPUT).read_to_string(&mut input);
    let wait_secs = std::env::var(HOOK_WAIT_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_HOOK_WAIT_SECS);
    let decision = match GatewayClient::from_env() {
        Ok(client) => decide_stop(&input, wait_secs, |secs| {
            let params = json!({ "wait_secs": secs });
            let reply = client.call_blocking(
                method::TAKE_NEWS,
                params,
                Duration::from_secs(secs) + REPLY_MARGIN,
            )?;
            serde_json::from_value(reply).map_err(|e| e.to_string())
        }),
        Err(_) => StopDecision::AllowStop,
    };
    match decision.stdout() {
        Some(line) => {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
        None => write_waiting_osc(),
    }
    0
}

/// Avisa o runner (pelo TTY) que o orquestrador parou e espera o humano.
fn write_waiting_osc() {
    if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write_all(conventions::osc::WAITING.as_bytes());
        let _ = tty.flush();
    }
}

fn news_reason(report: &NewsReport) -> String {
    let mut text = String::from("[orquestra] Novidades dos seus agentes:");
    for item in &report.news {
        text.push_str("\n- ");
        text.push_str(&news_line(item));
    }
    let working: Vec<String> = report.working().map(brief_label).collect();
    if working.is_empty() {
        text.push_str(
            "\nNenhum agente trabalhando agora. Se o objetivo foi atingido, resuma o que cada \
             branch entrega; senão, dê o próximo passo.",
        );
    } else {
        text.push_str(&format!(
            "\nAinda trabalhando: {}. Depois de tratar o que precisa, use wait_for para \
             esperar o próximo evento.",
            working.join(", ")
        ));
    }
    text
}

fn news_line(item: &NewsItem) -> String {
    let what = match (&item.detail, item.event.as_str()) {
        (Some(reason), "failed") => format!("falhou: {reason}"),
        (Some(detail), event) => format!("{} ({detail})", code_label(event)),
        (None, event) => code_label(event).to_owned(),
    };
    let hint = match item.event.as_str() {
        "waiting_for_input" => {
            "veja a tela com read_output; responda com send_input ou considere a tarefa concluída"
        }
        "done" => "confira o resultado com read_output",
        "failed" => "leia o erro com read_output e decida se recria a tarefa com spawn_agent",
        "awaiting_approval" => "o humano precisa aprovar na UI; nada a fazer por enquanto",
        "removed" => "o humano removeu este agente; não conte com ele",
        _ => "confira com read_output",
    };
    format!("{} ({}): {what} → {hint}.", item.agent, item.id)
}

fn working_reason(report: &NewsReport) -> String {
    let working: Vec<String> = report.working().map(brief_label).collect();
    let noun = if working.len() == 1 {
        "agente ainda trabalhando"
    } else {
        "agentes ainda trabalhando"
    };
    let mut text = format!(
        "[orquestra] {} {noun}: {}. Use wait_for para esperar o próximo evento.",
        working.len(),
        working.join(", ")
    );
    let waiting: Vec<String> = report
        .children
        .iter()
        .filter(|c| matches!(c.status.as_str(), "waiting_for_input" | "awaiting_approval"))
        .map(brief_label)
        .collect();
    if !waiting.is_empty() {
        text.push_str(&format!(" Parados esperando: {}.", waiting.join(", ")));
    }
    text
}

fn brief_label(brief: &AgentBrief) -> String {
    format!("{} ({})", brief.agent, code_label(&brief.status))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(name: &str, status: &str) -> AgentBrief {
        AgentBrief {
            agent: name.to_owned(),
            id: format!("{name}0000000")[..8].to_owned(),
            status: status.to_owned(),
            detail: None,
        }
    }

    fn news(name: &str, event: &str, detail: Option<&str>) -> NewsItem {
        NewsItem {
            agent: name.to_owned(),
            id: "abcd1234".to_owned(),
            event: event.to_owned(),
            detail: detail.map(str::to_owned),
            at: 0,
        }
    }

    /// Responde em sequência; devolve também os `wait_secs` pedidos.
    fn run(input: &str, replies: Vec<Result<NewsReport, String>>) -> (StopDecision, Vec<u64>) {
        let mut replies = replies.into_iter();
        let mut calls = Vec::new();
        let decision = decide_stop(input, 45, |secs| {
            calls.push(secs);
            replies.next().unwrap_or_else(|| Err("sem resposta".into()))
        });
        (decision, calls)
    }

    const FIRST: &str = r#"{"session_id":"s","hook_event_name":"Stop","stop_hook_active":false}"#;
    const AGAIN: &str = r#"{"session_id":"s","hook_event_name":"Stop","stop_hook_active":true}"#;

    #[test]
    fn news_blocks_with_a_summary_and_next_step() {
        let report = NewsReport {
            news: vec![
                news("api", "done", Some("código de saída 0")),
                news("qa", "failed", Some("boom")),
                news("ui", "waiting_for_input", None),
            ],
            children: vec![
                child("api", "done"),
                child("qa", "failed"),
                child("ui", "waiting_for_input"),
                child("docs", "running"),
            ],
        };
        let (decision, calls) = run(FIRST, vec![Ok(report)]);
        assert_eq!(calls, [0]);
        let StopDecision::Block(reason) = decision else {
            panic!("devia bloquear");
        };
        assert!(reason.starts_with("[orquestra] Novidades"));
        assert!(reason.contains("api (abcd1234): terminou (código de saída 0) → confira"));
        assert!(reason.contains("qa (abcd1234): falhou: boom"));
        assert!(reason.contains("ui (abcd1234): aguardando input → veja a tela"));
        assert!(reason.contains("Ainda trabalhando: docs (trabalhando)"));
        assert!(reason.contains("wait_for"));
    }

    #[test]
    fn working_children_block_once_then_wait_for_news() {
        let working = || NewsReport {
            news: vec![],
            children: vec![
                child("a", "running"),
                child("b", "starting"),
                child("c", "waiting_for_input"),
            ],
        };
        let (decision, calls) = run(FIRST, vec![Ok(working())]);
        assert_eq!(calls, [0]);
        assert_eq!(
            decision,
            StopDecision::Block(
                "[orquestra] 2 agentes ainda trabalhando: a (trabalhando), b (iniciando). \
                 Use wait_for para esperar o próximo evento. Parados esperando: c (aguardando input)."
                    .to_owned()
            )
        );

        // Já continuando: espera; chegou novidade → bloqueia com ela.
        let late = NewsReport {
            news: vec![news("a", "done", None)],
            children: vec![child("a", "done"), child("b", "running")],
        };
        let (decision, calls) = run(AGAIN, vec![Ok(working()), Ok(late)]);
        assert_eq!(calls, [0, 45]);
        assert!(matches!(decision, StopDecision::Block(r) if r.contains("a (abcd1234): terminou")));

        // Já continuando e nada chegou: deixa parar.
        let (decision, calls) = run(AGAIN, vec![Ok(working()), Ok(working())]);
        assert_eq!(calls, [0, 45]);
        assert_eq!(decision, StopDecision::AllowStop);
    }

    #[test]
    fn nothing_running_or_gateway_down_lets_it_stop() {
        let idle = NewsReport {
            news: vec![],
            children: vec![child("a", "waiting_for_input"), child("b", "done")],
        };
        assert_eq!(run(FIRST, vec![Ok(idle)]).0, StopDecision::AllowStop);
        assert_eq!(
            run(FIRST, vec![Ok(NewsReport::default())]).0,
            StopDecision::AllowStop
        );
        let (decision, calls) = run(FIRST, vec![Err("inacessível".into())]);
        assert_eq!((decision, calls), (StopDecision::AllowStop, vec![0]));
    }

    #[test]
    fn background_wait_for_lets_it_stop() {
        let working = NewsReport {
            news: vec![],
            children: vec![child("a", "running")],
        };
        let input = r#"{"stop_hook_active":false,"background_tasks":[
            {"id":"t1","type":"MCP task","status":"running","server":"orquestra","tool":"wait_for"}]}"#;
        let (decision, calls) = run(input, vec![Ok(working.clone())]);
        assert_eq!((decision, calls), (StopDecision::AllowStop, vec![0]));

        // Tarefa de outro servidor não conta.
        let other = r#"{"background_tasks":[{"id":"t2","type":"shell","command":"sleep 9"}]}"#;
        assert!(matches!(
            run(other, vec![Ok(working)]).0,
            StopDecision::Block(_)
        ));
    }

    #[test]
    fn garbage_input_counts_as_first_stop() {
        let working = NewsReport {
            news: vec![],
            children: vec![child("a", "running")],
        };
        let (decision, calls) = run("não é json", vec![Ok(working)]);
        assert_eq!(calls, [0]);
        assert!(
            matches!(decision, StopDecision::Block(r) if r.contains("1 agente ainda trabalhando"))
        );
    }

    #[test]
    fn stdout_is_the_block_json_or_nothing() {
        let line = StopDecision::Block("motivo \"x\"".into()).stdout().unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value,
            json!({"decision": "block", "reason": "motivo \"x\""})
        );
        assert!(StopDecision::AllowStop.stdout().is_none());
    }
}
