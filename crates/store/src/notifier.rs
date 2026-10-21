//! Notificações de desktop via D-Bus (`notify-rust`, backend zbus).
//!
//! `notify` só enfileira: uma thread própria fala com o D-Bus, então um
//! servidor de notificações travado não segura o recorder do hub. Notificações
//! que chegam a menos de `BURST_WINDOW` da anterior saem juntas, num resumo.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use orquestra_core::{Error, Notification, NotificationKind, Notifier, Result};
use tracing::{debug, warn};

const APP_NAME: &str = "orquestra";
/// Ícone padrão do freedesktop; o app ainda não instala um próprio.
const ICON: &str = "utilities-terminal";
const TIMEOUT: Duration = Duration::from_secs(5);
const BURST_WINDOW: Duration = Duration::from_secs(1);
/// Linhas por resumo; o resto vira "e mais N".
const MAX_LINES: usize = 4;

/// `Notifier` do desktop. Falha do D-Bus não derruba nada: vira um `warn!`
/// na thread de envio (só a primeira de uma sequência; as outras, `debug!`).
pub struct DesktopNotifier {
    tx: Option<Sender<Notification>>,
}

impl DesktopNotifier {
    pub fn new() -> Self {
        Self::with_sink(DbusSink::default(), BURST_WINDOW)
    }

    fn with_sink(sink: impl Sink, window: Duration) -> Self {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("orquestra-notify".to_owned())
            .spawn(move || deliver(rx, sink, window));
        match spawned {
            Ok(_) => Self { tx: Some(tx) },
            Err(err) => {
                warn!(%err, "sem thread de notificações; elas serão descartadas");
                Self { tx: None }
            }
        }
    }
}

impl Default for DesktopNotifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Notifier for DesktopNotifier {
    fn notify(&self, notification: &Notification) -> Result<()> {
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| Error::Other("notificações indisponíveis".to_owned()))?;
        tx.send(notification.clone())
            .map_err(|_| Error::Other("thread de notificações parou".to_owned()))
    }
}

/// O que vai para o servidor de notificações.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Message {
    summary: String,
    body: String,
    urgency: Level,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Normal,
    Critical,
}

fn level(kind: NotificationKind) -> Level {
    match kind {
        NotificationKind::Failed => Level::Critical,
        NotificationKind::NeedsInput | NotificationKind::NeedsApproval => Level::Normal,
        // Low some do banner em vários servidores (GNOME); "terminou" deve aparecer.
        NotificationKind::Finished => Level::Normal,
    }
}

/// Destino das mensagens: o D-Bus de verdade, ou um falso nos testes.
trait Sink: Send + 'static {
    fn show(&mut self, message: &Message) -> Result<()>;
}

/// Thread de envio. A primeira notificação depois de um silêncio sai na hora
/// (com o que já estiver na fila); dentro da janela da anterior, junta tudo
/// até a janela fechar e manda um resumo.
fn deliver(rx: Receiver<Notification>, mut sink: impl Sink, window: Duration) {
    let mut last_shown: Option<Instant> = None;
    let mut failing = false;
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        match last_shown.map(|t| t + window) {
            Some(deadline) if deadline > Instant::now() => loop {
                let wait = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(wait) {
                    Ok(next) => batch.push(next),
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                }
            },
            _ => batch.extend(rx.try_iter()),
        }
        match sink.show(&render(&batch)) {
            Ok(()) => failing = false,
            Err(err) if failing => debug!(%err, "notificação de desktop falhou"),
            Err(err) => {
                warn!(%err, "notificação de desktop falhou");
                failing = true;
            }
        }
        last_shown = Some(Instant::now());
    }
}

