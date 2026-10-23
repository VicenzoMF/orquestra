# orquestra — arquitetura

App desktop em Rust, leve, para rodar e acompanhar vários agentes de código
(Claude Code, Codex, shell...) ao mesmo tempo. Você abre um **agente** numa
pasta qualquer, como uma conversa; por padrão ele **pode criar outros
agentes** e decide sozinho quando vale dividir o trabalho. Tudo aparece num
**canvas**: cada agente que você abre é uma sessão (um grupo), ligado por
setas aos agentes que criou. Cada agente roda num git worktree próprio (numa
pasta git) ou direto numa pasta, e pode ter um Chromium próprio, que ele
controla via CDP/MCP e que a UI mostra e deixa interagir. O que estiver
aberto volta quando o app abre de novo, com cada conversa retomada.

No código, quem pode criar agentes ainda se chama `AgentRole::Orchestrator`
("orquestrador"): é o papel, não um tipo que a pessoa escolhe.

Layout aprovado (canvas interativo, 4 telas):
https://claude.ai/artifact/5cuERHSaTLUxzXJ6UPsmoa

Estado: Fases 0, 1 e 2 concluídas (MVP integrado). Como rodar: `README.md`.

---

## 1. Princípios

1. **A UI só fala por canais.** `Command` vai por `mpsc`, `AgentEvent` volta
   por `broadcast`. A UI nunca chama runner, browser, store ou mcp.
2. **`core` é o contrato.** Tipos, eventos, comandos, traits e o hub. Nenhuma
   dependência de UI, PTY, CDP, SQLite ou MCP.
3. **Quem delega é um agente comum.** É um Claude Code ou Codex que recebeu
   o MCP `orquestra` e, escondidas no prompt de sistema, as instruções de
   quando delegar. Ele cria agentes mandando os mesmos `Command`s que a UI
   manda, só que pelo gateway. A primeira mensagem dele é só o que você
   escreveu.
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

- `orq [pasta] [-g tarefa]`: a UI, como o `code .`.
  - Há uma instância por usuário. Ela segura `ui.lock` e atende `ui.sock` em
    `$XDG_RUNTIME_DIR/orquestra/`, um pedido JSON por linha (`instance.rs`).
    Outro `orq` entrega o pedido a ela e sai. Isso também evita duas
    instâncias reabrindo e controlando as mesmas sessões.
  - Sem instância, o `orq` sobe a UI noutro processo, em sessão própria e
    com o log em `<dados>/orquestra.log`, e devolve o terminal.
  - Qualquer pasta serve; dentro de um repositório git, vale a raiz dele.
    Sem tarefa, abre o rascunho no canvas já com a pasta; com tarefa, já
    abre um agente com ela.
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
pub enum Workspace {
    Worktree { repo, base, branch },  // git worktree e branch novos
    Directory { path },               // direto numa pasta qualquer
    Restore { path, worktree },       // reabertura: onde estava, retomando a conversa
}
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
- `orchestration {max_concurrent_workers = 8, orchestrator_preset (tipo padrão
  de um agente novo), delegation_prompt}`. As instruções de delegação vão
  escondidas para quem pode criar agentes (`{folder}`, `{name}`), em inglês,
  como tudo que só o modelo lê (instruções e ferramentas do MCP);
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
| `Restore { record }` | reabertura: vivo → `Spawn` com `Workspace::Restore` (sem nova aprovação); na fila → `Spawn` normal; terminado → `Created` + `Spawned` + status + `Replay` da saída gravada, sem processo |
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

**Agente novo.** A pessoa escreve a tarefa (opcional) no rascunho do canvas
(§6, app) e aperta Enter ou "Iniciar". A UI manda `CreateSession` (uma
sessão só dele) e `Spawn` do agente (`actions::new_agent`): direto na pasta
ou, se pedido numa pasta git, num worktree; `initial_prompt` = a tarefa como
foi escrita. Podendo criar agentes (o padrão, só Claude Code e Codex), ele
tem `role: Orchestrator`: o hub pede ao gateway um `grant` (credencial, spec
do MCP `orquestra` e `stop_hook`) e põe em `SpawnRequest.instructions` as
instruções de delegação; o runner as passa como `--append-system-prompt`
(Claude Code) ou `-c developer_instructions` (Codex), fora da conversa.

