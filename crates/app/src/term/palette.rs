//! Paleta do terminal: 16 cores ANSI no tema do app, cubo 256 e truecolor.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use egui::Color32;

const ANSI: [(u8, u8, u8); 16] = [
    (0x2c, 0x30, 0x38), // preto
    (0xff, 0x7b, 0x72), // vermelho
    (0x5f, 0xd3, 0x8d), // verde
    (0xf2, 0xb8, 0x4b), // amarelo
    (0x6c, 0xb6, 0xff), // azul
    (0xd2, 0xa8, 0xff), // magenta
    (0x56, 0xd4, 0xdd), // ciano
    (0xd7, 0xda, 0xe0), // branco
    (0x6e, 0x76, 0x81),
    (0xff, 0xa1, 0x98),
    (0x7e, 0xe2, 0xa8),
    (0xf8, 0xd2, 0x7a),
    (0xa5, 0xd6, 0xff),
    (0xe2, 0xc5, 0xff),
    (0x8b, 0xe9, 0xf0),
    (0xf0, 0xf3, 0xf6),
];

pub const FOREGROUND: Rgb = Rgb {
    r: 0xd7,
    g: 0xda,
    b: 0xe0,
};
pub const BACKGROUND: Rgb = Rgb {
    r: 0x12,
    g: 0x14,
    b: 0x18,
};

fn rgb((r, g, b): (u8, u8, u8)) -> Rgb {
    Rgb { r, g, b }
}

fn dim(c: Rgb) -> Rgb {
    let f = |v: u8| (f32::from(v) * 0.66) as u8;
    Rgb {
        r: f(c.r),
        g: f(c.g),
        b: f(c.b),
    }
}

/// Cor padrão de um índice da tabela do alacritty (0..269).
pub fn default_rgb(index: usize) -> Rgb {
    match index {
        0..16 => rgb(ANSI[index]),
        16..232 => {
            let i = index - 16;
            let level = |v: usize| if v == 0 { 0 } else { (v * 40 + 55) as u8 };
            Rgb {
                r: level(i / 36),
                g: level((i / 6) % 6),
                b: level(i % 6),
            }
        }
        232..256 => {
            let v = (8 + (index - 232) * 10) as u8;
            Rgb { r: v, g: v, b: v }
        }
        i if i == NamedColor::Foreground as usize => FOREGROUND,
        i if i == NamedColor::Background as usize => BACKGROUND,
        i if i == NamedColor::Cursor as usize => FOREGROUND,
        i if i == NamedColor::BrightForeground as usize => rgb(ANSI[15]),
        i if i == NamedColor::DimForeground as usize => dim(FOREGROUND),
        // DimBlack..DimWhite seguem a ordem das 8 cores básicas.
        i if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) => {
            dim(rgb(ANSI[i - NamedColor::DimBlack as usize]))
        }
        _ => FOREGROUND,
    }
}

/// Cor efetiva de um índice, respeitando o que o programa redefiniu (OSC 4/10/11).
pub fn indexed_rgb(colors: &Colors, index: usize) -> Rgb {
    colors[index].unwrap_or_else(|| default_rgb(index))
}

pub fn resolve(colors: &Colors, color: Color) -> Color32 {
    let c = match color {
        Color::Spec(rgb) => rgb,
        Color::Indexed(i) => indexed_rgb(colors, usize::from(i)),
        Color::Named(n) => indexed_rgb(colors, n as usize),
    };
    Color32::from_rgb(c.r, c.g, c.b)
}

/// Versão esmaecida (SGR 2) de uma cor de texto.
pub fn resolve_dim(colors: &Colors, color: Color) -> Color32 {
    match color {
        Color::Named(n) if (n as usize) < 8 || n == NamedColor::Foreground => {
            resolve(colors, Color::Named(n.to_dim()))
        }
        other => resolve(colors, other).gamma_multiply(0.66),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_and_grayscale_follow_xterm() {
        assert_eq!(default_rgb(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(
            default_rgb(231),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(default_rgb(196), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(default_rgb(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(
            default_rgb(255),
            Rgb {
                r: 238,
                g: 238,
                b: 238
            }
        );
    }

    #[test]
    fn named_and_overrides() {
        let mut colors = Colors::default();
        assert_eq!(
            resolve(&colors, Color::Named(NamedColor::Background)),
            Color32::from_rgb(0x12, 0x14, 0x18)
        );
        colors[NamedColor::Background] = Some(Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(
            resolve(&colors, Color::Named(NamedColor::Background)),
            Color32::from_rgb(1, 2, 3)
        );
        assert_eq!(
            resolve(&colors, Color::Spec(Rgb { r: 9, g: 8, b: 7 })),
            Color32::from_rgb(9, 8, 7)
        );
        let red = resolve(&colors, Color::Named(NamedColor::Red));
        assert_ne!(resolve_dim(&colors, Color::Named(NamedColor::Red)), red);
    }
}
