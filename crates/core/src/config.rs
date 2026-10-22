use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    AgentConfig, AgentKind, AgentRole, McpServerTemplate, SessionConfig, SessionId, TermSize,
    Viewport, Workspace,
};

/// `config.toml` do app. Toda seção tem default: um arquivo parcial (ou
/// vazio) é válido.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub ui: UiConfig,
    pub runner: RunnerConfig,
    pub browser: BrowserConfig,
    pub notifications: NotificationConfig,
    pub orchestration: OrchestrationConfig,
    pub presets: Vec<AgentPreset>,
    /// Repositórios usados recentemente (o diálogo "nova orquestração" sugere).
    pub recent_repos: Vec<PathBuf>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            ui: UiConfig::default(),
            runner: RunnerConfig::default(),
            browser: BrowserConfig::default(),
            notifications: NotificationConfig::default(),
            orchestration: OrchestrationConfig::default(),
            presets: AgentPreset::defaults(),
            recent_repos: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub font_size: f32,
    /// Linhas de scrollback por terminal. É o maior consumidor de RAM do app
    /// (~cols × 24 bytes por linha).
    pub scrollback_lines: usize,
    pub theme: Theme,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            font_size: 13.0,
            scrollback_lines: 3_000,
            theme: Theme::Dark,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunnerConfig {
    /// Onde criar os worktrees. `None` = `<data_dir>/worktrees`.
    pub worktree_root: Option<PathBuf>,
    pub branch_prefix: String,
    /// Bytes de saída guardados por agente para `Replay`.
    pub replay_bytes: usize,
    /// Sem saída por este tempo, o runner considera o agente ocioso (base da
    /// heurística de `WaitingForInput`).
    pub idle_ms: u64,
    /// Tempo entre SIGTERM e SIGKILL no `kill`.
    pub kill_grace_ms: u64,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            worktree_root: None,
            branch_prefix: "orquestra/".to_owned(),
            replay_bytes: 1 << 20,
            idle_ms: 1_500,
            kill_grace_ms: 3_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    /// Chromium/Chrome a usar. `None` = procurar no PATH e em locais conhecidos.
    pub executable: Option<PathBuf>,
    /// Headless por padrão: a UI mostra o screencast, nenhuma janela abre.
    pub headless: bool,
    pub viewport: Viewport,
    /// Qualidade JPEG do screencast (0–100).
    pub screencast_quality: u8,
    pub screencast_every_nth_frame: u32,
    pub extra_args: Vec<String>,
    /// MCP que o runner registra no agente para ele usar o navegador.
    /// `None` = só as variáveis de ambiente são injetadas.
    pub agent_mcp: Option<McpServerTemplate>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            executable: None,
            headless: true,
            viewport: Viewport::default(),
            screencast_quality: 70,
            screencast_every_nth_frame: 1,
            extra_args: Vec::new(),
            agent_mcp: Some(McpServerTemplate::playwright()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationConfig {
    pub enabled: bool,
    pub on_waiting_for_input: bool,
    pub on_done: bool,
    pub on_failed: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            on_waiting_for_input: true,
            on_done: true,
            on_failed: true,
        }
    }
}

/// Limites e as instruções de quem pode criar agentes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OrchestrationConfig {
    /// Agentes criados por outros, vivos ao mesmo tempo, somando todas as
    /// sessões. Acima disso o spawn espera na fila (`Queued { Capacity }`).
    /// Os que você abre não contam.
    pub max_concurrent_workers: u32,
    /// Tipo padrão de um agente novo.
    pub orchestrator_preset: String,
    /// Instruções escondidas (no prompt de sistema, fora da conversa) de um
    /// agente que pode criar outros. Placeholders: `{folder}`, `{name}`.
    pub delegation_prompt: String,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            max_concurrent_workers: 8,
            orchestrator_preset: "Claude Code".to_owned(),
            delegation_prompt: DEFAULT_DELEGATION_PROMPT.to_owned(),
        }
    }
}

impl OrchestrationConfig {
    /// As instruções de delegação para um agente desta sessão.
    pub fn render_delegation(&self, session: &SessionConfig) -> String {
        self.delegation_prompt
            .replace("{folder}", &session.repo.display().to_string())
            .replace("{name}", &session.name)
    }
}

/// Vai para o modelo, não para a pessoa: em inglês, como o resto do que só o
/// modelo lê (instruções e ferramentas do MCP).
const DEFAULT_DELEGATION_PROMPT: &str = "\
You are running inside orquestra, in a session whose folder is {folder}. \
Besides your usual tools, you have the `orquestra` MCP server, which creates \
other agents (Claude Code or Codex) that work in parallel with you: \
spawn_agent, list_agents, send_input, read_output, wait_for and kill_agent.

Do small or sequential work yourself. Delegate when the work splits into \
independent parts that benefit from running in parallel, or when one part is \
big enough to deserve an agent of its own.

When you delegate:
- write each task in full: the agent does not see this conversation;
- in a git folder, use workspace \"worktree\" for agents that will change \
code, so they do not step on each other;
- use workspace \"folder\" (with dir, if it is another folder) to research \
or work on files outside a repository;
- follow up with wait_for and read_output and, at the end, put together what \
each agent delivered.";

/// Modelo de agente oferecido na UI e usado pelo `spawn_agent` do MCP.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPreset {
    pub name: String,
    pub kind: AgentKind,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Marca "dar navegador" por padrão.
    #[serde(default)]
    pub browser: bool,
}

impl AgentPreset {
    pub fn defaults() -> Vec<Self> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
        vec![
            Self::new("Claude Code", AgentKind::ClaudeCode, "claude"),
            Self::new("Codex", AgentKind::Codex, "codex"),
            Self::new("Shell", AgentKind::Shell, &shell),
        ]
    }

    fn new(name: &str, kind: AgentKind, program: &str) -> Self {
        Self {
            name: name.to_owned(),
            kind,
            program: program.to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            browser: false,
        }
    }

    /// Config de um worker avulso; ajuste `role`/`parent` se for o caso.
    pub fn to_config(
        &self,
        name: String,
        session: SessionId,
        workspace: Workspace,
        size: TermSize,
    ) -> AgentConfig {
        AgentConfig {
            name,
            kind: self.kind,
            program: self.program.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            session,
            role: AgentRole::Worker,
            parent: None,
            workspace,
            initial_prompt: None,
            browser: self.browser,
            size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_round_trips_through_toml() {
        let cfg = AppConfig::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn partial_toml_fills_defaults() {
        let cfg: AppConfig = toml::from_str("[ui]\nfont_size = 15.0\n").unwrap();
        assert_eq!(cfg.ui.font_size, 15.0);
        assert_eq!(
            cfg.ui.scrollback_lines,
            UiConfig::default().scrollback_lines
        );
        assert_eq!(cfg.presets, AgentPreset::defaults());
    }

    #[test]
    fn delegation_prompt_fills_the_folder() {
        let session = SessionConfig::new("loja", "/src/loja", "login com e-mail");
        let prompt = OrchestrationConfig::default().render_delegation(&session);
        assert!(prompt.contains("whose folder is /src/loja."));
        assert!(!prompt.contains('{'));
        // A tarefa vai na conversa, não aqui.
        assert!(!prompt.contains("login com e-mail"));
        // O nome antigo do campo (o prompt visível) é ignorado.
        let cfg: AppConfig =
            toml::from_str("[orchestration]\norchestrator_prompt = \"Você é o orquestrador\"\n")
                .unwrap();
        assert_eq!(cfg.orchestration, OrchestrationConfig::default());
    }
}
