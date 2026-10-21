//! Detecção de status a partir da saída do PTY. Pura: recebe bytes e o
//! instante de cada evento, sem relógio próprio (testável com tempo simulado).
//!
//! Camadas, da mais forte para a mais fraca:
//! 1. fim do processo (`exit`): autoritativo e final;
//! 2. OSC `777;orquestra;waiting|running`, escrita pelos hooks injetados;
//! 3. BEL e OSC 9 (notificação de terminal): pedem atenção → aguardando;
//! 4. heurística: silêncio por `idle` + cara de prompt (por tipo) →
//!    aguardando; saída contínua que não é eco de input por `settle` → rodando;
//! 5. debounce: o mesmo estado nunca sai duas vezes, e rajadas curtas (eco,
//!    redesenho) não tiram o agente de "aguardando".

use std::time::{Duration, Instant};

use orquestra_core::conventions::osc;
use orquestra_core::{AgentKind, AgentStatus};

/// Tempos da heurística.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DetectorConfig {
    /// Silêncio que conta como ocioso (`RunnerConfig::idle_ms`).
    pub idle: Duration,
    /// Quanto tempo a saída precisa continuar para voltar a "rodando".
    pub settle: Duration,
    /// Saída até este tempo após um input é tratada como eco.
    pub echo_window: Duration,
}

impl DetectorConfig {
    pub fn from_idle_ms(idle_ms: u64) -> Self {
        let idle = Duration::from_millis(idle_ms.max(1));
        Self {
            idle,
            settle: (idle / 2).clamp(Duration::from_millis(100), Duration::from_secs(1)),
            echo_window: Duration::from_millis(250),
        }
    }
}

/// Como o processo terminou.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessExit {
    Code(i32),
    /// Morto por sinal (descrição, ex.: "Killed").
    Signal(String),
    /// Não foi possível saber (erro no `wait`).
    Unknown(String),
}

