//! Sinais para a árvore de processos de um agente, sem `unsafe` (via `nix`).
//!
//! O PTY sobe o processo como líder de sessão (`setsid`), então pgid = sid =
//! pid. `killpg` pega ele e os filhos do mesmo grupo; no Linux também varremos
//! a sessão em `/proc`, porque shells interativos põem cada job num grupo
//! próprio (`npm run dev &` escaparia do `killpg`).

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

pub(crate) use nix::sys::signal::Signal::{SIGKILL, SIGTERM};

/// Sinaliza o grupo do líder (se ele ainda não foi colhido) e todo processo
/// da sessão dele.
pub(crate) fn signal_tree(leader: u32, sig: Signal, leader_alive: bool) {
    let Ok(raw) = i32::try_from(leader) else {
        return;
    };
    // Nunca o init nem "todos os processos".
    if raw <= 1 {
        return;
    }
    if leader_alive {
        let _ = signal::killpg(Pid::from_raw(raw), sig);
    }
    #[cfg(target_os = "linux")]
    {
        let me = i32::try_from(std::process::id()).unwrap_or(0);
        for pid in session_members(raw) {
            if pid != me {
                let _ = signal::kill(Pid::from_raw(pid), sig);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn session_members(sid: i32) -> Vec<i32> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| stat_session(&stat))
                == Some(sid)
        })
        .collect()
}

/// Campo `session` de `/proc/<pid>/stat`: `pid (comm) state ppid pgrp session …`.
/// O `comm` pode ter espaços e parênteses, por isso o corte no último `)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn stat_session(stat: &str) -> Option<i32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(3)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_even_with_odd_comm() {
        let stat = "4242 (my (weird) prog) S 1 4242 4240 34816 4242 4194560 ...";
        assert_eq!(stat_session(stat), Some(4240));
        assert_eq!(stat_session("garbage"), None);
    }

    #[test]
    fn refuses_init_and_zero() {
        // Não pode panicar nem sinalizar nada.
        signal_tree(0, SIGTERM, true);
        signal_tree(1, SIGTERM, true);
    }
}
