//! Montagem dos comandos que a UI manda (puro, sem egui): nova orquestração,
//! agente novo numa pasta, agente avulso numa sessão e mensagem ao terminal.

use std::path::Path;

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

/// Onde um agente criado pela UI trabalha: direto em `folder` ou, com
/// `isolate` (só numa pasta git), num worktree e branch próprios.
pub fn workspace(folder: &Path, isolate: bool) -> Workspace {
    if isolate {
        Workspace::Worktree {
            repo: folder.to_owned(),
            base: None,
            branch: None,
        }
    } else {
        Workspace::Directory {
            path: folder.to_owned(),
        }
    }
}

/// Um agente sozinho numa pasta, como uma conversa no app do Claude: uma
/// sessão só dele (`CreateSession`) e o agente (`Spawn`), nesta ordem.
pub fn new_agent(
    folder: &Path,
    preset: &AgentPreset,
    task: Option<String>,
    browser: bool,
    isolate: bool,
    size: TermSize,
) -> (SessionId, AgentId, [Command; 2]) {
    let task = task.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
    let session_id = SessionId::generate();
    let session = SessionConfig::new(
        session_name(folder, task.as_deref().unwrap_or("")),
        folder,
        task.clone().unwrap_or_default(),
    );
    let agent_id = AgentId::generate();
    let mut agent = preset.to_config(
        agent_name(preset),
        session_id,
        workspace(folder, isolate),
        size,
    );
    agent.initial_prompt = task;
    agent.browser = browser;
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

/// "Claude Code" → "claude-code" (o nome entra na branch do worktree).
fn agent_name(preset: &AgentPreset) -> String {
    let name: String = preset
        .name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let name = name.trim_matches('-');
    if name.is_empty() {
        "agente".to_owned()
    } else {
        name.to_owned()
    }
}

/// Worker avulso (`parent: None`) numa sessão que já existe: na pasta dela
/// ou, com `isolate`, num worktree novo dela.
#[allow(clippy::too_many_arguments)]
pub fn solo_agent(
    session_id: SessionId,
    session: &SessionConfig,
    preset: &AgentPreset,
    name: String,
    task: Option<String>,
    browser: bool,
    isolate: bool,
    size: TermSize,
) -> (AgentId, Command) {
    let id = AgentId::generate();
    let mut config: AgentConfig =
        preset.to_config(name, session_id, workspace(&session.repo, isolate), size);
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

/// Preset do orquestrador: o escolhido na config ou, se ele sumiu, o primeiro
/// que pode orquestrar.
pub fn default_orchestrator(config: &AppConfig) -> Option<&AgentPreset> {
    let presets = orchestrator_presets(config);
    presets
        .iter()
        .find(|p| p.name == config.orchestration.orchestrator_preset)
        .or(presets.first())
        .copied()
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

    /// Agente sozinho numa pasta: uma sessão só dele, sem orquestrador; sem
    /// tarefa, ele só abre (como uma conversa nova).
    #[test]
    fn new_agent_gets_a_session_of_its_own() {
        let config = AppConfig::default();
        let folder = std::path::Path::new("/home/x/notas");
        let (sid, aid, [create, spawn]) = new_agent(
            folder,
            &config.presets[0],
            Some("  organize os PDFs  ".to_owned()),
            false,
            false,
            TermSize::default(),
        );
        let Command::CreateSession {
            id,
            config: session,
        } = create
        else {
            panic!("esperava CreateSession");
        };
        assert_eq!(id, sid);
        assert_eq!(session.name, "notas · organize os pdfs");
        assert_eq!(session.repo, folder);
        let Command::Spawn { id, config: agent } = spawn else {
            panic!("esperava Spawn");
        };
        assert_eq!(id, aid);
        assert_eq!(agent.name, "claude-code");
        assert_eq!(agent.session, sid);
        assert_eq!(agent.role, AgentRole::Worker);
        assert_eq!(agent.parent, None);
        assert_eq!(agent.initial_prompt.as_deref(), Some("organize os PDFs"));
        assert_eq!(
            agent.workspace,
            Workspace::Directory {
                path: folder.into()
            }
        );

        let (_, _, [create, spawn]) = new_agent(
            folder,
            &config.presets[1],
            None,
            true,
            true,
            TermSize::default(),
        );
        assert!(matches!(create, Command::CreateSession { config, .. } if config.name == "notas"));
        let Command::Spawn { config: agent, .. } = spawn else {
            panic!("esperava Spawn");
        };
        assert_eq!(agent.initial_prompt, None);
        assert!(agent.browser);
        assert!(matches!(agent.workspace, Workspace::Worktree { .. }));
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
        assert_eq!(
            session_name("/home/x/loja".as_ref(), "Login com e-mail"),
            "loja · login com e-mail"
        );
        assert_eq!(session_name("/r/blog".as_ref(), ""), "blog");
        let config = AppConfig::default();
        assert_eq!(orchestrator_presets(&config).len(), 2);
    }
}