/// Uma notificação vira ela mesma; várias, um resumo com uma linha por
/// agente (vale a última de cada um), falhas primeiro.
fn render(batch: &[Notification]) -> Message {
    let mut latest: Vec<&Notification> = Vec::with_capacity(batch.len());
    for n in batch {
        match latest.iter_mut().find(|m| m.agent == n.agent) {
            Some(slot) => *slot = n,
            None => latest.push(n),
        }
    }
    latest.sort_by_key(|n| std::cmp::Reverse(level(n.kind)));
    let urgency = latest
        .iter()
        .map(|n| level(n.kind))
        .max()
        .unwrap_or(Level::Normal);

    let count = latest.len();
    if let [only] = latest.as_slice() {
        return Message {
            summary: only.title.clone(),
            body: only.body.clone(),
            urgency,
        };
    }
    let kind = latest.first().map(|n| n.kind);
    let summary = if latest.iter().all(|n| Some(n.kind) == kind) {
        match kind {
            Some(NotificationKind::NeedsInput) => format!("{count} agentes aguardando você"),
            Some(NotificationKind::NeedsApproval) => {
                format!("{count} agentes aguardando aprovação")
            }
            Some(NotificationKind::Finished) => format!("{count} agentes terminaram"),
            Some(NotificationKind::Failed) => format!("{count} agentes falharam"),
            None => String::new(),
        }
    } else {
        format!("{count} novidades dos agentes")
    };
    let mut lines: Vec<String> = latest
        .iter()
        .take(MAX_LINES)
        .map(|n| format!("{}: {}", n.title, n.body))
        .collect();
    if count > MAX_LINES {
        lines.push(format!("e mais {}", count - MAX_LINES));
    }
    Message {
        summary,
        body: lines.join("\n"),
        urgency,
    }
}

/// Servidor de notificações do desktop. Descobre uma vez se ele interpreta
/// markup no corpo, para escapar `<`, `>` e `&` de nomes e motivos.
#[derive(Default)]
struct DbusSink {
    markup: Option<bool>,
}

impl Sink for DbusSink {
    fn show(&mut self, message: &Message) -> Result<()> {
        let markup = match self.markup {
            Some(markup) => markup,
            None => {
                let found = body_markup();
                self.markup = found;
                found.unwrap_or(false)
            }
        };
        let body = if markup {
            escape_markup(&message.body)
        } else {
            message.body.clone()
        };
        let mut n = notify_rust::Notification::new();
        n.appname(APP_NAME)
            .summary(&message.summary)
            .body(&body)
            .icon(ICON)
            .timeout(TIMEOUT);
        #[cfg(all(unix, not(target_os = "macos")))]
        n.urgency(match message.urgency {
            Level::Normal => notify_rust::Urgency::Normal,
            Level::Critical => notify_rust::Urgency::Critical,
        });
        n.show()
            .map(drop)
            .map_err(|err| Error::Other(format!("notificação: {err}")))
    }
}

