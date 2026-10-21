//! Estado do gateway, montado a partir do `EventBus`: sessões, agentes,
//! credenciais, telas dos workers e a fila de novidades de cada orquestrador.
//!
//! Tudo aqui é síncrono e sem I/O; o gateway guarda num `Mutex` e aplica os
//! eventos em ordem.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use orquestra_core::{
    AgentConfig, AgentEvent, AgentId, AgentRole, AgentStatus, QueueReason, SessionConfig,
    SessionId, Timestamp,
};
use tokio::sync::oneshot;

use crate::protocol::{
    AgentBrief, AgentSummary, NewsItem, NewsReport, WaitUntil, kind_code, status_code,
    status_detail,
};
use crate::screen::Screen;

/// Depois de um `send_input`, um `WaitingForInput` antigo não conta como
/// "parado" por este tempo (ou até o status mudar): o agente ainda não reagiu.
pub const INPUT_GRACE: Duration = Duration::from_secs(5);

/// Tamanho máximo da tarefa mostrada no `list_agents`.
const TASK_PREVIEW_CHARS: usize = 160;

/// Quem é o dono de um token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    pub session: SessionId,
    pub orchestrator: AgentId,
}

/// Resposta ao `spawn_agent`: o primeiro status ou o erro do hub.
pub type SpawnAck = Result<AgentStatus, String>;

pub struct Agent {
    pub config: Arc<AgentConfig>,
    /// `None` até o primeiro `StatusChanged` (spawn recém-enviado).
    pub status: Option<AgentStatus>,
    pub browser_port: Option<u16>,
    /// Quantas vezes o status mudou (base do `wait_for until=change`).
    pub changes: u64,
    /// Último `send_input` ainda sem reação (ver `INPUT_GRACE`).
    pub input_at: Option<Instant>,
    /// O orquestrador pediu `kill_agent`: o fim não é novidade para ele.
    pub kill_requested: bool,
}

impl Agent {
    fn new(config: Arc<AgentConfig>) -> Self {
        Self {
            config,
            status: None,
            browser_port: None,
            changes: 0,
            input_at: None,
            kill_requested: false,
        }
    }

    fn is_terminal(&self) -> bool {
        self.status.as_ref().is_some_and(AgentStatus::is_terminal)
    }

    /// Vivo e sem esperar ninguém (nem input, nem aprovação).
    fn is_working(&self) -> bool {
        self.status
            .as_ref()
            .is_none_or(|s| s.is_alive() && !s.needs_attention())
    }

    /// `WaitingForInput` que já vale como "parado" (passou a carência do input).
    fn settled_waiting(&self, now: Instant) -> bool {
        self.status == Some(AgentStatus::WaitingForInput)
            && self
                .input_at
                .is_none_or(|at| now.duration_since(at) >= INPUT_GRACE)
    }
}

struct News {
    child: AgentId,
    name: String,
    event: NewsEvent,
    at: Timestamp,
}

enum NewsEvent {
    Status(AgentStatus),
    Removed,
}

/// O que o gateway precisa fazer depois de aplicar um evento.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Effects {
    /// Telas que perderam saída: pedir `RequestReplay`.
    pub replay: Vec<AgentId>,
    /// Algo que `wait_for`/`take_news` observam mudou.
    pub changed: bool,
}

#[derive(Default)]
pub struct State {
    pub sessions: HashMap<SessionId, Arc<SessionConfig>>,
    pub agents: HashMap<AgentId, Agent>,
    grants: HashMap<String, Grant>,
    /// Por orquestrador, no máximo uma novidade por filho, em ordem de chegada.
    news: HashMap<AgentId, Vec<News>>,
    screens: HashMap<AgentId, Screen>,
    pending: HashMap<AgentId, oneshot::Sender<SpawnAck>>,
    /// O hub publicou `Shutdown` (ou o bus fechou).
    pub shutdown: bool,
}

impl State {
    // -- eventos -------------------------------------------------------------

