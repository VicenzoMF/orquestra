//! Task que drena o canal principal de eventos sem esperar o frame: aplica
//! cada evento no modelo, alimenta o terminal de cada agente (visível ou não,
//! para sempre responder ao PTY) e pede repaint.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use orquestra_core::bytes::Bytes;
use orquestra_core::{AgentEvent, AgentId, Command, CommandSender, EventReceiver, TermSize};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::time::Instant;
use tracing::warn;

use crate::browser::FrameStore;
use crate::instance::OpenRequest;
use crate::model::Model;
use crate::term::{AgentTerm, Preview};

pub type SharedTerm = Arc<Mutex<AgentTerm>>;

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Estado compartilhado entre as tasks de entrada e a thread de UI.
pub struct Shared {
    pub model: Mutex<Model>,
    terms: Mutex<HashMap<AgentId, SharedTerm>>,
    pub frames: Arc<FrameStore>,
    /// Scrollback dos terminais criados daqui em diante.
    scrollback: AtomicUsize,
    /// Pedidos do `orq` para a janela, com `true` quando vieram de outro
    /// processo (aí a janela pede atenção).
    opens: Mutex<Vec<(OpenRequest, bool)>>,
}

impl Shared {
    pub fn new(max_workers: u32, scrollback: usize) -> Arc<Self> {
        Arc::new(Self {
            model: Mutex::new(Model::new(max_workers)),
            terms: Mutex::new(HashMap::new()),
            frames: Arc::new(FrameStore::default()),
            scrollback: AtomicUsize::new(scrollback),
            opens: Mutex::new(Vec::new()),
        })
    }

    pub fn model(&self) -> MutexGuard<'_, Model> {
        lock(&self.model)
    }

    pub fn set_scrollback(&self, lines: usize) {
        self.scrollback.store(lines, Ordering::Relaxed);
    }

    pub fn term(&self, id: AgentId) -> Option<SharedTerm> {
        lock(&self.terms).get(&id).cloned()
    }

    /// Enfileira um pedido do `orq`; a UI o aplica no próximo frame.
    pub fn push_open(&self, request: OpenRequest, remote: bool) {
        lock(&self.opens).push((request, remote));
    }

    pub fn take_opens(&self) -> Vec<(OpenRequest, bool)> {
        std::mem::take(&mut *lock(&self.opens))
    }

    /// Prévia do terminal do agente (vazia enquanto não há saída).
    pub fn preview(&self, id: AgentId) -> Arc<Preview> {
        self.term(id)
            .map(|t| lock(&t).preview())
            .unwrap_or_default()
    }

    fn term_or_create(&self, id: AgentId, size: TermSize) -> SharedTerm {
        lock(&self.terms)
            .entry(id)
            .or_insert_with(|| {
                let scrollback = self.scrollback.load(Ordering::Relaxed);
                Arc::new(Mutex::new(AgentTerm::new(size, scrollback)))
            })
            .clone()
    }

    fn all_terms(&self) -> Vec<(AgentId, SharedTerm)> {
        lock(&self.terms)
            .iter()
            .map(|(id, t)| (*id, t.clone()))
            .collect()
    }
}

struct Feeder {
    shared: Arc<Shared>,
    cmd: CommandSender,
    /// Atualizações sincronizadas pendentes, por agente.
    deadlines: HashMap<AgentId, Instant>,
}

impl Feeder {
    fn send(&self, cmd: Command) {
        let _ = self.cmd.send(cmd);
    }

    fn handle(&mut self, event: AgentEvent) {
        self.shared.model().apply(&event);
        match event {
            AgentEvent::Created { id, config } => {
                self.shared.term_or_create(id, config.size);
            }
            AgentEvent::Output { id, seq, data } => {
                let term = self.shared.term_or_create(id, TermSize::default());
                let mut t = lock(&term);
                let fed = t.on_output(seq, &data);
                if fed.need_replay {
                    self.send(Command::RequestReplay { id });
                }
                if !fed.replies.is_empty() {
                    self.send(Command::SendInput {
                        id,
                        data: Bytes::from(fed.replies),
                    });
                }
                match t.sync_deadline() {
                    Some(d) => {
                        self.deadlines.insert(id, Instant::from_std(d));
                    }
                    None => {
                        self.deadlines.remove(&id);
                    }
                }
            }
            AgentEvent::Replay { id, next_seq, data } => {
                let term = self.shared.term_or_create(id, TermSize::default());
                lock(&term).on_replay(next_seq, &data);
            }
            AgentEvent::Removed { id } => {
                lock(&self.shared.terms).remove(&id);
                self.deadlines.remove(&id);
            }
            _ => {}
        }
    }

