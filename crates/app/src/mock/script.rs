//! Roteiros de saída dos agentes falsos, no estilo do Claude Code e do Codex.

use orquestra_core::{AgentConfig, AgentKind, AgentRole};

use super::director::plan_for;

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const GREEN: &str = "\x1b[32m";
pub const RED: &str = "\x1b[31m";
pub const YELLOW: &str = "\x1b[33m";
pub const CYAN: &str = "\x1b[36m";
/// Laranja do Claude Code (truecolor).
pub const CLAUDE: &str = "\x1b[38;2;215;119;87m";
/// Cinza 256 cores.
pub const GRAY: &str = "\x1b[38;5;245m";

#[derive(Clone, Debug)]
pub enum Step {
    /// Escreve no terminal (já com `\r\n` onde precisar).
    Out(String),
    /// Pausa em milissegundos (multiplicada pela escala do runner).
    Sleep(u64),
    /// Spinner animado com rótulo, por `ms` milissegundos.
    Spin(&'static str, u64),
    /// Pergunta e espera uma linha: `WaitingForInput` até o Enter. Se a
    /// resposta for "não", escreve `on_no`.
    Ask { prompt: String, on_no: String },
    /// Termina o processo com este código.
    Exit(i32),
    /// Fica `Running` sem saída (como um orquestrador bloqueado em
    /// `wait_for`), respondendo só ao que for digitado.
    Hold,
}

use Step::*;

fn out(s: impl Into<String>) -> Step {
    Out(s.into())
}

fn line(s: impl AsRef<str>) -> Step {
    Out(format!("{}\r\n", s.as_ref()))
}

/// Ferramenta no estilo do Claude Code: `● Nome(args)` + resultado indentado.
fn tool(name: &str, args: &str, result: &str) -> Vec<Step> {
    vec![
        line(format!("{GREEN}●{RESET} {BOLD}{name}{RESET}({args})")),
        line(format!("  {GRAY}└{RESET} {result}")),
        Sleep(500),
    ]
}

fn say(text: &str) -> Step {
    line(format!("{BOLD}●{RESET} {text}"))
}

fn claude_banner(cwd: &str, prompt: &str) -> Vec<Step> {
    let width: usize = 52;
    let row = |text: &str| {
        let pad = (width - 3).saturating_sub(visible_len(text));
        format!(
            "{CLAUDE}│{RESET} {text}{}{CLAUDE}│{RESET}\r\n",
            " ".repeat(pad)
        )
    };
    let bar = "─".repeat(width - 2);
    let cwd: String = cwd.chars().take(width - 10).collect();
    vec![
        out(format!("{CLAUDE}╭{bar}╮{RESET}\r\n")),
        out(row(&format!(
            "{CLAUDE}✻{RESET} Welcome to {BOLD}Claude Code{RESET}!"
        ))),
        out(row("")),
        out(row(&format!(
            "{GRAY}  /help for help, /status for your setup{RESET}"
        ))),
        out(row(&format!("{GRAY}  cwd: {cwd}{RESET}"))),
        out(format!("{CLAUDE}╰{bar}╯{RESET}\r\n\r\n")),
        // Terminais reais perguntam a posição do cursor ao abrir.
        out("\x1b[6n"),
        line(format!("{GRAY}>{RESET} {}", first_line(prompt, 70))),
        line(""),
    ]
}

fn codex_banner(cwd: &str, prompt: &str) -> Vec<Step> {
    vec![
        line(format!(
            "{BOLD}>_ OpenAI Codex{RESET} {GRAY}(v0.46.0){RESET}"
        )),
        line(format!(
            "{GRAY}model:{RESET} gpt-5-codex   {GRAY}directory:{RESET} {cwd}"
        )),
        line(""),
        out("\x1b[6n"),
        line(format!("{CYAN}›{RESET} {}", first_line(prompt, 70))),
        line(""),
    ]
}

fn codex(title: &str, detail: &str) -> Vec<Step> {
    vec![
        line(format!("{BOLD}•{RESET} {title}")),
        line(format!("  {GRAY}└{RESET} {detail}")),
        Sleep(600),
    ]
}

/// Largura visível, sem as sequências SGR (`ESC [ ... m`).
fn visible_len(text: &str) -> usize {
    let mut n = 0;
    let mut in_esc = false;
    for c in text.chars() {
        match (in_esc, c) {
            (false, '\x1b') => in_esc = true,
            (true, 'm') => in_esc = false,
            (true, _) => {}
            (false, _) => n += 1,
        }
    }
    n
}

fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    if line.chars().count() > max {
        let cut: String = line.chars().take(max - 1).collect();
        format!("{cut}…")
    } else {
        line.to_owned()
    }
}

