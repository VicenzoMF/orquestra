//! Fontes: as padrão do egui (Hack e Ubuntu-Light) mais, se existirem no
//! sistema, fontes de fallback para os símbolos que CLIs como Claude Code e
//! Codex desenham (⏺ ✻ ⎿, braille dos spinners...). Nada é embutido no
//! binário; sem as fontes do sistema esses glifos só não aparecem.

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

/// Candidatas, em ordem; as que existirem entram como fallback.
const SYSTEM_FALLBACKS: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
    "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
];

pub fn definitions() -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    // Hack cobre setas, ● e caixas, que a Ubuntu-Light não tem.
    if let Some(prop) = defs.families.get_mut(&FontFamily::Proportional) {
        let at = prop
            .iter()
            .position(|f| f == "Ubuntu-Light")
            .map_or(0, |i| i + 1);
        prop.insert(at, "Hack".to_owned());
    }
    let mut seen_dejavu = false;
    for path in SYSTEM_FALLBACKS {
        let is_dejavu = path.contains("DejaVu");
        if is_dejavu && seen_dejavu {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        seen_dejavu |= is_dejavu;
        let name = format!("system:{path}");
        defs.font_data
            .insert(name.clone(), Arc::new(FontData::from_owned(bytes)));
        for family in [FontFamily::Monospace, FontFamily::Proportional] {
            if let Some(list) = defs.families.get_mut(&family) {
                // Antes dos ícones do egui, depois das fontes principais.
                let at = list
                    .iter()
                    .position(|f| f == "egui-icons")
                    .unwrap_or(list.len());
                list.insert(at, name.clone());
            }
        }
    }
    defs
}

pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(definitions());
}

#[cfg(test)]
mod tests {
    #[test]
    fn hack_covers_symbols_in_proportional_text() {
        let ctx = egui::Context::default();
        super::install(&ctx);
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        let font = egui::FontId::proportional(13.0);
        ctx.fonts_mut(|f| {
            for c in "●←→└─│".chars() {
                assert!(f.has_glyph(&font, c), "sem glifo para {c}");
            }
        });
    }
}
