# orquestra — arquitetura

App desktop em Rust, leve, para rodar e acompanhar vários agentes de código
(Claude Code, Codex, shell...) ao mesmo tempo. O trabalho é organizado em
**orquestrações**. Em cada uma, um agente **orquestrador** recebe um objetivo,
cria os agentes que executam as tarefas e os acompanha. Tudo aparece num
**canvas**: cada orquestração é um grupo, o orquestrador está ligado por setas
aos agentes que criou. Cada agente roda isolado num git worktree e pode ter
um Chromium próprio, que ele controla via CDP/MCP e que a UI mostra e deixa
interagir.

Layout aprovado (canvas interativo, 4 telas):
https://claude.ai/artifact/5cuERHSaTLUxzXJ6UPsmoa

Estado: Fases 0, 1 e 2 concluídas (MVP integrado). Como rodar: `README.md`.

---

## 1. Princípios

1. **A UI só fala por canais.** `Command` vai por `mpsc`, `AgentEvent` volta
   por `broadcast`. A UI nunca chama runner, browser, store ou mcp.
2. **`core` é o contrato.** Tipos, eventos, comandos, traits e o hub. Nenhuma
   dependência de UI, PTY, CDP, SQLite ou MCP.
3. **O orquestrador é um agente comum.** É um Claude Code ou Codex que recebeu
   o MCP `orquestra`. Ele cria agentes mandando os mesmos `Command`s que a UI
   manda, só que pelo gateway.
4. **Leve por padrão.** eframe com `glow`, Chromium headless e só para quem
   pede, scrollback limitado, screencast desligado fora da tela, limite global
   de agentes simultâneos.
5. **Ordem por agente, paralelismo entre agentes.**

## 2. Workspace

```
orquestra/
├── Cargo.toml              # workspace + versões centralizadas
├── rust-toolchain.toml     # Rust 1.99.1 (egui 0.36+ exige ≥ 1.95)
├── docs/{ARCHITECTURE.md, TASKS.md}
└── crates/
    ├── core/     orquestra-core     contratos + hub (tokio sync, serde, uuid, bytes)
    ├── runner/   orquestra-runner   PTY, status, worktrees, injeção de MCP/hooks (portable-pty, vte)
    ├── browser/  orquestra-browser  Chromium/CDP, screencast, input (chromiumoxide)
    ├── store/    orquestra-store    SQLite, TOML, notificações (rusqlite, toml, notify-rust)
    ├── mcp/      orquestra-mcp      gateway do MCP `orquestra` + ponte stdio e hook (rmcp, alacritty_terminal)
    └── app/      orquestra-app      binário `orq` (eframe/egui, alacritty_terminal)
```

Os pacotes levam o prefixo `orquestra-`, porque um crate chamado `core`
sombrearia o `core` da std. `runner`, `browser`, `store` e `mcp` dependem só
de `core`; `app` depende de todos. O binário `orq` (com o apelido
`orquestra`, que só repassa os argumentos) tem três modos:

- `orq [pasta] [-g objetivo]`: a UI, como o `code .`.
  - Há uma instância por usuário. Ela segura `ui.lock` e atende `ui.sock` em
    `$XDG_RUNTIME_DIR/orquestra/`, um pedido JSON por linha (`instance.rs`).
    Outro `orq` entrega o pedido a ela e sai. Isso também evita que uma
    segunda instância feche as sessões da primeira como interrompidas.
  - Sem instância, o `orq` sobe a UI noutro processo, em sessão própria e
    com o log em `<dados>/orquestra.log`, e devolve o terminal.
  - A pasta vira a raiz do repositório git. Sem objetivo, abre o diálogo
    preenchido; com objetivo, já cria a orquestração.
  - Pedido vindo de outro terminal traz a janela para a frente. No Wayland o
    compositor não deixa: a janela pede atenção.
- `orq mcp`: a ponte stdio do MCP, que o CLI do orquestrador sobe;
- `orq hook stop`: o hook `Stop` do orquestrador Claude Code.

