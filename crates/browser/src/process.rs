//! Processo do Chromium: argumentos, perfil temporário e porta de debug.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use orquestra_core::BrowserConfig;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Prazo para o Chromium anunciar a porta de debug.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Linhas finais do stderr guardadas para explicar uma falha.
const STDERR_TAIL: usize = 12;
const DEVTOOLS_PREFIX: &str = "DevTools listening on ";
/// Prefixo dos perfis temporários, em `std::env::temp_dir()`.
const PROFILE_PREFIX: &str = "orquestra-chromium-";
const NO_SANDBOX: &str = "--no-sandbox";

/// Recursos que ficam fora do perfil e só pesam (sem first-run, sync,
/// extensões, telemetria). Mesma linha do que o Playwright usa.
const BASE_ARGS: &[&str] = &[
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-sync",
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-component-extensions-with-background-pages",
    "--disable-extensions",
    "--disable-default-apps",
    "--disable-breakpad",
    "--disable-client-side-phishing-detection",
    "--disable-field-trial-config",
    "--disable-hang-monitor",
    "--disable-popup-blocking",
    "--disable-prompt-on-repost",
    "--disable-search-engine-choice-screen",
    "--disable-dev-shm-usage",
    // Abas de teste não podem congelar por estarem "em segundo plano".
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-renderer-backgrounding",
    "--disable-features=Translate,MediaRouter,DialMediaRouteProvider,GlobalMediaControls,OptimizationHints,LensOverlay,PaintHolding,AcceptCHFrame",
    "--metrics-recording-only",
    "--password-store=basic",
    "--use-mock-keychain",
    "--no-service-autorun",
    "--mute-audio",
    "--force-color-profile=srgb",
];

/// Chromium no ar com a porta de debug conhecida.
#[derive(Debug)]
pub(crate) struct Process {
    pub child: Child,
    pub ws_url: String,
    pub port: u16,
    pub profile: TempDir,
}

struct SpawnError {
    reason: String,
    /// O stderr pediu `--no-sandbox` (Ubuntu 23.10+ sem userns, binário sem
    /// perfil do AppArmor, como o do Playwright).
    needs_no_sandbox: bool,
}

/// Sobe o Chromium e espera a porta de debug. Se o sandbox não estiver
/// disponível, tenta de novo com `--no-sandbox`, como o Playwright faz por
/// padrão.
pub(crate) async fn spawn(exe: &Path, config: &BrowserConfig) -> Result<Process, String> {
    let forced = config.extra_args.iter().any(|a| a == NO_SANDBOX);
    match spawn_once(exe, config, false).await {
        Err(err) if err.needs_no_sandbox && !forced => {
            warn!(exe = %exe.display(), "sandbox do Chromium indisponível; repetindo com --no-sandbox");
            spawn_once(exe, config, true).await.map_err(|e| e.reason)
        }
        other => other.map_err(|e| e.reason),
    }
}

/// Argumentos de linha de comando (sem o executável).
pub(crate) fn args(config: &BrowserConfig, profile: &Path, no_sandbox: bool) -> Vec<String> {
    let vp = config.viewport;
    let mut args = vec![
        "--remote-debugging-port=0".to_owned(),
        format!("--user-data-dir={}", profile.display()),
    ];
    if config.headless {
        args.push("--headless=new".to_owned());
    }
    args.extend(BASE_ARGS.iter().map(|a| (*a).to_owned()));
    // Tamanho da janela; o controlador corrige a moldura depois.
    args.push(format!(
        "--window-size={},{}",
        vp.width.max(1),
        vp.height.max(1)
    ));
    if (vp.device_scale_factor - 1.0).abs() > f64::EPSILON && vp.device_scale_factor > 0.0 {
        args.push(format!(
            "--force-device-scale-factor={}",
            vp.device_scale_factor
        ));
    }
    if no_sandbox {
        args.push(NO_SANDBOX.to_owned());
    }
    args.extend(config.extra_args.iter().cloned());
    args.push("about:blank".to_owned());
    args
}

