//! Reabrir um agente retoma a conversa dele, se ela existe. O Claude Code só
//! grava a conversa depois da primeira mensagem: um agente aberto sem tarefa
//! e fechado sem conversar não tem o que retomar, e `--resume` sairia com
//! erro ("No conversation found with session ID").

use std::io::ErrorKind;
use std::path::PathBuf;

use orquestra_core::{AgentKind, SpawnRequest};

/// Variável que muda a pasta de config (e das conversas) do Claude Code.
const CLAUDE_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";

/// Se há conversa para retomar. Só o Claude Code é verificado: o Codex
/// escolhe a mais recente da pasta, e os outros não têm conversa.
pub fn has_conversation(req: &SpawnRequest) -> bool {
    match req.config.kind {
        AgentKind::ClaudeCode => claude_has_conversation(req),
        AgentKind::Codex | AgentKind::Shell | AgentKind::Custom => true,
    }
}

/// `<config>/projects/<pasta>/<id do agente>.jsonl`, em qualquer pasta (o
/// nome dela é o caminho com outros caracteres; procurar evita reproduzir a
/// regra). Na dúvida, tenta retomar.
fn claude_has_conversation(req: &SpawnRequest) -> bool {
    // Mesma ordem do ambiente do agente: o pedido, o preset, o do app.
    let dir = req
        .env
        .get(CLAUDE_CONFIG_DIR)
        .or_else(|| req.config.env.get(CLAUDE_CONFIG_DIR))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os(CLAUDE_CONFIG_DIR).map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")));
    let Some(dir) = dir else {
        return true;
    };
    let file = format!("{}.jsonl", req.id);
    match std::fs::read_dir(dir.join("projects")) {
        Ok(projects) => projects
            .flatten()
            .any(|project| project.path().join(&file).is_file()),
        Err(err) => err.kind() != ErrorKind::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use orquestra_core::{AgentConfig, AgentId, AgentRole, SessionId, TermSize, Workspace};

    use super::*;

    fn request(kind: AgentKind, config_dir: &std::path::Path) -> SpawnRequest {
        SpawnRequest {
            id: AgentId::generate(),
            config: AgentConfig {
                name: "agente".to_owned(),
                kind,
                program: "claude".to_owned(),
                args: Vec::new(),
                env: BTreeMap::from([(
                    CLAUDE_CONFIG_DIR.to_owned(),
                    config_dir.display().to_string(),
                )]),
                session: SessionId::generate(),
                role: AgentRole::Orchestrator,
                parent: None,
                workspace: Workspace::Restore {
                    path: PathBuf::from("/w"),
                    worktree: None,
                },
                initial_prompt: None,
                browser: false,
                size: TermSize::default(),
            },
            browser: None,
            mcp_servers: Vec::new(),
            env: BTreeMap::new(),
            stop_hook: None,
            instructions: None,
        }
    }

    #[test]
    fn only_a_saved_claude_conversation_can_be_resumed() {
        let tmp = tempfile::tempdir().unwrap();
        let req = request(AgentKind::ClaudeCode, tmp.path());
        // Nunca abriu o Claude Code com esta config: nada para retomar.
        assert!(!has_conversation(&req));

        let project = tmp.path().join("projects").join("-w");
        std::fs::create_dir_all(&project).unwrap();
        let other = AgentId::generate();
        std::fs::write(project.join(format!("{other}.jsonl")), "{}").unwrap();
        assert!(
            !has_conversation(&req),
            "a conversa de outro agente não conta"
        );

        std::fs::write(project.join(format!("{}.jsonl", req.id)), "{}").unwrap();
        assert!(has_conversation(&req));

        // O Codex decide sozinho (`resume --last`).
        let codex = request(AgentKind::Codex, tmp.path());
        assert!(has_conversation(&codex));
    }
}