impl ProcessExit {
    /// Código 0 → `Done(0)`; morto por `kill` a pedido → `Done(None)`; o
    /// resto (código ≠ 0, sinal inesperado) → `Failed`.
    pub fn status(&self, kill_requested: bool) -> AgentStatus {
        match self {
            Self::Code(0) => AgentStatus::Done { exit_code: Some(0) },
            _ if kill_requested => AgentStatus::Done { exit_code: None },
            Self::Code(code) => AgentStatus::Failed {
                reason: format!("saiu com código {code}"),
            },
            Self::Signal(signal) => AgentStatus::Failed {
                reason: format!("encerrado por sinal ({signal})"),
            },
            Self::Unknown(reason) => AgentStatus::Failed {
                reason: reason.clone(),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signal {
    /// OSC `orquestra;waiting`.
    Waiting,
    /// OSC `orquestra;running`.
    Running,
    /// BEL, OSC 9, OSC `777;notify`.
    Attention,
}

/// Status de um agente vivo, alimentado pela saída, pelo input e por ticks.
pub struct StatusDetector {
    kind: AgentKind,
    /// O agente avisa "rodando" por hook (Claude Code). Depois do primeiro
    /// sinal explícito, só o hook tira o agente de "aguardando".
    reports_running: bool,
    config: DetectorConfig,
    parser: vte::Parser,
    scan: Scanner,
    status: AgentStatus,
    explicit: bool,
    last_output: Instant,
    last_input: Option<Instant>,
    /// Início de uma saída que pode virar "rodando" (heurística).
    activity_since: Option<Instant>,
}

impl StatusDetector {
    /// Começa em `Running` (o que o runner publica ao subir o processo).
    pub fn new(
        kind: AgentKind,
        reports_running: bool,
        config: DetectorConfig,
        now: Instant,
    ) -> Self {
        Self {
            kind,
            reports_running,
            config,
            parser: vte::Parser::new(),
            scan: Scanner::default(),
            status: AgentStatus::Running,
            explicit: false,
            last_output: now,
            last_input: None,
            activity_since: None,
        }
    }

    /// Último status emitido.
    pub fn status(&self) -> &AgentStatus {
        &self.status
    }

    /// Bytes foram enviados ao processo (para reconhecer o eco).
    pub fn input(&mut self, now: Instant) {
        self.last_input = Some(now);
    }

    /// Saída do processo. Devolve o novo status, se mudou.
    pub fn feed(&mut self, data: &[u8], now: Instant) -> Option<AgentStatus> {
        self.parser.advance(&mut self.scan, data);
        let signals = std::mem::take(&mut self.scan.signals);
        if self.status.is_terminal() {
            return None;
        }
        let gap = now.saturating_duration_since(self.last_output);
        self.last_output = now;

        // Sinais valem na hora; numa rajada vale o último.
        let next = signals.iter().fold(None, |_, signal| {
            Some(match signal {
                Signal::Waiting => {
                    self.explicit = true;
                    AgentStatus::WaitingForInput
                }
                Signal::Running => {
                    self.explicit = true;
                    AgentStatus::Running
                }
                Signal::Attention => AgentStatus::WaitingForInput,
            })
        });
        if let Some(next) = next {
            self.activity_since = None;
            return self.commit(next);
        }

        let echo = self
            .last_input
            .is_some_and(|t| now.saturating_duration_since(t) <= self.config.echo_window);
        if echo || self.status != AgentStatus::WaitingForInput || !self.heuristic_running() {
            return None;
        }
        match self.activity_since {
            Some(start) if gap < self.config.settle => {
                if now.saturating_duration_since(start) >= self.config.settle {
                    self.activity_since = None;
                    return self.commit(AgentStatus::Running);
                }
            }
            _ => self.activity_since = Some(now),
        }
        None
    }

    /// Passagem do tempo sem saída. Devolve o novo status, se mudou.
    pub fn tick(&mut self, now: Instant) -> Option<AgentStatus> {
        if self.status.is_terminal() {
            return None;
        }
        let silent = now.saturating_duration_since(self.last_output);
        if let Some(start) = self.activity_since {
            if silent >= self.config.settle {
                // Rajada curta: eco, redesenho, uma linha solta.
                self.activity_since = None;
            } else if now.saturating_duration_since(start) >= self.config.settle {
                self.activity_since = None;
                return self.commit(AgentStatus::Running);
            }
        }
        if self.status == AgentStatus::Running && silent >= self.config.idle && self.looks_idle() {
            return self.commit(AgentStatus::WaitingForInput);
        }
        None
    }

    /// O processo terminou. Final: nada mais muda o status depois disso.
    pub fn exit(&mut self, exit: &ProcessExit, kill_requested: bool) -> Option<AgentStatus> {
        if self.status.is_terminal() {
            return None;
        }
        self.activity_since = None;
        self.status = exit.status(kill_requested);
        Some(self.status.clone())
    }

    /// Sequências que recolocam um terminal novo nos modos atuais do agente
    /// (tela alternativa, teclas de cursor, mouse, bracketed paste...). Vão
    /// antes dos bytes do `Replay`, cujo começo pode ter sido descartado.
    pub fn mode_prefix(&self) -> Vec<u8> {
        self.scan.modes.prefix()
    }

    fn heuristic_running(&self) -> bool {
        !(self.reports_running && self.explicit)
    }

    /// TUIs (Claude Code, Codex) animam enquanto trabalham: silêncio basta.
    /// Shell e Custom precisam de cara de prompt na última linha.
    fn looks_idle(&self) -> bool {
        let line = self.scan.last_line();
        match self.kind {
            AgentKind::ClaudeCode | AgentKind::Codex => true,
            AgentKind::Shell => shell_prompt(line),
            AgentKind::Custom => shell_prompt(line) || question_prompt(line),
        }
    }

    fn commit(&mut self, next: AgentStatus) -> Option<AgentStatus> {
        if self.status.same_state(&next) {
            return None;
        }
        self.status = next.clone();
        Some(next)
    }
}

fn shell_prompt(line: &str) -> bool {
    let line = line.trim();
    line.ends_with(['$', '#', '%', '>', '❯', '»', 'λ']) || line.starts_with('➜')
}

fn question_prompt(line: &str) -> bool {
    let line = line.trim().to_lowercase();
    line.ends_with(['?', ':'])
        || ["[y/n]", "(y/n)", "[yes/no]", "(yes/no)"]
            .iter()
            .any(|p| line.contains(p))
}

/// Linha guardada para a heurística de prompt (em bytes).
const MAX_LINE: usize = 512;

/// `vte::Perform` que só extrai o que interessa ao status e ao replay.
#[derive(Default)]
struct Scanner {
    signals: Vec<Signal>,
    line: String,
    prev_line: String,
    /// Veio um `\r`: o próximo caractere recomeça a linha.
    carriage_return: bool,
    modes: Modes,
}

impl Scanner {
    fn last_line(&self) -> &str {
        if self.line.trim().is_empty() {
            &self.prev_line
        } else {
            &self.line
        }
    }
}

impl vte::Perform for Scanner {
    fn print(&mut self, c: char) {
        if self.carriage_return {
            self.line.clear();
            self.carriage_return = false;
        }
        if self.line.len() >= MAX_LINE {
            let mut cut = self.line.len() / 2;
            while !self.line.is_char_boundary(cut) {
                cut += 1;
            }
            self.line.drain(..cut);
        }
        self.line.push(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => self.signals.push(Signal::Attention),
            b'\n' => {
                if self.line.trim().is_empty() {
                    self.line.clear();
                } else {
                    self.prev_line = std::mem::take(&mut self.line);
                }
                self.carriage_return = false;
            }
            b'\r' => self.carriage_return = true,
            0x08 => {
                self.line.pop();
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if let Some(signal) = osc_signal(params) {
            self.signals.push(signal);
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        if intermediates == b"?" && matches!(action, 'h' | 'l') {
            for param in params.iter() {
                if let Some(&mode) = param.first() {
                    self.modes.set(mode, action == 'h');
                }
            }
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            b'=' => self.modes.keypad = true,
            b'>' => self.modes.keypad = false,
            // RIS: reset completo do terminal.
            b'c' => self.modes = Modes::default(),
            _ => {}
        }
    }
}

fn osc_signal(params: &[&[u8]]) -> Option<Signal> {
    let [kind, rest @ ..] = params else {
        return None;
    };
    if *kind == osc::PARAMS[0] {
        return match rest {
            [app, state, ..] if *app == osc::PARAMS[1] => match *state {
                b"waiting" => Some(Signal::Waiting),
                b"running" => Some(Signal::Running),
                _ => None,
            },
            // Notificação do rxvt/foot/ghostty: `777;notify;título;corpo`.
            [app, ..] if *app == b"notify" => Some(Signal::Attention),
            _ => None,
        };
    }
    if *kind == b"9" {
        // `9;<número>;…` são comandos do ConEmu (progresso etc.), não avisos.
        let conemu = rest
            .first()
            .is_some_and(|p| !p.is_empty() && p.iter().all(u8::is_ascii_digit));
        return (!conemu).then_some(Signal::Attention);
    }
    None
}

/// Modos DEC privados que mudam como a UI codifica teclas/mouse ou qual tela
/// mostra, com o valor padrão. Telas alternativas primeiro: ligar uma limpa a
/// tela, os demais modos vêm depois.
const TRACKED_MODES: [(u16, bool); 13] = [
    (47, false),
    (1047, false),
    (1049, false),
    (1, false),
    (25, true),
    (1000, false),
    (1002, false),
    (1003, false),
    (1004, false),
    (1005, false),
    (1006, false),
    (1015, false),
    (2004, false),
];

#[derive(Clone)]
struct Modes {
    values: [bool; TRACKED_MODES.len()],
    /// DECKPAM (`ESC =`).
    keypad: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            values: TRACKED_MODES.map(|(_, default)| default),
            keypad: false,
        }
    }
}

impl Modes {
    fn set(&mut self, mode: u16, on: bool) {
        if let Some(i) = TRACKED_MODES.iter().position(|(m, _)| *m == mode) {
            self.values[i] = on;
        }
    }

    fn prefix(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for ((mode, default), on) in TRACKED_MODES.iter().zip(self.values) {
            if on != *default {
                let action = if on { 'h' } else { 'l' };
                out.extend_from_slice(format!("\x1b[?{mode}{action}").as_bytes());
            }
        }
        if self.keypad {
            out.extend_from_slice(b"\x1b=");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAITING: AgentStatus = AgentStatus::WaitingForInput;
    const RUNNING: AgentStatus = AgentStatus::Running;

    fn config() -> DetectorConfig {
        DetectorConfig {
            idle: Duration::from_millis(1000),
            settle: Duration::from_millis(300),
            echo_window: Duration::from_millis(100),
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Detector com relógio simulado.
    struct Sim {
        det: StatusDetector,
        t0: Instant,
    }

    impl Sim {
        fn new(kind: AgentKind, reports_running: bool) -> Self {
            let t0 = Instant::now();
            Self {
                det: StatusDetector::new(kind, reports_running, config(), t0),
                t0,
            }
        }
        fn feed(&mut self, at: u64, data: &[u8]) -> Option<AgentStatus> {
            self.det.feed(data, self.t0 + ms(at))
        }
        fn tick(&mut self, at: u64) -> Option<AgentStatus> {
            self.det.tick(self.t0 + ms(at))
        }
        fn input(&mut self, at: u64) {
            self.det.input(self.t0 + ms(at));
        }
    }

    #[test]
    fn osc_waiting_and_running_switch_immediately() {
        let mut s = Sim::new(AgentKind::Custom, false);
        assert_eq!(s.feed(10, osc::WAITING.as_bytes()), Some(WAITING));
        assert_eq!(s.feed(20, osc::WAITING.as_bytes()), None, "sem repetir");
        assert_eq!(s.feed(30, osc::RUNNING.as_bytes()), Some(RUNNING));
    }

    #[test]
    fn osc_split_across_chunks_is_still_seen() {
        let mut s = Sim::new(AgentKind::ClaudeCode, true);
        let (a, b) = osc::WAITING.as_bytes().split_at(7);
        assert_eq!(s.feed(10, a), None);
        assert_eq!(s.feed(11, b), Some(WAITING));
    }

    #[test]
    fn last_signal_in_a_chunk_wins() {
        let mut s = Sim::new(AgentKind::ClaudeCode, true);
        let chunk = format!("texto{}mais{}fim", osc::RUNNING, osc::WAITING);
        assert_eq!(s.feed(10, chunk.as_bytes()), Some(WAITING));
    }

    #[test]
    fn st_terminated_osc_also_counts() {
        let mut s = Sim::new(AgentKind::Custom, false);
        assert_eq!(
            s.feed(10, b"\x1b]777;orquestra;waiting\x1b\\"),
            Some(WAITING)
        );
    }

    #[test]
    fn bel_and_osc9_ask_for_attention_but_conemu_progress_does_not() {
        let mut s = Sim::new(AgentKind::Codex, false);
        assert_eq!(s.feed(10, b"\x1b]9;4;1;50\x07"), None);
        assert_eq!(s.feed(20, b"\x1b]9;Codex quer aprovar\x07"), Some(WAITING));

        let mut s = Sim::new(AgentKind::Shell, false);
        assert_eq!(s.feed(10, b"bip\x07"), Some(WAITING));

        let mut s = Sim::new(AgentKind::Custom, false);
        assert_eq!(s.feed(10, b"\x1b]777;notify;oi;corpo\x07"), Some(WAITING));
        let mut s = Sim::new(AgentKind::Custom, false);
        assert_eq!(s.feed(10, b"\x1b]777;outro;waiting\x07"), None);
        assert_eq!(s.feed(20, b"\x1b]0;titulo\x07"), None);
    }

    #[test]
    fn shell_goes_idle_only_with_a_prompt() {
        let mut s = Sim::new(AgentKind::Shell, false);
        s.feed(0, b"compilando...\r\n");
        assert_eq!(s.tick(2000), None, "sem prompt continua rodando");
        s.feed(2100, b"\x1b[32mvicen@host\x1b[0m:~$ ");
        assert_eq!(s.tick(2500), None, "ainda não ficou ocioso");
        assert_eq!(s.tick(3200), Some(WAITING));
        assert_eq!(s.tick(5000), None);
    }

    #[test]
    fn shell_prompt_after_carriage_return_redraw() {
        let mut s = Sim::new(AgentKind::Shell, false);
        s.feed(0, b"progresso 10%\rprogresso 100%\r\n\r\x1b[K% ");
        assert_eq!(s.tick(1500), Some(WAITING));
    }

    #[test]
    fn custom_accepts_questions_as_prompts() {
        let mut s = Sim::new(AgentKind::Custom, false);
        s.feed(0, b"Continuar? [Y/n] ");
        assert_eq!(s.tick(1500), Some(WAITING));

        let mut s = Sim::new(AgentKind::Custom, false);
        s.feed(0, b"processando lote 3\n");
        assert_eq!(s.tick(1500), None);
    }

    #[test]
    fn tui_agents_go_idle_on_silence_alone() {
        for kind in [AgentKind::ClaudeCode, AgentKind::Codex] {
            let mut s = Sim::new(kind, kind == AgentKind::ClaudeCode);
            s.feed(0, b"\x1b[2J\x1b[H qualquer tela");
            assert_eq!(s.tick(900), None);
            assert_eq!(s.tick(1000), Some(WAITING), "{kind:?}");
        }
    }

    #[test]
    fn echo_of_input_does_not_wake_the_agent() {
        let mut s = Sim::new(AgentKind::Shell, false);
        s.feed(0, b"$ ");
        assert_eq!(s.tick(1000), Some(WAITING));
        for i in 0..20 {
            let at = 1100 + i * 50;
            s.input(at);
            assert_eq!(s.feed(at + 5, b"x"), None);
            assert_eq!(s.tick(at + 40), None);
        }
    }

    #[test]
    fn sustained_output_wakes_the_agent() {
        let mut s = Sim::new(AgentKind::Codex, false);
        assert_eq!(s.feed(0, osc::WAITING.as_bytes()), Some(WAITING));
        s.input(1000);
        s.feed(1010, b"eco");
        assert_eq!(s.feed(1200, b"Working"), None);
        assert_eq!(s.feed(1350, b"Working."), None);
        assert_eq!(s.tick(1450), None);
        assert_eq!(s.feed(1500, b"Working.."), Some(RUNNING));
    }

    #[test]
    fn sustained_output_detected_by_tick_too() {
        let mut s = Sim::new(AgentKind::Shell, false);
        s.feed(0, b"$ ");
        assert_eq!(s.tick(1000), Some(WAITING));
        s.feed(2000, b"linha 1\n");
        s.feed(2200, b"linha 2\n");
        assert_eq!(s.tick(2350), Some(RUNNING));
    }

    #[test]
    fn short_burst_does_not_wake_the_agent() {
        let mut s = Sim::new(AgentKind::Shell, false);
        s.feed(0, b"$ ");
        assert_eq!(s.tick(1000), Some(WAITING));
        assert_eq!(s.feed(2000, b"aviso solto\n$ "), None);
        assert_eq!(s.tick(2400), None, "a rajada parou antes do settle");
        assert_eq!(s.feed(5000, b"outra\n$ "), None, "intervalo longo recomeça");
        assert_eq!(s.tick(5400), None);
    }

    #[test]
    fn hooks_own_running_once_they_spoke() {
        let mut s = Sim::new(AgentKind::ClaudeCode, true);
        assert_eq!(s.feed(0, osc::RUNNING.as_bytes()), None, "já rodando");
        assert_eq!(s.feed(100, osc::WAITING.as_bytes()), Some(WAITING));
        // Redesenho contínuo (ex.: digitação lenta) não volta a rodar...
        for at in (200..3000).step_by(100) {
            assert_eq!(s.feed(at, b"\x1b[2K> texto"), None);
            assert_eq!(s.tick(at + 50), None);
        }
        // ...só o hook.
        assert_eq!(s.feed(3100, osc::RUNNING.as_bytes()), Some(RUNNING));
        // Silêncio ainda conta (Esc interrompe sem disparar o hook Stop).
        assert_eq!(s.tick(4200), Some(WAITING));
    }

    #[test]
    fn claude_without_working_hooks_falls_back_to_heuristics() {
        let mut s = Sim::new(AgentKind::ClaudeCode, true);
        assert_eq!(s.tick(1000), Some(WAITING));
        assert_eq!(s.feed(1100, b"a"), None);
        assert_eq!(s.feed(1300, b"b"), None);
        assert_eq!(s.feed(1450, b"c"), Some(RUNNING));
    }

    #[test]
    fn exit_is_final_and_maps_codes() {
        let mut s = Sim::new(AgentKind::Shell, false);
        assert_eq!(
            s.det.exit(&ProcessExit::Code(0), false),
            Some(AgentStatus::Done { exit_code: Some(0) })
        );
        assert_eq!(s.feed(10, osc::RUNNING.as_bytes()), None);
        assert_eq!(s.tick(5000), None);
        assert_eq!(s.det.exit(&ProcessExit::Code(1), false), None);

        let failed = ProcessExit::Code(3).status(false);
        assert!(matches!(failed, AgentStatus::Failed { reason } if reason.contains('3')));
        let signal = ProcessExit::Signal("Killed".to_owned()).status(false);
        assert!(matches!(signal, AgentStatus::Failed { reason } if reason.contains("Killed")));
        assert_eq!(
            ProcessExit::Signal("Terminated".to_owned()).status(true),
            AgentStatus::Done { exit_code: None }
        );
        assert_eq!(
            ProcessExit::Code(143).status(true),
            AgentStatus::Done { exit_code: None }
        );
        assert_eq!(
            ProcessExit::Code(0).status(true),
            AgentStatus::Done { exit_code: Some(0) }
        );
    }

    #[test]
    fn mode_prefix_restores_current_modes() {
        let mut s = Sim::new(AgentKind::Codex, false);
        assert!(s.det.mode_prefix().is_empty());
        s.feed(
            0,
            b"\x1b[?1049h\x1b[?1;2004h\x1b[?25l\x1b=\x1b[?1000h\x1b[?1000l",
        );
        assert_eq!(
            s.det.mode_prefix(),
            b"\x1b[?1049h\x1b[?1h\x1b[?25l\x1b[?2004h\x1b="
        );
        s.feed(10, b"\x1bc");
        assert!(s.det.mode_prefix().is_empty(), "RIS volta ao padrão");
    }

    #[test]
    fn long_lines_keep_only_the_tail() {
        let mut s = Sim::new(AgentKind::Shell, false);
        let long = "é".repeat(MAX_LINE * 2) + " $ ";
        s.feed(0, long.as_bytes());
        assert!(s.det.scan.line.len() <= MAX_LINE + 4);
        assert_eq!(s.tick(1000), Some(WAITING));
    }
}