async fn spawn_once(
    exe: &Path,
    config: &BrowserConfig,
    no_sandbox: bool,
) -> Result<Process, SpawnError> {
    let profile = tempfile::Builder::new()
        .prefix(PROFILE_PREFIX)
        .tempdir()
        .map_err(|e| SpawnError {
            reason: format!("perfil temporário: {e}"),
            needs_no_sandbox: false,
        })?;
    match run(exe, config, no_sandbox, profile.path()).await {
        Ok((child, ws_url, port)) => Ok(Process {
            child,
            ws_url,
            port,
            profile,
        }),
        Err(e) => {
            // Filhos do Chromium que falhou ainda podem gravar no perfil.
            let _ = tokio::task::spawn_blocking(move || remove_profile(profile)).await;
            Err(e)
        }
    }
}

/// Sobe o processo e espera o endereço de debug.
async fn run(
    exe: &Path,
    config: &BrowserConfig,
    no_sandbox: bool,
    profile: &Path,
) -> Result<(Child, String, u16), SpawnError> {
    let fail = |reason: String| SpawnError {
        reason,
        needs_no_sandbox: false,
    };
    let mut child = Command::new(exe)
        .args(args(config, profile, no_sandbox))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| fail(format!("executar {}: {e}", exe.display())))?;

    let log = SharedLog::default();
    let (url_tx, mut url_rx) = mpsc::channel(1);
    let stderr = child
        .stderr
        .take()
        .map(|s| tokio::spawn(read_stderr(s, url_tx, log.clone())));

    let active_port_file = profile.join("DevToolsActivePort");
    let deadline = tokio::time::sleep(LAUNCH_TIMEOUT);
    tokio::pin!(deadline);
    let mut poll = tokio::time::interval(Duration::from_millis(100));
    let mut stderr_open = true;

    let ws_url = loop {
        tokio::select! {
            url = url_rx.recv(), if stderr_open => match url {
                Some(url) => break url,
                None => stderr_open = false,
            },
            _ = poll.tick() => {
                if let Some(url) = read_active_port(&active_port_file).await {
                    break url;
                }
            }
            status = child.wait() => {
                // Deixa o leitor drenar o resto do stderr antes de explicar.
                if let Some(task) = stderr {
                    let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
                }
                return Err(exited(status, &log));
            }
            () = &mut deadline => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(fail(format!(
                    "Chromium não abriu a porta de debug em {LAUNCH_TIMEOUT:?}{}",
                    summary(&log)
                )));
            }
        }
    };

    let Some(port) = port_of(&ws_url) else {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(fail(format!("endereço de debug inesperado: {ws_url}")));
    };
    debug!(%ws_url, pid = child.id(), "Chromium no ar");
    Ok((child, ws_url, port))
}

/// O que o stderr disse, para explicar uma falha.
#[derive(Debug, Default)]
struct StderrLog {
    /// Primeira linha FATAL: é a que explica (depois vem a pilha).
    fatal: Option<String>,
    tail: VecDeque<String>,
}

impl StderrLog {
    fn push(&mut self, line: String) {
        if self.fatal.is_none() && line.contains(":FATAL:") {
            self.fatal = Some(line.clone());
        }
        if self.tail.len() == STDERR_TAIL {
            self.tail.pop_front();
        }
        self.tail.push_back(line);
    }

    fn needs_no_sandbox(&self) -> bool {
        self.fatal
            .iter()
            .chain(&self.tail)
            .any(|l| l.contains("No usable sandbox"))
    }

    /// `": <linha>"` com a FATAL ou a última linha, encurtada.
    fn summary(&self) -> String {
        let Some(line) = self.fatal.as_ref().or(self.tail.back()) else {
            return String::new();
        };
        let mut line = line.trim().to_owned();
        if line.len() > 400 {
            let cut = (0..=400)
                .rev()
                .find(|i| line.is_char_boundary(*i))
                .unwrap_or(0);
            line.truncate(cut);
            line.push('…');
        }
        format!(": {line}")
    }
}

