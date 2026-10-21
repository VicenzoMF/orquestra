//! Binário `orquestra`: a UI. Ver `docs/ARCHITECTURE.md` §4.6 e §6.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use orquestra_app::backend::{self, Backend};
use orquestra_app::feed::Shared;
use orquestra_app::mock::NullStore;
use orquestra_app::ui::OrquestraApp;
use orquestra_core::{Command, Store};
use tracing::warn;

const USAGE: &str = "uso: orquestra [--mock]

  --mock   serviços simulados (padrão nesta fase)

Subcomandos `mcp` e `hook` chegam na integração (Fase 2).";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("mcp" | "hook") => {
            eprintln!("subcomando disponível após a integração");
            return ExitCode::from(2);
        }
        Some("-h" | "--help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    if let Some(bad) = args.iter().find(|a| a.as_str() != "--mock") {
        eprintln!("argumento desconhecido: {bad}\n\n{USAGE}");
        return ExitCode::from(2);
    }
    init_tracing();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("orquestra: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,eframe=warn,egui_glow=warn,egui_winit=warn,winit=warn")
    });
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn run() -> anyhow::Result<()> {
    // Poucas threads: o trabalho pesado é dos agentes, não do app.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("orquestra-rt")
        .enable_all()
        .build()?;

    // Fase 2: `SqliteStore::open_default()?` aqui; a config vem dele, e o
    // `start_mock` abaixo vira `backend::start` com os serviços reais.
    let config = NullStore.load_config()?;
    let Backend {
        bus: _bus,
        cmd,
        events,
        frames,
        hub,
    } = backend::start_mock(runtime.handle(), &config, 1.0, true)?;

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
        Box::new(move |cc| Ok(Box::new(OrquestraApp::new(cc, ui_cmd, shared, config)))),
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
