//! Cores do tema escuro e os ícones de status (desenhados, não glifos de
//! fonte: assim não dependem da cobertura das fontes padrão).

use egui::{Color32, CornerRadius, Painter, Pos2, Shape, Stroke, Visuals, pos2, vec2};

use crate::model::StatusKind;

pub const BG: Color32 = Color32::from_rgb(0x16, 0x18, 0x1c);
pub const PANEL: Color32 = Color32::from_rgb(0x1d, 0x20, 0x26);
pub const PANEL_ALT: Color32 = Color32::from_rgb(0x1a, 0x1d, 0x22);
pub const CARD: Color32 = Color32::from_rgb(0x22, 0x26, 0x2d);
pub const BORDER: Color32 = Color32::from_rgb(0x2c, 0x30, 0x38);
pub const TEXT: Color32 = Color32::from_rgb(0xd7, 0xda, 0xe0);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8b, 0x94, 0x9e);
pub const TERM_BG: Color32 = Color32::from_rgb(0x12, 0x14, 0x18);
pub const DOT: Color32 = Color32::from_rgb(0x26, 0x2a, 0x31);
pub const ACCENT: Color32 = Color32::from_rgb(0x6c, 0xb6, 0xff);

pub const RUNNING: Color32 = Color32::from_rgb(0x6c, 0xb6, 0xff);
pub const WAITING: Color32 = Color32::from_rgb(0xf2, 0xb8, 0x4b);
pub const DONE: Color32 = Color32::from_rgb(0x5f, 0xd3, 0x8d);
pub const FAILED: Color32 = Color32::from_rgb(0xff, 0x7b, 0x72);
pub const IDLE: Color32 = Color32::from_rgb(0xa3, 0xac, 0xb9);
pub const APPROVAL: Color32 = Color32::from_rgb(0xe0, 0x9b, 0x3d);

pub fn status_color(kind: StatusKind) -> Color32 {
    match kind {
        StatusKind::Running => RUNNING,
        StatusKind::Waiting => WAITING,
        StatusKind::Done => DONE,
        StatusKind::Failed => FAILED,
        StatusKind::Starting | StatusKind::Queued => IDLE,
        StatusKind::Approval => APPROVAL,
    }
}

/// Visuals do egui no tema do orquestra.
pub fn visuals() -> Visuals {
    let mut v = Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.extreme_bg_color = TERM_BG;
    v.faint_bg_color = PANEL_ALT;
    v.code_bg_color = TERM_BG;
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = Color32::from_rgb(0x2a, 0x45, 0x66);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.window_corner_radius = CornerRadius::same(10);
    v.widgets.noninteractive.bg_fill = PANEL;
    v.widgets.noninteractive.weak_bg_fill = PANEL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.bg_fill = CARD;
    v.widgets.inactive.weak_bg_fill = CARD;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2b, 0x30, 0x39);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x2b, 0x30, 0x39);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x3d, 0x44, 0x4f));
    v.widgets.active.weak_bg_fill = Color32::from_rgb(0x30, 0x36, 0x40);
    v
}

/// Ícone de status com glifo próprio: ● rodando/aguardando, ✓ pronto,
/// ✕ falhou, ◌ iniciando/na fila, ● vazado para aprovação.
pub fn paint_status_icon(painter: &Painter, center: Pos2, radius: f32, kind: StatusKind) {
    let color = status_color(kind);
    let r = radius;
    match kind {
        StatusKind::Running => {
            painter.circle_filled(center, r, color);
        }
        StatusKind::Waiting => {
            painter.circle_filled(center, r, color);
            painter.circle_stroke(center, r + 2.0, Stroke::new(1.0, color.gamma_multiply(0.5)));
        }
        StatusKind::Done => {
            let pts = vec![
                center + vec2(-r, 0.0),
                center + vec2(-r * 0.25, r * 0.75),
                center + vec2(r, -r * 0.8),
            ];
            painter.add(Shape::line(pts, Stroke::new(r * 0.45, color)));
        }
        StatusKind::Failed => {
            let s = Stroke::new(r * 0.45, color);
            let d = r * 0.8;
            painter.line_segment([center + vec2(-d, -d), center + vec2(d, d)], s);
            painter.line_segment([center + vec2(-d, d), center + vec2(d, -d)], s);
        }
        StatusKind::Starting | StatusKind::Queued => {
            // Círculo pontilhado.
            let n = 8;
            for i in 0..n {
                let a = i as f32 / n as f32 * std::f32::consts::TAU;
                painter.circle_filled(center + vec2(a.cos(), a.sin()) * r, r * 0.18, color);
            }
        }
        StatusKind::Approval => {
            painter.circle_stroke(center, r * 0.9, Stroke::new(r * 0.35, color));
            painter.circle_filled(center, r * 0.3, color);
        }
    }
}

/// Estrela do orquestrador.
pub fn paint_star(painter: &Painter, center: Pos2, radius: f32, color: Color32) {
    let mut pts = Vec::with_capacity(10);
    for i in 0..10 {
        let r = if i % 2 == 0 { radius } else { radius * 0.45 };
        let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
        pts.push(pos2(center.x + r * a.cos(), center.y + r * a.sin()));
    }
    // Não convexo: desenha em triângulos a partir do centro.
    for i in 0..10 {
        let a = pts[i];
        let b = pts[(i + 1) % 10];
        painter.add(Shape::convex_polygon(
            vec![center, a, b],
            color,
            Stroke::NONE,
        ));
    }
}

/// Ponta de seta apontando para a direita, com a ponta em `tip`.
pub fn paint_arrow_head(painter: &Painter, tip: Pos2, size: f32, color: Color32) {
    painter.add(Shape::convex_polygon(
        vec![
            tip,
            tip + vec2(-size, -size * 0.6),
            tip + vec2(-size, size * 0.6),
        ],
        color,
        Stroke::NONE,
    ));
}
