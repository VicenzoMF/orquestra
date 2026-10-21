//! Tradução de `BrowserInput` para `Input.dispatch*Event` do CDP.

use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams,
    DispatchMouseEventPointerType, DispatchMouseEventType, MouseButton as CdpButton,
};
use orquestra_core::{
    KeyAction, KeyInput, Modifiers, MouseAction, MouseButton, MouseInput, WheelInput,
};

/// Máscara de modificadores do CDP: Alt=1, Ctrl=2, Meta=4, Shift=8.
pub(crate) fn modifiers_mask(m: Modifiers) -> i64 {
    let mut mask = 0;
    if m.alt {
        mask |= 1;
    }
    if m.ctrl {
        mask |= 2;
    }
    if m.meta {
        mask |= 4;
    }
    if m.shift {
        mask |= 8;
    }
    mask
}

/// Bit do botão no campo `buttons`: Left=1, Right=2, Middle=4, Back=8, Forward=16.
fn button_bit(b: MouseButton) -> i64 {
    match b {
        MouseButton::None => 0,
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 4,
        MouseButton::Back => 8,
        MouseButton::Forward => 16,
    }
}

fn cdp_button(b: MouseButton) -> CdpButton {
    match b {
        MouseButton::None => CdpButton::None,
        MouseButton::Left => CdpButton::Left,
        MouseButton::Middle => CdpButton::Middle,
        MouseButton::Right => CdpButton::Right,
        MouseButton::Back => CdpButton::Back,
        MouseButton::Forward => CdpButton::Forward,
    }
}

/// Primeiro botão pressionado na máscara (para arrastar com `mouseMoved`).
fn pressed_button(buttons: i64) -> MouseButton {
    [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::Back,
        MouseButton::Forward,
    ]
    .into_iter()
    .find(|b| buttons & button_bit(*b) != 0)
    .unwrap_or(MouseButton::None)
}

/// Botões pressionados, para o campo `buttons` (arrastar precisa dele).
#[derive(Debug, Default)]
pub(crate) struct MouseState {
    buttons: i64,
}

impl MouseState {
    pub fn mouse(&mut self, m: &MouseInput) -> DispatchMouseEventParams {
        let bit = button_bit(m.button);
        let (kind, click_count) = match m.action {
            MouseAction::Move => (DispatchMouseEventType::MouseMoved, None),
            MouseAction::Down => {
                self.buttons |= bit;
                (
                    DispatchMouseEventType::MousePressed,
                    Some(i64::from(m.click_count.max(1))),
                )
            }
            MouseAction::Up => {
                self.buttons &= !bit;
                (
                    DispatchMouseEventType::MouseReleased,
                    Some(i64::from(m.click_count.max(1))),
                )
            }
        };
        let button = match (m.action, m.button) {
            (MouseAction::Move, MouseButton::None) => pressed_button(self.buttons),
            (_, b) => b,
        };
        let mut p = DispatchMouseEventParams::new(kind, m.x, m.y);
        p.button = Some(cdp_button(button));
        p.buttons = Some(self.buttons);
        p.click_count = click_count;
        p.modifiers = Some(modifiers_mask(m.modifiers));
        p.pointer_type = Some(DispatchMouseEventPointerType::Mouse);
        p
    }

    pub fn wheel(&self, w: &WheelInput) -> DispatchMouseEventParams {
        let mut p = DispatchMouseEventParams::new(DispatchMouseEventType::MouseWheel, w.x, w.y);
        p.delta_x = Some(w.delta_x);
        p.delta_y = Some(w.delta_y);
        p.buttons = Some(self.buttons);
        p.modifiers = Some(modifiers_mask(w.modifiers));
        p.pointer_type = Some(DispatchMouseEventPointerType::Mouse);
        p
    }
}

/// `keyDown` com `text` gera keydown + keypress + input (como o Puppeteer);
/// sem texto vira `rawKeyDown`. Não mandamos `char` à parte: duplicaria o texto.
pub(crate) fn key_event(k: &KeyInput) -> DispatchKeyEventParams {
    let text = key_text(k);
    let kind = match (k.action, &text) {
        (KeyAction::Down, Some(_)) => DispatchKeyEventType::KeyDown,
        (KeyAction::Down, None) => DispatchKeyEventType::RawKeyDown,
        (KeyAction::Up, _) => DispatchKeyEventType::KeyUp,
    };
    let code = key_code(&k.key, &k.code);
    let mut p = DispatchKeyEventParams::new(kind);
    p.modifiers = Some(modifiers_mask(k.modifiers));
    p.key = Some(k.key.clone());
    p.code = (!k.code.is_empty()).then(|| k.code.clone());
    p.windows_virtual_key_code = Some(code);
    p.native_virtual_key_code = Some(code);
    p.unmodified_text = text.clone();
    p.text = text;
    p.location = key_location(&k.code);
    p.is_keypad = Some(k.code.starts_with("Numpad"));
    p
}

