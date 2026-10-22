//! Montagem do comando de cada tipo de agente: argumentos da CLI (MCP, hooks
//! de status, permissões, prompt inicial) e ambiente. Puro, sem efeitos.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use orquestra_core::conventions::{self, env, osc};
use orquestra_core::{AgentKind, AgentRole, McpServerSpec, SpawnRequest};
use serde_json::{Map, Value, json};

/// Variáveis herdadas do app que descrevem o terminal *dele*, não o PTY do
/// agente. Sem elas os programas não emitem sequências de outro terminal
/// (passthrough de tmux, gráficos do kitty...).
const INHERITED_ENV_REMOVE: &[&str] = &[
    // O Claude Code se recusa a abrir dentro de outra sessão dele.
    "CLAUDECODE",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "VTE_VERSION",
    "KITTY_WINDOW_ID",
    "KITTY_PID",
    "KITTY_LISTEN_ON",
    "WEZTERM_PANE",
    "WEZTERM_EXECUTABLE",
    "WEZTERM_UNIX_SOCKET",
    "ITERM_SESSION_ID",
    "WT_SESSION",
    "WT_PROFILE_ID",
    "TMUX",
    "TMUX_PANE",
    "STY",
    "COLUMNS",
    "LINES",
];

/// Matcher do hook `Notification` do Claude Code: só os tipos que esperam o
/// humano (`auth_success` e afins não contam).
const CLAUDE_WAITING_NOTIFICATIONS: &str = "permission_prompt|idle_prompt|elicitation_dialog";

/// O que o runner executa no PTY.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Aplicadas por cima do ambiente herdado do app.
    pub env: BTreeMap<String, String>,
    /// Removidas do ambiente herdado antes de aplicar `env`.
    pub env_remove: Vec<String>,
    /// Escrito no PTY logo após o start (prompt inicial de Shell/Custom).
    pub startup_input: Option<Vec<u8>>,
}

/// Traduz o pedido de spawn no comando de cada CLI:
/// - Claude Code: `--session-id <id do agente>` (ou `--resume`, reabrindo),
///   `--mcp-config`, `--settings` (hooks de status), `--allowedTools
///   mcp__orquestra` (orquestrador) e o prompt após `--`;
/// - Codex: `resume --last` (reabrindo), `-c mcp_servers.<nome>.*`, `-c
///   notify=[…]`, notificações OSC 9 da TUI e o prompt após `--`;
/// - Shell/Custom: só o ambiente; o prompt vai para o PTY seguido de `\r`.
///
/// Reabrindo (`Workspace::Restore`), a tarefa inicial não vai de novo: a
/// conversa retomada já a tem.
pub fn build_command(req: &SpawnRequest, workdir: &Path) -> CommandSpec {
    let config = &req.config;
    let resume = config.workspace.resumes();
    let prompt = config
        .initial_prompt
        .as_deref()
        .filter(|p| !p.is_empty() && !resume);
    let mut args = Vec::new();
    if resume && config.kind == AgentKind::Codex {
        // Subcomando: antes dos argumentos do preset. A sessão mais recente
        // da pasta (o Codex não deixa escolher o id ao criar).
        args.extend(["resume".to_owned(), "--last".to_owned()]);
    }
    args.extend(config.args.iter().cloned());
    let mut startup_input = None;
    match config.kind {
        AgentKind::ClaudeCode => args.extend(claude_args(req, prompt, resume)),
        AgentKind::Codex => args.extend(codex_args(req, prompt)),
        AgentKind::Shell | AgentKind::Custom => {
            startup_input = prompt.map(|p| format!("{p}\r").into_bytes());
        }
    }
    CommandSpec {
        program: config.program.clone(),
        args,
        cwd: workdir.to_owned(),
        env: agent_env(req, workdir),
        env_remove: INHERITED_ENV_REMOVE
            .iter()
            .map(|k| (*k).to_owned())
            .collect(),
        startup_input,
    }
}

