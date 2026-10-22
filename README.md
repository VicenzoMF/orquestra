# orquestra

App desktop leve (Rust + egui) para rodar e acompanhar agentes de código
(Claude Code, Codex) em paralelo. Você abre um agente em qualquer pasta, como
uma conversa; ele pode criar outros agentes pelo MCP `orquestra` e decide
sozinho quando vale dividir o trabalho. Cada agente roda num git worktree
próprio (numa pasta git) ou direto na pasta e pode ter um Chromium só dele,
visível e interagível na UI.

Arquitetura e decisões: `docs/ARCHITECTURE.md`. Tarefas: `docs/TASKS.md`.

## Instalar e abrir

```sh
cargo install --path crates/app --locked   # instala `orq` (e o apelido `orquestra`) em ~/.cargo/bin
```

Como o `code .` do VS Code:

```sh
orq .                                # abre a pasta atual (rascunho de agente no canvas)
orq ~/projetos/loja -g "implemente o login"   # já abre um agente com essa tarefa
orq                                  # só abre (ou traz para a frente) a janela
orq --mock                           # UI com agentes simulados
```

O `orq` devolve o terminal na hora: a janela roda desprendida, com o log em
`~/.local/share/orquestra/orquestra.log` (`--foreground` deixa no terminal).
Há uma janela só: com o app aberto, o `orq` entrega o pedido a ela. Qualquer
pasta serve; dentro de um repositório git, o `orq` abre a raiz dele.

O que estiver aberto volta quando você abrir o app de novo, tenha ele sido
fechado ou o PC desligado: cada agente reabre onde estava e retoma a conversa
(Claude Code pelo id da conversa; Codex, a mais recente da pasta). Uma sessão
só some quando você a encerra.

Config em `~/.config/orquestra/config.toml`. Dados (SQLite e worktrees) em
`~/.local/share/orquestra/`. Os subcomandos `orq mcp` e `orq hook stop` são
chamados pelos agentes, não por você.

Requisitos: `git`, Chromium ou Chrome (para o navegador dos agentes), `npx`
(para o Playwright MCP) e os CLIs `claude` e/ou `codex`.

## Testar

```sh
cargo test --workspace
cargo test -p orquestra-app --test e2e -- --ignored --nocapture   # ponta a ponta com PTY, git e Chromium reais
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check
```