**Um agente cria outro.** O CLI chama
`mcp__orquestra__spawn_agent`, que passa por `orquestra mcp` (stdio), pelo
socket e pelo gateway. O gateway valida contra a sessão (tipos, navegador,
nome único), monta o `AgentConfig`:
- `role: Worker`, `parent = orquestrador`;
- `Workspace::Worktree` (`workspace: "worktree"`, o padrão numa pasta git) ou
  `Workspace::Directory` (`workspace: "folder"`, o padrão fora de um
  repositório); `dir` troca a pasta da sessão por qualquer outra. Quem é
  criado assim não cria outros (um nível só);
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

**Shutdown.** Lanes têm 3 s para esvaziar. Então o hub publica `Shutdown` e
espera o recorder dar `flush`; só depois vêm `runner.shutdown` e
`browser.shutdown`. Assim o banco guarda cada agente como estava, sem
`closed_at` nas sessões: fechar o app e cair dão no mesmo.

**Reabertura.** Na abertura, `SqliteStore::open_sessions` devolve as sessões
sem `closed_at` e os agentes não removidos, com o último status. O app manda
`CreateSession` (mesmo id) e um `Restore` por agente, em ordem de criação.
- O Claude Code nasce com `--session-id <id do agente>`, e o `Restore` o sobe
  com `--resume <id>`: a conversa volta inteira. Ele só grava a conversa
  depois da primeira mensagem; sem ela (`<config>/projects/*/<id>.jsonl`), o
  runner abre do começo, com `--session-id` e a tarefa inicial. O Codex
  volta com `codex resume --last` na pasta do agente (com dois Codex na
  mesma pasta, pode trocar).
- O orquestrador ganha credencial nova no spawn; o gateway refaz o estado
  pelos eventos.
- A primeira espera de um agente reaberto (nos primeiros 30 s) e os
  terminados que voltam não viram notificação.
- O runner tira do ambiente herdado a identidade da sessão do Claude Code que
  abriu o `orq` (`CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_CHILD_SESSION`...):
  herdada, ela fazia o agente não gravar a conversa.

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
  - Cada moldura fica onde a pessoa a pôs. As posições são estado da UI,
    fora do banco: `<dados>/canvas.json` (`places.rs`), gravado quando muda
    (não no meio de um arrasto) e lido na abertura, junto com as sessões
    reabertas. A posição some quando a sessão sai do modelo.
    - "Iniciar" põe o canto da moldura onde estava o rascunho. Sessão sem
      posição (`orq <pasta> -g`, mocks) ganha uma na primeira vez que
      aparece: x = 0, logo abaixo da moldura mais baixa. Daí em diante fica
      fixa como as outras.
    - Arrastar o cabeçalho move a sessão inteira, sem pan do canvas; clique
      simples nele foca, como antes. Arrastar o título do rascunho move o
      rascunho.
    - O × no canto do cabeçalho (e "Fechar sessão", no botão direito dele)
      fecha a sessão: vazia, na hora; com agentes, depois de confirmar (as
      opções de worktree só aparecem se algum agente tem um). A pasta não é
      tocada.
  - `layout.rs` é uma função pura: recebe a origem de cada sessão e monta a
    árvore dentro da moldura. Molduras nunca se sobrepõem: a que cruza outra
    mais acima (faixas de x se cruzando, a menos de `SESSION_GAP` na
    vertical) desce só na exibição, sem mudar a posição guardada, e volta
    quando a de cima encolhe.
  - Botão direito no fundo (também dentro de uma moldura, fora dos cards)
    abre um menu: "Iniciar novo agente".
  - **Rascunho "Novo agente"** (`ui/draft.rs`): uma janela no estilo dos
    cards, desenhada dentro da `Scene`, então acompanha pan e zoom. Abre no
    ponto do botão direito ou, por "+ Novo agente", Ctrl+Shift+N e
    `orq <pasta>`, no centro da área visível. Há um por vez: abrir de novo
    só o reposiciona, com o texto. Enquanto ele está aberto, o canvas não se
    reajusta sozinho; com zoom abaixo de 80%, ele abre aproximando a vista.
    - Compacto: tarefa, opcional (Enter inicia, Shift+Enter quebra a linha,
      Esc cancela), e um resumo como `~/projetos/loja · Claude Code`.
    - "Mostrar mais opções": tipo, pasta (com os recentes), worktree (só
      numa pasta git), navegador e "Pode criar outros agentes" (ligado; só
      Claude Code e Codex), com as regras para os que ele criar: limite
      simultâneo, tipos, navegador e aprovação. Sem pasta recente, ele já
      abre assim.
    - Sem ComboBox lá dentro: o popup sai no lugar certo, mas em escala 1:1,
      e destoa da janela com zoom. Tipo e recentes são escolhas inline.
    - "Iniciar" manda `CreateSession` + `Spawn`, guarda a posição da sessão
      nova, seleciona o agente e põe a pasta nos recentes. A vista não se
      mexe e o ajuste automático segue desligado: a sessão aparece onde a
      janela estava.
  - Quem pode criar agentes tem o card maior (estrela, tarefa, progresso dos
    que criou e um campo "Fale com o agente…").