/// `TERM`, `COLORTERM`, `PWD`, as variáveis de `conventions::env`, depois
/// `config.env` e por fim `req.env` (cada camada sobrescreve a anterior).
fn agent_env(req: &SpawnRequest, workdir: &Path) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    // Senão o shell herda o diretório lógico do app.
    vars.insert("PWD".to_owned(), workdir.to_string_lossy().into_owned());
    vars.insert("TERM".to_owned(), "xterm-256color".to_owned());
    vars.insert("COLORTERM".to_owned(), "truecolor".to_owned());
    vars.insert(env::AGENT_ID.to_owned(), req.id.to_string());
    vars.insert(env::SESSION_ID.to_owned(), req.config.session.to_string());
    if let Some(browser) = &req.browser {
        vars.insert(env::CDP_URL.to_owned(), browser.http_url());
        vars.insert(env::CDP_PORT.to_owned(), browser.port.to_string());
    }
    vars.extend(req.config.env.clone());
    vars.extend(req.env.clone());
    vars
}

fn claude_args(req: &SpawnRequest, prompt: Option<&str>, resume: bool) -> Vec<String> {
    // A conversa tem o id do agente: assim dá para retomá-la depois de um
    // reinício sem guardar mais nada.
    let mut args = vec![
        if resume { "--resume" } else { "--session-id" }.to_owned(),
        req.id.to_string(),
    ];
    if !req.mcp_servers.is_empty() {
        args.push("--mcp-config".to_owned());
        args.push(claude_mcp_config(&req.mcp_servers));
    }
    args.push("--settings".to_owned());
    args.push(claude_settings(req.stop_hook.as_deref()));
    if req.config.role == AgentRole::Orchestrator {
        // `mcp__<servidor>` libera todas as ferramentas do servidor.
        args.push("--allowedTools".to_owned());
        args.push(format!("mcp__{}", conventions::MCP_SERVER_NAME));
    }
    if let Some(prompt) = prompt {
        // `--mcp-config` e `--allowedTools` são variádicos: sem `--` o prompt
        // viraria mais um valor deles.
        args.push("--".to_owned());
        args.push(prompt.to_owned());
    }
    args
}

/// `{"mcpServers": {nome: {command, args, env}}}`.
fn claude_mcp_config(servers: &[McpServerSpec]) -> String {
    let mut map = Map::new();
    for server in servers {
        map.insert(
            server.name.clone(),
            json!({
                "command": server.command,
                "args": server.args,
                "env": server.env,
            }),
        );
    }
    json!({ "mcpServers": map }).to_string()
}

/// Hooks de status: `Stop` e `Notification` → `waiting` (o `Stop` usa o
/// `stop_hook` do orquestrador quando houver); `UserPromptSubmit` e
/// `PostToolUse` → `running` (o segundo cobre a volta ao trabalho depois de
/// uma permissão concedida, que não dispara `UserPromptSubmit`).
fn claude_settings(stop_hook: Option<&str>) -> String {
    let waiting = osc_command(osc::WAITING);
    let running = osc_command(osc::RUNNING);
    let stop = stop_hook.map_or_else(|| waiting.clone(), str::to_owned);
    let hook = |command: &str, matcher: Option<&str>| -> Value {
        let mut group = Map::new();
        if let Some(matcher) = matcher {
            group.insert("matcher".to_owned(), json!(matcher));
        }
        group.insert(
            "hooks".to_owned(),
            json!([{ "type": "command", "command": command }]),
        );
        json!([group])
    };
    json!({
        "hooks": {
            "Stop": hook(&stop, None),
            "Notification": hook(&waiting, Some(CLAUDE_WAITING_NOTIFICATIONS)),
            "UserPromptSubmit": hook(&running, None),
            "PostToolUse": hook(&running, None),
        }
    })
    .to_string()
}

/// Teto das chamadas ao MCP `orquestra` no Codex: acima do maior `wait_for`
/// razoável (o padrão da ferramenta é 600 s).
const ORQUESTRA_TOOL_TIMEOUT_SECS: u32 = 3_700;