    pub fn apply(&mut self, event: &AgentEvent) -> Effects {
        let mut fx = Effects::default();
        match event {
            AgentEvent::SessionCreated { id, config } => {
                self.sessions.insert(*id, config.clone());
            }
            AgentEvent::SessionClosed { id } => {
                self.sessions.remove(id);
                fx.changed = true;
            }
            AgentEvent::Created { id, config } => {
                match self.agents.get_mut(id) {
                    Some(agent) => agent.config = config.clone(),
                    None => {
                        self.agents.insert(*id, Agent::new(config.clone()));
                    }
                }
                fx.changed = true;
            }
            AgentEvent::StatusChanged { id, status } => {
                self.on_status(*id, status);
                fx.changed = true;
            }
            AgentEvent::Spawned { info } => {
                if let Some(agent) = self.agents.get_mut(&info.id) {
                    agent.browser_port = info.browser.as_ref().map(|b| b.port);
                }
                fx.changed = true;
            }
            AgentEvent::Output { id, seq, data } => {
                if let Some(id) = self.on_output(*id, *seq, data) {
                    fx.replay.push(id);
                }
            }
            AgentEvent::Replay { id, next_seq, data } => {
                if (self.screens.contains_key(id) || self.wants_screen(*id))
                    && let Some(agent) = self.agents.get(id)
                {
                    let mut screen = Screen::new(agent.config.size);
                    screen.feed(data);
                    screen.next_seq = *next_seq;
                    self.screens.insert(*id, screen);
                }
            }
            AgentEvent::Removed { id } => {
                self.on_removed(*id);
                fx.changed = true;
            }
            AgentEvent::Error {
                id: Some(id),
                message,
            } => {
                if let Some(tx) = self.pending.remove(id) {
                    let _ = tx.send(Err(message.clone()));
                    // Spawn recusado: o agente nunca existiu para o hub.
                    if self.agents.get(id).is_some_and(|a| a.status.is_none()) {
                        self.agents.remove(id);
                    }
                    fx.changed = true;
                }
            }
            AgentEvent::Shutdown => {
                self.shutdown = true;
                fx.changed = true;
            }
            _ => {}
        }
        fx
    }

    /// O receptor do bus perdeu eventos: as telas podem estar furadas.
    pub fn on_lag(&mut self) -> Effects {
        let mut replay: Vec<AgentId> = self.screens.keys().copied().collect();
        replay.sort();
        for id in &replay {
            if let Some(screen) = self.screens.get_mut(id) {
                screen.replay_requested = true;
            }
        }
        Effects {
            replay,
            changed: true,
        }
    }

    fn on_status(&mut self, id: AgentId, status: &AgentStatus) {
        if let Some(tx) = self.pending.remove(&id) {
            let _ = tx.send(Ok(status.clone()));
        }
        let Some(agent) = self.agents.get_mut(&id) else {
            return;
        };
        let prev = agent.status.replace(status.clone());
        if prev.as_ref() != Some(status) {
            agent.changes += 1;
        }
        agent.input_at = None;

        let (Some(parent), AgentRole::Worker) = (agent.config.parent, agent.config.role) else {
            return;
        };
        let name = agent.config.name.clone();
        let expected = agent.kill_requested && status.is_terminal();
        if is_newsworthy(status) && !expected {
            if !prev.is_some_and(|p| p.same_state(status)) {
                self.push_news(parent, id, name, NewsEvent::Status(status.clone()));
            }
        } else if let Some(queue) = self.news.get_mut(&parent) {
            // Voltou a trabalhar: a novidade anterior ficou velha.
            queue.retain(|n| n.child != id);
        }
    }

    fn on_removed(&mut self, id: AgentId) {
        if let Some(tx) = self.pending.remove(&id) {
            let _ = tx.send(Err("o agente foi removido".to_owned()));
        }
        self.screens.remove(&id);
        self.news.remove(&id);
        self.revoke(id);
        let Some(agent) = self.agents.remove(&id) else {
            return;
        };
        if let Some(parent) = agent.config.parent {
            if let Some(queue) = self.news.get_mut(&parent) {
                queue.retain(|n| n.child != id);
            }
            // Remoção de quem ainda trabalhava interessa; de quem já terminou, não.
            if !agent.is_terminal() && self.agents.contains_key(&parent) {
                let name = agent.config.name.clone();
                self.push_news(parent, id, name, NewsEvent::Removed);
            }
        }
    }