/// Texto que a tecla insere, só no `Down`. Enter manda `\r` (o que dispara
/// keypress e o envio do formulário); teclas de controle não mandam texto; com
/// Ctrl/Meta (sem Alt, que pode ser AltGr) é atalho, não digitação.
fn key_text(k: &KeyInput) -> Option<String> {
    if k.action != KeyAction::Down {
        return None;
    }
    let m = k.modifiers;
    if (m.ctrl || m.meta) && !m.alt {
        return None;
    }
    match k.key.as_str() {
        "Enter" => Some("\r".to_owned()),
        key if named_key_code(key).is_some() && key != " " => None,
        _ => k.text.clone().filter(|t| !t.is_empty()),
    }
}

/// 1 = esquerda, 2 = direita (modificadores), 3 = teclado numérico.
fn key_location(code: &str) -> Option<i64> {
    const SIDED: [&str; 4] = ["Shift", "Control", "Alt", "Meta"];
    if code.starts_with("Numpad") {
        return Some(3);
    }
    SIDED.iter().find_map(|m| match code.strip_prefix(m) {
        Some("Left") => Some(1),
        Some("Right") => Some(2),
        _ => None,
    })
}

/// `windowsVirtualKeyCode` a partir do DOM: tecla nomeada pelo `key`; no
/// teclado numérico, o `code`; depois o caractere do `key` (letras e dígitos
/// seguem o layout); por fim o `code` (tecla física). 0 se desconhecida.
pub(crate) fn key_code(key: &str, code: &str) -> i64 {
    let numpad = code.starts_with("Numpad");
    named_key_code(key)
        .or_else(|| numpad.then(|| physical_key_code(code)).flatten())
        .or_else(|| char_key_code(key))
        .or_else(|| physical_key_code(code))
        .unwrap_or(0)
}

fn named_key_code(key: &str) -> Option<i64> {
    let code = match key {
        "Backspace" => 8,
        "Tab" => 9,
        "Enter" => 13,
        "Shift" => 16,
        "Control" => 17,
        "Alt" | "AltGraph" => 18,
        "Pause" => 19,
        "CapsLock" => 20,
        "Escape" | "Esc" => 27,
        " " | "Spacebar" => 32,
        "PageUp" => 33,
        "PageDown" => 34,
        "End" => 35,
        "Home" => 36,
        "ArrowLeft" | "Left" => 37,
        "ArrowUp" | "Up" => 38,
        "ArrowRight" | "Right" => 39,
        "ArrowDown" | "Down" => 40,
        "PrintScreen" => 44,
        "Insert" => 45,
        "Delete" | "Del" => 46,
        "Meta" | "OS" => 91,
        "ContextMenu" => 93,
        "NumLock" => 144,
        "ScrollLock" => 145,
        _ => return function_key_code(key),
    };
    Some(code)
}

/// F1–F24 → 112–135.
fn function_key_code(key: &str) -> Option<i64> {
    let n: i64 = key.strip_prefix('F')?.parse().ok()?;
    (1..=24).contains(&n).then_some(111 + n)
}

/// Um caractere: letras, dígitos e a pontuação do layout US (com e sem Shift).
fn char_key_code(key: &str) -> Option<i64> {
    let mut chars = key.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let code = match c {
        'a'..='z' | 'A'..='Z' => c.to_ascii_uppercase() as i64,
        '0'..='9' => c as i64,
        ')' => 48,
        '!' => 49,
        '@' => 50,
        '#' => 51,
        '$' => 52,
        '%' => 53,
        '^' => 54,
        '&' => 55,
        '*' => 56,
        '(' => 57,
        ';' | ':' => 186,
        '=' | '+' => 187,
        ',' | '<' => 188,
        '-' | '_' => 189,
        '.' | '>' => 190,
        '/' | '?' => 191,
        '`' | '~' => 192,
        '[' | '{' => 219,
        '\\' | '|' => 220,
        ']' | '}' => 221,
        '\'' | '"' => 222,
        _ => return None,
    };
    Some(code)
}