## 3. Fluxo de dados

```
            Command (mpsc)                     ┌──────────────── Hub (core) ────────────────────┐
 ┌──────┐ ───────────────────────────────────▶ │ Router: sessões, registro de status, fila      │
 │ app  │                                      │   de spawns (limite global/sessão, aprovação)  │
 └──────┘         ┌──────────────────────────▶ │ lanes por agente ─▶ AgentRunner/BrowserManager │
    ▲             │ Command (mesmo canal)      │ recorder (thread) ─▶ Store + Notifier          │
    │      ┌─────────────┐  socket local       └────────────────────────────────────────────────┘
    │      │ mcp gateway │◀──────────────── `orquestra mcp` (stdio) ◀── CLI do orquestrador
    │      └─────────────┘◀──────────────── `orquestra hook stop`   ◀── hook Stop (Claude Code)
    │             ▲
    │  EventBus.events (broadcast 4096): Output, StatusChanged, Created, Session*, ...
    └─────────────┴──── todos assinam; EventBus.frames (broadcast 4, com perda): BrowserFrame
```

- **Dois broadcasts.** `BrowserFrame` vai num canal curto que pode perder
  quadros, para nunca empurrar `Output` para fora da fila principal.
- **Comandos sem resposta direta.** O resultado volta como evento, inclusive
  `AgentEvent::Error`.
- **Router.** Vive na task do hub. Assina o próprio bus para saber o status de
  cada agente e decide se um `Spawn` sobe agora ou vai para a fila.
- **Lanes.** Uma task por agente com fila própria: comandos do agente em
  ordem, agentes diferentes em paralelo.
- **Recorder.** Thread dedicada que repassa todo evento ao `Store` e converte
  transições de status em `Notification`.

## 4. Contrato (`crates/core`)

### 4.1 Tipos principais

```rust
pub struct AgentId(Uuid);  pub struct SessionId(Uuid);   // UUIDv7; .short() = 8 hex aleatórios
pub struct Timestamp(pub i64);                            // ms desde a época Unix

pub struct SessionConfig {            // uma orquestração = um grupo no canvas
    name, repo: PathBuf, goal: String,
    max_workers: Option<u32>,         // limite da sessão (o global está na config)
    allowed_kinds: Vec<AgentKind>,    // o que o orquestrador pode criar (o mcp aplica)
    workers_may_browse: bool,         // o mcp aplica
    approve_spawns: bool,             // spawns com parent esperam o humano
}

pub enum AgentKind { ClaudeCode, Codex, Shell, Custom }
pub enum AgentRole { Orchestrator, Worker }
pub enum Workspace { Worktree { repo, base, branch }, Directory { path } }
pub struct AgentConfig {
    name, kind, program, args, env,
    session: SessionId, role: AgentRole,
    parent: Option<AgentId>,          // a seta no canvas; None = criado pela UI (avulso)
    workspace, initial_prompt, browser: bool, size: TermSize,
}

pub enum QueueReason { Capacity, Approval }
pub enum AgentStatus {
    Queued { reason },                // ainda sem processo
    Starting, Running, WaitingForInput,
    Done { exit_code: Option<i32> },  // 0, ou morto a pedido do usuário (None)
    Failed { reason: String },
}

pub struct SpawnRequest {            // o que o runner recebe
    id, config, browser: Option<BrowserEndpoint>,
    mcp_servers: Vec<McpServerSpec>,  // navegador e/ou `orquestra`, já renderizados
    env: BTreeMap<String,String>,     // credencial do gateway etc.
    stop_hook: Option<String>,        // orquestrador Claude Code: comando do hook Stop
}
pub struct McpServerSpec { name, command, args, env }
pub struct OrchestratorGrant { mcp: McpServerSpec, env, stop_hook: Option<String> }

pub struct AgentInfo { id, config, workdir, worktree: Option<WorktreeInfo>, pid, browser, started_at }
pub struct WorktreeInfo { repo, path, branch, base_ref, base_commit }
pub enum WorktreeCleanup { Keep /*padrão*/, RemoveWorktree, RemoveWorktreeAndBranch }

pub struct BrowserEndpoint { port, ws_url }               // .http_url()
pub enum BrowserStatus { Launching, Ready { endpoint }, Closed, Failed { reason } }
pub struct BrowserFrame { seq, format, data: Bytes, css_width, css_height, page_scale,
                          offset_top, scroll_x, scroll_y }   // .to_viewport(u, v)
pub enum BrowserInput { Mouse(..), Wheel(..), Key(KeyInput{key, code, text, ..}), Text(String) }
pub enum Navigation { Url(String), Back, Forward, Reload }

pub struct History { sessions: Vec<SessionRecord>, agents: Vec<AgentRecord> }
pub struct Notification { agent, kind: NeedsInput|NeedsApproval|Finished|Failed, title, body }
```