    /// Devolve o id se for preciso pedir `RequestReplay`.
    fn on_output(&mut self, id: AgentId, seq: u64, data: &[u8]) -> Option<AgentId> {
        if !self.screens.contains_key(&id) {
            if !self.wants_screen(id) {
                return None;
            }
            self.screens
                .insert(id, Screen::new(self.agents[&id].config.size));
        }
        let screen = self.screens.get_mut(&id)?;
        if seq < screen.next_seq {
            return None; // já veio no `Replay`
        }
        let mut replay = None;
        if seq > screen.next_seq && !screen.replay_requested {
            screen.replay_requested = true;
            replay = Some(id);
        }
        screen.feed(data);
        screen.next_seq = seq + 1;
        replay
    }

    /// Só workers de sessões com orquestrador têm tela no gateway.
    fn wants_screen(&self, id: AgentId) -> bool {
        let Some(agent) = self.agents.get(&id) else {
            return false;
        };
        agent.config.role != AgentRole::Orchestrator
            && self.agents.values().any(|a| {
                a.config.role == AgentRole::Orchestrator && a.config.session == agent.config.session
            })
    }

    fn push_news(&mut self, parent: AgentId, child: AgentId, name: String, event: NewsEvent) {
        let queue = self.news.entry(parent).or_default();
        queue.retain(|n| n.child != child);
        queue.push(News {
            child,
            name,
            event,
            at: Timestamp::now(),
        });
    }

    // -- credenciais ---------------------------------------------------------

    pub fn grant(&mut self, token: String, grant: Grant) {
        self.revoke(grant.orchestrator);
        self.grants.insert(token, grant);
    }

    pub fn revoke(&mut self, orchestrator: AgentId) {
        self.grants.retain(|_, g| g.orchestrator != orchestrator);
    }

    pub fn check(&self, token: &str) -> Option<Grant> {
        self.grants.get(token).copied()
    }

    // -- spawn ---------------------------------------------------------------

    /// Registra o agente antes de o hub confirmar: reserva o nome e já aparece
    /// no `list_agents` como `pending`.
    pub fn insert_pending(
        &mut self,
        id: AgentId,
        config: AgentConfig,
    ) -> oneshot::Receiver<SpawnAck> {
        let (tx, rx) = oneshot::channel();
        self.agents.insert(id, Agent::new(Arc::new(config)));
        self.pending.insert(id, tx);
        rx
    }

    pub fn cancel_pending(&mut self, id: AgentId, forget: bool) {
        self.pending.remove(&id);
        if forget && self.agents.get(&id).is_some_and(|a| a.status.is_none()) {
            self.agents.remove(&id);
        }
    }

    /// `base` se estiver livre na sessão; senão `base-2`, `base-3`...
    /// (comparação sem diferenciar maiúsculas, para referências não ficarem ambíguas).
    pub fn unique_name(&self, session: SessionId, base: &str) -> String {
        let taken: HashSet<String> = self
            .agents
            .values()
            .filter(|a| a.config.session == session)
            .map(|a| a.config.name.to_lowercase())
            .collect();
        if !taken.contains(&base.to_lowercase()) {
            return base.to_owned();
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|name| !taken.contains(&name.to_lowercase()))
            .expect("sempre há um sufixo livre")
    }

    // -- consultas -----------------------------------------------------------

    /// Agente da sessão por nome, id completo ou id curto. Agente de outra
    /// sessão é tão desconhecido quanto um inexistente.
    pub fn resolve(&self, session: SessionId, reference: &str) -> Result<AgentId, String> {
        let r = reference.trim();
        let in_session = || {
            self.agents
                .iter()
                .filter(move |(_, a)| a.config.session == session)
        };
        let found = in_session()
            .find(|(_, a)| a.config.name == r)
            .or_else(|| in_session().find(|(_, a)| a.config.name.eq_ignore_ascii_case(r)))
            .or_else(|| {
                let id = r.parse::<AgentId>().ok()?;
                in_session().find(|(i, _)| **i == id)
            })
            .or_else(|| {
                let short = r.to_ascii_lowercase();
                in_session().find(|(i, _)| i.short() == short)
            });
        found
            .map(|(id, _)| *id)
            .ok_or_else(|| format!("agente desconhecido: {r}"))
    }

    pub fn name_of(&self, id: AgentId) -> String {
        self.agents
            .get(&id)
            .map_or_else(|| id.short(), |a| a.config.name.clone())
    }

