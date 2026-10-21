//! Anel com o fim da saída crua de um agente, para `Replay`.

use std::collections::VecDeque;

pub(crate) struct ReplayRing {
    buf: VecDeque<u8>,
    cap: usize,
    /// Já descartou o começo: o primeiro byte pode estar no meio de um UTF-8.
    truncated: bool,
}

impl ReplayRing {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap,
            truncated: false,
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) {
        if self.cap == 0 || data.is_empty() {
            return;
        }
        if data.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.cap..]);
            self.truncated = true;
            return;
        }
        // Descarta antes de crescer, para a capacidade nunca passar de `cap`.
        let excess = (self.buf.len() + data.len()).saturating_sub(self.cap);
        if excess > 0 {
            self.buf.drain(..excess);
            self.truncated = true;
        }
        self.buf.extend(data);
    }

    /// Cópia do conteúdo, sem bytes de continuação UTF-8 órfãos no começo.
    pub(crate) fn snapshot(&self) -> Vec<u8> {
        let (a, b) = self.buf.as_slices();
        let mut out = Vec::with_capacity(self.buf.len());
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        if self.truncated {
            let orphans = out
                .iter()
                .take(3)
                .take_while(|byte| (0x80..0xc0).contains(*byte))
                .count();
            out.drain(..orphans);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_tail() {
        let mut ring = ReplayRing::new(8);
        ring.push(b"abcde");
        assert_eq!(ring.snapshot(), b"abcde");
        ring.push(b"fghij");
        assert_eq!(ring.snapshot(), b"cdefghij");
        ring.push(b"0123456789");
        assert_eq!(ring.snapshot(), b"23456789");
    }

    #[test]
    fn never_starts_mid_utf8() {
        let mut ring = ReplayRing::new(5);
        ring.push("aé".as_bytes());
        ring.push("çõ".as_bytes());
        // "aéçõ" = 61 c3 a9 c3 a7 c3 b5: sobram a9 c3 a7 c3 b5 → corta o a9.
        assert_eq!(ring.snapshot(), "çõ".as_bytes());
    }

    #[test]
    fn zero_capacity_keeps_nothing() {
        let mut ring = ReplayRing::new(0);
        ring.push(b"abc");
        assert!(ring.snapshot().is_empty());
    }
}
