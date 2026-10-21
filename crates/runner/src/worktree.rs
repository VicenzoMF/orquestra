//! Um git worktree por agente, pela CLI `git` (nunca libgit2).
//!
//! Local: `<raiz>/<repo>-<hash8 do caminho>/<id curto>`, com raiz =
//! `worktree_root` ou `<data_dir>/worktrees`. Branch:
//! `<branch_prefix><nome-sanitizado>-<id curto>`, salvo se pedida.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use orquestra_core::{AgentId, Error, Result, RunnerConfig, WorktreeCleanup, WorktreeInfo};
use tracing::warn;

/// Worktree recém-criado.
pub(crate) struct Created {
    pub(crate) info: WorktreeInfo,
    /// A branch foi criada agora (e pode ser apagada se o spawn falhar).
    pub(crate) new_branch: bool,
}

/// O que identifica o worktree a criar.
pub(crate) struct Request<'a> {
    pub(crate) repo: &'a Path,
    pub(crate) base: Option<&'a str>,
    pub(crate) branch: Option<&'a str>,
    pub(crate) agent_name: &'a str,
    pub(crate) id: AgentId,
}

pub(crate) fn root_dir(config: &RunnerConfig) -> Result<PathBuf> {
    if let Some(root) = &config.worktree_root {
        return Ok(root.clone());
    }
    directories::ProjectDirs::from("", "", "orquestra")
        .map(|dirs| dirs.data_dir().join("worktrees"))
        .ok_or_else(|| Error::Config("diretório de dados do usuário indisponível".to_owned()))
}

pub(crate) fn create(root: &Path, prefix: &str, req: &Request<'_>) -> Result<Created> {
    if !req.repo.is_dir() {
        return Err(Error::Git(format!(
            "repositório não existe: {}",
            req.repo.display()
        )));
    }
    let repo = PathBuf::from(git(req.repo, ["rev-parse", "--show-toplevel"])?);
    let repo = repo.canonicalize().unwrap_or(repo);
    let path = root.join(repo_dir_name(&repo)).join(req.id.short());

    // Branch pedida que já existe: o worktree só faz checkout dela.
    let existing = match req.branch {
        Some(branch) => branch_exists(&repo, branch)?,
        None => false,
    };
    let branch = match req.branch {
        Some(branch) => branch.to_owned(),
        None => default_branch(prefix, req.agent_name, req.id),
    };
    let base_ref = if existing {
        branch.clone()
    } else {
        req.base.unwrap_or("HEAD").to_owned()
    };
    let base_commit = rev_parse_commit(&repo, &base_ref)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path_arg = path.as_os_str();
    if existing {
        git(&repo, [os("worktree"), os("add"), path_arg, os(&branch)])?;
    } else {
        git(
            &repo,
            [
                os("worktree"),
                os("add"),
                os("-b"),
                os(&branch),
                path_arg,
                os(&base_commit),
            ],
        )?;
    }
    Ok(Created {
        info: WorktreeInfo {
            repo,
            path,
            branch,
            base_ref,
            base_commit,
        },
        new_branch: !existing,
    })
}

/// Aplica o `cleanup` do `remove`.
pub(crate) fn cleanup(info: &WorktreeInfo, cleanup: WorktreeCleanup) -> Result<()> {
    match cleanup {
        WorktreeCleanup::Keep => Ok(()),
        WorktreeCleanup::RemoveWorktree => remove_worktree(info),
        WorktreeCleanup::RemoveWorktreeAndBranch => {
            remove_worktree(info)?;
            git(&info.repo, [os("branch"), os("-D"), os(&info.branch)]).map(drop)
        }
    }
}

/// Desfaz um worktree criado para um spawn que falhou.
pub(crate) fn discard(created: &Created) {
    let how = if created.new_branch {
        WorktreeCleanup::RemoveWorktreeAndBranch
    } else {
        WorktreeCleanup::RemoveWorktree
    };
    if let Err(err) = cleanup(&created.info, how) {
        warn!(path = %created.info.path.display(), %err, "não foi possível desfazer o worktree");
    }
}

