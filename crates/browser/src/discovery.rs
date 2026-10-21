//! Descoberta do executável do Chromium.
//!
//! Ordem: `config.executable` → `$ORQUESTRA_CHROMIUM` → `PATH` (`chromium`,
//! `chromium-browser`, `google-chrome-stable`, `google-chrome`, ignorando snap)
//! → cache do Playwright (`~/.cache/ms-playwright/chromium-*`, revisão mais nova).
//!
//! O snap fica de fora porque o confinamento quebra `--user-data-dir` fora do
//! `$HOME/snap` e a porta de debug.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use orquestra_core::BrowserConfig;
use tracing::warn;

/// Variável de ambiente que aponta um executável específico.
pub const ENV_EXECUTABLE: &str = "ORQUESTRA_CHROMIUM";

/// Nomes procurados no `PATH`, nesta ordem.
const PATH_NAMES: [&str; 4] = [
    "chromium",
    "chromium-browser",
    "google-chrome-stable",
    "google-chrome",
];

/// Executáveis dentro de `chromium-<revisão>/` no cache do Playwright.
const PLAYWRIGHT_BINARIES: [&str; 2] = ["chrome-linux64/chrome", "chrome-linux/chrome"];

/// Acha o Chromium a usar. `None` se nada servir.
///
/// Um caminho explícito (config ou variável) inválido gera um aviso e a busca
/// continua. Caminhos explícitos são respeitados mesmo se forem snap.
pub fn find_executable(config: &BrowserConfig) -> Option<PathBuf> {
    Search::from_env().find(config)
}

/// Onde procurar; separado do ambiente real para os testes.
#[derive(Debug, Default)]
struct Search {
    env_override: Option<OsString>,
    path: Option<OsString>,
    playwright_roots: Vec<PathBuf>,
}

