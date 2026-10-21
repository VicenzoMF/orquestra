//! Teclado → bytes para o PTY, conforme o `TermMode` (sequências do xterm).

use alacritty_terminal::term::TermMode;
use egui::{Key, Modifiers};

/// Parâmetro de modificador do xterm: 1 + shift + 2·alt + 4·ctrl.
fn modifier_param(m: Modifiers) -> u8 {
    1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.ctrl)
}

fn has_mods(m: Modifiers) -> bool {
    m.shift || m.alt || m.ctrl
}

/// Setas, Home/End e F1–F4: `ESC [ 1 ; m X` com modificador, senão
/// `ESC O X` (modo de cursor de aplicação) ou `ESC [ X`.
fn cursor_key(letter: u8, m: Modifiers, mode: TermMode) -> Vec<u8> {
    if has_mods(m) {
        format!("\x1b[1;{}{}", modifier_param(m), letter as char).into_bytes()
    } else if mode.contains(TermMode::APP_CURSOR) {
        vec![0x1b, b'O', letter]
    } else {
        vec![0x1b, b'[', letter]
    }
}

/// F1–F4 sem modificador sempre usam SS3 (`ESC O P`...).
fn ss3_key(letter: u8, m: Modifiers) -> Vec<u8> {
    if has_mods(m) {
        format!("\x1b[1;{}{}", modifier_param(m), letter as char).into_bytes()
    } else {
        vec![0x1b, b'O', letter]
    }
}

/// `ESC [ n ~` ou `ESC [ n ; m ~`.
fn tilde_key(n: u8, m: Modifiers) -> Vec<u8> {
    if has_mods(m) {
        format!("\x1b[{n};{}~", modifier_param(m)).into_bytes()
    } else {
        format!("\x1b[{n}~").into_bytes()
    }
}

fn letter(key: Key) -> Option<u8> {
    use Key::*;
    let c = match key {
        A => b'a',
        B => b'b',
        C => b'c',
        D => b'd',
        E => b'e',
        F => b'f',
        G => b'g',
        H => b'h',
        I => b'i',
        J => b'j',
        K => b'k',
        L => b'l',
        M => b'm',
        N => b'n',
        O => b'o',
        P => b'p',
        Q => b'q',
        R => b'r',
        S => b's',
        T => b't',
        U => b'u',
        V => b'v',
        W => b'w',
        X => b'x',
        Y => b'y',
        Z => b'z',
        _ => return None,
    };
    Some(c)
}

fn digit(key: Key) -> Option<u8> {
    use Key::*;
    let c = match key {
        Num0 => b'0',
        Num1 => b'1',
        Num2 => b'2',
        Num3 => b'3',
        Num4 => b'4',
        Num5 => b'5',
        Num6 => b'6',
        Num7 => b'7',
        Num8 => b'8',
        Num9 => b'9',
        _ => return None,
    };
    Some(c)
}

/// Byte de controle de Ctrl+tecla (Ctrl+A = 0x01 ... Ctrl+[ = ESC).
fn ctrl_byte(key: Key) -> Option<u8> {
    if let Some(c) = letter(key) {
        return Some(c - b'a' + 1);
    }
    let b = match key {
        Key::Space | Key::Num2 => 0x00,
        Key::OpenBracket => 0x1b,
        Key::Backslash => 0x1c,
        Key::CloseBracket => 0x1d,
        Key::Num6 => 0x1e,
        Key::Minus | Key::Slash => 0x1f,
        _ => return None,
    };
    Some(b)
}