fn codex_args(req: &SpawnRequest, prompt: Option<&str>) -> Vec<String> {
    let orchestrator = req.config.role == AgentRole::Orchestrator;
    let mut args = Vec::new();
    let mut set = |key: &str, value: String| {
        args.push("-c".to_owned());
        args.push(format!("{key}={value}"));
    };
    for server in &req.mcp_servers {
        let base = format!("mcp_servers.{}", codex_server_name(&server.name));
        set(&format!("{base}.command"), toml_string(&server.command));
        set(&format!("{base}.args"), toml_array(&server.args));
        if !server.env.is_empty() {
            set(&format!("{base}.env"), toml_table(&server.env));
        }
        if orchestrator && server.name == conventions::MCP_SERVER_NAME {
            // Equivalente ao `--allowedTools` do Claude Code.
            set(
                &format!("{base}.default_tools_approval_mode"),
                toml_string("approve"),
            );
            // O padrão do Codex (300 s) cortaria um `wait_for` longo.
            set(
                &format!("{base}.tool_timeout_sec"),
                ORQUESTRA_TOOL_TIMEOUT_SECS.to_string(),
            );
        }
    }
    // `notify` roda ao fim de cada turno, com o JSON do evento como último
    // argumento (aqui vira `$1`, ignorado).
    let notify = [
        "sh".to_owned(),
        "-c".to_owned(),
        osc_command(osc::WAITING),
        "orquestra-notify".to_owned(),
    ];
    set("notify", toml_array(&notify));
    // A TUI avisa por OSC 9 também quando pede aprovação (o `notify` não).
    set("tui.notifications", "true".to_owned());
    set("tui.notification_method", toml_string("osc9"));
    set("tui.notification_condition", toml_string("always"));
    if let Some(prompt) = prompt {
        args.push("--".to_owned());
        args.push(prompt.to_owned());
    }
    args
}

/// O Codex divide a chave do `-c` em `.` sem tirar aspas: o nome do servidor
/// precisa ser uma chave TOML nua.
fn codex_server_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        "mcp".to_owned()
    } else {
        clean
    }
}

/// Comando de shell que escreve `seq` no PTY do agente, onde o runner a lê:
/// o caminho vem de `ORQUESTRA_TTY` (hooks do Claude Code não têm `/dev/tty`).
/// Silencioso e com status 0 mesmo sem TTY, para o hook nunca virar erro.
pub(crate) fn osc_command(seq: &str) -> String {
    let mut format = String::new();
    for byte in seq.bytes() {
        match byte {
            b'\\' => format.push_str("\\\\"),
            b'%' => format.push_str("%%"),
            b'\'' => format.push_str("'\\''"),
            0x20..=0x7e => format.push(char::from(byte)),
            _ => {
                let _ = write!(format, "\\{byte:03o}");
            }
        }
    }
    format!(
        "{{ printf '{format}' > \"${{{}:-/dev/tty}}\"; }} 2>/dev/null || true",
        env::TTY
    )
}

/// String básica TOML entre aspas, com escapes.
pub(crate) fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn toml_key(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        key.to_owned()
    } else {
        toml_string(key)
    }
}

fn toml_array<S: AsRef<str>>(items: &[S]) -> String {
    let items: Vec<String> = items.iter().map(|s| toml_string(s.as_ref())).collect();
    format!("[{}]", items.join(", "))
}

fn toml_table(map: &BTreeMap<String, String>) -> String {
    let pairs: Vec<String> = map
        .iter()
        .map(|(k, v)| format!("{} = {}", toml_key(k), toml_string(v)))
        .collect();
    format!("{{{}}}", pairs.join(", "))
}

#[cfg(test)]
mod tests {
    use orquestra_core::{AgentConfig, AgentId, BrowserEndpoint, SessionId, TermSize, Workspace};

    use super::*;

    fn request(kind: AgentKind, program: &str) -> SpawnRequest {
        SpawnRequest {
            id: AgentId::generate(),
            config: AgentConfig {
                name: "agente".to_owned(),
                kind,
                program: program.to_owned(),
                args: Vec::new(),
                env: BTreeMap::new(),
                session: SessionId::generate(),
                role: AgentRole::Worker,
                parent: None,
                workspace: Workspace::Directory {
                    path: PathBuf::from("/tmp"),
                },
                initial_prompt: None,
                browser: false,
                size: TermSize::default(),
            },
            browser: None,
            mcp_servers: Vec::new(),
            env: BTreeMap::new(),
            stop_hook: None,
        }
    }

