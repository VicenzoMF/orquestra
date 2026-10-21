//! Diretor falso: faz o papel do MCP `orquestra`. Quando um orquestrador
//! fica `Running`, manda (com atrasos) os `Spawn` dos workers dele.
//! Também semeia as duas orquestrações de demonstração do modo `--mock`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use orquestra_core::{
    AgentConfig, AgentEvent, AgentId, AgentKind, AgentRole, AgentStatus, AppConfig, Command,
    CommandSender, EventReceiver, QueueReason, SessionConfig, SessionId, TermSize, Workspace,
};
use tokio::sync::broadcast::error::RecvError;

use crate::actions;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerPlan {
    pub name: String,
    pub kind: AgentKind,
    pub task: String,
    pub browser: bool,
}

fn w(name: &str, kind: AgentKind, task: &str, browser: bool) -> WorkerPlan {
    WorkerPlan {
        name: name.to_owned(),
        kind,
        task: task.to_owned(),
        browser,
    }
}

/// Plano de workers de uma sessão, pelo nome.
pub fn plan_for(session_name: &str) -> Vec<WorkerPlan> {
    use AgentKind::{ClaudeCode as Cc, Codex};
    if session_name.starts_with("loja") {
        vec![
            w(
                "login-ui",
                Cc,
                "Implementar a tela de login com e-mail e senha",
                false,
            ),
            w(
                "api",
                Codex,
                "Criar a API de produtos (GET /produtos)",
                false,
            ),
            w(
                "qa-navegador",
                Cc,
                "Testar login e listagem no navegador",
                true,
            ),
            w("docs", Cc, "Documentar as rotas novas no README", false),
        ]
    } else if session_name.starts_with("blog") {
        vec![
            w(
                "migra-posts",
                Codex,
                "Migrar os posts para coleções do Astro",
                false,
            ),
            w(
                "estilos",
                Cc,
                "Portar o tema e os estilos para o Astro",
                false,
            ),
            w(
                "links",
                Cc,
                "Corrigir links internos e redirecionamentos",
                false,
            ),
        ]
    } else {
        vec![
            w(
                "analise",
                Cc,
                "Mapear o código envolvido no objetivo",
                false,
            ),
            w(
                "implementa",
                Codex,
                "Implementar a mudança principal",
                false,
            ),
            w("testes", Cc, "Escrever testes e validar no navegador", true),
        ]
    }
}

fn program(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::ClaudeCode => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Shell | AgentKind::Custom => "sh",
    }
}

fn worker_config(
    plan: &WorkerPlan,
    session_id: SessionId,
    session: &SessionConfig,
    parent: AgentId,
) -> AgentConfig {
    let kind = if session.allowed_kinds.contains(&plan.kind) {
        plan.kind
    } else {
        session.allowed_kinds.first().copied().unwrap_or(plan.kind)
    };
    AgentConfig {
        name: plan.name.clone(),
        kind,
        program: program(kind).to_owned(),
        args: Vec::new(),
        env: Default::default(),
        session: session_id,
        role: AgentRole::Worker,
        parent: Some(parent),
        workspace: Workspace::Worktree {
            repo: session.repo.clone(),
            base: None,
            branch: None,
        },
        initial_prompt: Some(plan.task.clone()),
        browser: plan.browser && session.workers_may_browse,
        size: TermSize::default(),
    }
}