    /// Agentes da sessão em ordem de criação.
    pub fn session_agents(&self, session: SessionId) -> Vec<AgentId> {
        let mut ids: Vec<AgentId> = self
            .agents
            .iter()
            .filter(|(_, a)| a.config.session == session)
            .map(|(id, _)| *id)
            .collect();
        ids.sort();
        ids
    }

    /// Filhos do orquestrador em ordem de criação.
    pub fn children(&self, orchestrator: AgentId) -> Vec<AgentId> {
        let mut ids: Vec<AgentId> = self
            .agents
            .iter()
            .filter(|(_, a)| a.config.parent == Some(orchestrator))
            .map(|(id, _)| *id)
            .collect();
        ids.sort();
        ids
    }

    pub fn has_working_children(&self, orchestrator: AgentId) -> bool {
        self.agents
            .values()
            .any(|a| a.config.parent == Some(orchestrator) && a.is_working())
    }

    pub fn summary(&self, id: AgentId) -> Option<AgentSummary> {
        let agent = self.agents.get(&id)?;
        let config = &agent.config;
        Some(AgentSummary {
            name: config.name.clone(),
            id: id.short(),
            kind: kind_code(config.kind).to_owned(),
            role: match config.role {
                AgentRole::Orchestrator => "orchestrator",
                AgentRole::Worker => "worker",
            }
            .to_owned(),
            status: agent
                .status
                .as_ref()
                .map_or("pending", status_code)
                .to_owned(),
            detail: agent.status.as_ref().and_then(status_detail),
            parent: config.parent.map(|p| self.name_of(p)),
            browser_port: agent.browser_port,
            task: config.initial_prompt.as_deref().map(preview),
        })
    }

    /// Status atual; um agente que já sumiu aparece como `removed`.
    pub fn brief(&self, id: AgentId) -> AgentBrief {
        match self.agents.get(&id) {
            Some(agent) => AgentBrief {
                agent: agent.config.name.clone(),
                id: id.short(),
                status: agent
                    .status
                    .as_ref()
                    .map_or("pending", status_code)
                    .to_owned(),
                detail: agent.status.as_ref().and_then(status_detail),
            },
            None => AgentBrief {
                agent: id.short(),
                id: id.short(),
                status: "removed".to_owned(),
                detail: None,
            },
        }
    }

    pub fn has_news(&self, orchestrator: AgentId) -> bool {
        self.news.get(&orchestrator).is_some_and(|q| !q.is_empty())
    }

    /// Devolve e limpa as novidades, junto com o status de todos os filhos.
    pub fn take_news(&mut self, orchestrator: AgentId) -> NewsReport {
        let news = self
            .news
            .remove(&orchestrator)
            .unwrap_or_default()
            .into_iter()
            .map(|n| {
                let (event, detail) = match &n.event {
                    NewsEvent::Status(s) => (status_code(s), status_detail(s)),
                    NewsEvent::Removed => ("removed", None),
                };
                NewsItem {
                    agent: n.name,
                    id: n.child.short(),
                    event: event.to_owned(),
                    detail,
                    at: n.at.as_millis(),
                }
            })
            .collect();
        let children = self
            .children(orchestrator)
            .into_iter()
            .map(|id| self.brief(id))
            .collect();
        NewsReport { news, children }
    }

    /// O orquestrador já soube destes agentes (por `wait_for`).
    pub fn consume_news(&mut self, orchestrator: AgentId, agents: &[AgentId]) {
        if let Some(queue) = self.news.get_mut(&orchestrator) {
            queue.retain(|n| !agents.contains(&n.child));
        }
    }

    pub fn screen_lines(&self, id: AgentId, max: usize) -> Option<Vec<String>> {
        self.screens.get(&id).map(|s| s.last_lines(max))
    }

    pub fn bracketed_paste(&self, id: AgentId) -> bool {
        self.screens.get(&id).is_some_and(Screen::bracketed_paste)
    }

    // -- wait_for ------------------------------------------------------------

    /// Fotografia no início do `wait_for`.
    pub fn wait_baseline(&self, targets: &[AgentId], until: WaitUntil, now: Instant) -> Baseline {
        targets
            .iter()
            .map(|id| {
                let agent = self.agents.get(id);
                let mark = Mark {
                    changes: agent.map_or(0, |a| a.changes),
                    satisfied: satisfies(agent, until, now),
                    terminal: agent.is_none_or(Agent::is_terminal),
                };
                (*id, mark)
            })
            .collect()
    }

