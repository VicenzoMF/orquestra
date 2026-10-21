//! Buffer em memória dos `Output`, gravado em lote pelo `SqliteStore`.

use std::collections::HashMap;

use orquestra_core::{AgentId, Timestamp};

/// Saída acumulada de um agente desde a última gravação: vira uma linha de
/// `output_chunks`.
#[derive(Debug)]
pub(crate) struct Chunk {
    pub agent: AgentId,
    /// `seq` do primeiro `Output` (aproximado se o começo foi descartado).
    pub first_seq: u64,
    /// Hora do primeiro `Output` do chunk.
    pub at: Timestamp,
    pub data: Vec<u8>,
}

/// Um chunk por agente; `bytes` soma todos.
#[derive(Debug, Default)]
pub(crate) struct OutputBuffer {
    chunks: HashMap<AgentId, Chunk>,
    bytes: usize,
}

impl OutputBuffer {
    /// Acrescenta `data` ao chunk do agente e devolve o total em buffer.
    ///
    /// Um chunk nunca passa de `cap` bytes: o excesso sai do começo, que o
    /// teto de retenção apagaria de qualquer jeito.
    pub fn push(
        &mut self,
        agent: AgentId,
        seq: u64,
        data: &[u8],
        at: Timestamp,
        cap: usize,
    ) -> usize {
        let chunk = self.chunks.entry(agent).or_insert_with(|| Chunk {
            agent,
            first_seq: seq,
            at,
            data: Vec::new(),
        });
        chunk.data.extend_from_slice(data);
        self.bytes += data.len();
        if chunk.data.len() > cap {
            let excess = chunk.data.len() - cap;
            chunk.data.drain(..excess);
            chunk.first_seq = seq;
            self.bytes -= excess;
        }
        self.bytes
    }

    pub fn is_empty(&self) -> bool {
        self.bytes == 0
    }

    /// Esvazia o buffer, devolvendo os chunks não vazios.
    pub fn take(&mut self) -> Vec<Chunk> {
        self.bytes = 0;
        self.chunks
            .drain()
            .map(|(_, chunk)| chunk)
            .filter(|chunk| !chunk.data.is_empty())
            .collect()
    }

    /// Os últimos `max` bytes ainda não gravados do agente.
    pub fn tail(&self, agent: AgentId, max: usize) -> &[u8] {
        self.chunks.get(&agent).map_or(&[], |chunk| {
            let start = chunk.data.len().saturating_sub(max);
            &chunk.data[start..]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_per_agent_and_takes_everything() {
        let (a, b) = (AgentId::generate(), AgentId::generate());
        let mut buf = OutputBuffer::default();
        assert!(buf.is_empty());
        assert_eq!(buf.push(a, 0, b"ola ", Timestamp(1), 100), 4);
        assert_eq!(buf.push(b, 0, b"x", Timestamp(2), 100), 5);
        assert_eq!(buf.push(a, 1, b"mundo", Timestamp(3), 100), 10);
        assert_eq!(buf.tail(a, 100), b"ola mundo");
        assert_eq!(buf.tail(a, 5), b"mundo");
        assert_eq!(buf.tail(AgentId::generate(), 5), b"");

        let mut chunks = buf.take();
        chunks.sort_by_key(|c| c.at);
        assert!(buf.is_empty());
        assert_eq!(chunks.len(), 2);
        assert_eq!((chunks[0].agent, chunks[0].first_seq), (a, 0));
        assert_eq!(chunks[0].at, Timestamp(1));
        assert_eq!(chunks[0].data, b"ola mundo");
        assert_eq!(chunks[1].data, b"x");
        assert!(buf.take().is_empty());
    }

    #[test]
    fn chunk_never_exceeds_cap() {
        let a = AgentId::generate();
        let mut buf = OutputBuffer::default();
        buf.push(a, 0, b"0123456789", Timestamp(1), 8);
        assert_eq!(buf.tail(a, 100), b"23456789");
        assert_eq!(buf.push(a, 1, b"ab", Timestamp(2), 8), 8);
        assert_eq!(buf.tail(a, 100), b"456789ab");
        let chunks = buf.take();
        assert_eq!(chunks[0].first_seq, 1);
    }
}
