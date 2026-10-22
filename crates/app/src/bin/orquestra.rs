//! `orquestra`: o nome completo do `orq`. Executa o `orq` instalado ao lado
//! com os mesmos argumentos.

use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let orq = std::env::current_exe()
        .map(|exe| exe.with_file_name("orq"))
        .unwrap_or_else(|_| "orq".into());
    // `exec` só volta se falhar.
    let err = Command::new(&orq).args(std::env::args_os().skip(1)).exec();
    eprintln!("orquestra: não consegui executar {}: {err}", orq.display());
    ExitCode::FAILURE
}
