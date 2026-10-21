//! Tela de um worker, emulada sem UI (`alacritty_terminal`), para o
//! `read_output` devolver texto puro em vez de bytes com ANSI.
//!
//! Não responde a consultas do programa (`CSI 6n` etc.): quem responde é o
//! terminal da UI; responder aqui duplicaria a resposta.

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Point};
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use orquestra_core::TermSize;

/// Scrollback curto: o gateway só precisa do fim da tela.
pub const SCROLLBACK_LINES: usize = 500;

pub struct Screen {
    term: Term<VoidListener>,
    parser: Processor,
    /// Próximo `seq` de `Output` esperado.
    pub next_seq: u64,
    /// Já pedimos `RequestReplay` e ainda não chegou o `Replay`.
    pub replay_requested: bool,
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

impl Screen {
    pub fn new(size: TermSize) -> Self {
        let size = Size {
            cols: usize::from(size.cols.max(2)),
            rows: usize::from(size.rows.max(2)),
        };
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, VoidListener),
            parser: Processor::new(),
            next_seq: 0,
            replay_requested: false,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// O programa ligou o bracketed paste (`CSI ? 2004 h`).
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Últimas `max` linhas não vazias (scrollback + tela), sem espaços no fim.
    /// Linhas quebradas pelo terminal voltam inteiras.
    pub fn last_lines(&self, max: usize) -> Vec<String> {
        let grid = self.term.grid();
        let start = Point::new(grid.topmost_line(), Column(0));
        let end = Point::new(grid.bottommost_line(), grid.last_column());
        let text = self.term.bounds_to_string(start, end);
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .collect();
        let skip = lines.len().saturating_sub(max);
        lines[skip..].iter().map(|l| (*l).to_owned()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> TermSize {
        TermSize {
            cols: 20,
            rows: 5,
            ..TermSize::default()
        }
    }

    #[test]
    fn strips_ansi_and_keeps_text() {
        let mut s = Screen::new(TermSize::default());
        s.feed(b"\x1b[1;32mok\x1b[0m pronto\r\n\x1b[31merro:\x1b[0m nada\r\n");
        s.feed(b"\x1b]0;titulo\x07\x1b[2K\rlinha final");
        assert_eq!(s.last_lines(10), ["ok pronto", "erro: nada", "linha final"]);
        assert_eq!(s.last_lines(1), ["linha final"]);
    }

    #[test]
    fn redraws_replace_text_and_history_scrolls() {
        let mut s = Screen::new(small());
        for i in 0..30 {
            s.feed(format!("linha {i}\r\n").as_bytes());
        }
        // Spinner reescrevendo a mesma linha.
        s.feed(b"carregando 1\rcarregando 2\r\x1b[Kfeito");
        let lines = s.last_lines(3);
        assert_eq!(lines, ["linha 28", "linha 29", "feito"]);
        assert_eq!(s.last_lines(100).len(), 31);
    }

    #[test]
    fn wrapped_lines_come_back_whole() {
        let mut s = Screen::new(small());
        s.feed(b"0123456789abcdefghijKLMNO\r\n");
        assert_eq!(s.last_lines(5), ["0123456789abcdefghijKLMNO"]);
    }

    #[test]
    fn alt_screen_and_bracketed_paste() {
        let mut s = Screen::new(small());
        s.feed(b"shell\r\n");
        assert!(!s.bracketed_paste());
        s.feed(b"\x1b[?2004h\x1b[?1049h\x1b[2J\x1b[Htui");
        assert!(s.bracketed_paste());
        assert_eq!(s.last_lines(5), ["tui"]);
        s.feed(b"\x1b[?1049l");
        assert_eq!(s.last_lines(5), ["shell"]);
    }
}