impl Search {
    fn from_env() -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        let mut playwright_roots = Vec::new();
        if let Some(dir) = var("PLAYWRIGHT_BROWSERS_PATH").filter(|v| v != "0") {
            playwright_roots.push(PathBuf::from(dir));
        }
        if let Some(cache) = var("XDG_CACHE_HOME") {
            playwright_roots.push(PathBuf::from(cache).join("ms-playwright"));
        }
        if let Some(home) = var("HOME") {
            playwright_roots.push(PathBuf::from(home).join(".cache/ms-playwright"));
        }
        playwright_roots.dedup();
        Self {
            env_override: var(ENV_EXECUTABLE),
            path: var("PATH"),
            playwright_roots,
        }
    }

    fn find(&self, config: &BrowserConfig) -> Option<PathBuf> {
        let explicit = [
            ("browser.executable", config.executable.clone()),
            (ENV_EXECUTABLE, self.env_override.clone().map(PathBuf::from)),
        ];
        for (origin, candidate) in explicit {
            let Some(candidate) = candidate else { continue };
            match self.resolve_explicit(&candidate) {
                Some(path) => return Some(path),
                None => warn!(
                    origem = origin,
                    caminho = %candidate.display(),
                    "Chromium indicado não é executável; procurando outro"
                ),
            }
        }
        self.find_in_path().or_else(|| self.find_in_playwright())
    }

    /// Caminho com `/` vale como está; nome solto é procurado no `PATH`.
    fn resolve_explicit(&self, candidate: &Path) -> Option<PathBuf> {
        if candidate.components().count() > 1 {
            return is_executable(candidate).then(|| candidate.to_owned());
        }
        self.path_dirs()
            .map(|dir| dir.join(candidate))
            .find(|p| is_executable(p))
    }

    fn find_in_path(&self) -> Option<PathBuf> {
        PATH_NAMES.iter().find_map(|name| {
            self.path_dirs()
                .map(|dir| dir.join(name))
                .find(|p| is_executable(p) && !is_snap(p))
        })
    }

    fn find_in_playwright(&self) -> Option<PathBuf> {
        self.playwright_roots.iter().find_map(|root| {
            let mut revisions: Vec<(u64, PathBuf)> = fs::read_dir(root)
                .ok()?
                .filter_map(|entry| {
                    let entry = entry.ok()?;
                    let name = entry.file_name();
                    let revision = name.to_str()?.strip_prefix("chromium-")?.parse().ok()?;
                    Some((revision, entry.path()))
                })
                .collect();
            revisions.sort_by_key(|r| std::cmp::Reverse(r.0));
            revisions.into_iter().find_map(|(_, dir)| {
                PLAYWRIGHT_BINARIES
                    .iter()
                    .map(|bin| dir.join(bin))
                    .find(|p| is_executable(p))
            })
        })
    }

    fn path_dirs(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.path
            .iter()
            .flat_map(std::env::split_paths)
            .filter(|dir| !dir.as_os_str().is_empty())
    }
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Snap: caminho em `/snap/`, link para o lançador `/usr/bin/snap` (é o que
/// `/snap/bin/chromium` é) ou script de transição que chama o snap (o
/// `chromium-browser` do Ubuntu).
fn is_snap(path: &Path) -> bool {
    if path.starts_with("/snap") {
        return true;
    }
    if let Ok(real) = fs::canonicalize(path)
        && (real.starts_with("/snap") || real.file_name().is_some_and(|n| n == "snap"))
    {
        return true;
    }
    let mut head = [0u8; 4096];
    let Ok(n) = fs::File::open(path).and_then(|mut f| f.read(&mut head)) else {
        return false;
    };
    let head = &head[..n];
    head.starts_with(b"#!") && head.windows(4).any(|w| w == b"snap")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn search(path: &[&Path]) -> Search {
        Search {
            path: Some(std::env::join_paths(path).unwrap()),
            ..Search::default()
        }
    }

    #[test]
    fn config_wins_over_env_and_path() {
        let tmp = tempfile::tempdir().unwrap();
        let configured = tmp.path().join("meu/chrome");
        let from_env = tmp.path().join("env/chrome");
        let bin = tmp.path().join("bin");
        exe(&configured, "");
        exe(&from_env, "");
        exe(&bin.join("chromium"), "");

        let mut s = search(&[&bin]);
        s.env_override = Some(from_env.clone().into());
        let config = BrowserConfig {
            executable: Some(configured.clone()),
            ..BrowserConfig::default()
        };
        assert_eq!(s.find(&config), Some(configured));
        assert_eq!(s.find(&BrowserConfig::default()), Some(from_env));
    }

    #[test]
    fn invalid_explicit_path_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        exe(&bin.join("google-chrome"), "");
        let config = BrowserConfig {
            executable: Some(tmp.path().join("nao/existe")),
            ..BrowserConfig::default()
        };
        assert_eq!(
            search(&[&bin]).find(&config),
            Some(bin.join("google-chrome"))
        );
    }

    #[test]
    fn bare_name_in_config_is_looked_up_in_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        exe(&bin.join("brave"), "");
        let config = BrowserConfig {
            executable: Some("brave".into()),
            ..BrowserConfig::default()
        };
        assert_eq!(search(&[&bin]).find(&config), Some(bin.join("brave")));
    }

    #[test]
    fn path_order_and_snap_wrappers() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        exe(
            &a.join("chromium-browser"),
            "#!/bin/sh\nexec /snap/bin/chromium \"$@\"\n",
        );
        exe(&b.join("google-chrome"), "");
        exe(&b.join("google-chrome-stable"), "");
        // Como /snap/bin/chromium -> /usr/bin/snap.
        exe(&tmp.path().join("lancador/snap"), "");
        std::os::unix::fs::symlink(tmp.path().join("lancador/snap"), b.join("chromium")).unwrap();
        // Sem permissão de execução: ignorado.
        fs::write(a.join("chromium"), "").unwrap();

        assert_eq!(
            search(&[&a, &b]).find(&BrowserConfig::default()),
            Some(b.join("google-chrome-stable"))
        );
    }

    #[test]
    fn playwright_cache_picks_newest_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ms-playwright");
        exe(&root.join("chromium-1200/chrome-linux/chrome"), "");
        exe(&root.join("chromium-1248/chrome-linux64/chrome"), "");
        exe(
            &root.join("chromium_headless_shell-1300/chrome-linux64/chrome"),
            "",
        );
        let s = Search {
            playwright_roots: vec![tmp.path().join("vazio"), root.clone()],
            ..Search::default()
        };
        assert_eq!(
            s.find(&BrowserConfig::default()),
            Some(root.join("chromium-1248/chrome-linux64/chrome"))
        );
    }

    #[test]
    fn nothing_found() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(search(&[tmp.path()]).find(&BrowserConfig::default()), None);
    }
}
