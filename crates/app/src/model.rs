//! Estado da UI reduzido a partir dos `AgentEvent`. Puro: sem egui, sem tokio.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use orquestra_core::{
    AgentConfig, AgentEvent, AgentId, AgentInfo, AgentRole, AgentStatus, BrowserStatus,
    QueueReason, SessionConfig, SessionId,
};

/// Quantos erros recentes o modelo guarda (para a barra de status).
const MAX_ERRORS: usize = 20;

/// Estado resumido, para cor/ícone/legenda. Cada um tem glifo próprio além da cor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StatusKind {
    Running,
    Waiting,
    Done,
    Failed,
    Starting,
    Queued,
    Approval,
}

impl StatusKind {
    pub fn of(status: &AgentStatus) -> Self {
        match status {
            AgentStatus::Queued {
                reason: QueueReason::Capacity,
            } => Self::Queued,
            AgentStatus::Queued {
                reason: QueueReason::Approval,
            } => Self::Approval,
            AgentStatus::Starting => Self::Starting,
            AgentStatus::Running => Self::Running,
            AgentStatus::WaitingForInput => Self::Waiting,
            AgentStatus::Done { .. } => Self::Done,
            AgentStatus::Failed { .. } => Self::Failed,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "rodando",
            Self::Waiting => "aguardando você",
            Self::Done => "pronto",
            Self::Failed => "falhou",
            Self::Starting => "iniciando",
            Self::Queued => "na fila",
            Self::Approval => "aguardando aprovação",
        }
    }
}

