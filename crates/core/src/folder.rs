//! Pastas de trabalho. Qualquer pasta serve; numa pasta git, um agente pode
//! ganhar um worktree e uma branch só dele (`Workspace::Worktree`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Raiz do repositório git que contém `dir`, ou `None` se `dir` não está num
/// repositório (ou o git não está instalado).
pub fn git_root(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let root = String::from_utf8(out.stdout).ok()?;
    let root = root.trim_end_matches('\n');
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// Caminho escrito por alguém (uma pessoa, um orquestrador): `~` é a home e
/// um caminho relativo parte de `base`.
pub fn resolve(input: &str, base: &Path) -> PathBuf {
    let input = input.trim();
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    let path = match input.strip_prefix('~') {
        Some("") => home(),
        Some(rest) if rest.starts_with('/') => home().map(|h| h.join(&rest[1..])),
        _ => None,
    }
    .unwrap_or_else(|| PathBuf::from(input));
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_home_relative_and_absolute() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let base = Path::new("/src/loja");
        assert_eq!(resolve("~", base), home);
        assert_eq!(resolve("~/Downloads", base), home.join("Downloads"));
        assert_eq!(resolve("/etc", base), PathBuf::from("/etc"));
        assert_eq!(resolve("docs", base), PathBuf::from("/src/loja/docs"));
        assert_eq!(
            resolve("  ../blog ", base),
            PathBuf::from("/src/loja/../blog")
        );
        // `~fulano` não é a home de ninguém aqui.
        assert_eq!(resolve("~x", base), PathBuf::from("/src/loja/~x"));
    }

    #[test]
    fn git_root_finds_the_repository_or_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("loja");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let ok = Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let root = repo.canonicalize().unwrap();
        assert_eq!(git_root(&repo.join("src")), Some(root));
        let loose = tmp.path().join("solta");
        std::fs::create_dir(&loose).unwrap();
        assert_eq!(git_root(&loose), None);
        assert_eq!(git_root(&tmp.path().join("nada")), None);
    }
}
