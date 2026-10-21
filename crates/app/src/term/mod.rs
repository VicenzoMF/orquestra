//! Terminal de um agente na UI: um `alacritty_terminal::Term` alimentado pelos
//! `Output` na ordem do `seq`, com as respostas do terminal (CSI 6n, OSC 11...)
//! devolvidas como bytes para o PTY.

pub mod keys;
pub mod palette;
pub mod view;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode, viewport_to_point};
use alacritty_terminal::vte::ansi::{Processor, Rgb};
use orquestra_core::TermSize;

/// Dimensões no formato que o `Term` pede.
#[derive(Clone, Copy, Debug)]
struct Dims {
    cols: usize,
    rows: usize,
}

impl Dims {
    fn of(size: TermSize) -> Self {
        Self {
            cols: usize::from(size.cols.max(2)),
            rows: usize::from(size.rows.max(1)),
        }
    }
}

impl Dimensions for Dims {
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

type ColorFormatter = Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>;
type SizeFormatter = Arc<dyn Fn(WindowSize) -> String + Sync + Send + 'static>;

enum Reply {
    Text(String),
    Color(usize, ColorFormatter),
    Size(SizeFormatter),
}

#[derive(Default)]
struct ListenerState {
    replies: Vec<Reply>,
    title: Option<String>,
    clipboard: Option<String>,
}

/// Recebe os eventos do `Term`. As respostas ficam guardadas até o
/// `AgentTerm` drená-las depois do `advance`.
#[derive(Clone, Default)]
pub struct Listener(Arc<Mutex<ListenerState>>);

impl Listener {
    fn state(&self) -> std::sync::MutexGuard<'_, ListenerState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let mut st = self.state();
        match event {
            Event::PtyWrite(text) => st.replies.push(Reply::Text(text)),
            Event::ColorRequest(index, format) => st.replies.push(Reply::Color(index, format)),
            Event::TextAreaSizeRequest(format) => st.replies.push(Reply::Size(format)),
            Event::Title(title) => st.title = Some(title),
            Event::ResetTitle => st.title = None,
            Event::ClipboardStore(_, text) => st.clipboard = Some(text),
            _ => {}
        }
    }
}

/// Resultado de alimentar um `Output`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Fed {
    /// Respostas do terminal para mandar ao PTY (`Command::SendInput`).
    pub replies: Vec<u8>,
    /// Houve buraco no `seq`: peça `Command::RequestReplay`.
    pub need_replay: bool,
}

pub struct AgentTerm {
    term: Term<Listener>,
    parser: Processor,
    listener: Listener,
    size: TermSize,
    scrollback: usize,
    next_seq: u64,
    awaiting_replay: bool,
    /// Cresce a cada mudança de conteúdo (cache de render e do rabo).
    generation: u64,
    tail_cache: Option<(u64, Vec<String>)>,
}

impl AgentTerm {
    pub fn new(size: TermSize, scrollback: usize) -> Self {
        let listener = Listener::default();
        Self {
            term: Self::make_term(size, scrollback, &listener),
            parser: Processor::new(),
            listener,
            size,
            scrollback,
            next_seq: 0,
            awaiting_replay: false,
            generation: 0,
            tail_cache: None,
        }
    }

    fn make_term(size: TermSize, scrollback: usize, listener: &Listener) -> Term<Listener> {
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        Term::new(config, &Dims::of(size), listener.clone())
    }

    /// Alimenta um `Output`. Fora de ordem: antigo é ignorado; buraco pede
    /// replay e os próximos `Output` são ignorados até o `Replay` chegar.
    pub fn on_output(&mut self, seq: u64, data: &[u8]) -> Fed {
        if self.awaiting_replay || seq < self.next_seq {
            return Fed::default();
        }
        if seq > self.next_seq {
            self.awaiting_replay = true;
            return Fed {
                replies: Vec::new(),
                need_replay: true,
            };
        }
        self.next_seq = seq + 1;
        self.advance(data);
        Fed {
            replies: self.take_replies(),
            need_replay: false,
        }
    }

    /// Aplica um `Replay`: recria o `Term` com o histórico. As respostas que
    /// o histórico provocaria já foram dadas antes, então são descartadas.
    /// Um `Replay` pedido por outro assinante, com o terminal já em dia, é
    /// ignorado para não perder rolagem e seleção.
    pub fn on_replay(&mut self, next_seq: u64, data: &[u8]) {
        if !self.awaiting_replay && next_seq == self.next_seq {
            return;
        }
        self.term = Self::make_term(self.size, self.scrollback, &self.listener);
        self.parser = Processor::new();
        self.advance(data);
        self.take_replies();
        self.next_seq = next_seq;
        self.awaiting_replay = false;
    }

    pub fn awaiting_replay(&self) -> bool {
        self.awaiting_replay
    }

