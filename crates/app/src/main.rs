//! Binário `orquestra`: a UI e os subcomandos que os agentes chamam.
//! Ver `docs/ARCHITECTURE.md` §2 e §4.6.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use orquestra_app::actions;
use orquestra_app::backend::{self, Backend};
use orquestra_app::feed::Shared;
use orquestra_app::mock::NullStore;
use orquestra_app::ui::OrquestraApp;
use orquestra_browser::Chromium;
use orquestra_core::{AppConfig, Command, Services, SessionConfig, Store, TermSize};
use orquestra_mcp::McpGateway;
use orquestra_runner::Runner;
use orquestra_store::{DesktopNotifier, SqliteStore};
use tracing::{info, warn};

const USAGE: &str = "uso: orquestra [--mock] [--repo <dir> --goal <objetivo>]
       orquestra mcp         ponte stdio do MCP `orquestra` (chamada pelo CLI do orquestrador)
       orquestra hook stop   hook Stop do orquestrador Claude Code

  --mock              serviços simulados: a UI sem agentes reais
  --repo, --goal      já abre uma orquestração nesse repositório com esse objetivo";

#[derive(Default)]
struct UiArgs {
    mock: bool,
    repo: Option<PathBuf>,
    goal: Option<String>,
}

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
    let ui_args = match parse_ui_args(&args) {
        Ok(a) => a,
        Err(err) => {
            eprintln!("{err}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    init_tracing("info,eframe=warn,egui_glow=warn,egui_winit=warn,winit=warn");
    match run(ui_args, started) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("orquestra: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn parse_ui_args(args: &[String]) -> anyhow::Result<UiArgs> {
    let mut out = UiArgs::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--mock" => out.mock = true,
            "--repo" => out.repo = Some(it.next().context("--repo sem valor")?.into()),
            "--goal" => out.goal = Some(it.next().context("--goal sem valor")?.clone()),
            other => bail!("argumento desconhecido: {other}"),
        }
    }
    if out.repo.is_some() != out.goal.is_some() {
        bail!("--repo e --goal andam juntos");
    }
    Ok(out)
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
            eprintln!("orquestra mcp: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(orquestra_mcp::run_stdio_bridge()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("orquestra mcp: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: UiArgs, started: Instant) -> anyhow::Result<()> {
    // Poucas threads: o trabalho pesado é dos agentes, não do app.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("orquestra-rt")
        .enable_all()
        .build()?;

    let (config, backend) = if args.mock {
        let config = NullStore.load_config()?;
        let backend = backend::start_mock(runtime.handle(), &config, 1.0, args.repo.is_none())?;
        (config, backend)
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
        repaint,
    );

    if let (Some(repo), Some(goal)) = (args.repo, args.goal) {
        start_orchestration(&config, &cmd, repo, goal)?;
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
    let result = eframe::run_native_ext(
        "orquestra",
        options,
        Some(egui_ctx),
        Box::new(move |cc| {
            Ok(Box::new(FirstFrame {
                app: OrquestraApp::new(cc, ui_cmd, shared, config),
                started,
                logged: false,
            }))
        }),
    );

    // Janela fechada: pede o shutdown e espera o hub por pouco tempo.
    let _ = cmd.send(Command::Shutdown);
    let joined =
        runtime.block_on(async { tokio::time::timeout(Duration::from_secs(5), hub).await });
    if joined.is_err() {
        warn!("hub não encerrou a tempo");
    }
    runtime.shutdown_timeout(Duration::from_millis(500));
    result.map_err(|err| anyhow!("{err}"))
}

/// Serviços reais: SQLite + config, PTYs, Chromium, notificações e o gateway MCP.
fn start_real(runtime: &tokio::runtime::Runtime) -> anyhow::Result<(AppConfig, Backend)> {
    let store = SqliteStore::open_default().context("abrir o banco")?;
    let interrupted = store.close_interrupted()?;
    if interrupted > 0 {
        info!(
            interrupted,
            "sessões da execução anterior fechadas como interrompidas"
        );
    }
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
    Ok((config, backend))
}

/// `--repo`/`--goal`: a mesma orquestração do diálogo "Nova orquestração".
fn start_orchestration(
    config: &AppConfig,
    cmd: &orquestra_core::CommandSender,
    repo: PathBuf,
    goal: String,
) -> anyhow::Result<()> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("repositório {}", repo.display()))?;
    let name = repo.file_name().map_or_else(
        || "orquestração".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let preset = config
        .presets
        .iter()
        .find(|p| p.name == config.orchestration.orchestrator_preset)
        .or_else(|| config.presets.first())
        .context("nenhum preset de agente na config")?;
    let session = SessionConfig::new(name, repo, goal);
    let (_, _, commands) = actions::new_orchestration(config, session, preset, TermSize::default());
    for c in commands {
        cmd.send(c).map_err(|_| anyhow!("hub parou"))?;
    }
    Ok(())
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
