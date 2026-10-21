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

- [x] A · runner — PTY, I/O, status por OSC/heurística, worktrees, injeção de MCP/hooks
- [x] B · browser — Chromium por agente, CDP, screencast, input, teste com página local
- [x] C · app — canvas, painel lateral, terminal, navegador, diálogos; sobre o Hub com mocks
- [x] D · store — SQLite, histórico, log de saída, config TOML, notificações
- [x] E · mcp — gateway (socket + tokens), ferramentas, ponte stdio, hook `stop`

## Fase 2 — integração (sequencial)
- [x] Merge das 5 branches (sem conflitos; 256 testes verdes)
- [x] `app/main.rs` com os serviços reais; subcomandos `mcp` e `hook stop`; `--mock` mantido;
      `--repo/--goal` abre uma orquestração ao iniciar
- [x] Ajustes do Codex: socket/token no env do MCP, `tool_timeout_sec` 3700
- [x] E2E automatizado (`tests/e2e.rs`): 3 agentes em worktrees, navegador com clique,
      orquestrador via gateway e ponte stdio, hook, histórico, shutdown — 1,9 s, estável
- [x] E2E com Claude Code real: orquestrador criou a, b, c pelo MCP; arquivos em worktrees
      próprios; `c` usou Playwright MCP no próprio Chromium e a UI mostrou a página ao vivo
- [x] Medido: primeiro quadro 44 ms; RSS 78,5 MB (28 MB privados, ~37 MB do driver Mesa)

## Próximos passos sugeridos
- [ ] Sim/Não nos cards só em pedido de permissão (hoje aparece em qualquer agente ocioso)
- [ ] Prévia do card ignorar a barra de status/caixa de prompt do Claude Code
- [ ] Evento `Resized` no core para o gateway acompanhar o tamanho do terminal
- [ ] Gateway receber a config salva (presets) sem reiniciar
- [ ] Testar com Codex real; tela de histórico (`LoadHistory`)