    /// Marca o terminal como defasado (a UI perdeu eventos) e diz se é
    /// preciso pedir replay agora.
    pub fn mark_lagged(&mut self) -> bool {
        !std::mem::replace(&mut self.awaiting_replay, true)
    }

    fn advance(&mut self, data: &[u8]) {
        self.parser.advance(&mut self.term, data);
        self.generation += 1;
    }

    /// Prazo de uma atualização sincronizada (`CSI ? 2026 h`) pendente.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// Encerra uma atualização sincronizada que estourou o prazo.
    pub fn flush_sync(&mut self) -> Vec<u8> {
        if self.parser.sync_bytes_count() == 0 && self.sync_deadline().is_none() {
            return Vec::new();
        }
        self.parser.stop_sync(&mut self.term);
        self.generation += 1;
        self.take_replies()
    }

    fn take_replies(&mut self) -> Vec<u8> {
        let replies = std::mem::take(&mut self.listener.state().replies);
        let mut out = Vec::new();
        for reply in replies {
            let text = match reply {
                Reply::Text(text) => text,
                Reply::Color(index, format) => {
                    format(palette::indexed_rgb(self.term.colors(), index))
                }
                Reply::Size(format) => format(WindowSize {
                    num_lines: self.size.rows,
                    num_cols: self.size.cols,
                    cell_width: self.size.cell_width,
                    cell_height: self.size.cell_height,
                }),
            };
            out.extend(text.into_bytes());
        }
        out
    }

    pub fn size(&self) -> TermSize {
        self.size
    }

    /// Redimensiona o `Term`. `true` se as colunas/linhas mudaram (aí a UI
    /// manda `Command::Resize` para o PTY acompanhar).
    pub fn resize(&mut self, size: TermSize) -> bool {
        let changed = (size.cols, size.rows) != (self.size.cols, self.size.rows);
        self.size = size;
        if changed {
            self.term.resize(Dims::of(size));
            self.generation += 1;
        }
        changed
    }

    pub fn term(&self) -> &Term<Listener> {
        &self.term
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn title(&self) -> Option<String> {
        self.listener.state().title.clone()
    }

    /// Texto que o programa pediu para copiar (OSC 52).
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.listener.state().clipboard.take()
    }

