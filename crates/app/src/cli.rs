//! Linha de comando do `orq`, como a do `code`: `orq .` abre o repositório
//! da pasta atual na janela (a aberta, se houver).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};

use crate::instance::OpenRequest;

pub const USAGE: &str = "uso: orq [pasta] [-g <objetivo>] [--foreground]
       orq --mock
       orq mcp         ponte stdio do MCP `orquestra` (chamada pelo CLI do orquestrador)
       orq hook stop   hook Stop do orquestrador Claude Code

  pasta               abre o repositório git dessa pasta (ex.: `orq .`): sem
                      objetivo, no diálogo de nova orquestração; se o app já
                      estiver aberto, nele
  -g, --goal          já começa a orquestração com esse objetivo (pasta: `.`)
  --foreground        não se desprende do terminal (logs aqui)
  --mock              serviços simulados: a UI sem agentes reais";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct UiArgs {
    pub mock: bool,
    pub foreground: bool,
    /// Interno: é o processo que o `orq` desprendeu do terminal.
    pub detached: bool,
    pub folder: Option<PathBuf>,
    pub goal: Option<String>,
}

impl UiArgs {
    pub fn parse(args: &[String]) -> anyhow::Result<Self> {
        let mut out = Self::default();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--mock" => out.mock = true,
                "--foreground" => out.foreground = true,
                "--detached" => out.detached = true,
                "-g" | "--goal" => {
                    let goal = it.next().context("--goal sem valor")?;
                    if goal.trim().is_empty() {
                        bail!("--goal vazio");
                    }
                    out.goal = Some(goal.clone());
                }
                flag if flag.starts_with('-') && flag != "-" => {
                    bail!("opção desconhecida: {flag}")
                }
                folder => {
                    if out.folder.is_some() {
                        bail!("só uma pasta por vez (recebi também {folder})");
                    }
                    out.folder = Some(folder.into());
                }
            }
        }
        if out.goal.is_some() && out.folder.is_none() {
            out.folder = Some(".".into());
        }
        Ok(out)
    }

    /// O pedido para a janela. Fora do `--mock`, a pasta vira a raiz do seu
    /// repositório git (cada agente trabalha num worktree dele).
    pub fn request(&self) -> anyhow::Result<OpenRequest> {
        let repo = match &self.folder {
            None => None,
            Some(folder) if self.mock => Some(
                folder
                    .canonicalize()
                    .with_context(|| folder.display().to_string())?,
            ),
            Some(folder) => Some(repo_root(folder)?),
        };
        Ok(OpenRequest {
            repo,
            goal: self.goal.clone(),
        })
    }
}

/// Raiz do repositório git que contém `folder`.
pub fn repo_root(folder: &Path) -> anyhow::Result<PathBuf> {
    let dir = folder
        .canonicalize()
        .with_context(|| folder.display().to_string())?;
    if !dir.is_dir() {
        bail!("{} não é uma pasta", dir.display());
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("rodar o git")?;
    if !out.status.success() {
        bail!(
            "{} não está num repositório git (cada agente trabalha num worktree dele)",
            dir.display()
        );
    }
    let root = String::from_utf8(out.stdout).context("caminho do repositório")?;
    Ok(PathBuf::from(root.trim_end_matches('\n')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> anyhow::Result<UiArgs> {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        UiArgs::parse(&args)
    }

    #[test]
    fn parses_like_code() {
        assert_eq!(parse(&[]).unwrap(), UiArgs::default());
        let a = parse(&["."]).unwrap();
        assert_eq!(a.folder.as_deref(), Some(Path::new(".")));
        assert_eq!(a.goal, None);
        let a = parse(&["~/x", "-g", "corrigir o login", "--foreground"]).unwrap();
        assert_eq!(a.folder.as_deref(), Some(Path::new("~/x")));
        assert_eq!(a.goal.as_deref(), Some("corrigir o login"));
        assert!(a.foreground);
        // Objetivo sem pasta: a pasta atual.
        let a = parse(&["--goal", "x"]).unwrap();
        assert_eq!(a.folder.as_deref(), Some(Path::new(".")));
        assert!(parse(&["--mock"]).unwrap().mock);
    }

    #[test]
    fn rejects_what_it_does_not_know() {
        assert!(parse(&["-g"]).is_err());
        assert!(parse(&["-g", "  "]).is_err());
        assert!(parse(&["--repo", "x"]).is_err());
        assert!(parse(&["a", "b"]).is_err());
    }

    #[test]
    fn folder_resolves_to_the_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("loja");
        std::fs::create_dir_all(repo.join("src/rotas")).unwrap();
        let git = Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap();
        assert!(git.success());
        let root = repo.canonicalize().unwrap();
        assert_eq!(repo_root(&repo.join("src/rotas")).unwrap(), root);

        let a = UiArgs {
            folder: Some(repo.join("src")),
            goal: Some("x".to_owned()),
            ..UiArgs::default()
        };
        let request = a.request().unwrap();
        assert_eq!(request.repo, Some(root));
        assert_eq!(request.goal.as_deref(), Some("x"));

        let outside = tmp.path().join("solta");
        std::fs::create_dir(&outside).unwrap();
        let err = repo_root(&outside).unwrap_err().to_string();
        assert!(err.contains("não está num repositório git"), "{err}");
        assert!(repo_root(&tmp.path().join("nada")).is_err());
    }
}