/// Rótulo do status em português (com o detalhe que o `StatusKind` perde).
pub fn status_label(status: &AgentStatus) -> String {
    match status {
        AgentStatus::Done { exit_code: None } => "encerrado".to_owned(),
        AgentStatus::Done {
            exit_code: Some(code),
        } if *code != 0 => format!("saiu com {code}"),
        other => StatusKind::of(other).label().to_owned(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserState {
    None,
    Launching,
    Ready { port: u16 },
    Closed,
    Failed(String),
}

impl BrowserState {
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Launching | Self::Ready { .. })
    }

    pub fn port(&self) -> Option<u16> {
        match self {
            Self::Ready { port } => Some(*port),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AgentView {
    pub id: AgentId,
    pub config: Arc<AgentConfig>,
    pub status: AgentStatus,
    pub info: Option<Arc<AgentInfo>>,
    pub browser: BrowserState,
    /// Última navegação da página principal: (url, título).
    pub page: Option<(String, String)>,
    pub last_error: Option<String>,
    /// Ordem de criação (estável entre frames).
    pub order: u64,
}

impl AgentView {
    pub fn kind(&self) -> StatusKind {
        StatusKind::of(&self.status)
    }

    pub fn is_orchestrator(&self) -> bool {
        self.config.role == AgentRole::Orchestrator
    }
}

#[derive(Clone, Debug)]
pub struct SessionView {
    pub id: SessionId,
    pub config: Arc<SessionConfig>,
    pub orchestrator: Option<AgentId>,
    /// Agentes da sessão em ordem de criação (inclui o orquestrador).
    pub agents: Vec<AgentId>,
    pub order: u64,
}

/// Contagem por estado. `workers` conta só quem não é orquestrador.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub workers: u32,
    pub running: u32,
    pub waiting: u32,
    pub done: u32,
    pub failed: u32,
    pub starting: u32,
    pub queued: u32,
    pub approval: u32,
}

impl Counts {
    fn add(&mut self, agent: &AgentView) {
        if !agent.is_orchestrator() {
            self.workers += 1;
        }
        match agent.kind() {
            StatusKind::Running => self.running += 1,
            StatusKind::Waiting => self.waiting += 1,
            StatusKind::Done => self.done += 1,
            StatusKind::Failed => self.failed += 1,
            StatusKind::Starting => self.starting += 1,
            StatusKind::Queued => self.queued += 1,
            StatusKind::Approval => self.approval += 1,
        }
    }

    pub fn attention(&self) -> u32 {
        self.waiting + self.failed + self.approval
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttentionKind {
    Input,
    Approval,
    Failure,
}

/// Item da fila "Precisa de você".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attention {
    pub agent: AgentId,
    pub session: SessionId,
    pub kind: AttentionKind,
}

#[derive(Debug)]
pub struct Model {
    sessions: HashMap<SessionId, SessionView>,
    agents: HashMap<AgentId, AgentView>,
    next_order: u64,
    /// `orchestration.max_concurrent_workers` (limite global de vagas).
    pub max_workers: u32,
    pub errors: VecDeque<(Option<AgentId>, String)>,
    /// `false` depois do `AgentEvent::Shutdown`.
    pub hub_alive: bool,
    /// A UI perdeu eventos (broadcast atrasou); o estado pode estar defasado.
    pub lagged: bool,
}

impl Model {
    pub fn new(max_workers: u32) -> Self {
        Self {
            sessions: HashMap::new(),
            agents: HashMap::new(),
            next_order: 0,
            max_workers,
            errors: VecDeque::new(),
            hub_alive: true,
            lagged: false,
        }
    }

    fn order(&mut self) -> u64 {
        self.next_order += 1;
        self.next_order
    }

    pub fn apply(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::SessionCreated { id, config } => {
                let order = self.order();
                self.sessions.entry(*id).or_insert(SessionView {
                    id: *id,
                    config: config.clone(),
                    orchestrator: None,
                    agents: Vec::new(),
                    order,
                });
            }
            AgentEvent::SessionClosed { id } => {
                if let Some(session) = self.sessions.remove(id) {
                    for agent in session.agents {
                        self.agents.remove(&agent);
                    }
                }
            }
            AgentEvent::Created { id, config } => {
                if self.agents.contains_key(id) {
                    return;
                }
                let order = self.order();
                if let Some(session) = self.sessions.get_mut(&config.session) {
                    session.agents.push(*id);
                    if config.role == AgentRole::Orchestrator {
                        session.orchestrator = Some(*id);
                    }
                }
                self.agents.insert(
                    *id,
                    AgentView {
                        id: *id,
                        config: config.clone(),
                        status: AgentStatus::Starting,
                        info: None,
                        browser: BrowserState::None,
                        page: None,
                        last_error: None,
                        order,
                    },
                );
            }
            AgentEvent::Spawned { info } => {
                if let Some(agent) = self.agents.get_mut(&info.id) {
                    agent.info = Some(info.clone());
                }
            }
            AgentEvent::StatusChanged { id, status } => {
                if let Some(agent) = self.agents.get_mut(id) {
                    agent.status = status.clone();
                }
            }
            AgentEvent::Removed { id } => {
                if let Some(agent) = self.agents.remove(id)
                    && let Some(session) = self.sessions.get_mut(&agent.config.session)
                {
                    session.agents.retain(|a| a != id);
                    if session.orchestrator == Some(*id) {
                        session.orchestrator = None;
                    }
                }
            }
            AgentEvent::BrowserStatus { id, status } => {
                if let Some(agent) = self.agents.get_mut(id) {
                    agent.browser = match status {
                        BrowserStatus::Launching => BrowserState::Launching,
                        BrowserStatus::Ready { endpoint } => BrowserState::Ready {
                            port: endpoint.port,
                        },
                        BrowserStatus::Closed => BrowserState::Closed,
                        BrowserStatus::Failed { reason } => BrowserState::Failed(reason.clone()),
                    };
                }
            }
            AgentEvent::BrowserNavigated { id, url, title } => {
                if let Some(agent) = self.agents.get_mut(id) {
                    agent.page = Some((url.clone(), title.clone()));
                }
            }
            AgentEvent::Error { id, message } => {
                if let Some(agent) = id.and_then(|id| self.agents.get_mut(&id)) {
                    agent.last_error = Some(message.clone());
                }
                self.errors.push_back((*id, message.clone()));
                while self.errors.len() > MAX_ERRORS {
                    self.errors.pop_front();
                }
            }
            AgentEvent::Shutdown => self.hub_alive = false,
            AgentEvent::Output { .. }
            | AgentEvent::Replay { .. }
            | AgentEvent::BrowserFrame { .. }
            | AgentEvent::History(_) => {}
        }
    }

    /// Sessões em ordem de criação.
    pub fn sessions(&self) -> Vec<&SessionView> {
        let mut sessions: Vec<_> = self.sessions.values().collect();
        sessions.sort_by_key(|s| s.order);
        sessions
    }

    pub fn session(&self, id: SessionId) -> Option<&SessionView> {
        self.sessions.get(&id)
    }

    pub fn agent(&self, id: AgentId) -> Option<&AgentView> {
        self.agents.get(&id)
    }

    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    /// Agentes da sessão em ordem de criação.
    pub fn agents_of(&self, session: SessionId) -> Vec<&AgentView> {
        self.sessions
            .get(&session)
            .map(|s| s.agents.iter().filter_map(|a| self.agents.get(a)).collect())
            .unwrap_or_default()
    }

    /// Filhos diretos de `parent` (as setas que saem dele), em ordem de criação.
    pub fn children(&self, parent: AgentId) -> Vec<&AgentView> {
        let Some(session) = self.agents.get(&parent).map(|a| a.config.session) else {
            return Vec::new();
        };
        self.agents_of(session)
            .into_iter()
            .filter(|a| a.config.parent == Some(parent))
            .collect()
    }

    /// Arestas pai → filho cujas duas pontas estão no modelo.
    pub fn edges(&self) -> Vec<(AgentId, AgentId)> {
        let mut agents: Vec<_> = self.agents.values().collect();
        agents.sort_by_key(|a| a.order);
        agents
            .into_iter()
            .filter_map(|a| {
                let parent = a.config.parent?;
                self.agents.contains_key(&parent).then_some((parent, a.id))
            })
            .collect()
    }

    /// Contagem de toda a UI (`None`) ou de uma sessão.
    pub fn counts(&self, session: Option<SessionId>) -> Counts {
        let mut counts = Counts::default();
        match session {
            Some(id) => self.agents_of(id).into_iter().for_each(|a| counts.add(a)),
            None => self.agents.values().for_each(|a| counts.add(a)),
        }
        counts
    }

    /// Fila "Precisa de você": input, aprovação e falha de todas as sessões.
    pub fn attention(&self) -> Vec<Attention> {
        let mut items = Vec::new();
        for session in self.sessions() {
            for agent in self.agents_of(session.id) {
                let kind = match agent.kind() {
                    StatusKind::Waiting => AttentionKind::Input,
                    StatusKind::Approval => AttentionKind::Approval,
                    StatusKind::Failed => AttentionKind::Failure,
                    _ => continue,
                };
                items.push(Attention {
                    agent: agent.id,
                    session: session.id,
                    kind,
                });
            }
        }
        items
    }

    /// Vagas ocupadas do limite global (só workers contam).
    pub fn slots_used(&self) -> u32 {
        self.agents
            .values()
            .filter(|a| !a.is_orchestrator() && a.status.occupies_slot())
            .count() as u32
    }

    pub fn browsers_open(&self) -> u32 {
        self.agents.values().filter(|a| a.browser.is_open()).count() as u32
    }

    /// (prontos, total) dos filhos de um orquestrador.
    pub fn progress(&self, orchestrator: AgentId) -> (u32, u32) {
        let children = self.children(orchestrator);
        let done = children
            .iter()
            .filter(|a| matches!(a.status, AgentStatus::Done { .. }))
            .count() as u32;
        (done, children.len() as u32)
    }

    /// Resumo curto da sessão para as pills e a moldura, com o estado que
    /// define a cor. Prioriza o que pede atenção.
    pub fn session_summary(&self, session: SessionId) -> (String, Option<StatusKind>) {
        let c = self.counts(Some(session));
        if c.waiting > 0 {
            (
                format!("{} aguardando você", c.waiting),
                Some(StatusKind::Waiting),
            )
        } else if c.failed > 0 {
            let verb = if c.failed == 1 { "falhou" } else { "falharam" };
            (format!("{} {verb}", c.failed), Some(StatusKind::Failed))
        } else if c.approval > 0 {
            (
                format!("{} aguardando aprovação", c.approval),
                Some(StatusKind::Approval),
            )
        } else if c.running + c.starting > 0 {
            // "N rodando" conta só workers, como o "N agentes" ao lado.
            let workers = self
                .agents_of(session)
                .iter()
                .filter(|a| {
                    !a.is_orchestrator()
                        && matches!(a.kind(), StatusKind::Running | StatusKind::Starting)
                })
                .count();
            let text = if workers == 0 {
                "orquestrador rodando".to_owned()
            } else {
                format!("{workers} rodando")
            };
            (text, Some(StatusKind::Running))
        } else if c.queued > 0 {
            (format!("{} na fila", c.queued), Some(StatusKind::Queued))
        } else if c.done > 0 {
            ("concluída".to_owned(), Some(StatusKind::Done))
        } else {
            ("vazia".to_owned(), None)
        }
    }

    /// Agentes na ordem em que aparecem no canvas (sessões em ordem, e dentro
    /// delas orquestrador primeiro). Base da navegação Ctrl+Shift+↑/↓.
    pub fn canvas_order(&self, focus: Option<SessionId>) -> Vec<AgentId> {
        let mut out = Vec::new();
        for session in self.sessions() {
            if focus.is_some_and(|f| f != session.id) {
                continue;
            }
            let agents = self.agents_of(session.id);
            out.extend(agents.iter().filter(|a| a.is_orchestrator()).map(|a| a.id));
            out.extend(agents.iter().filter(|a| !a.is_orchestrator()).map(|a| a.id));
        }
        out
    }
}

/// "1 agente", "2 agentes"...
pub fn plural(n: u32, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orquestra_core::{AgentKind, BrowserEndpoint, TermSize, Workspace};

    fn session_created(name: &str) -> (SessionId, AgentEvent) {
        let id = SessionId::generate();
        let config = SessionConfig::new(name, "/tmp/repo", "objetivo");
        (
            id,
            AgentEvent::SessionCreated {
                id,
                config: Arc::new(config),
            },
        )
    }

    fn created(
        session: SessionId,
        name: &str,
        role: AgentRole,
        parent: Option<AgentId>,
    ) -> (AgentId, AgentEvent) {
        let id = AgentId::generate();
        let config = AgentConfig {
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
            initial_prompt: None,
            browser: false,
            size: TermSize::default(),
        };
        (
            id,
            AgentEvent::Created {
                id,
                config: Arc::new(config),
            },
        )
    }

    fn status(id: AgentId, status: AgentStatus) -> AgentEvent {
        AgentEvent::StatusChanged { id, status }
    }

    /// Sessão com orquestrador e três workers em estados variados.
    fn sample() -> (Model, SessionId, AgentId, Vec<AgentId>) {
        let mut m = Model::new(8);
        let (s, ev) = session_created("loja");
        m.apply(&ev);
        let (orq, ev) = created(s, "orquestrador", AgentRole::Orchestrator, None);
        m.apply(&ev);
        m.apply(&status(orq, AgentStatus::Running));
        let mut workers = Vec::new();
        for (name, st) in [
            ("login-ui", AgentStatus::WaitingForInput),
            ("api", AgentStatus::Done { exit_code: Some(0) }),
            (
                "docs",
                AgentStatus::Queued {
                    reason: QueueReason::Approval,
                },
            ),
        ] {
            let (w, ev) = created(s, name, AgentRole::Worker, Some(orq));
            m.apply(&ev);
            m.apply(&status(w, st));
            workers.push(w);
        }
        (m, s, orq, workers)
    }

    #[test]
    fn created_links_orchestrator_and_children() {
        let (m, s, orq, workers) = sample();
        let session = m.session(s).unwrap();
        assert_eq!(session.orchestrator, Some(orq));
        assert_eq!(session.agents.len(), 4);
        let children: Vec<_> = m.children(orq).iter().map(|a| a.id).collect();
        assert_eq!(children, workers);
        assert_eq!(m.edges().len(), 3);
        assert!(m.edges().iter().all(|(p, _)| *p == orq));
    }

    #[test]
    fn counts_and_attention_queue() {
        let (m, s, orq, workers) = sample();
        let c = m.counts(None);
        assert_eq!(c.workers, 3);
        assert_eq!((c.running, c.waiting, c.done, c.approval), (1, 1, 1, 1));
        assert_eq!(c.attention(), 2);
        assert_eq!(m.counts(Some(s)), c);

        let att = m.attention();
        assert_eq!(att.len(), 2);
        assert_eq!(att[0].agent, workers[0]);
        assert_eq!(att[0].kind, AttentionKind::Input);
        assert_eq!(att[1].kind, AttentionKind::Approval);
        assert_eq!(m.progress(orq), (1, 3));
        // Só o worker aguardando ocupa vaga; orquestrador não conta.
        assert_eq!(m.slots_used(), 1);
        assert_eq!(m.session_summary(s).0, "1 aguardando você");
    }

    #[test]
    fn running_summary_counts_workers_only() {
        let mut m = Model::new(8);
        let (s, ev) = session_created("loja");
        m.apply(&ev);
        let (orq, ev) = created(s, "orquestrador", AgentRole::Orchestrator, None);
        m.apply(&ev);
        m.apply(&status(orq, AgentStatus::Running));
        assert_eq!(m.session_summary(s).0, "orquestrador rodando");
        let (w, ev) = created(s, "w", AgentRole::Worker, Some(orq));
        m.apply(&ev);
        m.apply(&status(w, AgentStatus::Running));
        assert_eq!(m.session_summary(s).0, "1 rodando");
    }

    #[test]
    fn failed_shows_in_queue_and_summary() {
        let (mut m, s, _, workers) = sample();
        m.apply(&status(workers[0], AgentStatus::Running));
        m.apply(&status(
            workers[1],
            AgentStatus::Failed {
                reason: "exit 1".to_owned(),
            },
        ));
        let kinds: Vec<_> = m.attention().iter().map(|a| a.kind).collect();
        assert_eq!(kinds, [AttentionKind::Failure, AttentionKind::Approval]);
        assert_eq!(
            m.session_summary(s),
            ("1 falhou".to_owned(), Some(StatusKind::Failed))
        );
    }

    #[test]
    fn removed_and_session_closed_forget_agents() {
        let (mut m, s, orq, workers) = sample();
        m.apply(&AgentEvent::Removed { id: workers[1] });
        assert!(m.agent(workers[1]).is_none());
        assert_eq!(m.children(orq).len(), 2);

        m.apply(&AgentEvent::Removed { id: orq });
        assert_eq!(m.session(s).unwrap().orchestrator, None);
        // Filhos sem pai no modelo não geram aresta.
        assert!(m.edges().is_empty());

        m.apply(&AgentEvent::SessionClosed { id: s });
        assert!(m.session(s).is_none());
        assert_eq!(m.agent_count(), 0);
        // `Removed` atrasado depois do fechamento é inofensivo.
        m.apply(&AgentEvent::Removed { id: workers[0] });
    }

    #[test]
    fn browser_status_and_navigation() {
        let (mut m, _, _, workers) = sample();
        let id = workers[0];
        m.apply(&AgentEvent::BrowserStatus {
            id,
            status: BrowserStatus::Launching,
        });
        assert_eq!(m.browsers_open(), 1);
        m.apply(&AgentEvent::BrowserStatus {
            id,
            status: BrowserStatus::Ready {
                endpoint: BrowserEndpoint {
                    port: 41745,
                    ws_url: String::new(),
                },
            },
        });
        assert_eq!(m.agent(id).unwrap().browser.port(), Some(41745));
        m.apply(&AgentEvent::BrowserNavigated {
            id,
            url: "http://localhost:5173".to_owned(),
            title: "Loja".to_owned(),
        });
        assert_eq!(m.agent(id).unwrap().page.as_ref().unwrap().1, "Loja");
        m.apply(&AgentEvent::BrowserStatus {
            id,
            status: BrowserStatus::Closed,
        });
        assert_eq!(m.browsers_open(), 0);
    }

    #[test]
    fn errors_are_kept_and_bounded() {
        let (mut m, _, _, workers) = sample();
        for i in 0..30 {
            m.apply(&AgentEvent::Error {
                id: Some(workers[0]),
                message: format!("e{i}"),
            });
        }
        assert_eq!(m.errors.len(), MAX_ERRORS);
        assert_eq!(
            m.agent(workers[0]).unwrap().last_error.as_deref(),
            Some("e29")
        );
        m.apply(&AgentEvent::Shutdown);
        assert!(!m.hub_alive);
    }

    #[test]
    fn canvas_order_puts_orchestrator_first_and_respects_focus() {
        let (mut m, s, orq, workers) = sample();
        let (other, ev) = session_created("blog");
        m.apply(&ev);
        let (solo, ev) = created(other, "solo", AgentRole::Worker, None);
        m.apply(&ev);
        let order = m.canvas_order(None);
        assert_eq!(order[0], orq);
        assert_eq!(&order[1..4], &workers[..]);
        assert_eq!(order[4], solo);
        assert_eq!(m.canvas_order(Some(other)), [solo]);
        assert_eq!(m.canvas_order(Some(s)).len(), 4);
    }

    #[test]
    fn labels_in_portuguese() {
        assert_eq!(
            status_label(&AgentStatus::Done { exit_code: None }),
            "encerrado"
        );
        assert_eq!(
            status_label(&AgentStatus::Done { exit_code: Some(2) }),
            "saiu com 2"
        );
        assert_eq!(
            status_label(&AgentStatus::WaitingForInput),
            "aguardando você"
        );
        assert_eq!(plural(1, "agente", "agentes"), "1 agente");
        assert_eq!(plural(3, "agente", "agentes"), "3 agentes");
    }
}