`AppConfig` (`config.toml`, todas as seções com default):
- `ui {font_size, scrollback_lines, theme}`;
- `runner {worktree_root, branch_prefix, replay_bytes, idle_ms, kill_grace_ms}`;
- `browser {executable, headless, viewport, screencast_quality, screencast_every_nth_frame, extra_args, agent_mcp}`;
- `notifications {enabled, on_waiting_for_input, on_done, on_failed}`;
- `orchestration {max_concurrent_workers = 8, orchestrator_preset, orchestrator_prompt}`;
- `presets`, `recent_repos`.

`HubConfig::from_app(&cfg)` extrai o que o hub usa.

`core::conventions` reúne o que cruza processos:
- variáveis de ambiente: `ORQUESTRA_AGENT_ID`, `_SESSION_ID`, `_CDP_URL`,
  `_CDP_PORT`, `_SOCKET`, `_TOKEN` e `_TTY` (o PTY do agente, onde os hooks
  escrevem);
- as OSC de status (`ESC]777;orquestra;waiting|running BEL`);
- o nome do servidor MCP (`orquestra`).

### 4.2 Comandos (UI e gateway → hub)

| Comando | Efeito |
|---|---|
| `CreateSession { id, config }` | → `SessionCreated` |
| `CloseSession { id, cleanup }` | remove todos os agentes da sessão → `SessionClosed` + `Removed`… |
| `Spawn { id, config }` | `Created` → `Queued{..}` ou `Starting` → [navegador] → [grant] → `runner.spawn` → `Spawned` |
| `ApproveSpawn { id }` / `RejectSpawn { id }` | libera ou descarta (→ `Removed`) um `Queued{Approval}` |
| `SendInput` / `Resize` / `Kill` / `Remove { cleanup }` / `RequestReplay` | por agente; `Kill` na fila → `Done`; outros na fila → `Error` |
| `OpenBrowser` / `CloseBrowser` / `SetBrowserVisible` / `BrowserInput` / `BrowserNavigate` / `BrowserResize` | repasse ao browser |
| `LoadHistory { limit }` / `SaveConfig(cfg)` / `Shutdown` | globais |

### 4.3 Eventos

| Evento | Quem publica | Notas |
|---|---|---|
| `SessionCreated { id, config }` / `SessionClosed { id }` | hub | |
| `Created { id, config }` | hub | primeiro do agente; `config.parent` = aresta no canvas |
| `StatusChanged { id, status }` | hub (`Queued`, `Starting`, `Failed` de spawn), runner (o resto) | |
| `Spawned { info }` | hub | pode chegar depois dos primeiros `Output` |
| `Output { id, seq, data }` / `Replay { id, next_seq, data }` | runner | buraco no `seq` = lag → `RequestReplay` |
| `Removed { id }` | hub | |
| `BrowserStatus` / `BrowserFrame` (canal de quadros) / `BrowserNavigated` | browser | |
| `History(..)` / `Error { id, message }` / `Shutdown` | hub | |

O `AgentEvent` ocupa 64 bytes; um teste trava esse tamanho em ≤ 80.

### 4.4 Traits

