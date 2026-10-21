//! Montagem dos comandos que a UI manda (puro, sem egui): nova orquestração,
//! agente avulso, respostas rápidas e mensagem ao orquestrador.

use orquestra_core::bytes::Bytes;
use orquestra_core::{
    AgentConfig, AgentId, AgentKind, AgentPreset, AgentRole, AppConfig, Command, SessionConfig,
    SessionId, TermSize, Workspace,
};

/// `CreateSession` + `Spawn` do orquestrador, nesta ordem (o hub processa em
/// ordem, então a sessão existe quando o spawn chega).
pub fn new_orchestration(
    config: &AppConfig,
    session: SessionConfig,
    preset: &AgentPreset,
    size: TermSize,
) -> (SessionId, AgentId, [Command; 2]) {
    let session_id = SessionId::generate();
    let agent_id = AgentId::generate();
    let mut agent = preset.to_config(
        "orquestrador".to_owned(),
        session_id,
        Workspace::Directory {
            path: session.repo.clone(),
        },
        size,
    );
    agent.role = AgentRole::Orchestrator;
    agent.browser = false;
    agent.initial_prompt = Some(config.orchestration.render_prompt(&session));
    let commands = [
        Command::CreateSession {
            id: session_id,
            config: Box::new(session),
        },
        Command::Spawn {
            id: agent_id,
            config: Box::new(agent),
        },
    ];
    (session_id, agent_id, commands)
}

/// Worker avulso (`parent: None`) num worktree novo do repositório da sessão.
pub fn solo_agent(
    session_id: SessionId,
    session: &SessionConfig,
    preset: &AgentPreset,
    name: String,
    task: Option<String>,
    browser: bool,
    size: TermSize,
) -> (AgentId, Command) {
    let id = AgentId::generate();
    let mut config: AgentConfig = preset.to_config(
        name,
        session_id,
        Workspace::Worktree {
            repo: session.repo.clone(),
            base: None,
            branch: None,
        },
        size,
    );
    config.initial_prompt = task.filter(|t| !t.trim().is_empty());
    config.browser = browser;
    (
        id,
        Command::Spawn {
            id,
            config: Box::new(config),
        },
    )
}

/// Texto + Enter (`\r`), como se o humano digitasse no terminal.
pub fn message(text: &str) -> Bytes {
    Bytes::from(format!("{}\r", text.trim_end()))
}

/// Bytes de "Sim" e "Não" para um pedido de permissão, por tipo de CLI.
/// Claude Code: Enter confirma a opção destacada, Esc recusa. Codex: `y`/Esc.
pub fn quick_replies(kind: AgentKind) -> (Bytes, Bytes) {
    match kind {
        AgentKind::ClaudeCode => (Bytes::from_static(b"\r"), Bytes::from_static(b"\x1b")),
        AgentKind::Codex => (Bytes::from_static(b"y"), Bytes::from_static(b"\x1b")),
        AgentKind::Shell | AgentKind::Custom => {
            (Bytes::from_static(b"s\r"), Bytes::from_static(b"n\r"))
        }
    }
}

/// Nome padrão de uma sessão: "<pasta do repo> · <objetivo curto>".
pub fn session_name(repo: &std::path::Path, goal: &str) -> String {
    let base = repo
        .file_name()
        .map_or_else(|| "sessão".to_owned(), |n| n.to_string_lossy().into_owned());
    let goal = goal.lines().next().unwrap_or("").trim();
    let short: String = goal.chars().take(32).collect();
    let short = if goal.chars().count() > 32 {
        format!("{}…", short.trim_end())
    } else {
        short
    };
    if short.is_empty() {
        base
    } else {
        format!("{base} · {}", short.to_lowercase())
    }
}

/// Presets que podem ser orquestrador (precisam do MCP: Claude Code ou Codex).
pub fn orchestrator_presets(config: &AppConfig) -> Vec<&AgentPreset> {
    config
        .presets
        .iter()
        .filter(|p| matches!(p.kind, AgentKind::ClaudeCode | AgentKind::Codex))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_orchestration_creates_session_then_orchestrator() {
        let config = AppConfig::default();
        let session = SessionConfig::new("loja · login", "/src/loja", "login com e-mail");
        let preset = &config.presets[0];
        let (sid, aid, [create, spawn]) =
            new_orchestration(&config, session, preset, TermSize::default());
        assert!(matches!(create, Command::CreateSession { id, .. } if id == sid));
        let Command::Spawn { id, config: agent } = spawn else {
            panic!("esperava Spawn");
        };
        assert_eq!(id, aid);
        assert_eq!(agent.session, sid);
        assert_eq!(agent.role, AgentRole::Orchestrator);
        assert_eq!(agent.parent, None);
        assert_eq!(
            agent.workspace,
            Workspace::Directory {
                path: "/src/loja".into()
            }
        );
        assert!(agent.initial_prompt.unwrap().contains("login com e-mail"));
    }

    #[test]
    fn solo_agent_uses_a_worktree_of_the_session_repo() {
        let config = AppConfig::default();
        let session = SessionConfig::new("loja", "/src/loja", "x");
        let sid = SessionId::generate();
        let (id, cmd) = solo_agent(
            sid,
            &session,
            &config.presets[1],
            "revisor".to_owned(),
            Some("revise o PR".to_owned()),
            true,
            TermSize::default(),
        );
        let Command::Spawn {
            id: i,
            config: agent,
        } = cmd
        else {
            panic!("esperava Spawn");
        };
        assert_eq!(i, id);
        assert_eq!(agent.role, AgentRole::Worker);
        assert_eq!(agent.parent, None);
        assert!(agent.browser);
        assert_eq!(agent.kind, AgentKind::Codex);
        assert!(
            matches!(agent.workspace, Workspace::Worktree { ref repo, .. } if repo.as_os_str() == "/src/loja")
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(&message("oi  ")[..], b"oi\r");
        assert_eq!(&quick_replies(AgentKind::ClaudeCode).1[..], b"\x1b");
        assert_eq!(
            session_name("/home/x/loja".as_ref(), "Login com e-mail"),
            "loja · login com e-mail"
        );
        assert_eq!(session_name("/r/blog".as_ref(), ""), "blog");
        let config = AppConfig::default();
        assert_eq!(orchestrator_presets(&config).len(), 2);
    }
}