    fn lagged(&mut self, n: u64) {
        warn!(n, "a UI perdeu eventos; pedindo replay dos terminais");
        self.shared.model().lagged = true;
        for (id, term) in self.shared.all_terms() {
            if lock(&term).mark_lagged() {
                self.send(Command::RequestReplay { id });
            }
        }
    }

    fn flush_due(&mut self) {
        let now = Instant::now();
        let due: Vec<AgentId> = self
            .deadlines
            .iter()
            .filter(|(_, d)| **d <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            self.deadlines.remove(&id);
            if let Some(term) = self.shared.term(id) {
                let replies = lock(&term).flush_sync();
                if !replies.is_empty() {
                    self.send(Command::SendInput {
                        id,
                        data: Bytes::from(replies),
                    });
                }
            }
        }
    }
}

/// Roda até o canal fechar (o hub e todos os serviços caíram).
pub async fn run_event_feeder(
    mut rx: EventReceiver,
    shared: Arc<Shared>,
    cmd: CommandSender,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    let mut feeder = Feeder {
        shared,
        cmd,
        deadlines: HashMap::new(),
    };
    loop {
        let next = feeder.deadlines.values().min().copied();
        let first = tokio::select! {
            ev = rx.recv() => ev,
            _ = tokio::time::sleep_until(next.unwrap_or_else(Instant::now)), if next.is_some() => {
                feeder.flush_due();
                repaint();
                continue;
            }
        };
        match first {
            Ok(event) => feeder.handle(event),
            Err(RecvError::Lagged(n)) => feeder.lagged(n),
            Err(RecvError::Closed) => break,
        }
        // Drena o que já chegou antes de pedir um único repaint.
        for _ in 0..512 {
            match rx.try_recv() {
                Ok(event) => feeder.handle(event),
                Err(TryRecvError::Lagged(n)) => feeder.lagged(n),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => {
                    repaint();
                    return;
                }
            }
        }
        repaint();
    }
    repaint();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use orquestra_core::{
        AgentConfig, AgentKind, AgentRole, EventBus, SessionConfig, SessionId, Workspace,
        command_channel,
    };

    use super::*;

    fn agent_config(session: SessionId) -> AgentConfig {
        AgentConfig {
            name: "w".to_owned(),
            kind: AgentKind::Shell,
            program: "sh".to_owned(),
            args: Vec::new(),
            env: Default::default(),
            session,
            role: AgentRole::Worker,
            parent: None,
            workspace: Workspace::Directory {
                path: "/tmp".into(),
            },
            initial_prompt: None,
            browser: false,
            size: TermSize {
                cols: 40,
                rows: 10,
                cell_width: 0,
                cell_height: 0,
            },
        }
    }

    #[tokio::test]
    async fn feeds_terms_answers_queries_and_requests_replay_on_gap() {
        let bus = EventBus::new();
        let rx = bus.subscribe();
        let (tx, mut cmd_rx) = command_channel();
        let shared = Shared::new(8, 100);
        let task = tokio::spawn(run_event_feeder(rx, shared.clone(), tx, Arc::new(|| {})));

        let session = SessionId::generate();
        let id = AgentId::generate();
        bus.publish(AgentEvent::SessionCreated {
            id: session,
            config: Arc::new(SessionConfig::new("s", "/tmp", "g")),
        });
        bus.publish(AgentEvent::Created {
            id,
            config: Arc::new(agent_config(session)),
        });
        bus.publish(AgentEvent::Output {
            id,
            seq: 0,
            data: Bytes::from_static(b"oi\x1b[6n"),
        });
        match tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv()).await {
            Ok(Some(Command::SendInput { id: i, data })) => {
                assert_eq!(i, id);
                assert_eq!(&data[..], b"\x1b[1;3R");
            }
            other => panic!("esperava SendInput, veio {other:?}"),
        }
        // Buraco: seq 1 faltou.
        bus.publish(AgentEvent::Output {
            id,
            seq: 2,
            data: Bytes::from_static(b"x"),
        });
        match tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv()).await {
            Ok(Some(Command::RequestReplay { id: i })) => assert_eq!(i, id),
            other => panic!("esperava RequestReplay, veio {other:?}"),
        }
        bus.publish(AgentEvent::Replay {
            id,
            next_seq: 3,
            data: Bytes::from_static(b"oi tudo"),
        });
        bus.publish(AgentEvent::Removed { id });
        drop(bus);
        task.await.unwrap();
        assert!(shared.term(id).is_none());
        assert_eq!(shared.model().agent_count(), 0);
    }
}
