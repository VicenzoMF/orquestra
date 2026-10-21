# Tarefas

Legenda: `[x]` feito · `[~]` em andamento · `[ ]` pendente

## Fase 0 — contrato (sequencial)
- [x] Workspace Cargo compilando; versões centralizadas; Rust 1.99.1 fixado
- [x] Avaliar `egui_term` → não usar como dependência; renderer próprio
- [x] `core` v1: tipos, eventos, comandos, `EventBus`, traits, `Hub`
- [x] Layout v1 (lista + terminal + painel) → recusado
- [x] Layout v2: canvas com orquestrador ligado aos agentes; duas orquestrações → aprovado
- [x] `core` v2: sessões, papéis/`parent`, fila (limite global/sessão, aprovação),
      `OrchestratorGateway`, MCP specs, convenções; diff removido; 24 testes
- [x] `docs/ARCHITECTURE.md` v2
- [x] Commit inicial em `main` (base dos worktrees da Fase 1)

## Fase 1 — paralela
Worktrees em `../orquestra-worktrees/<crate>`, branches `phase1/<crate>`.

- [~] A · runner — PTY, I/O, status por OSC/heurística, worktrees, injeção de MCP/hooks
- [~] B · browser — Chromium por agente, CDP, screencast, input, teste com página local
- [~] C · app — canvas, painel lateral, terminal, navegador, diálogos; sobre o Hub com mocks
- [~] D · store — SQLite, histórico, log de saída, config TOML, notificações
- [~] E · mcp — gateway (socket + tokens), ferramentas, ponte stdio, hook `stop`

## Fase 2 — integração (sequencial)
- [ ] Merge das 5 branches
- [ ] `app/main.rs` com os serviços reais; subcomandos `mcp` e `hook`; `--mock` mantido
- [ ] E2E: orquestração com orquestrador real criando 3 agentes em worktrees; terminais,
      status, aprovação, navegador de um deles interagível
- [ ] Medir RAM (VmRSS/VmHWM) e tempo de abertura sem navegador
