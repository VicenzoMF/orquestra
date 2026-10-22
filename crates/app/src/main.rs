//! Binário `orq`: abre a UI como o `code .` e atende os subcomandos que os
//! agentes chamam. Ver `docs/ARCHITECTURE.md` §2 e §4.6.

use std::fs::File;
use std::path::Path;
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};
use orquestra_app::backend::{self, Backend};
use orquestra_app::cli::{USAGE, UiArgs};
use orquestra_app::feed::Shared;
use orquestra_app::instance::{self, Instance, OpenRequest};
use orquestra_app::mock::NullStore;
use orquestra_app::places::Places;
use orquestra_app::ui::OrquestraApp;
use orquestra_browser::Chromium;
use orquestra_core::{AppConfig, Command, History, Services, Store};
use orquestra_mcp::McpGateway;
use orquestra_runner::Runner;
use orquestra_store::{DesktopNotifier, SqliteStore};
use tracing::{info, warn};

const LOG_FILTER: &str = "info,eframe=warn,egui_glow=warn,egui_winit=warn,winit=warn";
/// Quanto esperar por uma instância que está subindo agora.
const HANDOFF_WAIT: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    let started = Instant::now();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["mcp"] => return run_mcp(),
        ["hook", "stop"] => {
            let code = orquestra_mcp::run_stop_hook();
            return ExitCode::from(u8::try_from(code).unwrap_or(1));
        }
        ["-h" | "--help"] => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let ui_args = match UiArgs::parse(&args) {
        Ok(a) => a,
        Err(err) => {
            eprintln!("orq: {err:#}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let request = match ui_args.request() {
        Ok(r) => r,
        Err(err) => {
            eprintln!("orq: {err:#}");
            return ExitCode::from(2);
        }
    };
    match launch(&ui_args, request, started) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("orq: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Como o `code .`: entrega o pedido à janela aberta ou abre uma nova,
/// desprendida do terminal (a não ser com `--foreground`).
fn launch(args: &UiArgs, request: OpenRequest, started: Instant) -> anyhow::Result<()> {
    if args.mock {
        // Demonstração: sempre no terminal e fora da instância única.
        init_tracing(LOG_FILTER);
        return run(true, started, None, request);
    }
    let dir = instance::default_dir().context("diretório do socket da instância")?;
    if args.detached {
        // Sessão própria: fechar o terminal de onde veio não derruba o app.
        let _ = nix::unistd::setsid();
    } else {
        if instance::send(&dir, &request).is_ok() {
            return Ok(());
        }
        if !args.foreground {
            return detach(&request);
        }
    }
    let Some(instance) = Instance::claim(&dir).context("instância única")? else {
        // Outra instância está subindo agora: entrega a ela.
        return hand_off(&dir, &request);
    };
    init_tracing(LOG_FILTER);
    run(false, started, Some(instance), request)
}

/// Sobe a UI noutro processo, com a saída no log, e devolve o terminal.
fn detach(request: &OpenRequest) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("caminho do executável")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--detached");
    if let Some(repo) = &request.repo {
        cmd.arg(repo);
    }
    if let Some(goal) = &request.goal {
        cmd.args(["--goal", goal]);
    }
    let log = log_file();
    let out = match &log {
        Some(file) => Stdio::from(file.try_clone()?),
        None => Stdio::null(),
    };
    let err = log.map_or_else(Stdio::null, Stdio::from);
    cmd.stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .context("abrir a janela")?;
    Ok(())
}

/// `<dados>/orquestra.log`, recomeçado a cada abertura.
fn log_file() -> Option<File> {
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    File::create(dir.join("orquestra.log")).ok()
}

/// A pasta do banco (`~/.local/share/orquestra`).
fn data_dir() -> Option<std::path::PathBuf> {
    let (db, _) = SqliteStore::default_paths()?;
    db.parent().map(Path::to_owned)
}

fn hand_off(dir: &Path, request: &OpenRequest) -> anyhow::Result<()> {
    let deadline = Instant::now() + HANDOFF_WAIT;
    loop {
        match instance::send(dir, request) {
            Ok(()) => return Ok(()),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Err(err) => return Err(err).context("o orquestra já está abrindo, mas não respondeu"),
        }
    }
}

/// Logs sempre no stderr: em `mcp` e `hook` o stdout é do protocolo.
fn init_tracing(default: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn run_mcp() -> ExitCode {
    init_tracing("warn");
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("orq mcp: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(orquestra_mcp::run_stdio_bridge()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("orq mcp: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(
    mock: bool,
    started: Instant,
    mut instance: Option<Instance>,
    request: OpenRequest,
) -> anyhow::Result<()> {
    // Poucas threads: o trabalho pesado é dos agentes, não do app.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("orquestra-rt")
        .enable_all()
        .build()?;

    let (config, backend, reopen) = if mock {
        let config = NullStore.load_config()?;
        let backend = backend::start_mock(runtime.handle(), &config, 1.0, request.repo.is_none())?;
        (config, backend, History::default())
    } else {
        start_real(&runtime)?
    };
    let Backend {
        bus: _bus,
        cmd,
        events,
        frames,
        hub,
    } = backend;

    let shared = Shared::new(
        config.orchestration.max_concurrent_workers,
        config.ui.scrollback_lines,
    );
    let egui_ctx = egui::Context::default();
    let repaint_ctx = egui_ctx.clone();
    let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(move || repaint_ctx.request_repaint());
    backend::spawn_ui_feeds(
        runtime.handle(),
        events,
        frames,
        cmd.clone(),
        shared.clone(),
        repaint.clone(),
    );
    // Depois dos feeds: a UI vê cada sessão e agente reaberto chegar.
    reopen_sessions(&cmd, reopen);

    // O pedido desta execução e os que vierem de outros `orq`.
    shared.push_open(request, false);
    if let Some(instance) = instance.as_mut() {
        let shared = shared.clone();
        instance.serve(move |request| {
            shared.push_open(request, true);
            repaint();
        })?;
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("orquestra")
            .with_app_id("orquestra")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([960.0, 600.0]),
        ..Default::default()
    };
    let ui_cmd = cmd.clone();
    let places_file = data_dir().map(|dir| dir.join("canvas.json"));
    let result = eframe::run_native_ext(
        "orquestra",
        options,
        Some(egui_ctx),
        Box::new(move |cc| {
            let mut app = OrquestraApp::new(cc, ui_cmd, shared, config);
            if let Some(path) = places_file.filter(|_| !mock) {
                app = app.with_places(Places::open(path));
            }
            Ok(Box::new(FirstFrame {
                app,
                started,
                logged: false,
            }))
        }),
    );

    // Janela fechada: some do socket, pede o shutdown e espera o hub um pouco.
    drop(instance);
    let _ = cmd.send(Command::Shutdown);
    let joined =
        runtime.block_on(async { tokio::time::timeout(Duration::from_secs(5), hub).await });
    if joined.is_err() {
        warn!("hub não encerrou a tempo");
    }
    runtime.shutdown_timeout(Duration::from_millis(500));
    result.map_err(|err| anyhow!("{err}"))
}

/// Serviços reais: SQLite + config, PTYs, Chromium, notificações e o gateway
/// MCP. Devolve também o que estava aberto quando o app parou.
fn start_real(runtime: &tokio::runtime::Runtime) -> anyhow::Result<(AppConfig, Backend, History)> {
    let store = SqliteStore::open_default().context("abrir o banco")?;
    let reopen = store.open_sessions().unwrap_or_else(|err| {
        warn!(%err, "não deu para ler as sessões abertas; começando vazio");
        History::default()
    });
    let config = store.load_config()?;
    let backend = backend::start(runtime.handle(), &config, |bus, cmd| {
        Ok(Services {
            runner: Runner::new(bus.clone(), config.runner.clone()),
            browser: Chromium::new(bus.clone(), config.browser.clone()),
            store,
            notifier: DesktopNotifier::new(),
            gateway: McpGateway::start(cmd.clone(), bus.clone(), config.clone())?,
        })
    })?;
    Ok((config, backend, reopen))
}

/// Reabre as sessões da execução anterior com os mesmos ids: cada agente
/// vivo volta onde estava e retoma a conversa, quem estava na fila volta à
/// fila e os terminados voltam como registro (`Command::Restore`). Pais antes
/// dos filhos: o store devolve em ordem de criação.
fn reopen_sessions(cmd: &orquestra_core::CommandSender, reopen: History) {
    if reopen.sessions.is_empty() {
        return;
    }
    info!(
        sessions = reopen.sessions.len(),
        agents = reopen.agents.len(),
        "reabrindo as sessões da execução anterior"
    );
    for session in reopen.sessions {
        let _ = cmd.send(Command::CreateSession {
            id: session.id,
            config: Box::new(session.config),
        });
    }
    for record in reopen.agents {
        let _ = cmd.send(Command::Restore {
            record: Box::new(record),
        });
    }
}

/// Registra quanto o app levou do `main` até o primeiro quadro.
struct FirstFrame {
    app: OrquestraApp,
    started: Instant,
    logged: bool,
}

impl eframe::App for FirstFrame {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame);
        if !self.logged {
            self.logged = true;
            info!(
                ms = self.started.elapsed().as_millis() as u64,
                "primeiro quadro"
            );
        }
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        self.app.clear_color(visuals)
    }
}
