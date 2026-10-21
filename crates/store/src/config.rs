//! `config.toml`: leitura tolerante e gravação atômica.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use orquestra_core::{AppConfig, Error, Result};

/// Lê a config. Arquivo ausente = `AppConfig::default()` (nada é criado);
/// TOML inválido = `Error::Config` com arquivo, linha e coluna.
pub fn load_config_file(path: &Path) -> Result<AppConfig> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(AppConfig::default()),
        Err(err) => {
            return Err(Error::Config(format!(
                "não foi possível ler {}: {err}",
                path.display()
            )));
        }
    };
    toml::from_str(&text).map_err(|err| Error::Config(describe(path, &text, &err)))
}

/// Grava de forma atômica: arquivo temporário no mesmo diretório, `fsync` e
/// `rename`. Cria o diretório se preciso, mantém a permissão do arquivo
/// antigo e, se `path` é um symlink, grava no alvo (dotfiles continuam
/// ligados).
pub fn save_config_file(path: &Path, config: &AppConfig) -> Result<()> {
    let text = toml::to_string_pretty(config)
        .map_err(|err| Error::Config(format!("não foi possível serializar a config: {err}")))?;
    let target = resolve_symlink(path);
    let dir = match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_owned(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&dir)
        .map_err(|err| Error::Config(format!("não foi possível criar {}: {err}", dir.display())))?;

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = target
        .file_name()
        .map_or_else(|| "config.toml".into(), |n| n.to_string_lossy());
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let written = write_then_rename(&tmp, &target, text.as_bytes());
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.map_err(|err| {
        Error::Config(format!(
            "não foi possível gravar {}: {err}",
            target.display()
        ))
    })?;
    sync_dir(&dir);
    Ok(())
}

fn write_then_rename(tmp: &Path, target: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create_new(tmp)?;
    if let Ok(meta) = fs::metadata(target) {
        file.set_permissions(meta.permissions())?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, target)
}

fn resolve_symlink(path: &Path) -> PathBuf {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
        }
        _ => path.to_owned(),
    }
}

/// Garante que o `rename` sobreviva a uma queda de energia.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// `config.toml, linha 3, coluna 13: invalid type: ...`
fn describe(path: &Path, text: &str, err: &toml::de::Error) -> String {
    match err.span() {
        Some(span) => {
            let (line, column) = line_column(text, span.start);
            format!(
                "{}, linha {line}, coluna {column}: {}",
                path.display(),
                err.message().trim_end()
            )
        }
        None => format!("{}: {}", path.display(), err.to_string().trim_end()),
    }
}

/// Linha e coluna (em caracteres), a partir de 1.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text.as_bytes()[..offset.min(text.len())];
    let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
    let start = before
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let column = String::from_utf8_lossy(&before[start..]).chars().count() + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use orquestra_core::Theme;

    use super::*;

    fn temp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        (dir, path)
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn missing_file_is_default_and_is_not_created() {
        let (_dir, path) = temp();
        assert_eq!(load_config_file(&path).unwrap(), AppConfig::default());
        assert!(!path.exists());
    }

    #[test]
    fn round_trips_and_creates_directory() {
        let (_dir, path) = temp();
        let mut cfg = AppConfig::default();
        cfg.ui.font_size = 15.5;
        cfg.ui.theme = Theme::Light;
        cfg.orchestration.max_concurrent_workers = 3;
        cfg.recent_repos = vec!["/src/loja".into(), "/src/blog".into()];
        cfg.runner.worktree_root = Some("/tmp/wt".into());

        save_config_file(&path, &cfg).unwrap();
        assert_eq!(load_config_file(&path).unwrap(), cfg);

        cfg.ui.font_size = 11.0;
        save_config_file(&path, &cfg).unwrap();
        assert_eq!(load_config_file(&path).unwrap(), cfg);
        assert!(leftovers(path.parent().unwrap()).is_empty());
    }

    #[test]
    fn invalid_toml_reports_line_and_column() {
        let (_dir, path) = temp();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[ui]\nfont_size = \"grande\"\n").unwrap();
        let Err(Error::Config(msg)) = load_config_file(&path) else {
            panic!("esperava Error::Config");
        };
        assert!(msg.contains("config.toml, linha 2, coluna 13"), "{msg}");
        assert!(msg.contains("invalid type"), "{msg}");

        fs::write(&path, "[ui\nfont_size = 1\n").unwrap();
        let Err(Error::Config(msg)) = load_config_file(&path) else {
            panic!("esperava Error::Config");
        };
        assert!(msg.contains("linha 1, coluna 4"), "{msg}");
    }

    #[test]
    fn partial_file_fills_defaults() {
        let (_dir, path) = temp();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[orchestration]\nmax_concurrent_workers = 2\n").unwrap();
        let cfg = load_config_file(&path).unwrap();
        assert_eq!(cfg.orchestration.max_concurrent_workers, 2);
        assert_eq!(cfg.ui, AppConfig::default().ui);
    }

    #[test]
    fn save_replaces_atomically_and_keeps_permissions() {
        let (_dir, path) = temp();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "lixo antigo").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        save_config_file(&path, &AppConfig::default()).unwrap();
        assert_eq!(load_config_file(&path).unwrap(), AppConfig::default());
        assert!(leftovers(path.parent().unwrap()).is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn failed_save_leaves_old_file_and_no_temp() {
        let (_dir, path) = temp();
        fs::create_dir_all(&path).unwrap(); // um diretório no lugar do arquivo: rename falha
        let err = save_config_file(&path, &AppConfig::default()).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err}");
        assert!(path.is_dir());
        assert!(leftovers(path.parent().unwrap()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn save_through_symlink_keeps_the_link() {
        let (dir, path) = temp();
        let real = dir.path().join("dotfiles").join("orquestra.toml");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&real, "").unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();

        let mut cfg = AppConfig::default();
        cfg.ui.font_size = 17.0;
        save_config_file(&path, &cfg).unwrap();
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(load_config_file(&real).unwrap(), cfg);
    }

    #[test]
    fn line_column_counts_characters() {
        assert_eq!(line_column("abc", 0), (1, 1));
        assert_eq!(line_column("a\nção = x", 8), (2, 5));
        assert_eq!(line_column("a\n", 99), (2, 1));
    }
}