    /// Rola o histórico (`delta` > 0 sobe).
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
        self.generation += 1;
    }

    pub fn scroll_to_bottom(&mut self) {
        if self.term.grid().display_offset() != 0 {
            self.term.scroll_display(Scroll::Bottom);
            self.generation += 1;
        }
    }

    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Ponto do grid na célula (`col`, `row`) da área visível.
    pub fn viewport_point(&self, col: usize, row: usize) -> Point {
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        let p = Point::new(row.min(rows - 1), Column(col.min(cols - 1)));
        viewport_to_point(self.display_offset(), p)
    }

    pub fn start_selection(&mut self, ty: SelectionType, point: Point, side: Side) {
        self.term.selection = Some(Selection::new(ty, point, side));
        self.generation += 1;
    }

    pub fn update_selection(&mut self, point: Point, side: Side) {
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side);
            self.generation += 1;
        }
    }

    pub fn clear_selection(&mut self) {
        if self.term.selection.take().is_some() {
            self.generation += 1;
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    /// Texto de uma linha da tela (0 = topo da tela, sem rolagem).
    pub fn screen_line(&self, line: usize) -> String {
        let grid = self.term.grid();
        let row = &grid[Line(line as i32)];
        let mut text = String::with_capacity(grid.columns());
        for col in 0..grid.columns() {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            text.push(cell.c);
        }
        text.trim_end().to_owned()
    }

    /// Tela inteira em texto puro (testes e depuração).
    pub fn screen_text(&self) -> String {
        (0..self.term.screen_lines())
            .map(|l| self.screen_line(l))
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_owned()
    }

    /// Últimas `n` linhas com conteúdo da tela, em texto puro. Pula linhas só
    /// de moldura (caixas de TUI), que não dizem nada no card.
    pub fn tail(&mut self, n: usize) -> Vec<String> {
        if let Some((generation, lines)) = &self.tail_cache
            && *generation == self.generation
        {
            return last_n(lines, n);
        }
        let mut lines = Vec::new();
        for line in (0..self.term.screen_lines()).rev() {
            let text = self.screen_line(line);
            if is_meaningful(&text) {
                lines.push(text.trim().to_owned());
                if lines.len() == 8 {
                    break;
                }
            }
        }
        lines.reverse();
        let out = last_n(&lines, n);
        self.tail_cache = Some((self.generation, lines));
        out
    }
}

fn last_n(lines: &[String], n: usize) -> Vec<String> {
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// Linha com algo além de espaços e caracteres de moldura.
fn is_meaningful(text: &str) -> bool {
    text.chars().any(|c| {
        !c.is_whitespace() && !matches!(c, '\u{2500}'..='\u{259f}' | '|' | '-' | '_' | '=')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term() -> AgentTerm {
        AgentTerm::new(
            TermSize {
                cols: 40,
                rows: 10,
                cell_width: 8,
                cell_height: 16,
            },
            100,
        )
    }

    #[test]
    fn output_in_order_is_rendered() {
        let mut t = term();
        assert_eq!(t.on_output(0, b"ola\r\n"), Fed::default());
        assert_eq!(t.on_output(1, b"\x1b[31mmundo\x1b[0m"), Fed::default());
        assert_eq!(t.screen_text(), "ola\nmundo");
        assert_eq!(t.next_seq(), 2);
    }

    #[test]
    fn gap_requests_replay_and_ignores_until_it_arrives() {
        let mut t = term();
        t.on_output(0, b"a");
        let fed = t.on_output(2, b"c");
        assert!(fed.need_replay);
        assert!(t.awaiting_replay());
        // Enquanto espera, nada é aplicado nem pedido de novo.
        assert_eq!(t.on_output(3, b"d"), Fed::default());
        assert_eq!(t.screen_text(), "a");

        t.on_replay(4, b"abcd");
        assert!(!t.awaiting_replay());
        assert_eq!(t.screen_text(), "abcd");
        // Antigos são ignorados; o próximo esperado é o 4.
        t.on_output(3, b"X");
        t.on_output(4, b"e");
        assert_eq!(t.screen_text(), "abcde");
    }

    #[test]
    fn duplicate_output_is_ignored() {
        let mut t = term();
        t.on_output(0, b"a");
        t.on_output(0, b"a");
        assert_eq!(t.screen_text(), "a");
    }

    #[test]
    fn replay_in_sync_from_another_subscriber_is_ignored() {
        let mut t = term();
        t.on_output(0, b"abc");
        t.scroll(0);
        t.on_replay(1, b"zzz");
        assert_eq!(t.screen_text(), "abc");
        // Fora de sincronia, aplica.
        t.on_replay(5, b"novo");
        assert_eq!(t.screen_text(), "novo");
        assert_eq!(t.next_seq(), 5);
    }

    #[test]
    fn cursor_position_query_is_answered_but_not_on_replay() {
        let mut t = term();
        let fed = t.on_output(0, b"ab\x1b[6n");
        assert_eq!(fed.replies, b"\x1b[1;3R");
        // OSC 11 (cor de fundo) também responde.
        let fed = t.on_output(1, b"\x1b]11;?\x07");
        assert_eq!(fed.replies, b"\x1b]11;rgb:1212/1414/1818\x07");

        t.mark_lagged();
        t.on_replay(10, b"ab\x1b[6n");
        assert_eq!(t.on_output(10, b"").replies, b"");
    }

    #[test]
    fn resize_changes_grid() {
        let mut t = term();
        let size = TermSize {
            cols: 20,
            rows: 5,
            cell_width: 8,
            cell_height: 16,
        };
        assert!(t.resize(size));
        assert!(!t.resize(size));
        assert_eq!(t.term().columns(), 20);
        assert_eq!(t.term().screen_lines(), 5);
        let fed = t.on_output(0, b"\x1b[18t");
        assert_eq!(fed.replies, b"\x1b[8;5;20t");
    }

    #[test]
    fn tail_skips_frame_lines() {
        let mut t = term();
        t.on_output(
            0,
            b"primeira\r\nsegunda\r\n\xe2\x95\xad\xe2\x94\x80\xe2\x94\x80\xe2\x95\xae\r\n",
        );
        assert_eq!(t.tail(1), ["segunda"]);
        assert_eq!(t.tail(5), ["primeira", "segunda"]);
        t.on_output(1, b"terceira");
        assert_eq!(t.tail(1), ["terceira"]);
    }

    #[test]
    fn selection_and_copy() {
        let mut t = term();
        t.on_output(0, b"hello world");
        let start = t.viewport_point(0, 0);
        let end = t.viewport_point(4, 0);
        t.start_selection(SelectionType::Simple, start, Side::Left);
        t.update_selection(end, Side::Right);
        assert_eq!(t.selection_text().as_deref(), Some("hello"));
        t.clear_selection();
        assert!(t.selection_text().is_none());
    }

    #[test]
    fn scrollback_scrolls() {
        let mut t = term();
        for i in 0..30u64 {
            t.on_output(i, format!("linha {i}\r\n").as_bytes());
        }
        assert_eq!(t.display_offset(), 0);
        t.scroll(5);
        assert_eq!(t.display_offset(), 5);
        t.scroll_to_bottom();
        assert_eq!(t.display_offset(), 0);
    }

    #[test]
    fn synchronized_update_is_flushed_after_deadline() {
        let mut t = term();
        t.on_output(0, b"\x1b[?2026hpendente");
        assert!(t.sync_deadline().is_some());
        assert_eq!(t.screen_text(), "");
        t.flush_sync();
        assert_eq!(t.screen_text(), "pendente");
        assert!(t.sync_deadline().is_none());
    }
}