type SharedLog = Arc<Mutex<StderrLog>>;

fn summary(log: &SharedLog) -> String {
    log.lock().map(|l| l.summary()).unwrap_or_default()
}

/// Repassa o endereço anunciado e continua drenando o stderr até o fim (um
/// pipe cheio travaria o Chromium).
async fn read_stderr(stderr: ChildStderr, url_tx: mpsc::Sender<String>, log: SharedLog) {
    let mut lines = BufReader::new(stderr).lines();
    let mut url_tx = Some(url_tx);
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(url) = line.trim().strip_prefix(DEVTOOLS_PREFIX) {
            if let Some(tx) = url_tx.take() {
                let _ = tx.send(url.trim().to_owned()).await;
            }
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        debug!(target: "orquestra_browser::chromium", "{line}");
        if let Ok(mut log) = log.lock() {
            log.push(line);
        }
    }
}

/// `DevToolsActivePort` no perfil: porta e caminho do WebSocket.
async fn read_active_port(file: &Path) -> Option<String> {
    let text = tokio::fs::read_to_string(file).await.ok()?;
    let mut lines = text.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    path.starts_with('/')
        .then(|| format!("ws://127.0.0.1:{port}{path}"))
}

/// Porta de `ws://127.0.0.1:<porta>/devtools/browser/<id>`.
pub(crate) fn port_of(ws_url: &str) -> Option<u16> {
    let rest = ws_url.split_once("://")?.1;
    let authority = rest.split('/').next()?;
    authority.rsplit_once(':')?.1.parse().ok()
}

fn exited(status: std::io::Result<ExitStatus>, log: &SharedLog) -> SpawnError {
    let status = match status {
        Ok(s) => s.to_string(),
        Err(e) => e.to_string(),
    };
    SpawnError {
        reason: format!(
            "Chromium saiu durante a inicialização ({status}){}",
            summary(log)
        ),
        needs_no_sandbox: log.lock().is_ok_and(|l| l.needs_no_sandbox()),
    }
}

/// Apaga o perfil, com algumas tentativas: filhos do Chromium que ainda
/// estão saindo podem recriar arquivos no meio da remoção.
pub(crate) fn remove_profile(profile: TempDir) {
    let path: PathBuf = profile.keep();
    let mut last = None;
    for attempt in 0..5 {
        match std::fs::remove_dir_all(&path) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_millis(50 << attempt));
    }
    if let Some(e) = last {
        warn!(perfil = %path.display(), %e, "não consegui apagar o perfil do Chromium");
    }
}

/// Apaga perfis largados por execuções que morreram sem `shutdown`: mais
/// velhos que `min_age` (um lançamento em curso ainda não tem trava) e sem
/// Chromium vivo (`SingletonLock` ausente ou apontando para um pid morto).
/// Devolve quantos apagou.
pub(crate) fn sweep_stale_profiles(dir: &Path, min_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let ours = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(PROFILE_PREFIX));
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age >= min_age));
        if !ours || !old || !path.is_dir() || locked_by_live_chromium(&path) {
            continue;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        debug!(removed, "perfis do Chromium largados apagados");
    }
    removed
}