```rust
trait AgentRunner          { spawn, send_input, resize, kill, remove, request_replay, shutdown }   // runner
trait BrowserManager       { launch, close, set_streaming, navigate, input, resize, shutdown }     // browser
trait Store                { record, history, output_log, load_config, save_config, flush }      // store (síncrono)
trait Notifier             { notify }                                                             // store
trait OrchestratorGateway  { grant(session, agent) -> OrchestratorGrant, revoke(agent) }          // mcp (síncrono)
```

Regras:
- **Fiação.** Os serviços recebem o `EventBus` no construtor. O gateway
  recebe também um `CommandSender`.
- **Retorno rápido.** Trabalho longo roda em tasks internas e reporta por
  evento. Exceções: `spawn` e `launch`.
- **Sem chamadas concorrentes por agente.** O hub nunca chama o mesmo agente
  em paralelo, mas agentes diferentes sim, então o serviço precisa ser `Sync`.
- **Sem `async-trait`.** As traits retornam `impl Future + Send` e os impls
  podem usar `async fn`. O hub é genérico, sem `dyn`.

### 4.5 Fila e limites (no hub)

- **Quem conta.** Só workers. O limite é
  `orchestration.max_concurrent_workers` (global) e
  `SessionConfig.max_workers` (por sessão). Ocupam vaga `Starting`, `Running`
  e `WaitingForInput`.
- **Fila por limite.** `Spawn` de worker sem vaga → `Queued { Capacity }`,
  em ordem de chegada. Quando um worker termina ou é removido, o primeiro
  que cabe sobe (`Starting`).
- **Fila por aprovação.** Se a sessão tem `approve_spawns`, todo spawn com
  `parent` vai para `Queued { Approval }`. Spawns da UI (sem `parent`) não
  precisam. A UI mostra aprovar/recusar no card e na fila "Precisa de você".

### 4.6 Montagem (Fase 2, `app/main.rs`)

```rust
let store = SqliteStore::open_default()?;  let config = store.load_config()?;
let bus = EventBus::new();  let (cmd_tx, cmd_rx) = command_channel();
let (ui_events, ui_frames) = (bus.subscribe(), bus.subscribe_frames());
let gateway = McpGateway::start(cmd_tx.clone(), bus.clone(), config.clone())?;
let services = Services {
    runner: Runner::new(bus.clone(), config.runner.clone()),
    browser: Chromium::new(bus.clone(), config.browser.clone()),
    store, notifier: DesktopNotifier::new(), gateway,
};
runtime.spawn(Hub::new(services, bus, HubConfig::from_app(&config)).run(cmd_rx));
eframe::run_native(…, App::new(cmd_tx, ui_events, ui_frames, config));
```

O modo `--mock` é a mesma montagem com serviços falsos do agente C.

## 5. Sequências

**Nova orquestração.** A UI manda `CreateSession`. Depois manda `Spawn` do
orquestrador (`role: Orchestrator`, `Workspace::Directory { repo }`,
`initial_prompt = orchestration.render_prompt(&session)`). O hub pede ao
gateway um `grant`: credencial, spec do MCP `orquestra` e `stop_hook`. O
runner registra o MCP na CLI e sobe o processo.

**O orquestrador cria um agente.** O CLI chama
`mcp__orquestra__spawn_agent`, que passa por `orquestra mcp` (stdio), pelo
socket e pelo gateway. O gateway valida contra a sessão (tipos, navegador,
nome único), monta o `AgentConfig`:
- `role: Worker`, `parent = orquestrador`;
- `Workspace::Worktree { repo da sessão }`;
- `initial_prompt = tarefa`.

Então manda `Command::Spawn`. O hub publica `Created` (a UI desenha o nó e a
seta) e `Starting` ou `Queued`. O gateway responde ao orquestrador com o
nome, o id e o status.

**Retorno ao orquestrador.**
- **Puxado.** `wait_for` bloqueia até a condição (`idle`, `done`, `attention`
  ou `change`) ou o timeout. `read_output` devolve as últimas linhas da tela
  do agente, em texto puro: o gateway mantém um `alacritty_terminal::Term` por
  worker, alimentado pelos `Output`.