/// Codifica uma tecla pressionada. `None` = tecla imprimível sem Ctrl/Alt:
/// o texto chega por `egui::Event::Text`.
pub fn encode_key(key: Key, m: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    let bytes = match key {
        Key::Enter => {
            let base: &[u8] = if mode.contains(TermMode::LINE_FEED_NEW_LINE) {
                b"\r\n"
            } else {
                b"\r"
            };
            if m.alt {
                [b"\x1b".as_slice(), base].concat()
            } else {
                base.to_vec()
            }
        }
        Key::Backspace => {
            if m.ctrl {
                vec![0x08]
            } else if m.alt {
                vec![0x1b, 0x7f]
            } else {
                vec![0x7f]
            }
        }
        Key::Tab => {
            if m.shift {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        Key::Escape => vec![0x1b],
        Key::ArrowUp => cursor_key(b'A', m, mode),
        Key::ArrowDown => cursor_key(b'B', m, mode),
        Key::ArrowRight => cursor_key(b'C', m, mode),
        Key::ArrowLeft => cursor_key(b'D', m, mode),
        Key::Home => cursor_key(b'H', m, mode),
        Key::End => cursor_key(b'F', m, mode),
        Key::Insert => tilde_key(2, m),
        Key::Delete => tilde_key(3, m),
        Key::PageUp => tilde_key(5, m),
        Key::PageDown => tilde_key(6, m),
        Key::F1 => ss3_key(b'P', m),
        Key::F2 => ss3_key(b'Q', m),
        Key::F3 => ss3_key(b'R', m),
        Key::F4 => ss3_key(b'S', m),
        Key::F5 => tilde_key(15, m),
        Key::F6 => tilde_key(17, m),
        Key::F7 => tilde_key(18, m),
        Key::F8 => tilde_key(19, m),
        Key::F9 => tilde_key(20, m),
        Key::F10 => tilde_key(21, m),
        Key::F11 => tilde_key(23, m),
        Key::F12 => tilde_key(24, m),
        _ if m.ctrl => {
            let b = ctrl_byte(key)?;
            if m.alt { vec![0x1b, b] } else { vec![b] }
        }
        _ if m.alt => {
            let c = letter(key).or_else(|| digit(key))?;
            let c = if m.shift { c.to_ascii_uppercase() } else { c };
            vec![0x1b, c]
        }
        _ => return None,
    };
    Some(bytes)
}

/// Texto colado: com bracketed paste vai entre `ESC[200~` e `ESC[201~` (sem
/// ESC no meio); sem ele, quebras de linha viram `\r` (o que o Enter manda).
pub fn encode_paste(text: &str, mode: TermMode) -> Vec<u8> {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        let mut out = b"\x1b[200~".to_vec();
        out.extend(text.replace('\x1b', "").bytes());
        out.extend(b"\x1b[201~");
        out
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Botões do relatório de mouse do xterm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left = 0,
    Middle = 1,
    Right = 2,
    LeftDrag = 32,
    WheelUp = 64,
    WheelDown = 65,
}

/// Relatório de mouse (SGR quando o programa pediu, senão o formato X10).
/// `None` se o programa não pediu mouse ou a posição não cabe no X10.
pub fn mouse_report(
    button: MouseButton,
    col: usize,
    line: usize,
    pressed: bool,
    m: Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    let mods = 4 * u8::from(m.shift) + 8 * u8::from(m.alt) + 16 * u8::from(m.ctrl);
    let code = button as u8 + mods;
    if mode.contains(TermMode::SGR_MOUSE) {
        let end = if pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{code};{};{}{end}", col + 1, line + 1).into_bytes());
    }
    if col >= 223 || line >= 223 {
        return None;
    }
    let code = if pressed || button as u8 >= 64 {
        code
    } else {
        3 + mods
    };
    Some(vec![
        0x1b,
        b'[',
        b'M',
        32 + code,
        32 + 1 + col as u8,
        32 + 1 + line as u8,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;

    fn ctrl() -> Modifiers {
        Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::NONE
        }
    }

    fn alt() -> Modifiers {
        Modifiers {
            alt: true,
            ..Modifiers::NONE
        }
    }

    fn shift() -> Modifiers {
        Modifiers {
            shift: true,
            ..Modifiers::NONE
        }
    }

    fn enc(key: Key, m: Modifiers, mode: TermMode) -> Vec<u8> {
        encode_key(key, m, mode).unwrap()
    }

    #[test]
    fn basic_keys() {
        let mode = TermMode::default();
        assert_eq!(enc(Key::Enter, NONE, mode), b"\r");
        assert_eq!(enc(Key::Backspace, NONE, mode), b"\x7f");
        assert_eq!(enc(Key::Tab, NONE, mode), b"\t");
        assert_eq!(enc(Key::Tab, shift(), mode), b"\x1b[Z");
        assert_eq!(enc(Key::Escape, NONE, mode), b"\x1b");
        assert_eq!(enc(Key::Delete, NONE, mode), b"\x1b[3~");
        assert_eq!(enc(Key::PageUp, NONE, mode), b"\x1b[5~");
        assert!(encode_key(Key::A, NONE, mode).is_none());
        assert!(encode_key(Key::A, shift(), mode).is_none());
    }

    #[test]
    fn arrows_follow_app_cursor_mode() {
        let normal = TermMode::default();
        let app = normal | TermMode::APP_CURSOR;
        assert_eq!(enc(Key::ArrowUp, NONE, normal), b"\x1b[A");
        assert_eq!(enc(Key::ArrowUp, NONE, app), b"\x1bOA");
        assert_eq!(enc(Key::ArrowLeft, NONE, app), b"\x1bOD");
        assert_eq!(enc(Key::Home, NONE, normal), b"\x1b[H");
        assert_eq!(enc(Key::End, NONE, app), b"\x1bOF");
        // Com modificador o formato CSI vale nos dois modos.
        assert_eq!(enc(Key::ArrowRight, ctrl(), app), b"\x1b[1;5C");
        assert_eq!(enc(Key::ArrowDown, shift(), normal), b"\x1b[1;2B");
        assert_eq!(enc(Key::ArrowUp, alt(), normal), b"\x1b[1;3A");
    }

    #[test]
    fn function_keys() {
        let mode = TermMode::default();
        assert_eq!(enc(Key::F1, NONE, mode), b"\x1bOP");
        assert_eq!(enc(Key::F4, NONE, mode), b"\x1bOS");
        assert_eq!(enc(Key::F5, NONE, mode), b"\x1b[15~");
        assert_eq!(enc(Key::F12, NONE, mode), b"\x1b[24~");
        assert_eq!(enc(Key::F2, ctrl(), mode), b"\x1b[1;5Q");
        assert_eq!(enc(Key::F9, shift(), mode), b"\x1b[20;2~");
    }

    #[test]
    fn ctrl_and_alt_combinations() {
        let mode = TermMode::default();
        assert_eq!(enc(Key::C, ctrl(), mode), [0x03]);
        assert_eq!(enc(Key::A, ctrl(), mode), [0x01]);
        assert_eq!(enc(Key::Z, ctrl(), mode), [0x1a]);
        assert_eq!(enc(Key::OpenBracket, ctrl(), mode), [0x1b]);
        assert_eq!(enc(Key::Space, ctrl(), mode), [0x00]);
        assert_eq!(enc(Key::Backspace, ctrl(), mode), [0x08]);
        assert_eq!(enc(Key::B, alt(), mode), b"\x1bb");
        let alt_shift = Modifiers {
            shift: true,
            ..alt()
        };
        assert_eq!(enc(Key::B, alt_shift, mode), b"\x1bB");
        let ctrl_alt = Modifiers {
            alt: true,
            ..ctrl()
        };
        assert_eq!(enc(Key::D, ctrl_alt, mode), [0x1b, 0x04]);
        assert_eq!(enc(Key::Enter, alt(), mode), b"\x1b\r");
        assert_eq!(enc(Key::Backspace, alt(), mode), b"\x1b\x7f");
    }

    #[test]
    fn enter_respects_line_feed_new_line() {
        let mode = TermMode::default() | TermMode::LINE_FEED_NEW_LINE;
        assert_eq!(enc(Key::Enter, NONE, mode), b"\r\n");
    }

    #[test]
    fn paste_brackets_and_newlines() {
        let plain = TermMode::default();
        let bracketed = plain | TermMode::BRACKETED_PASTE;
        assert_eq!(encode_paste("a\nb\r\nc", plain), b"a\rb\rc");
        assert_eq!(
            encode_paste("x\x1b[31my\n", bracketed),
            b"\x1b[200~x[31my\n\x1b[201~"
        );
    }

    #[test]
    fn mouse_reports() {
        let off = TermMode::default();
        assert!(mouse_report(MouseButton::Left, 0, 0, true, NONE, off).is_none());
        let sgr = off | TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            mouse_report(MouseButton::Left, 4, 2, true, NONE, sgr).unwrap(),
            b"\x1b[<0;5;3M"
        );
        assert_eq!(
            mouse_report(MouseButton::Left, 4, 2, false, NONE, sgr).unwrap(),
            b"\x1b[<0;5;3m"
        );
        assert_eq!(
            mouse_report(MouseButton::WheelUp, 0, 0, true, NONE, sgr).unwrap(),
            b"\x1b[<64;1;1M"
        );
        let x10 = off | TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            mouse_report(MouseButton::Left, 0, 0, true, NONE, x10).unwrap(),
            [0x1b, b'[', b'M', 32, 33, 33]
        );
        assert_eq!(
            mouse_report(MouseButton::Left, 0, 0, false, NONE, x10).unwrap(),
            [0x1b, b'[', b'M', 35, 33, 33]
        );
        assert!(mouse_report(MouseButton::Left, 300, 0, true, NONE, x10).is_none());
    }
}