- **Topo.** Pills de sessão (Todas / cada sessão, com resumo) e "+ Novo
  agente" (abre o rascunho no canvas).
- **"+ Agente na sessão"** (barra do canvas): mais um agente numa sessão
  aberta, direto na pasta dela ou num worktree (pasta git).
- **Painéis flutuantes** (`ui/overlay`), fixos na tela sobre o canvas; o
  enquadramento automático deixa livres as faixas de cima e de baixo (e não
  aproxima além de 100%; o zoom manual vai até 150%):
  - **Sessões** (canto de cima à esquerda): um botão que abre a lista, uma
    sessão de cada vez (cascata), com focar, o agente dela e fechar. Esc ou
    clique no canvas fecha;
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
- **Atalhos.** Ctrl+Shift+N (novo agente), Ctrl+Shift+↑/↓
  (agente), Ctrl+Shift+B (navegador), Esc (fecha o rascunho ou a seleção).
  Com o terminal em foco, tudo mais vai para o PTY.

## 7. Layout

Referência visual: o canvas do link no topo do documento. Esboço da tela
principal:

```
┌ orquestra  [Todas][loja · 1 aguardando][blog · 1 falhou]            [+ Novo agente]      [cfg] ┐
├ Visão geral  [- 70% +] [Ajustar]                         ● rodando ● aguardando ✓ ✕ ◌ ┤
│ (≡ Sessões 2 ∨)                                          ┌ ● Precisa de você 2    ∧ ┐ │
│      ┌ loja · login + produtos ───────────────────────────┐│ ● login-ui  loja         │ │
│      │ ┌ ★ claude-code ─────┐     ┌ ● login-ui aguardando ┐││  Permitir npm install?  │ │
│      │ │ Claude Code ● rod. │──┬─▶│   Permitir npm…       │││  [Sim] [Não] [Terminal] │ │
│      │ │ objetivo / plano   │  ├─▶│ ✓ api        pronto   │││ ✕ estilos  blog         │ │
│      │ │ [fale com ele…]    │  └─▶│ ● qa-navegador :41745 │││  [Terminal] [Remover…]  │ │
│      │ └────────────────────┘     └───────────────────────┘│└──────────────────────────┘ │
│      └────────────────────────────────────────────────────┘                             │
│      ┌ blog · migração para Astro ────────────────────────┐                             │
│      │ ★ codex ─────────────┬──▶ migra-posts / ✕ estilos  │                             │
│      └────────────────────────────────────────────────────┘                             │
│                                              (vagas ▮▮▮▮▯▯▯▯ 4/8 · 1 navegador · 52 MB) │
├─────────────────────────────────────────────────────────────────────────────────────────┤
│ 2 sessões · 7 agentes · 1 aguardando você · 1 falhou | ● hub ok                        │
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