- **Empurrado (orquestrador Claude Code).** O hook `Stop` do orquestrador é
  `orquestra hook stop`. Ao fim de cada turno ele pergunta ao gateway se há
  novidades dos filhos (terminou, falhou, pede input, pede aprovação).
  - Com novidade, responde `{"decision":"block","reason":"…"}` e o Claude
    continua já sabendo.
  - Sem novidade, escreve a OSC `waiting` no PTY do agente e deixa parar.
  - Parado com filhos vivos (o `wait_for` em segundo plano o acorda), o
    orquestrador espera os agentes, não o humano: a UI mostra "aguardando
    agentes" (azul, fora de "Precisa de você") e o hub não manda a
    notificação "Aguardando sua resposta". Se a tela dele perguntar sim/não,
    volta a ser com você.

  Para Codex (sem hook equivalente) vale só o modo puxado.

**Terminal, lag e reabertura.** A UI alimenta um `Term` por agente na ordem
do `seq`. Um buraco gera `RequestReplay`; o `Replay` recria o `Term`.

**Respostas do terminal.** O `PtyWrite` do `Term` da UI (por exemplo, a
resposta a `CSI 6n`) volta como `SendInput`. A UI processa a saída de todos
os agentes, visíveis ou não, então sempre responde.

**Kill / Remove / CloseSession.**
- `Kill` deixa worktree e navegador.
- `Remove` mata, aplica o cleanup (padrão `Keep`), fecha o navegador, revoga a
  credencial e publica `Removed`.
- `CloseSession` faz `Remove` em todos os agentes da sessão.

**Shutdown.** Lanes têm 3 s para esvaziar. Depois `runner.shutdown` e
`browser.shutdown`, o hub publica `Shutdown` e o recorder dá `flush`.

## 6. Decisões por crate (guia da Fase 1)

### runner
- **PTY.** `portable-pty`. Uma thread de leitura por agente (64 KiB) publica
  `Output` com `seq`; uma thread de escrita consome a fila de `send_input`.
  O anel de replay tem `replay_bytes`.
- **Ambiente.** `TERM=xterm-256color`, `COLORTERM=truecolor`, as variáveis de
  `conventions::env` e `req.env`.
- **MCP por CLI.**
  - Claude Code: `--mcp-config '{"mcpServers":{…}}'`. Para orquestradores,
    liberar `mcp__orquestra` nas permissões, para que um prompt de permissão
    não trave a orquestração.
  - Codex: `-c mcp_servers.<nome>.command=…` e `.args=[…]` / `.env={…}`.
  - Demais: só as variáveis de ambiente.

  Confirme a sintaxe com `claude --help` e com a documentação via Context7.
- **Prompt inicial.** Argumento posicional em Claude Code e Codex; nos demais
  tipos, escrito no PTY.
- **Status, em camadas.**
  1. Saída do processo: `Done`/`Failed`. É autoritativa.
  2. OSC de `conventions::osc`, escritas por hooks injetados no PTY do
     agente, cujo caminho vai em `ORQUESTRA_TTY`. O Claude Code roda os hooks
     numa sessão nova, sem terminal de controle, então `/dev/tty` não serve
     (fica só como reserva). No Claude Code, via `--settings` com hooks:
     - `Stop` e `Notification` → `waiting`; o `Stop` usa o `stop_hook` quando
       houver. Num pedido de permissão, o `Notification` chega uns 6 s depois
       de o menu aparecer;
     - `UserPromptSubmit` e `PostToolUse` → `running`.
     No Codex, via `-c notify=[…]` → `waiting`. BEL e OSC 9 também contam.
  3. Heurística: sem saída há `idle_ms` + padrões por tipo.
  4. Debounce.
- **Worktrees.** `git` via `std::process::Command`, nunca `git2`. Local:
  `<worktree_root|data_dir/worktrees>/<repo>-<hash8>/<id curto>`. Branch:
  `<branch_prefix><nome>-<id curto>`.