    fn server(name: &str) -> McpServerSpec {
        McpServerSpec {
            name: name.to_owned(),
            command: "/usr/bin/orquestra".to_owned(),
            args: vec!["mcp".to_owned(), "--x=\"y\"".to_owned()],
            env: BTreeMap::from([("ORQUESTRA_TOKEN".to_owned(), "t\\o\"k".to_owned())]),
        }
    }

    /// Valor de `flag` (o argumento seguinte).
    fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let i = args.iter().position(|a| a == flag)?;
        args.get(i + 1).map(String::as_str)
    }

    fn json_arg(args: &[String], flag: &str) -> Value {
        serde_json::from_str(flag_value(args, flag).expect(flag)).expect("JSON válido")
    }

    /// Valores dos `-c chave=valor` do Codex, já interpretados como TOML.
    fn codex_overrides(args: &[String]) -> BTreeMap<String, toml::Value> {
        let mut out = BTreeMap::new();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            if arg == "-c" {
                let kv = it.next().expect("valor do -c");
                let (key, value) = kv.split_once('=').expect("chave=valor");
                let doc: toml::Table =
                    toml::from_str(&format!("v = {value}")).expect("TOML válido");
                out.insert(key.to_owned(), doc["v"].clone());
            }
        }
        out
    }

    #[test]
    fn osc_command_escapes_the_sequence_for_printf() {
        assert_eq!(
            osc_command(osc::WAITING),
            "{ printf '\\033]777;orquestra;waiting\\007' > \"${ORQUESTRA_TTY:-/dev/tty}\"; } 2>/dev/null || true"
        );
        assert!(osc_command(osc::RUNNING).contains("orquestra;running\\007"));
        assert_eq!(
            osc_command("a%b\\c'd"),
            "{ printf 'a%%b\\\\c'\\''d' > \"${ORQUESTRA_TTY:-/dev/tty}\"; } 2>/dev/null || true"
        );
    }

    #[test]
    fn env_layers_conventions_then_config_then_request() {
        let mut req = request(AgentKind::Shell, "bash");
        req.browser = Some(BrowserEndpoint {
            port: 9333,
            ws_url: "ws://127.0.0.1:9333/devtools/browser/x".to_owned(),
        });
        req.config.env.insert("A".to_owned(), "config".to_owned());
        req.config.env.insert("B".to_owned(), "config".to_owned());
        req.env.insert("B".to_owned(), "hub".to_owned());
        let spec = build_command(&req, Path::new("/w"));

        assert_eq!(spec.env["TERM"], "xterm-256color");
        assert_eq!(spec.env["COLORTERM"], "truecolor");
        assert_eq!(spec.env[env::AGENT_ID], req.id.to_string());
        assert_eq!(spec.env[env::SESSION_ID], req.config.session.to_string());
        assert_eq!(spec.env[env::CDP_URL], "http://127.0.0.1:9333");
        assert_eq!(spec.env[env::CDP_PORT], "9333");
        assert_eq!(spec.env["A"], "config");
        assert_eq!(spec.env["B"], "hub");
        assert!(spec.env_remove.iter().any(|k| k == "CLAUDECODE"));
        assert!(spec.env_remove.iter().any(|k| k == "TMUX"));
        assert_eq!(spec.cwd, PathBuf::from("/w"));
        assert_eq!(spec.env["PWD"], "/w");
    }

    #[test]
    fn no_browser_means_no_cdp_vars() {
        let spec = build_command(&request(AgentKind::Shell, "bash"), Path::new("/w"));
        assert!(!spec.env.contains_key(env::CDP_URL));
        assert!(!spec.env.contains_key(env::CDP_PORT));
    }

    #[test]
    fn shell_gets_prompt_on_the_pty_and_no_extra_args() {
        let mut req = request(AgentKind::Shell, "bash");
        req.config.args = vec!["-l".to_owned()];
        req.config.initial_prompt = Some("ls -la".to_owned());
        req.mcp_servers.push(server("browser"));
        let spec = build_command(&req, Path::new("/w"));
        assert_eq!(spec.program, "bash");
        assert_eq!(spec.args, ["-l"]);
        assert_eq!(spec.startup_input.as_deref(), Some(&b"ls -la\r"[..]));

        req.config.kind = AgentKind::Custom;
        req.config.initial_prompt = Some(String::new());
        let spec = build_command(&req, Path::new("/w"));
        assert_eq!(spec.startup_input, None);
    }

    #[test]
    fn claude_worker_gets_mcp_config_hooks_and_prompt() {
        let mut req = request(AgentKind::ClaudeCode, "claude");
        req.config.args = vec!["--model".to_owned(), "opus".to_owned()];
        req.config.initial_prompt = Some("-faça o login".to_owned());
        req.mcp_servers.push(server("browser"));
        let spec = build_command(&req, Path::new("/w"));
        let args = &spec.args;

        assert_eq!(&args[..2], ["--model", "opus"]);
        assert_eq!(&args[args.len() - 2..], ["--", "-faça o login"]);
        assert!(!args.iter().any(|a| a == "--allowedTools"));
        assert_eq!(spec.startup_input, None);

        let mcp = json_arg(args, "--mcp-config");
        let browser = &mcp["mcpServers"]["browser"];
        assert_eq!(browser["command"], "/usr/bin/orquestra");
        assert_eq!(browser["args"], json!(["mcp", "--x=\"y\""]));
        assert_eq!(browser["env"]["ORQUESTRA_TOKEN"], "t\\o\"k");

        let settings = json_arg(args, "--settings");
        let hooks = &settings["hooks"];
        let command = |event: &str| hooks[event][0]["hooks"][0]["command"].as_str().unwrap();
        for event in ["Stop", "Notification", "UserPromptSubmit", "PostToolUse"] {
            assert_eq!(hooks[event][0]["hooks"][0]["type"], "command", "{event}");
        }
        assert_eq!(command("Stop"), osc_command(osc::WAITING));
        assert_eq!(command("Notification"), osc_command(osc::WAITING));
        assert_eq!(command("UserPromptSubmit"), osc_command(osc::RUNNING));
        assert_eq!(command("PostToolUse"), osc_command(osc::RUNNING));
        assert_eq!(
            hooks["Notification"][0]["matcher"],
            CLAUDE_WAITING_NOTIFICATIONS
        );
        assert!(hooks["Stop"][0].get("matcher").is_none());
    }

    #[test]
    fn claude_without_mcp_or_prompt_has_session_id_and_settings() {
        let req = request(AgentKind::ClaudeCode, "claude");
        let spec = build_command(&req, Path::new("/w"));
        assert_eq!(spec.args.len(), 4);
        assert_eq!(&spec.args[..2], ["--session-id", &req.id.to_string()]);
        assert_eq!(spec.args[2], "--settings");
        assert!(!spec.args.iter().any(|a| a == "--"));
    }

    /// Reabrindo: a conversa é retomada pelo id do agente (Claude Code) ou a
    /// mais recente da pasta (Codex), e a tarefa inicial não vai de novo.
    #[test]
    fn restore_resumes_the_conversation_without_the_first_task() {
        let restore = Workspace::Restore {
            path: PathBuf::from("/w"),
            worktree: None,
        };
        let mut req = request(AgentKind::ClaudeCode, "claude");
        req.config.args = vec!["--model".to_owned(), "opus".to_owned()];
        req.config.initial_prompt = Some("faça o login".to_owned());
        req.config.workspace = restore.clone();
        let spec = build_command(&req, Path::new("/w"));
        let id = req.id.to_string();
        assert_eq!(&spec.args[..4], ["--model", "opus", "--resume", &id]);
        assert!(!spec.args.iter().any(|a| a == "--session-id"));
        assert!(!spec.args.iter().any(|a| a == "faça o login"));

        req.config.kind = AgentKind::Codex;
        req.config.program = "codex".to_owned();
        let spec = build_command(&req, Path::new("/w"));
        assert_eq!(&spec.args[..4], ["resume", "--last", "--model", "opus"]);
        assert!(!spec.args.iter().any(|a| a == "faça o login"));

        req.config.kind = AgentKind::Shell;
        let spec = build_command(&req, Path::new("/w"));
        assert_eq!(spec.startup_input, None);
    }

    #[test]
    fn claude_orchestrator_uses_stop_hook_and_allows_orquestra_tools() {
        let mut req = request(AgentKind::ClaudeCode, "claude");
        req.config.role = AgentRole::Orchestrator;
        req.config.initial_prompt = Some("coordene".to_owned());
        req.mcp_servers.push(server(conventions::MCP_SERVER_NAME));
        req.stop_hook = Some("/usr/bin/orquestra hook stop".to_owned());
        let spec = build_command(&req, Path::new("/w"));

        let settings = json_arg(&spec.args, "--settings");
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/usr/bin/orquestra hook stop"
        );
        assert_eq!(
            settings["hooks"]["Notification"][0]["hooks"][0]["command"],
            osc_command(osc::WAITING)
        );
        assert_eq!(
            flag_value(&spec.args, "--allowedTools"),
            Some("mcp__orquestra")
        );
        let mcp = json_arg(&spec.args, "--mcp-config");
        assert!(mcp["mcpServers"]["orquestra"].is_object());
        assert_eq!(&spec.args[spec.args.len() - 2..], ["--", "coordene"]);
    }

    #[test]
    fn codex_gets_mcp_overrides_notify_and_prompt() {
        let mut req = request(AgentKind::Codex, "codex");
        req.config.initial_prompt = Some("corrija o teste".to_owned());
        let mut weird = server("meu.browser");
        weird
            .env
            .insert("chave estranha".to_owned(), "a\nb".to_owned());
        req.mcp_servers.push(weird);
        let mut plain = server("plain");
        plain.env.clear();
        req.mcp_servers.push(plain);
        let spec = build_command(&req, Path::new("/w"));
        let o = codex_overrides(&spec.args);

        let base = "mcp_servers.meu_browser";
        assert_eq!(
            o[&format!("{base}.command")].as_str(),
            Some("/usr/bin/orquestra")
        );
        let args: Vec<&str> = o[&format!("{base}.args")]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(args, ["mcp", "--x=\"y\""]);
        let env = o[&format!("{base}.env")].as_table().unwrap();
        assert_eq!(env["ORQUESTRA_TOKEN"].as_str(), Some("t\\o\"k"));
        assert_eq!(env["chave estranha"].as_str(), Some("a\nb"));
        assert!(!o.contains_key("mcp_servers.plain.env"));
        assert!(!o.contains_key(&format!("{base}.default_tools_approval_mode")));

        let notify: Vec<&str> = o["notify"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(notify[..2], ["sh", "-c"]);
        assert_eq!(notify[2], osc_command(osc::WAITING));
        assert_eq!(o["tui.notifications"].as_bool(), Some(true));
        assert_eq!(o["tui.notification_method"].as_str(), Some("osc9"));
        assert_eq!(o["tui.notification_condition"].as_str(), Some("always"));

        assert_eq!(&spec.args[spec.args.len() - 2..], ["--", "corrija o teste"]);
        assert!(!spec.args.iter().any(|a| a == "--settings"));
    }

    #[test]
    fn codex_orchestrator_auto_approves_orquestra_tools() {
        let mut req = request(AgentKind::Codex, "codex");
        req.config.role = AgentRole::Orchestrator;
        req.mcp_servers.push(server(conventions::MCP_SERVER_NAME));
        req.mcp_servers.push(server("browser"));
        let o = codex_overrides(&build_command(&req, Path::new("/w")).args);
        assert_eq!(
            o["mcp_servers.orquestra.default_tools_approval_mode"].as_str(),
            Some("approve")
        );
        assert!(!o.contains_key("mcp_servers.browser.default_tools_approval_mode"));
        assert_eq!(
            o["mcp_servers.orquestra.tool_timeout_sec"].as_integer(),
            Some(i64::from(ORQUESTRA_TOOL_TIMEOUT_SECS))
        );
        assert!(!o.contains_key("mcp_servers.browser.tool_timeout_sec"));
    }

    #[test]
    fn toml_strings_escape_everything_toml_forbids() {
        let nasty = "a\"b\\c\nd\te\u{7f}f\u{1}g é";
        let doc: toml::Table = toml::from_str(&format!("v = {}", toml_string(nasty))).unwrap();
        assert_eq!(doc["v"].as_str(), Some(nasty));
        assert_eq!(toml_key("ok_key-1"), "ok_key-1");
        assert_eq!(toml_key("a.b"), "\"a.b\"");
        assert_eq!(codex_server_name("a.b c"), "a_b_c");
        assert_eq!(codex_server_name(""), "mcp");
    }
}