/// Nome da sessão a partir do prompt do orquestrador (`... sessão "nome" ...`).
fn session_name(prompt: &str) -> Option<&str> {
    let start = prompt.find('"')? + 1;
    let end = start + prompt[start..].find('"')?;
    Some(&prompt[start..end])
}

pub fn script_for(config: &AgentConfig, cwd: &str) -> Vec<Step> {
    let prompt = config.initial_prompt.clone().unwrap_or_default();
    if config.kind == AgentKind::Shell || config.kind == AgentKind::Custom {
        return vec![line(format!("{GREEN}mock{RESET}:{CYAN}{cwd}{RESET}$ "))];
    }
    let mut steps = match config.kind {
        AgentKind::Codex => codex_banner(cwd, &prompt),
        _ => claude_banner(cwd, &prompt),
    };
    if config.role == AgentRole::Orchestrator {
        steps.extend(orchestrator(config.kind, &prompt));
        return steps;
    }
    steps.extend(worker(config.kind, &config.name));
    steps
}

fn orchestrator(kind: AgentKind, prompt: &str) -> Vec<Step> {
    let plan = session_name(prompt).map(plan_for).unwrap_or_default();
    let mut s = vec![Spin("Planejando", 2000)];
    s.push(say(&format!(
        "Vou dividir o objetivo em {} tarefas independentes, uma por agente:",
        plan.len()
    )));
    for (i, w) in plan.iter().enumerate() {
        s.push(line(format!(
            "  {}. {BOLD}{}{RESET} — {}",
            i + 1,
            w.name,
            w.task
        )));
    }
    s.push(line(""));
    for (i, w) in plan.iter().enumerate() {
        if i > 0 {
            s.push(Sleep(1500));
        }
        let browser = if w.browser { ", browser: true" } else { "" };
        match kind {
            AgentKind::Codex => s.extend(codex(
                &format!("Called orquestra.spawn_agent({}{browser})", w.name),
                "agente criado",
            )),
            _ => s.extend(tool(
                "orquestra - spawn_agent",
                &format!("name: \"{}\"{browser}", w.name),
                "agente criado",
            )),
        }
    }
    s.push(line(""));
    match kind {
        AgentKind::Codex => s.extend(codex(
            "Called orquestra.wait_for(until: attention)",
            "acompanhando os agentes…",
        )),
        _ => s.push(line(format!(
            "{GREEN}●{RESET} {BOLD}orquestra - wait_for{RESET}(until: \"attention\", timeout_secs: 600)"
        ))),
    }
    s.push(Sleep(45_000));
    s.push(say(
        "Parte das tarefas está pronta; sigo acompanhando as demais.",
    ));
    s.push(Hold);
    s
}