/// `None` se não deu para perguntar ao servidor (tenta de novo depois).
#[cfg(all(unix, not(target_os = "macos")))]
fn body_markup() -> Option<bool> {
    notify_rust::get_capabilities()
        .ok()
        .map(|caps| caps.iter().any(|c| c == "body-markup"))
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn body_markup() -> Option<bool> {
    Some(false)
}

fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use orquestra_core::AgentId;

    use super::*;

    fn note(agent: AgentId, kind: NotificationKind, title: &str) -> Notification {
        Notification {
            agent,
            kind,
            title: title.to_owned(),
            body: format!("corpo de {title}"),
        }
    }

    #[derive(Clone, Default)]
    struct FakeSink(Arc<Mutex<Vec<Message>>>);

    impl Sink for FakeSink {
        fn show(&mut self, message: &Message) -> Result<()> {
            self.0.lock().unwrap().push(message.clone());
            Ok(())
        }
    }

    impl FakeSink {
        fn wait_for(&self, count: usize) -> Vec<Message> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let shown = self.0.lock().unwrap().clone();
                if shown.len() >= count || Instant::now() > deadline {
                    return shown;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    #[test]
    fn single_notification_maps_title_body_and_urgency() {
        let id = AgentId::generate();
        let msg = render(&[note(id, NotificationKind::Failed, "api")]);
        assert_eq!(msg.summary, "api");
        assert_eq!(msg.body, "corpo de api");
        assert_eq!(msg.urgency, Level::Critical);

        let msg = render(&[note(id, NotificationKind::NeedsInput, "api")]);
        assert_eq!(msg.urgency, Level::Normal);
        let msg = render(&[note(id, NotificationKind::NeedsApproval, "api")]);
        assert_eq!(msg.urgency, Level::Normal);
    }

    #[test]
    fn burst_becomes_a_summary_with_failures_first() {
        let (a, b, c) = (
            AgentId::generate(),
            AgentId::generate(),
            AgentId::generate(),
        );
        let msg = render(&[
            note(a, NotificationKind::NeedsInput, "login-ui"),
            note(b, NotificationKind::Failed, "estilos"),
            note(a, NotificationKind::Finished, "login-ui"),
            note(c, NotificationKind::NeedsInput, "docs"),
        ]);
        assert_eq!(msg.summary, "3 novidades dos agentes");
        assert_eq!(msg.urgency, Level::Critical);
        let lines: Vec<&str> = msg.body.lines().collect();
        assert_eq!(
            lines,
            [
                "estilos: corpo de estilos",
                "login-ui: corpo de login-ui",
                "docs: corpo de docs"
            ]
        );

        let many: Vec<_> = (0..6)
            .map(|i| {
                note(
                    AgentId::generate(),
                    NotificationKind::NeedsInput,
                    &format!("a{i}"),
                )
            })
            .collect();
        let msg = render(&many);
        assert_eq!(msg.summary, "6 agentes aguardando você");
        assert_eq!(msg.body.lines().count(), MAX_LINES + 1);
        assert!(msg.body.ends_with("e mais 2"));
    }

    #[test]
    fn worker_shows_first_at_once_and_coalesces_the_burst() {
        let sink = FakeSink::default();
        let notifier = DesktopNotifier::with_sink(sink.clone(), Duration::from_millis(300));

        notifier
            .notify(&note(AgentId::generate(), NotificationKind::Finished, "um"))
            .unwrap();
        let shown = sink.wait_for(1);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].summary, "um");

        for name in ["dois", "três", "quatro"] {
            let n = note(AgentId::generate(), NotificationKind::Finished, name);
            notifier.notify(&n).unwrap();
        }
        let shown = sink.wait_for(2);
        assert_eq!(shown.len(), 2, "{shown:?}");
        assert_eq!(shown[1].summary, "3 agentes terminaram");

        // Depois de um silêncio maior que a janela, volta a sair na hora.
        std::thread::sleep(Duration::from_millis(400));
        let started = Instant::now();
        notifier
            .notify(&note(
                AgentId::generate(),
                NotificationKind::Failed,
                "cinco",
            ))
            .unwrap();
        let shown = sink.wait_for(3);
        assert_eq!(shown[2].summary, "cinco");
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn failing_sink_does_not_break_notify() {
        struct Broken;
        impl Sink for Broken {
            fn show(&mut self, _: &Message) -> Result<()> {
                Err(Error::Other("sem D-Bus".to_owned()))
            }
        }
        let notifier = DesktopNotifier::with_sink(Broken, Duration::from_millis(10));
        for _ in 0..3 {
            let n = note(AgentId::generate(), NotificationKind::Failed, "x");
            assert!(notifier.notify(&n).is_ok());
        }
    }

    #[test]
    fn escapes_markup() {
        assert_eq!(escape_markup("a<b> & c"), "a&lt;b&gt; &amp; c");
    }

    /// Mostra uma notificação de verdade: `cargo test -p orquestra-store -- --ignored`.
    #[test]
    #[ignore = "precisa de um servidor de notificações no D-Bus da sessão"]
    fn shows_a_real_desktop_notification() {
        let msg = render(&[note(
            AgentId::generate(),
            NotificationKind::NeedsInput,
            "orquestra <teste>",
        )]);
        DbusSink::default().show(&msg).unwrap();
    }
}