fn remove_worktree(info: &WorktreeInfo) -> Result<()> {
    let removed = git(
        &info.repo,
        [
            os("worktree"),
            os("remove"),
            os("--force"),
            info.path.as_os_str(),
        ],
    );
    match removed {
        Ok(_) => Ok(()),
        // Diretório apagado à mão: só limpa o registro.
        Err(_) if !info.path.exists() => git(&info.repo, ["worktree", "prune"]).map(drop),
        Err(err) => Err(err),
    }
}

fn branch_exists(repo: &Path, branch: &str) -> Result<bool> {
    let refname = format!("refs/heads/{branch}");
    Ok(git(repo, ["rev-parse", "--verify", "--quiet", refname.as_str()]).is_ok())
}

fn rev_parse_commit(repo: &Path, rev: &str) -> Result<String> {
    let spec = format!("{rev}^{{commit}}");
    git(repo, ["rev-parse", "--verify", spec.as_str()])
        .map_err(|err| Error::Git(format!("ref inválida {rev:?}: {err}")))
}

fn os<S: AsRef<OsStr> + ?Sized>(s: &S) -> &OsStr {
    s.as_ref()
}

/// Roda `git` em `dir` e devolve o stdout aparado.
fn git<I, S>(dir: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let output = Command::new("git")
        .current_dir(dir)
        .args(&args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .output()
        .map_err(|err| Error::Git(format!("não foi possível executar git: {err}")))?;
    if !output.status.success() {
        let shown: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        return Err(Error::Git(format!(
            "git {}: {}",
            shown.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// `<nome-do-repo>-<hash8 do caminho>`: repositórios homônimos não colidem.
fn repo_dir_name(repo: &Path) -> String {
    let name = repo
        .file_name()
        .map(|n| sanitize(&n.to_string_lossy()))
        .unwrap_or_else(|| "repo".to_owned());
    format!("{name}-{}", hash8(repo))
}

fn default_branch(prefix: &str, agent_name: &str, id: AgentId) -> String {
    format!("{prefix}{}-{}", sanitize(agent_name), id.short())
}

/// Minúsculas, dígitos e `_`; o resto vira um `-` só. Até 40 caracteres.
fn sanitize(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        "agent".to_owned()
    } else {
        out.to_owned()
    }
}

/// FNV-1a de 32 bits: estável entre versões do Rust (o `DefaultHasher` não é).
fn hash8(path: &Path) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in path.as_os_str().as_encoded_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_names_for_branches() {
        assert_eq!(sanitize("Login UI"), "login-ui");
        assert_eq!(sanitize("  api/v2: rotas!! "), "api-v2-rotas");
        assert_eq!(sanitize("migração_posts"), "migra-o_posts");
        assert_eq!(sanitize("///"), "agent");
        assert_eq!(sanitize(&"x".repeat(100)).len(), 40);
    }

    #[test]
    fn names_follow_the_layout() {
        let id = AgentId::generate();
        assert_eq!(
            default_branch("orquestra/", "Login UI", id),
            format!("orquestra/login-ui-{}", id.short())
        );
        let a = repo_dir_name(Path::new("/src/loja"));
        let b = repo_dir_name(Path::new("/outro/loja"));
        assert!(a.starts_with("loja-") && a.len() == "loja-".len() + 8);
        assert_ne!(a, b);
        assert_eq!(a, repo_dir_name(Path::new("/src/loja")));
        assert_eq!(hash8(Path::new("")), "811c9dc5");
    }

    #[test]
    fn root_dir_prefers_config() {
        let config = RunnerConfig {
            worktree_root: Some(PathBuf::from("/wt")),
            ..RunnerConfig::default()
        };
        assert_eq!(root_dir(&config).unwrap(), PathBuf::from("/wt"));
        let default = root_dir(&RunnerConfig::default()).unwrap();
        assert!(default.ends_with("orquestra/worktrees"), "{default:?}");
    }
}