- **Kill.** SIGTERM no grupo, SIGKILL após `kill_grace_ms`. Prefira `nix`/
  `rustix` a `unsafe` com `libc`.

### browser
- **Um Chromium por agente.** Headless (`--headless=new`), `--user-data-dir`
  temporário e porta de debug só daquele agente: isolamento total, e o MCP de
  um agente não enxerga as abas de outro.
- **Porta.** `--remote-debugging-port=0` e ler a porta real; sem corrida.
- **Executável.** config → `$ORQUESTRA_CHROMIUM` → `chromium`,
  `chromium-browser`, `google-chrome-stable`, `google-chrome` → cache do
  Playwright (`~/.cache/ms-playwright/chromium-*`). Evitar o snap.
- **Screencast.** Da página ativa (segue abas novas), com `ack` por quadro. O
  `data` já sai decodificado do base64. Quadros só vão para a UI com o
  streaming ligado.
- **Input.** `Input.dispatchMouseEvent` / `dispatchKeyEvent` (mapa DOM →
  `windowsVirtualKeyCode`) / `insertText`. Resize via `Browser.setWindowBounds`.
- **Teste de integração.** Página local num `TcpListener`, receber ≥ 1 quadro,
  clicar num botão por `BrowserInput` e checar o efeito via `Runtime.evaluate`.

### store
- **Arquivos.** SQLite (bundled, WAL) em `<data_dir>/orquestra.db`; config em
  `<config_dir>/config.toml` (`directories`, "orquestra"). Grava a config de
  forma atômica (tmp + rename).
- **Schema** (com `user_version` para migrações).
  `sessions(id, name, config_json, created_at, closed_at)`;
  `agents(id, session_id, name, kind, role, parent_id, config_json, info_json, status_json, created_at, updated_at)`;
  `status_log(agent_id, at, status_json)`;
  `output_chunks(agent_id, first_seq, at, data)`.
- **Log de saída.** `Output` vai em lote, a cada ~250 ms ou 64 KiB, com teto
  por agente (por exemplo, 8 MiB).
- **Notificações.** `notify-rust` (zbus).

### mcp
- **Gateway** (`McpGateway: OrchestratorGateway`, dentro do app).
  - Socket Unix em `$XDG_RUNTIME_DIR/orquestra/<pid>.sock` (permissão 0600)
    com protocolo JSON por linha `{token, method, params}`.
  - `grant` gera um token aleatório por orquestrador e devolve
    `McpServerSpec { name: "orquestra", command: <current_exe>, args: ["mcp"] }`,
    as variáveis `ORQUESTRA_SOCKET`/`_TOKEN` e `stop_hook: "<exe> hook stop"`.
- **Estado.** Montado a partir do bus: sessões, agentes, status, um `Term` por
  worker (scrollback curto) e uma fila de novidades por orquestrador.
- **Ferramentas.** `list_agents`, `spawn_agent(name, task, kind?, browser?)`,
  `send_input(agent, text, submit=true)`, `read_output(agent, lines=80)`,
  `wait_for(agents?, until, timeout_secs)` e `kill_agent(agent)`. Todas
  restritas à sessão do token.
- **Ponte stdio** (`orquestra mcp`). `rmcp`, com as ferramentas acima
  repassadas ao socket.
- **Hook** (`orquestra hook stop`). Lê o JSON do hook (respeita
  `stop_hook_active`) e devolve `decision: block` com as novidades, ou escreve
  a OSC `waiting` no PTY do agente (`ORQUESTRA_TTY`).

