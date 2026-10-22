//! Linha de comando do `orq`, como a do `code`: `orq .` abre a pasta atual
//! na janela (a aberta, se houver).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use orquestra_core::folder;

use crate::instance::OpenRequest;

pub const USAGE: &str = "uso: orq [pasta] [-g <tarefa>] [--foreground]
       orq --mock
       orq mcp         ponte stdio do MCP `orquestra` (chamada pelo CLI do orquestrador)
       orq hook stop   hook Stop do orquestrador Claude Code

  pasta               abre essa pasta (ex.: `orq .`; num repositório git, a
                      raiz dele): sem objetivo, no rascunho do canvas; se o
                      app já estiver aberto, nele
  -g, --goal          já abre um agente com essa tarefa (pasta: `.`)
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

    /// O pedido para a janela. Fora do `--mock`, uma pasta dentro de um
    /// repositório git vira a raiz dele (onde os worktrees nascem).
    pub fn request(&self) -> anyhow::Result<OpenRequest> {
        let repo = match &self.folder {
            None => None,
            Some(folder) if self.mock => Some(
                folder
                    .canonicalize()
                    .with_context(|| folder.display().to_string())?,
            ),
            Some(folder) => Some(project_dir(folder)?),
        };
        Ok(OpenRequest {
            repo,
            goal: self.goal.clone(),
        })
    }
}

/// A pasta a abrir: a raiz do repositório git que contém `dir`, ou a própria
/// pasta, se ela não está num repositório.
pub fn project_dir(dir: &Path) -> anyhow::Result<PathBuf> {
    let dir = dir
        .canonicalize()
        .with_context(|| dir.display().to_string())?;
    if !dir.is_dir() {
        bail!("{} não é uma pasta", dir.display());
    }
    Ok(folder::git_root(&dir).unwrap_or(dir))
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
    fn folder_resolves_to_the_repo_root_or_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("loja");
        std::fs::create_dir_all(repo.join("src/rotas")).unwrap();
        let git = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap();
        assert!(git.success());
        let root = repo.canonicalize().unwrap();
        assert_eq!(project_dir(&repo.join("src/rotas")).unwrap(), root);

        let a = UiArgs {
            folder: Some(repo.join("src")),
            goal: Some("x".to_owned()),
            ..UiArgs::default()
        };
        let request = a.request().unwrap();
        assert_eq!(request.repo, Some(root));
        assert_eq!(request.goal.as_deref(), Some("x"));

        // Fora de um repositório, a própria pasta.
        let outside = tmp.path().join("solta");
        std::fs::create_dir(&outside).unwrap();
        assert_eq!(
            project_dir(&outside).unwrap(),
            outside.canonicalize().unwrap()
        );
        assert!(project_dir(&tmp.path().join("nada")).is_err());
        let file = outside.join("a.txt");
        std::fs::write(&file, "").unwrap();
        assert!(project_dir(&file).is_err());
    }
}