/// `SingletonLock` é um link simbólico para `<host>-<pid>`.
fn locked_by_live_chromium(profile: &Path) -> bool {
    let Ok(target) = std::fs::read_link(profile.join("SingletonLock")) else {
        return false;
    };
    let pid = target
        .to_str()
        .and_then(|t| t.rsplit_once('-'))
        .and_then(|(_, pid)| pid.parse::<u32>().ok());
    match pid {
        Some(pid) => Path::new(&format!("/proc/{pid}")).exists(),
        // Formato desconhecido: na dúvida, está em uso.
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_from_ws_url() {
        assert_eq!(
            port_of("ws://127.0.0.1:33693/devtools/browser/a2e1"),
            Some(33693)
        );
        assert_eq!(port_of("ws://[::1]:9222/devtools/browser/x"), Some(9222));
        assert_eq!(port_of("ws://127.0.0.1/devtools"), None);
    }

    #[test]
    fn args_follow_config() {
        let mut config = BrowserConfig {
            extra_args: vec!["--lang=pt-BR".to_owned()],
            ..BrowserConfig::default()
        };
        let a = args(&config, Path::new("/tmp/perfil"), false);
        assert!(a.contains(&"--headless=new".to_owned()));
        assert!(a.contains(&"--remote-debugging-port=0".to_owned()));
        assert!(a.contains(&"--user-data-dir=/tmp/perfil".to_owned()));
        assert!(a.contains(&"--window-size=1280,800".to_owned()));
        assert!(
            !a.iter()
                .any(|x| x.starts_with("--force-device-scale-factor"))
        );
        assert!(!a.contains(&NO_SANDBOX.to_owned()));
        // Extras do usuário depois dos nossos (vencem em flags repetidas);
        // a URL inicial por último.
        assert_eq!(a[a.len() - 2], "--lang=pt-BR");
        assert_eq!(a.last().unwrap(), "about:blank");

        config.headless = false;
        config.viewport.device_scale_factor = 2.0;
        let a = args(&config, Path::new("/tmp/perfil"), true);
        assert!(!a.iter().any(|x| x.starts_with("--headless")));
        assert!(a.contains(&"--force-device-scale-factor=2".to_owned()));
        assert!(a.contains(&NO_SANDBOX.to_owned()));
    }

    #[tokio::test]
    async fn active_port_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("DevToolsActivePort");
        assert_eq!(read_active_port(&file).await, None);
        std::fs::write(&file, "41745\n/devtools/browser/abc\n").unwrap();
        assert_eq!(
            read_active_port(&file).await.as_deref(),
            Some("ws://127.0.0.1:41745/devtools/browser/abc")
        );
    }

    #[test]
    fn stderr_log_keeps_the_fatal_line() {
        let mut log = StderrLog::default();
        log.push("[1:1:ERROR:bus.cc] dbus".to_owned());
        log.push(
            "[2:2:FATAL:zygote_host_impl_linux.cc:130] No usable sandbox! If you are running on Ubuntu"
                .to_owned(),
        );
        for i in 0..40 {
            log.push(format!("#{i} 0x64d40e65ce33 (chrome+0x6e7ce32)"));
        }
        assert!(log.needs_no_sandbox());
        assert!(
            log.summary().contains("No usable sandbox"),
            "{}",
            log.summary()
        );
        assert_eq!(log.tail.len(), STDERR_TAIL);
    }

    #[test]
    fn sweep_removes_only_dead_profiles() {
        let root = tempfile::tempdir().unwrap();
        let mk = |name: &str| {
            let p = root.path().join(name);
            std::fs::create_dir_all(p.join("Default/Cache")).unwrap();
            p
        };
        let orphan = mk("orquestra-chromium-orfao");
        let dead = mk("orquestra-chromium-morto");
        std::os::unix::fs::symlink("maquina-4294967295", dead.join("SingletonLock")).unwrap();
        let live = mk("orquestra-chromium-vivo");
        let me = format!("maquina-{}", std::process::id());
        std::os::unix::fs::symlink(me, live.join("SingletonLock")).unwrap();
        let other = mk("outra-coisa");

        assert_eq!(sweep_stale_profiles(root.path(), Duration::ZERO), 2);
        assert!(!orphan.exists() && !dead.exists());
        assert!(live.exists() && other.exists());
        // Recentes ficam: podem ser um lançamento em curso.
        let fresh = mk("orquestra-chromium-novo");
        assert_eq!(
            sweep_stale_profiles(root.path(), Duration::from_secs(600)),
            0
        );
        assert!(fresh.exists());
    }

    #[test]
    fn remove_profile_deletes_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        std::fs::create_dir_all(path.join("Default/Local Storage")).unwrap();
        std::fs::write(path.join("Default/Cookies"), "x").unwrap();
        remove_profile(dir);
        assert!(!path.exists());
    }
}
