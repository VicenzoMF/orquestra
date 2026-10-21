//! Montagem do backend: bus, fila de comandos, assinaturas da UI e o `Hub`.
//!
//! Fase 2: troque `start_mock` por `start` com um `make` que monte os
//! serviços reais (ver `docs/ARCHITECTURE.md` §4.6):
//!
//! ```ignore
//! let backend = backend::start(rt.handle(), &config, |bus, cmd| {
//!     Ok(Services {
//!         runner: Runner::new(bus.clone(), config.runner.clone()),
//!         browser: Chromium::new(bus.clone(), config.browser.clone()),
//!         store,
//!         notifier: DesktopNotifier::new(),
//!         gateway: McpGateway::start(cmd.clone(), bus.clone(), config.clone())?,
//!     })
//! })?;
//! ```
//!
//! `make` roda dentro do runtime (pode dar `tokio::spawn`) e depois de a UI
//! assinar os dois canais.

use std::sync::Arc;

use orquestra_core::{
    AgentRunner, AppConfig, BrowserManager, CommandSender, EventBus, EventReceiver, Hub, HubConfig,
    Notifier, OrchestratorGateway, Services, Store, command_channel,
};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::mock::{LogNotifier, MockBrowser, MockGateway, MockRunner, NullStore};

pub type MockServices = Services<MockRunner, MockBrowser, NullStore, LogNotifier, MockGateway>;

/// Serviços falsos do modo `--mock`. `scale` multiplica os tempos simulados.
pub fn mock_services(
    bus: &EventBus,
    _cmd: &CommandSender,
    config: &AppConfig,
    scale: f64,
) -> MockServices {
    Services {
        runner: MockRunner::new(bus.clone(), config.runner.replay_bytes, scale),
        browser: MockBrowser::new(bus.clone(), scale),
        store: NullStore,
        notifier: LogNotifier,
        gateway: MockGateway,
    }
}

/// O que a UI recebe do backend.
pub struct Backend {
    pub bus: EventBus,
    pub cmd: CommandSender,
    /// Canal principal, assinado antes de o hub subir.
    pub events: EventReceiver,
    /// Canal de quadros do navegador.
    pub frames: EventReceiver,
    pub hub: JoinHandle<()>,
}

/// Cria bus e fila, assina os dois canais para a UI e sobe o hub com os
/// serviços que `make` montar.
pub fn start<R, B, S, N, G>(
    runtime: &Handle,
    config: &AppConfig,
    make: impl FnOnce(&EventBus, &CommandSender) -> anyhow::Result<Services<R, B, S, N, G>>,
) -> anyhow::Result<Backend>
where
    R: AgentRunner,
    B: BrowserManager,
    S: Store,
    N: Notifier,
    G: OrchestratorGateway,
{
    let bus = EventBus::new();
    let (cmd, cmd_rx) = command_channel();
    let events = bus.subscribe();
    let frames = bus.subscribe_frames();
    let services = {
        let _guard = runtime.enter();
        make(&bus, &cmd)?
    };
    let hub = Hub::new(services, bus.clone(), HubConfig::from_app(config));
    let hub = runtime.spawn(hub.run(cmd_rx));
    Ok(Backend {
        bus,
        cmd,
        events,
        frames,
        hub,
    })
}

/// Backend do modo `--mock`: serviços falsos, diretor (faz o papel do MCP)
/// e, se `seed`, as duas orquestrações de demonstração.
pub fn start_mock(
    runtime: &Handle,
    config: &AppConfig,
    scale: f64,
    seed: bool,
) -> anyhow::Result<Backend> {
    let backend = start(runtime, config, |bus, cmd| {
        Ok(mock_services(bus, cmd, config, scale))
    })?;
    runtime.spawn(crate::mock::run_director(
        backend.bus.subscribe(),
        backend.cmd.clone(),
        scale,
    ));
    if seed {
        runtime.spawn(crate::mock::seed(
            backend.cmd.clone(),
            backend.bus.subscribe(),
            config.clone(),
            scale,
        ));
    }
    Ok(backend)
}

/// Sobe as tasks de entrada da UI (eventos e quadros) sobre `shared`.
pub fn spawn_ui_feeds(
    runtime: &Handle,
    events: EventReceiver,
    frames: EventReceiver,
    cmd: CommandSender,
    shared: Arc<crate::feed::Shared>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    runtime.spawn(crate::feed::run_event_feeder(
        events,
        shared.clone(),
        cmd,
        repaint.clone(),
    ));
    runtime.spawn(crate::browser::run_frame_decoder(
        frames,
        shared.frames.clone(),
        repaint,
    ));
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use orquestra_core::{AgentRole, AgentStatus, Command, SessionConfig, TermSize};

    use super::*;
    use crate::actions;
    use crate::feed::{Shared, lock};

    /// Hub real + mocks: criar sessão + orquestrador faz os workers do
    /// diretor aparecerem no modelo, ligados ao orquestrador.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn orchestration_spawns_workers_into_the_model() {
        let config = AppConfig::default();
        let Backend {
            bus: _bus,
            cmd,
            events,
            frames,
            hub,
        } = start_mock(&Handle::current(), &config, 0.01, false).unwrap();
        let shared = Shared::new(config.orchestration.max_concurrent_workers, 500);
        let rt = Handle::current();
        spawn_ui_feeds(
            &rt,
            events,
            frames,
            cmd.clone(),
            shared.clone(),
            Arc::new(|| {}),
        );

        let session = SessionConfig::new("loja · teste", "/tmp/loja", "login");
        let (sid, orq, commands) =
            actions::new_orchestration(&config, session, &config.presets[0], TermSize::default());
        for c in commands {
            cmd.send(c).unwrap();
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            {
                let model = shared.model();
                let children = model.children(orq);
                if children.len() == 4
                    && children.iter().all(|c| {
                        c.status.is_terminal()
                            || c.status.occupies_slot()
                            || matches!(c.status, AgentStatus::Queued { .. })
                    })
                {
                    let session = model.session(sid).unwrap();
                    assert_eq!(session.orchestrator, Some(orq));
                    assert!(children.iter().all(|c| c.config.role == AgentRole::Worker));
                    assert!(children.iter().any(|c| c.config.browser));
                    break;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "workers não apareceram"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // O terminal do orquestrador recebeu a saída do roteiro.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let text = lock(&shared.term(orq).unwrap()).screen_text();
            if text.contains("spawn_agent") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "saída não chegou: {text}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        cmd.send(Command::Shutdown).unwrap();
        tokio::time::timeout(Duration::from_secs(5), hub)
            .await
            .expect("hub não encerrou")
            .unwrap();
    }
}