fn worker(kind: AgentKind, name: &str) -> Vec<Step> {
    let codex_kind = kind == AgentKind::Codex;
    match name {
        "login-ui" => {
            let mut s = vec![Spin("Pensando", 1500), say("Vou ler as rotas existentes.")];
            s.extend(tool("Read", "src/routes/login.tsx", "Leu 84 linhas"));
            s.extend(tool(
                "Update",
                "src/routes/login.tsx",
                &format!("{GREEN}+18{RESET} {RED}-3{RESET}"),
            ));
            s.push(line(format!(
                "      {GREEN}+ const [email, setEmail] = useState(\"\");{RESET}"
            )));
            s.push(line(format!(
                "      {GREEN}+ const [senha, setSenha] = useState(\"\");{RESET}"
            )));
            s.push(Spin("Escrevendo testes", 1500));
            s.push(line(""));
            s.push(Ask {
                prompt: format!(
                    "{YELLOW}●{RESET} Bash(npm install zod @hookform/resolvers)\r\nPermitir npm install? (s/n) "
                ),
                on_no: "Ok, sigo sem dependências novas.".to_owned(),
            });
            s.extend(tool(
                "Bash",
                "npm install zod @hookform/resolvers",
                "added 3 packages in 2s",
            ));
            s.push(Spin("Finalizando", 2500));
            s.push(say("Tela de login pronta: e-mail e senha com validação."));
            s.push(Exit(0));
            s
        }
        "api" => {
            let mut s = codex("Explorando src/server", "Listed 12 files");
            s.extend(codex(
                "Edited src/server/produtos.ts",
                &format!("{GREEN}+64{RESET} {RED}-0{RESET}"),
            ));
            s.push(Sleep(800));
            s.extend(codex(
                "Ran npm test -- produtos",
                &format!("{GREEN}✓ 14 passed{RESET}"),
            ));
            s.push(line(format!("{GRAY}─ Worked for 0m 06s ─{RESET}")));
            s.push(Exit(0));
            s
        }
        "qa-navegador" => {
            let mut s = vec![Spin("Pensando", 1200)];
            s.extend(tool(
                "browser - browser_navigate (MCP)",
                "url: \"http://localhost:5173/login\"",
                "Página carregada: Entrar · Loja",
            ));
            s.extend(tool(
                "browser - browser_snapshot (MCP)",
                "",
                "2 campos, 1 botão",
            ));
            s.push(Spin("Testando o login", 9000));
            s.extend(tool(
                "browser - browser_click (MCP)",
                "element: \"Entrar\"",
                &format!("{YELLOW}O botão não mostra erro com campos vazios{RESET}"),
            ));
            s.push(Spin("Testando a listagem", 20_000));
            s.push(Ask {
                prompt: "Encontrei 1 problema no botão Entrar. Abrir uma issue? (s/n) ".to_owned(),
                on_no: "Ok, só registro no relatório.".to_owned(),
            });
            s.push(say("Relatório de QA salvo em docs/qa.md."));
            s.push(Exit(0));
            s
        }
        "docs" => {
            let mut s = vec![Spin("Pensando", 1500)];
            s.extend(tool("Read", "README.md", "Leu 120 linhas"));
            s.push(Spin("Escrevendo", 8000));
            s.extend(tool("Update", "README.md", &format!("{GREEN}+42{RESET}")));
            s.push(Exit(0));
            s
        }
        "migra-posts" => {
            let mut s = codex("Explorando _posts/", "Listed 48 files");
            for n in [8, 16, 24, 32, 40, 48] {
                s.push(line(format!("{BOLD}•{RESET} Migrando posts {n}/48")));
                s.push(Sleep(3500));
            }
            s.extend(codex(
                "Ran npm run build",
                &format!("{GREEN}✓ built in 3.1s{RESET}"),
            ));
            s.push(Exit(0));
            s
        }
        "estilos" => {
            let mut s = vec![Spin("Pensando", 1500)];
            s.extend(tool(
                "Bash",
                "npx astro add tailwind",
                "instalando integração…",
            ));
            s.push(Sleep(1500));
            s.push(line(format!(
                "{RED}Error: tailwind.config.js não encontrado em /src{RESET}"
            )));
            s.push(line(format!("{RED}✕ A integração falhou (exit 1){RESET}")));
            s.push(Exit(1));
            s
        }
        "links" => {
            let mut s = vec![Spin("Pensando", 1500)];
            s.extend(tool("Grep", "\"](/\" em src/", "37 links internos"));
            s.push(Spin("Corrigindo links", 7000));
            s.push(Exit(0));
            s
        }
        other => generic(other, codex_kind),
    }
}

/// Roteiro de agentes sem cenário: o desfecho sai do nome.
fn generic(name: &str, codex_kind: bool) -> Vec<Step> {
    let h = name
        .bytes()
        .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(u32::from(b)));
    let mut s = if codex_kind {
        codex("Explorando o repositório", "Listed 20 files")
    } else {
        let mut s = vec![Spin("Pensando", 1500)];
        s.extend(tool("Read", "src/", "Leu 6 arquivos"));
        s
    };
    match h % 4 {
        0 => {
            s.push(Spin("Trabalhando", 5000));
            s.push(say("Tarefa concluída."));
            s.push(Exit(0));
        }
        1 => {
            s.push(Spin("Trabalhando", 3000));
            s.push(Ask {
                prompt: "Continuar com a refatoração? (s/n) ".to_owned(),
                on_no: "Ok, paro aqui.".to_owned(),
            });
            s.push(Spin("Trabalhando", 3000));
            s.push(Exit(0));
        }
        2 => {
            s.push(Spin("Trabalhando", 10_000));
            s.push(say("Tarefa concluída."));
            s.push(Exit(0));
        }
        _ => {
            s.push(Spin("Trabalhando", 3000));
            s.push(line(format!(
                "{RED}Error: testes falharam (3 de 20){RESET}"
            )));
            s.push(Exit(1));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_name_comes_from_prompt() {
        assert_eq!(
            session_name("Você é o orquestrador da sessão \"loja · x\" do orquestra."),
            Some("loja · x")
        );
        assert_eq!(session_name("sem aspas"), None);
    }

    #[test]
    fn visible_len_ignores_sgr() {
        assert_eq!(visible_len(&format!("{BOLD}abc{RESET}")), 3);
    }

    #[test]
    fn every_worker_script_ends() {
        for name in [
            "login-ui",
            "api",
            "qa-navegador",
            "docs",
            "migra-posts",
            "estilos",
            "links",
            "x",
            "y",
            "z",
            "w",
        ] {
            let steps = worker(AgentKind::ClaudeCode, name);
            assert!(matches!(steps.last(), Some(Exit(_))), "{name}");
        }
    }
}