### app
- **Canvas.** `egui::Scene` (pan/zoom).
  - O `Scene` escala as formas da camada, e texto escalado é a rasterização
    de 100% esticada (borrada). Depois de desenhar, cada texto da camada é
    refeito no tamanho em que aparece e encolhido de volta (`sharpen_text`),
    em degraus de 1/24 de oitava para não encher o atlas de fontes no zoom.
  - Uma moldura por sessão, com nome, repositório e resumo. O orquestrador fica
    à esquerda e os workers numa coluna, ligados por linhas ortogonais na cor
    do status, com a tarefa como rótulo. Avulsos ficam sem seta.
  - Card compacto (status + última linha) ou detalhado (fim do terminal,
    miniatura do navegador, aprovar/recusar, responder Sim/Não), conforme o
    zoom e o foco.
  - A prévia vem de `term::preview`, sobre a tela do terminal. Ela ignora a
    caixa de entrada do Claude Code e tudo abaixo dela (statusline, modo de
    permissão), além do spinner em qualquer quadro (`*` incluso, senão a
    linha pisca). Sim/Não só aparecem se o agente aguarda e a tela pergunta:
    - menu com uma opção "Yes…" e uma "No…": responde pelo atalho da opção,
      como `(y)` ou `(esc)`, ou pelo número dela; sem nenhum dos dois, usa
      setas a partir da destacada + Enter (é o caso do aviso de confiança na
      pasta);
    - pergunta de uma linha terminada em `(s/n)` ou `[Y/n]`.
  - Layout automático em árvore, sem persistência no MVP.
- **Topo.** Pills de sessão (Todas / cada sessão, com resumo) e
  "+ Nova orquestração".
- **Painéis flutuantes** (`ui/overlay`), fixos na tela sobre o canvas; o
  enquadramento automático deixa livres as faixas de cima e de baixo:
  - **Orquestrações** (canto de cima à esquerda): um botão que abre a lista,
    uma orquestração de cada vez (cascata), com focar, orquestrador e
    encerrar. Esc ou clique no canvas fecha;
  - **Precisa de você** (canto de cima à direita, recolhível; só aparece com
    algo pendente): input, aprovação e falhas de todas as sessões;
  - **Recursos** (canto de baixo à direita, uma linha): vagas, fila,
    navegadores abertos e RAM do app.
- **Painel lateral.** Só existe com um nó selecionado: terminal completo ou
  navegador.
- **Terminal.** Renderer próprio sobre `alacritty_terminal`; o `egui_term`
  serve só de referência (MIT) e não entra como dependência, porque cria o
  próprio PTY e está preso ao egui 0.31.
  - Um `Term` por agente, alimentado por uma task que drena o bus.
  - Teclas codificadas conforme o `TermMode`; `PtyWrite` volta como `SendInput`.
  - Seleção e cópia; `Resize` acompanha o painel.
- **Navegador.** O JPEG é decodificado fora da thread de UI. A textura só é
  atualizada quando chega quadro novo. O input passa por `to_viewport`.
  `SetBrowserVisible` liga e desliga o screencast conforme o painel.
- **Atalhos.** Ctrl+Shift+N (nova), Ctrl+Shift+↑/↓ (agente), Ctrl+Shift+B
  (navegador). Com o terminal em foco, tudo mais vai para o PTY.

## 7. Layout

Referência visual: o canvas do link no topo do documento. Esboço da tela
principal:

```
┌ orquestra  [Todas][loja · 1 aguardando][blog · 1 falhou]  [+ Nova orquestração]      [cfg] ┐
├ Visão geral  [- 70% +] [Ajustar]                         ● rodando ● aguardando ✓ ✕ ◌ ┤
│ (≡ Orquestrações 2 ∨)                                    ┌ ● Precisa de você 2    ∧ ┐ │
│      ┌ loja · login + produtos ───────────────────────────┐│ ● login-ui  loja         │ │
│      │ ┌ orquestrador ──────┐     ┌ ● login-ui aguardando ┐││  Permitir npm install?  │ │
│      │ │ Claude Code ● rod. │──┬─▶│   Permitir npm…       │││  [Sim] [Não] [Terminal] │ │
│      │ │ objetivo / plano   │  ├─▶│ ✓ api        pronto   │││ ✕ estilos  blog         │ │
│      │ │ [fale com ele…]    │  └─▶│ ● qa-navegador :41745 │││  [Terminal] [Remover…]  │ │
│      │ └────────────────────┘     └───────────────────────┘│└──────────────────────────┘ │
│      └────────────────────────────────────────────────────┘                             │
│      ┌ blog · migração para Astro ────────────────────────┐                             │
│      │ orquestrador Codex ──┬──▶ migra-posts / ✕ estilos  │                             │
│      └────────────────────────────────────────────────────┘                             │
│                                              (vagas ▮▮▮▮▯▯▯▯ 4/8 · 1 navegador · 52 MB) │
├─────────────────────────────────────────────────────────────────────────────────────────┤
│ 2 orquestrações · 7 agentes · 1 aguardando você · 1 falhou | ● hub ok                  │
└─────────────────────────────────────────────────────────────────────────────────────────┘
```

