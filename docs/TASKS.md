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

## Depois da Fase 2
- [x] Sim/Não só quando a tela pergunta (permissão, confiança na pasta, `(s/n)`),
      com as teclas tiradas da própria tela; agente que só terminou o turno
      fica com "Abrir terminal"
- [x] Prévia do card sem a caixa de entrada, a statusline e o spinner do Claude Code
- [x] Hooks de status chegam ao runner: escrevem em `ORQUESTRA_TTY`, porque o
      Claude Code roda hooks sem `/dev/tty` (antes o status vinha só da
      heurística de silêncio)

- [x] Comando `orq` como o `code .`: abre o repositório da pasta (diálogo ou, com
      `-g`, a orquestração), desprende do terminal e reaproveita a janela aberta
      (instância única por socket); `orquestra` vira apelido
- [x] Orquestrador parado com filhos vivos fica "aguardando agentes" (sem
      notificação de "aguardando sua resposta")
- [x] Texto do canvas nítido em qualquer zoom (refeito no tamanho da tela)
- [x] Spinner `*` do Claude Code fora da prévia (a linha do card piscava)
- [x] Painel da direita só para o agente selecionado; "Precisa de você",
      "Orquestrações" (botão com lista em cascata) e "Recursos" (uma linha)
      flutuam fixos sobre o canvas

- [x] Qualquer pasta: o `orq` abre pasta sem git, e cada agente trabalha num
      worktree (pasta git) ou direto numa pasta, escolhido por spawn
      (`workspace`, `dir`); o prompt antigo do orquestrador é migrado
- [x] Agente sozinho numa pasta, numa sessão só dele ("+ Novo agente", modo
      Agente do rascunho); "Orquestrações" vira "Sessões"
- [x] O que estava aberto volta ao abrir o app (fechado ou PC desligado): a
      conversa do Claude Code é retomada pelo id do agente (testado com o
      Claude real); Codex por `resume --last`; posições do canvas em
      `canvas.json`

- [x] Sem "orquestrador" à parte: um só "Novo agente", que por padrão pode
      criar outros e decide quando delegar. A primeira mensagem é só a
      tarefa; as instruções de delegação (em inglês) vão escondidas no
      prompt de sistema (`--append-system-prompt` / `developer_instructions`),
      testado com o Claude real. Instruções e ferramentas do MCP em inglês

## Próximos passos sugeridos
- [ ] Testar a reabertura com o Codex real
- [ ] Evento `Resized` no core para o gateway acompanhar o tamanho do terminal
- [ ] Gateway receber a config salva (presets) sem reiniciar
- [ ] Testar com Codex real; tela de histórico (`LoadHistory`)