    /// Quem satisfaz a condição agora. Com `fresh_only` (alvo padrão), quem já
    /// a satisfazia no início só conta depois de mudar.
    pub fn wait_triggered(&self, spec: &WaitSpec, now: Instant) -> Vec<AgentId> {
        spec.targets
            .iter()
            .filter(|id| {
                let agent = self.agents.get(id);
                let mark = spec.baseline.get(id).copied().unwrap_or_default();
                let changed = agent.is_none_or(|a| a.changes > mark.changes);
                match spec.until {
                    WaitUntil::Change => changed,
                    until => {
                        satisfies(agent, until, now)
                            && (!spec.fresh_only || !mark.satisfied || changed)
                    }
                }
            })
            .copied()
            .collect()
    }

    /// Próximo instante em que uma carência de input expira (reavaliar).
    pub fn next_grace_expiry(&self, targets: &[AgentId]) -> Option<Instant> {
        targets
            .iter()
            .filter_map(|id| self.agents.get(id)?.input_at)
            .map(|at| at + INPUT_GRACE)
            .min()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Mark {
    changes: u64,
    satisfied: bool,
    pub terminal: bool,
}

pub type Baseline = HashMap<AgentId, Mark>;

pub struct WaitSpec {
    pub targets: Vec<AgentId>,
    pub until: WaitUntil,
    pub all: bool,
    pub fresh_only: bool,
    pub baseline: Baseline,
}

/// A condição vale para o agente agora (sumido = encerrado).
fn satisfies(agent: Option<&Agent>, until: WaitUntil, now: Instant) -> bool {
    let Some(agent) = agent else {
        return until != WaitUntil::Change;
    };
    let Some(status) = &agent.status else {
        return false;
    };
    let terminal = status.is_terminal();
    match until {
        WaitUntil::Idle => terminal || agent.settled_waiting(now),
        WaitUntil::Done => terminal,
        WaitUntil::Attention => {
            terminal
                || match status {
                    AgentStatus::WaitingForInput => agent.settled_waiting(now),
                    other => other.needs_attention(),
                }
        }
        WaitUntil::Change => false,
    }
}

/// Estados que o orquestrador precisa saber ao fim do turno.
fn is_newsworthy(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::WaitingForInput
            | AgentStatus::Done { .. }
            | AgentStatus::Failed { .. }
            | AgentStatus::Queued {
                reason: QueueReason::Approval
            }
    )
}

fn preview(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= TASK_PREVIEW_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(TASK_PREVIEW_CHARS).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
pub(crate) mod tests {
    use bytes::Bytes;
    use orquestra_core::{AgentKind, TermSize, Workspace};

    use super::*;

    pub fn config(
        session: SessionId,
        name: &str,
        role: AgentRole,
        parent: Option<AgentId>,
    ) -> AgentConfig {
        AgentConfig {
            name: name.to_owned(),
            kind: AgentKind::ClaudeCode,
            program: "claude".to_owned(),
            args: Vec::new(),
            env: Default::default(),
            session,
            role,
            parent,
            workspace: Workspace::Directory {
                path: "/tmp".into(),
            },
            initial_prompt: Some("faça a coisa".to_owned()),
            browser: false,
            size: TermSize::default(),
        }
    }

    struct World {
        st: State,
        session: SessionId,
        orq: AgentId,
    }

    impl World {
        fn new() -> Self {
            let mut st = State::default();
            let session = SessionId::generate();
            st.apply(&AgentEvent::SessionCreated {
                id: session,
                config: Arc::new(SessionConfig::new("loja", "/tmp/loja", "login")),
            });
            let orq = AgentId::generate();
            st.apply(&AgentEvent::Created {
                id: orq,
                config: Arc::new(config(session, "orq", AgentRole::Orchestrator, None)),
            });
            Self { st, session, orq }
        }

        fn child(&mut self, name: &str) -> AgentId {
            let id = AgentId::generate();
            let cfg = config(self.session, name, AgentRole::Worker, Some(self.orq));
            self.st.apply(&AgentEvent::Created {
                id,
                config: Arc::new(cfg),
            });
            id
        }

        fn status(&mut self, id: AgentId, status: AgentStatus) {
            self.st.apply(&AgentEvent::StatusChanged { id, status });
        }

        fn output(&mut self, id: AgentId, seq: u64, data: &'static [u8]) -> Effects {
            self.st.apply(&AgentEvent::Output {
                id,
                seq,
                data: Bytes::from_static(data),
            })
        }
    }

    #[test]
    fn news_queue_keeps_latest_per_child_and_drops_stale() {
        let mut w = World::new();
        let (a, b) = (w.child("a"), w.child("b"));
        w.status(a, AgentStatus::Running);
        w.status(b, AgentStatus::Running);
        assert!(!w.st.has_news(w.orq));

        w.status(a, AgentStatus::WaitingForInput);
        w.status(b, AgentStatus::WaitingForInput);
        w.status(a, AgentStatus::Running); // a voltou a trabalhar sozinho
        w.status(b, AgentStatus::Done { exit_code: Some(0) });
        w.status(b, AgentStatus::Done { exit_code: Some(0) }); // repetido não duplica

        let report = w.st.take_news(w.orq);
        let events: Vec<_> = report
            .news
            .iter()
            .map(|n| (n.agent.as_str(), n.event.as_str()))
            .collect();
        assert_eq!(events, [("b", "done")]);
        assert_eq!(report.children.len(), 2);
        assert!(report.working().any(|c| c.agent == "a"));
        assert!(!w.st.has_news(w.orq), "take_news limpa a fila");
    }

    #[test]
    fn approval_failure_and_removal_are_news() {
        let mut w = World::new();
        let (a, b, c) = (w.child("a"), w.child("b"), w.child("c"));
        w.status(
            a,
            AgentStatus::Queued {
                reason: QueueReason::Approval,
            },
        );
        w.status(
            b,
            AgentStatus::Failed {
                reason: "boom".into(),
            },
        );
        w.status(c, AgentStatus::Running);
        w.st.apply(&AgentEvent::Removed { id: c });
        let news = w.st.take_news(w.orq).news;
        let got: Vec<_> = news
            .iter()
            .map(|n| (n.agent.as_str(), n.event.as_str(), n.detail.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("a", "awaiting_approval", None),
                ("b", "failed", Some("boom")),
                ("c", "removed", None)
            ]
        );
    }

    #[test]
    fn requested_kill_is_not_news() {
        let mut w = World::new();
        let a = w.child("a");
        w.status(a, AgentStatus::Running);
        w.st.agents.get_mut(&a).unwrap().kill_requested = true;
        w.status(a, AgentStatus::Done { exit_code: None });
        assert!(!w.st.has_news(w.orq));
    }

    #[test]
    fn orchestrator_and_strangers_produce_no_news() {
        let mut w = World::new();
        w.status(w.orq, AgentStatus::WaitingForInput);
        let solo = AgentId::generate();
        w.st.apply(&AgentEvent::Created {
            id: solo,
            config: Arc::new(config(w.session, "solo", AgentRole::Worker, None)),
        });
        w.status(solo, AgentStatus::Done { exit_code: Some(0) });
        assert!(!w.st.has_news(w.orq));
    }

    #[test]
    fn screens_only_for_orchestrated_workers_and_gaps_request_replay() {
        let mut w = World::new();
        let a = w.child("a");
        assert_eq!(w.output(a, 0, b"\x1b[1mol\x1b[0ma\r\n"), Effects::default());
        assert_eq!(w.output(w.orq, 0, b"eu"), Effects::default());
        assert!(w.st.screen_lines(w.orq, 10).is_none());

        // Buraco no seq: pede replay uma vez só.
        assert_eq!(w.output(a, 3, b"x\r\n").replay, [a]);
        assert!(w.output(a, 5, b"y\r\n").replay.is_empty());
        w.st.apply(&AgentEvent::Replay {
            id: a,
            next_seq: 6,
            data: Bytes::from_static(b"ola\r\nx\r\ny\r\n"),
        });
        assert!(
            w.output(a, 5, b"velho").replay.is_empty(),
            "já veio no replay"
        );
        w.output(a, 6, b"z");
        assert_eq!(w.st.screen_lines(a, 10).unwrap(), ["ola", "x", "y", "z"]);

        // Tela criada no meio do fluxo pede replay.
        let b = w.child("b");
        assert_eq!(w.output(b, 7, b"meio").replay, [b]);
        assert_eq!(w.st.on_lag().replay.len(), 2);
    }

    #[test]
    fn sessions_without_orchestrator_get_no_screen() {
        let mut st = State::default();
        let session = SessionId::generate();
        let id = AgentId::generate();
        st.apply(&AgentEvent::Created {
            id,
            config: Arc::new(config(session, "solo", AgentRole::Worker, None)),
        });
        st.apply(&AgentEvent::Output {
            id,
            seq: 0,
            data: Bytes::from_static(b"oi"),
        });
        assert!(st.screen_lines(id, 5).is_none());
    }

    #[test]
    fn resolve_by_name_or_id_within_the_session_only() {
        let mut w = World::new();
        let a = w.child("api");
        assert_eq!(w.st.resolve(w.session, "api"), Ok(a));
        assert_eq!(w.st.resolve(w.session, "API"), Ok(a));
        assert_eq!(w.st.resolve(w.session, &a.short()), Ok(a));
        assert_eq!(w.st.resolve(w.session, &a.to_string()), Ok(a));
        let other = SessionId::generate();
        assert_eq!(
            w.st.resolve(other, "api"),
            Err("agente desconhecido: api".to_owned())
        );
        assert_eq!(w.st.unique_name(w.session, "api"), "api-2");
        assert_eq!(w.st.unique_name(w.session, "web"), "web");
        assert_eq!(w.st.unique_name(other, "api"), "api");
    }

    #[test]
    fn pending_spawn_resolves_on_status_or_error() {
        let mut w = World::new();
        let id = AgentId::generate();
        let mut ok =
            w.st.insert_pending(id, config(w.session, "x", AgentRole::Worker, Some(w.orq)));
        assert_eq!(w.st.summary(id).unwrap().status, "pending");
        w.status(id, AgentStatus::Starting);
        assert_eq!(ok.try_recv().unwrap(), Ok(AgentStatus::Starting));

        let id = AgentId::generate();
        let mut err =
            w.st.insert_pending(id, config(w.session, "y", AgentRole::Worker, Some(w.orq)));
        w.st.apply(&AgentEvent::Error {
            id: Some(id),
            message: "spawn: sessão desconhecida".into(),
        });
        assert!(err.try_recv().unwrap().is_err());
        assert!(!w.st.agents.contains_key(&id), "placeholder some");
    }

    #[test]
    fn wait_ignores_already_satisfied_children_until_they_change() {
        let mut w = World::new();
        let (a, b) = (w.child("a"), w.child("b"));
        w.status(a, AgentStatus::WaitingForInput);
        w.status(b, AgentStatus::Running);
        let now = Instant::now();
        let targets = vec![a, b];
        let spec = |st: &State, fresh_only| WaitSpec {
            baseline: st.wait_baseline(&targets, WaitUntil::Idle, now),
            targets: targets.clone(),
            until: WaitUntil::Idle,
            all: false,
            fresh_only,
        };
        let fresh = spec(&w.st, true);
        let explicit = spec(&w.st, false);
        assert!(w.st.wait_triggered(&fresh, now).is_empty());
        assert_eq!(w.st.wait_triggered(&explicit, now), [a]);

        w.status(a, AgentStatus::Running);
        w.status(a, AgentStatus::WaitingForInput);
        assert_eq!(w.st.wait_triggered(&fresh, now), [a]);
    }

    #[test]
    fn recent_input_hides_stale_waiting() {
        let mut w = World::new();
        let a = w.child("a");
        w.status(a, AgentStatus::WaitingForInput);
        let now = Instant::now();
        w.st.agents.get_mut(&a).unwrap().input_at = Some(now);
        let spec = WaitSpec {
            baseline: w.st.wait_baseline(&[a], WaitUntil::Idle, now),
            targets: vec![a],
            until: WaitUntil::Idle,
            all: false,
            fresh_only: false,
        };
        assert!(w.st.wait_triggered(&spec, now).is_empty());
        assert_eq!(w.st.next_grace_expiry(&[a]), Some(now + INPUT_GRACE));
        assert_eq!(w.st.wait_triggered(&spec, now + INPUT_GRACE), [a]);
    }

    #[test]
    fn task_preview_is_short_and_flat() {
        assert_eq!(preview("a\n  b"), "a b");
        let long = "x".repeat(500);
        assert_eq!(preview(&long).chars().count(), TASK_PREVIEW_CHARS + 1);
    }
}
