//! Onde cada sessão fica no canvas, guardado entre aberturas do app (as
//! sessões voltam ao abrir; a arrumação delas também). Um JSON pequeno ao lado
//! do banco: `{"<id da sessão>": [x, y]}`. É estado da UI, não do hub.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use egui::{Pos2, pos2};
use orquestra_core::SessionId;
use tracing::warn;

/// O arquivo e o último conteúdo gravado (só regrava quando muda).
pub struct Places {
    path: PathBuf,
    saved: HashMap<SessionId, Pos2>,
}

impl Places {
    /// Lê o que houver; arquivo ausente ou ilegível é começar sem posições.
    pub fn open(path: PathBuf) -> Self {
        let saved = match std::fs::read(&path) {
            Ok(bytes) => decode(&bytes).unwrap_or_else(|err| {
                warn!(path = %path.display(), %err, "posições do canvas ilegíveis; ignorando");
                HashMap::new()
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(err) => {
                warn!(path = %path.display(), %err, "não deu para ler as posições do canvas");
                HashMap::new()
            }
        };
        Self { path, saved }
    }

    pub fn positions(&self) -> &HashMap<SessionId, Pos2> {
        &self.saved
    }

    /// Grava se mudou desde a última vez.
    pub fn save(&mut self, positions: &HashMap<SessionId, Pos2>) {
        if *positions == self.saved {
            return;
        }
        match write(&self.path, positions) {
            Ok(()) => self.saved = positions.clone(),
            Err(err) => {
                warn!(path = %self.path.display(), %err, "não deu para gravar as posições do canvas");
                // Não insiste a cada frame: tenta de novo na próxima mudança.
                self.saved = positions.clone();
            }
        }
    }
}

fn decode(bytes: &[u8]) -> Result<HashMap<SessionId, Pos2>, serde_json::Error> {
    let raw: HashMap<SessionId, [f32; 2]> = serde_json::from_slice(bytes)?;
    Ok(raw
        .into_iter()
        .filter(|(_, [x, y])| x.is_finite() && y.is_finite())
        .map(|(id, [x, y])| (id, pos2(x, y)))
        .collect())
}

/// Arquivo temporário ao lado e `rename`: nunca fica pela metade.
fn write(path: &Path, positions: &HashMap<SessionId, Pos2>) -> io::Result<()> {
    let raw: HashMap<&SessionId, [f32; 2]> =
        positions.iter().map(|(id, p)| (id, [p.x, p.y])).collect();
    let json = serde_json::to_vec(&raw).map_err(io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_survive_a_restart_and_garbage_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("canvas.json");
        let mut places = Places::open(path.clone());
        assert!(places.positions().is_empty(), "sem arquivo, sem posições");

        let (a, b) = (SessionId::generate(), SessionId::generate());
        let positions = HashMap::from([(a, pos2(10.0, -20.5)), (b, pos2(900.0, 0.0))]);
        places.save(&positions);
        assert_eq!(Places::open(path.clone()).positions(), &positions);

        std::fs::write(&path, b"{nada").unwrap();
        assert!(Places::open(path).positions().is_empty());
    }
}
