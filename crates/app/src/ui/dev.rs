//! Ganchos de desenvolvimento (feature `screenshot`): prepara uma tela,
//! tira um screenshot em PNG e fecha.
//!
//! - `ORQUESTRA_SCREENSHOT=/caminho.png`: liga os ganchos;
//! - `ORQUESTRA_SCREENSHOT_DELAY=segundos` (padrão 14);
//! - `ORQUESTRA_DEMO_SELECT=nome`: seleciona o agente com esse nome;
//! - `ORQUESTRA_DEMO_TAB=navegador`: abre a aba do navegador;
//! - `ORQUESTRA_DEMO_FOCUS=prefixo`: foca a sessão cujo nome começa assim;
//! - `ORQUESTRA_DEMO_DIALOG=nova|nova-mais|avulso|config`: abre o rascunho
//!   de orquestração no canvas (`nova-mais`: com todas as opções) ou um
//!   diálogo.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use egui::ViewportCommand;

use super::canvas::CanvasState;
use super::dialogs::Dialogs;
use super::{Ctx, SideTab, UiState};

pub struct DevHooks {
    path: Option<PathBuf>,
    delay: Duration,
    start: Instant,
    select: Option<String>,
    browser_tab: bool,
    focus: Option<String>,
    dialog: Option<String>,
    prepared: bool,
    requested: bool,
    saved: Arc<AtomicBool>,
}

impl DevHooks {
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            path: var("ORQUESTRA_SCREENSHOT").map(PathBuf::from),
            delay: Duration::from_secs_f64(
                var("ORQUESTRA_SCREENSHOT_DELAY")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(14.0),
            ),
            start: Instant::now(),
            select: var("ORQUESTRA_DEMO_SELECT"),
            browser_tab: var("ORQUESTRA_DEMO_TAB").is_some_and(|t| t.starts_with("nav")),
            focus: var("ORQUESTRA_DEMO_FOCUS"),
            dialog: var("ORQUESTRA_DEMO_DIALOG"),
            prepared: false,
            requested: false,
            saved: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn frame(
        &mut self,
        ctx: &egui::Context,
        cx: &Ctx,
        st: &mut UiState,
        canvas: &mut CanvasState,
        dialogs: &mut Dialogs,
    ) {
        let model = cx.model;
        let Some(path) = self.path.clone() else {
            return;
        };
        let elapsed = self.start.elapsed();
        if !self.prepared && elapsed >= self.delay.mul_f64(0.7) {
            self.prepared = true;
            match self.dialog.as_deref() {
                Some(which @ ("nova" | "nova-mais")) => {
                    // O `--mock` não tem recentes: usa a pasta da primeira sessão.
                    let repo = cx
                        .config
                        .recent_repos
                        .first()
                        .cloned()
                        .or_else(|| model.sessions().first().map(|s| s.config.repo.clone()));
                    match repo {
                        Some(repo) => canvas.open_draft_in(cx.config, &repo),
                        None => canvas.open_draft(cx.config),
                    }
                    if let Some(d) = canvas.draft_mut() {
                        d.more = which == "nova-mais";
                    }
                }
                Some("avulso") => dialogs.open_solo(cx, st),
                Some("config") => dialogs.open_settings(cx.config),
                _ => {}
            }
            if let Some(prefix) = &self.focus
                && let Some(s) = model
                    .sessions()
                    .into_iter()
                    .find(|s| s.config.name.starts_with(prefix.as_str()))
            {
                st.focus = Some(s.id);
                canvas.request_fit();
            }
            if let Some(name) = &self.select {
                let found = model
                    .canvas_order(None)
                    .into_iter()
                    .find(|id| model.agent(*id).is_some_and(|a| &a.config.name == name));
                if let Some(id) = found {
                    st.select(id);
                    if self.browser_tab {
                        st.tab = SideTab::Browser;
                    }
                }
            }
        }
        if self.saved.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        if !self.requested && elapsed >= self.delay {
            self.requested = true;
            let saved = self.saved.clone();
            let repaint = ctx.clone();
            ctx.request_screenshot(move |image| {
                let [w, h] = image.size;
                match image::save_buffer(
                    &path,
                    image.as_raw(),
                    w as u32,
                    h as u32,
                    image::ExtendedColorType::Rgba8,
                ) {
                    Ok(()) => tracing::info!(path = %path.display(), "screenshot salvo"),
                    Err(err) => tracing::error!(%err, "screenshot falhou"),
                }
                saved.store(true, Ordering::Relaxed);
                repaint.request_repaint();
            });
        }
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}