/// Pelo `code` (tecla física), para layouts em que o `key` não ajuda.
fn physical_key_code(code: &str) -> Option<i64> {
    if let Some(letter) = code.strip_prefix("Key") {
        return char_key_code(letter);
    }
    if let Some(digit) = code.strip_prefix("Digit") {
        return char_key_code(digit);
    }
    if let Some(rest) = code.strip_prefix("Numpad") {
        return match rest {
            "Multiply" => Some(106),
            "Add" => Some(107),
            "Subtract" => Some(109),
            "Decimal" => Some(110),
            "Divide" => Some(111),
            "Enter" => Some(13),
            d => {
                let d: i64 = d.parse().ok()?;
                (0..=9).contains(&d).then_some(96 + d)
            }
        };
    }
    let code = match code {
        "Space" => 32,
        "Semicolon" => 186,
        "Equal" => 187,
        "Comma" => 188,
        "Minus" => 189,
        "Period" => 190,
        "Slash" => 191,
        "Backquote" => 192,
        "BracketLeft" => 219,
        "Backslash" => 220,
        "BracketRight" => 221,
        "Quote" => 222,
        "IntlBackslash" => 226,
        "ShiftLeft" | "ShiftRight" => 16,
        "ControlLeft" | "ControlRight" => 17,
        "AltLeft" | "AltRight" => 18,
        "MetaLeft" => 91,
        "MetaRight" => 92,
        other => return named_key_code(other),
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(action: KeyAction, key: &str, code: &str, text: Option<&str>) -> KeyInput {
        KeyInput {
            action,
            key: key.to_owned(),
            code: code.to_owned(),
            text: text.map(str::to_owned),
            modifiers: Modifiers::default(),
        }
    }

    #[test]
    fn modifier_bitmask() {
        assert_eq!(modifiers_mask(Modifiers::default()), 0);
        let all = Modifiers {
            alt: true,
            ctrl: true,
            shift: true,
            meta: true,
        };
        assert_eq!(modifiers_mask(all), 15);
        let only = |f: fn(&mut Modifiers)| {
            let mut m = Modifiers::default();
            f(&mut m);
            modifiers_mask(m)
        };
        assert_eq!(only(|m| m.alt = true), 1);
        assert_eq!(only(|m| m.ctrl = true), 2);
        assert_eq!(only(|m| m.meta = true), 4);
        assert_eq!(only(|m| m.shift = true), 8);
    }

    #[test]
    fn named_keys() {
        let cases = [
            ("Enter", "Enter", 13),
            ("Backspace", "Backspace", 8),
            ("Tab", "Tab", 9),
            ("Escape", "Escape", 27),
            ("ArrowLeft", "ArrowLeft", 37),
            ("ArrowUp", "ArrowUp", 38),
            ("ArrowRight", "ArrowRight", 39),
            ("ArrowDown", "ArrowDown", 40),
            ("Home", "Home", 36),
            ("End", "End", 35),
            ("PageUp", "PageUp", 33),
            ("PageDown", "PageDown", 34),
            ("Delete", "Delete", 46),
            ("F1", "F1", 112),
            ("F12", "F12", 123),
            (" ", "Space", 32),
            ("Shift", "ShiftLeft", 16),
        ];
        for (k, c, expected) in cases {
            assert_eq!(key_code(k, c), expected, "{k}");
        }
        assert_eq!(key_code("F25", ""), 0);
    }

    #[test]
    fn letters_digits_and_punctuation() {
        assert_eq!(key_code("a", "KeyA"), 65);
        assert_eq!(key_code("Z", "KeyZ"), 90);
        assert_eq!(key_code("0", "Digit0"), 48);
        assert_eq!(key_code("9", "Digit9"), 57);
        assert_eq!(key_code("!", "Digit1"), 49);
        assert_eq!(key_code(";", "Semicolon"), 186);
        assert_eq!(key_code("/", "Slash"), 191);
        // Layout: o caractere manda, não a tecla física (AZERTY: "a" em KeyQ).
        assert_eq!(key_code("a", "KeyQ"), 65);
        // Sem caractere conhecido, vale a tecla física (ç no ABNT2).
        assert_eq!(key_code("ç", "Semicolon"), 186);
        assert_eq!(key_code("", "KeyB"), 66);
        assert_eq!(key_code("5", "Numpad5"), 101);
        assert_eq!(key_code("+", "NumpadAdd"), 107);
        // Num Lock desligado: vale a tecla nomeada.
        assert_eq!(key_code("ArrowLeft", "Numpad4"), 37);
        assert_eq!(key_code("é", ""), 0);
    }

    #[test]
    fn key_events_carry_text_only_when_printable() {
        let down = key_event(&key(KeyAction::Down, "a", "KeyA", Some("a")));
        assert_eq!(down.r#type, DispatchKeyEventType::KeyDown);
        assert_eq!(down.text.as_deref(), Some("a"));
        assert_eq!(down.windows_virtual_key_code, Some(65));

        let up = key_event(&key(KeyAction::Up, "a", "KeyA", Some("a")));
        assert_eq!(up.r#type, DispatchKeyEventType::KeyUp);
        assert_eq!(up.text, None);

        let bs = key_event(&key(
            KeyAction::Down,
            "Backspace",
            "Backspace",
            Some("\u{8}"),
        ));
        assert_eq!(bs.r#type, DispatchKeyEventType::RawKeyDown);
        assert_eq!(bs.text, None);

        let enter = key_event(&key(KeyAction::Down, "Enter", "Enter", None));
        assert_eq!(enter.r#type, DispatchKeyEventType::KeyDown);
        assert_eq!(enter.text.as_deref(), Some("\r"));

        let space = key_event(&key(KeyAction::Down, " ", "Space", Some(" ")));
        assert_eq!(space.text.as_deref(), Some(" "));

        let mut ctrl_a = key(KeyAction::Down, "a", "KeyA", Some("a"));
        ctrl_a.modifiers.ctrl = true;
        let ctrl_a = key_event(&ctrl_a);
        assert_eq!(ctrl_a.r#type, DispatchKeyEventType::RawKeyDown);
        assert_eq!(ctrl_a.text, None);
        assert_eq!(ctrl_a.modifiers, Some(2));

        let mut altgr = key(KeyAction::Down, "@", "KeyQ", Some("@"));
        altgr.modifiers.ctrl = true;
        altgr.modifiers.alt = true;
        assert_eq!(key_event(&altgr).text.as_deref(), Some("@"));
    }

    #[test]
    fn key_location_and_keypad() {
        assert_eq!(key_location("ShiftLeft"), Some(1));
        assert_eq!(key_location("ControlRight"), Some(2));
        assert_eq!(key_location("ArrowRight"), None);
        assert_eq!(key_location("BracketLeft"), None);
        let p = key_event(&key(KeyAction::Down, "1", "Numpad1", Some("1")));
        assert_eq!(p.location, Some(3));
        assert_eq!(p.is_keypad, Some(true));
    }

    #[test]
    fn mouse_tracks_pressed_buttons_for_drag() {
        let mut state = MouseState::default();
        let at = |action, button| MouseInput {
            action,
            x: 10.0,
            y: 20.0,
            button,
            click_count: 1,
            modifiers: Modifiers::default(),
        };
        let down = state.mouse(&at(MouseAction::Down, MouseButton::Left));
        assert_eq!(down.r#type, DispatchMouseEventType::MousePressed);
        assert_eq!(down.buttons, Some(1));
        assert_eq!(down.click_count, Some(1));

        let drag = state.mouse(&at(MouseAction::Move, MouseButton::None));
        assert_eq!(drag.r#type, DispatchMouseEventType::MouseMoved);
        assert_eq!(drag.button, Some(CdpButton::Left));
        assert_eq!(drag.buttons, Some(1));
        assert_eq!(drag.click_count, None);

        let up = state.mouse(&at(MouseAction::Up, MouseButton::Left));
        assert_eq!(up.r#type, DispatchMouseEventType::MouseReleased);
        assert_eq!(up.buttons, Some(0));

        let hover = state.mouse(&at(MouseAction::Move, MouseButton::None));
        assert_eq!(hover.button, Some(CdpButton::None));
        assert_eq!((hover.x, hover.y), (10.0, 20.0));
    }

    #[test]
    fn wheel_event() {
        let state = MouseState::default();
        let p = state.wheel(&WheelInput {
            x: 5.0,
            y: 6.0,
            delta_x: 0.0,
            delta_y: 120.0,
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        });
        assert_eq!(p.r#type, DispatchMouseEventType::MouseWheel);
        assert_eq!(p.delta_y, Some(120.0));
        assert_eq!(p.modifiers, Some(8));
    }
}
