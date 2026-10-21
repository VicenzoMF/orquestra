# orquestra

App desktop leve (Rust + egui) para rodar e acompanhar agentes de código
(Claude Code, Codex) em paralelo. Um agente orquestrador recebe um objetivo e
cria os outros pelo MCP `orquestra`. Cada agente roda num git worktree próprio
e pode ter um Chromium só dele, visível e interagível na UI.

Arquitetura e decisões: `docs/ARCHITECTURE.md`. Tarefas: `docs/TASKS.md`.

## Rodar

```sh
cargo run --release -p orquestra-app                     # modo real
cargo run --release -p orquestra-app -- --mock           # UI com agentes simulados
cargo run --release -p orquestra-app -- --repo ~/projetos/loja --goal "implemente o login"
```

Config em `~/.config/orquestra/config.toml`. Dados (SQLite e worktrees) em
`~/.local/share/orquestra/`. Os subcomandos `orquestra mcp` e
`orquestra hook stop` são chamados pelos agentes, não por você.

Requisitos: `git`, Chromium ou Chrome (para o navegador dos agentes), `npx`
(para o Playwright MCP) e os CLIs `claude` e/ou `codex`.

## Testar

```sh
cargo test --workspace                                        # 256 testes
cargo test -p orquestra-app --test e2e -- --ignored --nocapture   # ponta a ponta com PTY, git e Chromium reais
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check
```
