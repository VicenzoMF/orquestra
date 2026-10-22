//! Montagem dos comandos que a UI manda (puro, sem egui): agente novo numa
//! pasta, agente avulso numa sessão e mensagem ao terminal.

use std::path::{Path, PathBuf};

use orquestra_core::bytes::Bytes;
use orquestra_core::{
    AgentConfig, AgentId, AgentKind, AgentPreset, AgentRole, AppConfig, Command, SessionConfig,
    SessionId, TermSize, Workspace,
};

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

/// Um agente novo numa pasta, numa sessão só dele.
#[derive(Clone, Debug)]
pub struct NewAgent {
    pub folder: PathBuf,
    /// A primeira mensagem, como você a escreveu. Sem ela o agente só abre.
    pub task: Option<String>,
    /// Nome da sessão; `None` = "<pasta> · <tarefa curta>".
    pub name: Option<String>,
    pub browser: bool,
    /// Num worktree próprio (só numa pasta git).
    pub isolate: bool,
    /// Pode criar outros agentes: ganha o MCP `orquestra` e as instruções de
    /// delegação, escondidas no prompt de sistema. Só Claude Code e Codex.
    pub delegate: bool,
    /// Regras para os agentes que ele criar.
    pub max_workers: Option<u32>,
    pub allowed_kinds: Vec<AgentKind>,
    pub workers_may_browse: bool,
    pub approve_spawns: bool,
}

impl NewAgent {
    /// Pode delegar; sem worktree, sem navegador; regras padrão da sessão.
    pub fn new(folder: impl Into<PathBuf>, task: Option<String>) -> Self {
        let rules = SessionConfig::new("", "", "");
        Self {
            folder: folder.into(),
            task,
            name: None,
            browser: false,
            isolate: false,
            delegate: true,
            max_workers: rules.max_workers,
            allowed_kinds: rules.allowed_kinds,
            workers_may_browse: rules.workers_may_browse,
            approve_spawns: rules.approve_spawns,
        }
    }
}

/// O tipo pode criar outros agentes (precisa do MCP: Claude Code ou Codex).
pub fn can_delegate(kind: AgentKind) -> bool {
    matches!(kind, AgentKind::ClaudeCode | AgentKind::Codex)
}

/// `CreateSession` e `Spawn` do agente, nesta ordem (o hub processa em
/// ordem, então a sessão existe quando o spawn chega).
pub fn new_agent(
    spec: NewAgent,
    preset: &AgentPreset,
    size: TermSize,
) -> (SessionId, AgentId, [Command; 2]) {
    let task = spec
        .task
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty());
    let name = spec
        .name
        .unwrap_or_else(|| session_name(&spec.folder, task.as_deref().unwrap_or("")));
    let session_id = SessionId::generate();
    let session = SessionConfig {
        max_workers: spec.max_workers,
        allowed_kinds: spec.allowed_kinds,
        workers_may_browse: spec.workers_may_browse,
        approve_spawns: spec.approve_spawns,
        ..SessionConfig::new(name, &spec.folder, task.clone().unwrap_or_default())
    };
    let agent_id = AgentId::generate();
    let mut agent = preset.to_config(
        agent_name(preset),
        session_id,
        workspace(&spec.folder, spec.isolate),
        size,
    );
    if spec.delegate && can_delegate(preset.kind) {
        agent.role = AgentRole::Orchestrator;
    }
    agent.initial_prompt = task;
    agent.browser = spec.browser;
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

/// Tipo padrão de um agente novo: o da config ou, se ele sumiu, o primeiro.
pub fn default_preset(config: &AppConfig) -> Option<&AgentPreset> {
    config
        .presets
        .iter()
        .find(|p| p.name == config.orchestration.orchestrator_preset)
        .or(config.presets.first())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Um agente numa pasta: uma sessão só dele; a primeira mensagem é a
    /// tarefa como foi escrita (as instruções de delegação vão escondidas,
    /// pelo hub). Por padrão ele pode criar outros agentes.
    #[test]
    fn new_agent_gets_a_session_of_its_own() {
        let config = AppConfig::default();
        let folder = std::path::Path::new("/home/x/notas");
        let spec = NewAgent::new(folder, Some("  organize os PDFs  ".to_owned()));
        let (sid, aid, [create, spawn]) = new_agent(spec, &config.presets[0], TermSize::default());
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
        assert_eq!(session.goal, "organize os PDFs");
        let Command::Spawn { id, config: agent } = spawn else {
            panic!("esperava Spawn");
        };
        assert_eq!(id, aid);
        assert_eq!(agent.name, "claude-code");
        assert_eq!(agent.session, sid);
        assert_eq!(agent.role, AgentRole::Orchestrator, "pode delegar");
        assert_eq!(agent.parent, None);
        assert_eq!(agent.initial_prompt.as_deref(), Some("organize os PDFs"));
        assert_eq!(
            agent.workspace,
            Workspace::Directory {
                path: folder.into()
            }
        );

        // Sem tarefa, sem delegar, num worktree e com navegador.
        let spec = NewAgent {
            delegate: false,
            browser: true,
            isolate: true,
            ..NewAgent::new(folder, None)
        };
        let (_, _, [create, spawn]) = new_agent(spec, &config.presets[1], TermSize::default());
        assert!(matches!(create, Command::CreateSession { config, .. } if config.name == "notas"));
        let Command::Spawn { config: agent, .. } = spawn else {
            panic!("esperava Spawn");
        };
        assert_eq!(agent.role, AgentRole::Worker);
        assert_eq!(agent.initial_prompt, None);
        assert!(agent.browser);
        assert!(matches!(agent.workspace, Workspace::Worktree { .. }));

        // Um shell não tem o MCP: não delega mesmo pedindo.
        let shell = config
            .presets
            .iter()
            .find(|p| p.kind == AgentKind::Shell)
            .unwrap();
        let (_, _, [_, spawn]) = new_agent(NewAgent::new(folder, None), shell, TermSize::default());
        assert!(matches!(spawn, Command::Spawn { config, .. } if config.role == AgentRole::Worker));
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
        assert_eq!(default_preset(&config).unwrap().kind, AgentKind::ClaudeCode);
    }
}