## 8. Memória e abertura (medido)

Release (`lto = "thin"`, binário de 19,4 MB). Medido em Linux com Wayland
e Intel/Mesa, depois de 10–40 s ocioso, sem navegador aberto, mediana de 3–5
execuções:

| Cenário | Primeiro quadro | RSS | Privada (anônima) | PSS | CPU parado |
|---|---|---|---|---|---|
| Vazio (modo real) | 44 ms | 78,5 MB | 28,2 MB | 69,9 MB | 0 |
| 2 orquestrações, 9 agentes (`--mock`) | 48 ms | 80,0 MB | 31,8 MB | 71,3 MB | 0 |
| Vazio com `wgpu` (Vulkan), para comparar | 83 ms | 105,3 MB | 40,2 MB | 97,5 MB | 0 |

- **De onde vem o RSS.** Cerca de 37 MB são o driver OpenGL do Mesa:
  `libLLVM` (26 MB) e `libgallium` (11 MB). São páginas de arquivo,
  compartilhadas com outros processos que usam Mesa. O app em si ocupa
  ~28 MB privados, e cada agente acrescenta pouco, porque o scrollback cresce
  sob demanda.
- **`wgpu` não ajuda.** Ele também carrega a `libLLVM` e soma o driver Vulkan.
  O `glow` fica.
- **Como medir de novo.** O app loga `primeiro quadro ms=…` no stderr; o
  script de medição lê `/proc/<pid>/status` e `smaps_rollup`.

## 9. Notas de operação

- **Confiança no Claude Code.** Num repositório em que você nunca rodou o
  Claude Code, o orquestrador para no aviso "trust this folder". A opção
  já selecionada é "No, exit". Confirme uma vez: pelo "Sim" do card, que
  navega até "Yes", ou no terminal dele. O Claude
  Code guarda a confiança pela raiz do repositório principal, e os worktrees
  usam essa mesma raiz: os agentes criados depois não perguntam de novo.
- **Codex.** O socket e o token vão também no `env` do servidor MCP, porque o
  Codex não repassa o ambiente. O `tool_timeout_sec` do `orquestra` é 3700 s.
  Ainda não foi testado com o Codex real (não instalado nesta máquina).

## 10. Fase 1 — divisão do trabalho (histórico)

| Agente | Branch / worktree | Escopo (só mexe aqui) |
|---|---|---|
| A | `phase1/runner` | `crates/runner/**` |
| B | `phase1/browser` | `crates/browser/**` |
| C | `phase1/app` | `crates/app/**` (UI sobre o `Hub` real com serviços mock) |
| D | `phase1/store` | `crates/store/**` |
| E | `phase1/mcp` | `crates/mcp/**` |

Regras:
- `crates/core` é **somente leitura**. Se o contrato precisar mudar, o agente
  para e reporta o motivo.
- Dependência nova vai no `Cargo.toml` do próprio crate, nunca no da raiz.
- Cada crate passa em `cargo build`, `cargo test`,
  `cargo clippy --all-targets -- -D warnings` e `cargo fmt --check`.
- Commits pequenos e descritivos, sem trailer de coautoria.
- Relatório final: feito, faltando, decisões.