/// Roda até o `Shutdown`. `scale` multiplica os atrasos.
pub async fn run_director(mut rx: EventReceiver, cmd: CommandSender, scale: f64) {
    let mut sessions: HashMap<SessionId, Arc<SessionConfig>> = HashMap::new();
    let mut orchestrators: HashMap<AgentId, SessionId> = HashMap::new();
    let mut started: HashSet<AgentId> = HashSet::new();
    loop {
        let event = match rx.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        };
        match event {
            AgentEvent::SessionCreated { id, config } => {
                sessions.insert(id, config);
            }
            AgentEvent::SessionClosed { id } => {
                sessions.remove(&id);
            }
            AgentEvent::Created { id, config } if config.role == AgentRole::Orchestrator => {
                orchestrators.insert(id, config.session);
            }
            AgentEvent::StatusChanged {
                id,
                status: AgentStatus::Running,
            } if orchestrators.contains_key(&id) && started.insert(id) => {
                let session_id = orchestrators[&id];
                let Some(session) = sessions.get(&session_id).cloned() else {
                    continue;
                };
                let cmd = cmd.clone();
                tokio::spawn(async move {
                    for (i, plan) in plan_for(&session.name).iter().enumerate() {
                        let delay = if i == 0 { 2.6 } else { 1.5 };
                        tokio::time::sleep(Duration::from_secs_f64(delay * scale)).await;
                        let config = worker_config(plan, session_id, &session, id);
                        let spawn = Command::Spawn {
                            id: AgentId::generate(),
                            config: Box::new(config),
                        };
                        if cmd.send(spawn).is_err() {
                            return;
                        }
                    }
                });
            }
            AgentEvent::Removed { id } => {
                orchestrators.remove(&id);
            }
            AgentEvent::Shutdown => break,
            _ => {}
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
}

/// Semeia as duas orquestrações de demonstração. Na do blog, a sessão pede
/// aprovação: as duas primeiras são aprovadas aqui (como se o humano já
/// tivesse decidido) e a terceira fica esperando na UI.
pub async fn seed(cmd: CommandSender, mut rx: EventReceiver, config: AppConfig, scale: f64) {
    let projects = home().join("projetos");
    let preset = |name: &str| {
        config
            .presets
            .iter()
            .find(|p| p.name == name)
            .cloned()
            .unwrap_or_else(|| config.presets[0].clone())
    };

    let loja = SessionConfig {
        max_workers: Some(3),
        ..SessionConfig::new(
            "loja · login + produtos",
            projects.join("loja"),
            "Login com e-mail e página de produtos",
        )
    };
    let (_, _, commands) =
        actions::new_orchestration(&config, loja, &preset("Claude Code"), TermSize::default());
    for c in commands {
        let _ = cmd.send(c);
    }

    let blog = SessionConfig {
        approve_spawns: true,
        ..SessionConfig::new(
            "blog · migração para Astro",
            projects.join("blog"),
            "Migrar o blog de Jekyll para Astro sem quebrar links",
        )
    };
    let (blog_id, _, commands) =
        actions::new_orchestration(&config, blog, &preset("Codex"), TermSize::default());
    for c in commands {
        let _ = cmd.send(c);
    }

    let mut blog_agents = HashSet::new();
    let mut approved = 0;
    while approved < 2 {
        let event = match rx.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        match event {
            AgentEvent::Created { id, config } if config.session == blog_id => {
                blog_agents.insert(id);
            }
            AgentEvent::StatusChanged {
                id,
                status:
                    AgentStatus::Queued {
                        reason: QueueReason::Approval,
                    },
            } if blog_agents.contains(&id) => {
                approved += 1;
                tokio::time::sleep(Duration::from_secs_f64(0.6 * scale)).await;
                let _ = cmd.send(Command::ApproveSpawn { id });
            }
            AgentEvent::Shutdown => return,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_have_unique_names() {
        for name in ["loja · x", "blog · y", "outra"] {
            let plan = plan_for(name);
            let names: HashSet<_> = plan.iter().map(|p| p.name.clone()).collect();
            assert_eq!(names.len(), plan.len());
            assert!((3..=4).contains(&plan.len()));
        }
        assert!(plan_for("loja").iter().any(|p| p.browser));
    }

    #[test]
    fn worker_config_respects_session_rules() {
        let session = SessionConfig {
            allowed_kinds: vec![AgentKind::Codex],
            workers_may_browse: false,
            ..SessionConfig::new("loja", "/src/loja", "g")
        };
        let parent = AgentId::generate();
        let plan = &plan_for("loja")[2];
        let c = worker_config(plan, SessionId::generate(), &session, parent);
        assert_eq!(c.kind, AgentKind::Codex);
        assert_eq!(c.program, "codex");
        assert!(!c.browser);
        assert_eq!(c.parent, Some(parent));
        assert_eq!(c.role, AgentRole::Worker);
    }
}
