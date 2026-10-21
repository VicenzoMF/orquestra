//! Canvas com pan/zoom (`egui::Scene`): uma moldura por sessão, orquestrador
//! à esquerda, workers em coluna, ligados por linhas ortogonais na cor do
//! status do filho.

use std::hash::{DefaultHasher, Hash, Hasher};

use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Id, Key, Layout, Margin, Pos2, Rect, RichText,
    Scene, Sense, Shape, Stroke, StrokeKind, TextEdit, Ui, Vec2, pos2, vec2,
};
use orquestra_core::{AgentStatus, Command, QueueReason, SessionId};

use super::widgets::{self, kind_label, paint_elided};
use super::{Ctx, Resources, UiState};
use crate::actions;
use crate::browser::fit_rect;
use crate::feed::lock;
use crate::layout::{self, ANCHOR_Y, CanvasLayout, Edge, NodeSpec, SessionLayout, SessionSpec};
use crate::model::{AgentView, Model, SessionView, StatusKind, plural, status_label};
use crate::theme;

pub const ORQ_W: f32 = 330.0;
pub const WORKER_W: f32 = 300.0;
const ORQ_H: f32 = 206.0;
const BASE_H: f32 = 86.0;
const TAIL_H: f32 = 66.0;
const THUMB_H: f32 = 126.0;
const ROW_H: f32 = 34.0;
const ZOOM_MIN: f32 = 0.2;
const ZOOM_MAX: f32 = 1.5;
/// Histerese do modo detalhado conforme o zoom.
const DETAIL_ON: f32 = 0.8;
const DETAIL_OFF: f32 = 0.65;

pub struct CanvasState {
    pub scene_rect: Rect,
    /// Reajusta sozinho quando a estrutura muda, até o usuário mexer.
    auto_fit: bool,
    fit_now: bool,
    pub detailed: bool,
    pub zoom: f32,
    structure: u64,
    viewport: Rect,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            scene_rect: Rect::ZERO,
            auto_fit: true,
            fit_now: true,
            detailed: false,
            zoom: 1.0,
            structure: 0,
            viewport: Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 800.0)),
        }
    }
}

impl CanvasState {
    pub fn request_fit(&mut self) {
        self.auto_fit = true;
        self.fit_now = true;
    }

    fn zoom_by(&mut self, factor: f32) {
        let outer = self.viewport;
        let z = (effective_zoom(outer, self.scene_rect) * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        self.scene_rect = Rect::from_center_size(self.scene_rect.center(), outer.size() / z);
        self.auto_fit = false;
        self.update_detail(z);
    }

    fn update_detail(&mut self, zoom: f32) {
        self.zoom = zoom;
        if zoom >= DETAIL_ON {
            self.detailed = true;
        } else if zoom < DETAIL_OFF {
            self.detailed = false;
        }
    }
}

fn effective_zoom(outer: Rect, scene: Rect) -> f32 {
    if scene.width() <= 0.0 || scene.height() <= 0.0 {
        return 1.0;
    }
    (outer.width() / scene.width())
        .min(outer.height() / scene.height())
        .clamp(ZOOM_MIN, ZOOM_MAX)
}

fn has_browser(agent: &AgentView) -> bool {
    agent.config.browser || agent.browser.is_open()
}

fn worker_size(agent: &AgentView, detailed: bool) -> Vec2 {
    let mut h = BASE_H;
    if detailed {
        h += TAIL_H;
        if has_browser(agent) {
            h += THUMB_H;
        }
        if agent.kind() == StatusKind::Waiting {
            h += ROW_H;
        }
    }
    if agent.kind() == StatusKind::Approval {
        h += ROW_H;
    }
    vec2(WORKER_W, h)
}

fn specs(model: &Model, detailed: impl Fn(SessionId) -> bool) -> Vec<SessionSpec> {
    model
        .sessions()
        .into_iter()
        .map(|s| {
            let d = detailed(s.id);
            SessionSpec {
                id: s.id,
                nodes: model
                    .agents_of(s.id)
                    .into_iter()
                    .map(|a| NodeSpec {
                        id: a.id,
                        parent: a.config.parent,
                        orchestrator: a.is_orchestrator(),
                        size: if a.is_orchestrator() {
                            vec2(ORQ_W, ORQ_H)
                        } else {
                            worker_size(a, d)
                        },
                    })
                    .collect(),
            }
        })
        .collect()
}

/// Assinatura do que muda o layout (para reajustar sozinho).
fn structure_hash(model: &Model) -> u64 {
    let mut h = DefaultHasher::new();
    for s in model.sessions() {
        s.id.hash(&mut h);
        for a in model.agents_of(s.id) {
            a.id.hash(&mut h);
        }
    }
    h.finish()
}

fn fit(cs: &mut CanvasState, model: &Model, focus: Option<SessionId>, outer: Rect) {
    let target = |detailed: bool| {
        let l = layout::layout(&specs(model, |s| detailed || focus == Some(s)));
        let rect = focus
            .and_then(|f| l.session(f).map(|s| s.frame))
            .unwrap_or(l.bounds)
            .expand(28.0);
        (rect, effective_zoom(outer, rect))
    };
    let (rect, zoom) = target(true);
    if zoom >= DETAIL_OFF + 0.05 {
        cs.detailed = true;
        cs.scene_rect = rect;
        cs.zoom = zoom;
    } else {
        let (rect, zoom) = target(false);
        cs.detailed = false;
        cs.scene_rect = rect;
        cs.zoom = zoom;
    }
    cs.fit_now = false;
}

pub fn show(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    cs: &mut CanvasState,
    res: &mut Resources,
    dialogs: &mut super::dialogs::Dialogs,
) {
    toolbar(ui, cx, st, cs, dialogs);
    let outer = ui.available_rect_before_wrap();
    // Janela/painel mudou de tamanho: reajusta se o usuário não assumiu o controle.
    if cs.auto_fit && (outer.size() - cs.viewport.size()).length() > 1.0 {
        cs.fit_now = true;
    }
    cs.viewport = outer;

    let structure = structure_hash(cx.model);
    if structure != cs.structure {
        cs.structure = structure;
        if cs.auto_fit {
            cs.fit_now = true;
        }
    }
    if cs.fit_now {
        fit(cs, cx.model, st.focus, outer);
    }
    let detailed = cs.detailed;
    let focus = st.focus;
    let layout = layout::layout(&specs(cx.model, |s| detailed || focus == Some(s)));

    let mut scene_rect = cs.scene_rect;
    let zoom = cs.zoom;
    let response = Scene::new()
        .zoom_range(ZOOM_MIN..=ZOOM_MAX)
        .max_inner_size(Vec2::splat(100_000.0))
        .show(ui, &mut scene_rect, |ui| {
            draw(ui, cx, st, res, &layout, detailed, zoom);
        })
        .response;
    cs.scene_rect = scene_rect;
    if response.changed() {
        cs.auto_fit = false;
        cs.update_detail(effective_zoom(outer, scene_rect));
    } else {
        cs.zoom = effective_zoom(outer, scene_rect);
    }
    if response.clicked() {
        st.selected = None;
    }
}

fn toolbar(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    cs: &mut CanvasState,
    dialogs: &mut super::dialogs::Dialogs,
) {
    egui::Frame::new()
        .fill(theme::PANEL_ALT)
        .inner_margin(Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let title = st
                    .focus
                    .and_then(|f| cx.model.session(f))
                    .map_or_else(|| "Visão geral".to_owned(), |s| s.config.name.clone());
                ui.label(RichText::new(title).strong().color(theme::TEXT));
                ui.separator();
                let has_sessions = !cx.model.sessions().is_empty();
                let solo = ui
                    .add_enabled(has_sessions, egui::Button::new("+ Agente avulso"))
                    .on_disabled_hover_text("Crie uma orquestração primeiro");
                if solo.clicked() {
                    dialogs.open_solo(cx, st);
                }
                ui.separator();
                if ui.button("−").on_hover_text("Afastar").clicked() {
                    cs.zoom_by(1.0 / 1.2);
                }
                ui.label(
                    RichText::new(format!("{:.0}%", cs.zoom * 100.0))
                        .monospace()
                        .color(theme::TEXT_DIM),
                );
                if ui.button("+").on_hover_text("Aproximar").clicked() {
                    cs.zoom_by(1.2);
                }
                if ui.button("Ajustar à tela").clicked() {
                    cs.request_fit();
                }
                // Legenda à direita; sem espaço, só os ícones (texto na dica).
                let full = ui.available_width() > 380.0;
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    for (kind, text) in [
                        (StatusKind::Queued, "na fila"),
                        (StatusKind::Failed, "falhou"),
                        (StatusKind::Done, "pronto"),
                        (StatusKind::Waiting, "aguardando"),
                        (StatusKind::Running, "rodando"),
                    ] {
                        ui.add_space(4.0);
                        if full {
                            ui.label(RichText::new(text).size(12.0).color(theme::TEXT_DIM));
                        }
                        widgets::status_icon(ui, kind, 12.0).on_hover_text(text);
                    }
                });
            });
        });
}

fn dots(painter: &egui::Painter, clip: Rect, zoom: f32) {
    let mut spacing = 24.0;
    while spacing * zoom < 16.0 {
        spacing *= 2.0;
    }
    let size = Vec2::splat((1.6 / zoom).max(1.0));
    let x0 = (clip.left() / spacing).floor() * spacing;
    let y0 = (clip.top() / spacing).floor() * spacing;
    let mut shapes = Vec::new();
    let mut y = y0;
    while y <= clip.bottom() {
        let mut x = x0;
        while x <= clip.right() {
            shapes.push(Shape::rect_filled(
                Rect::from_center_size(pos2(x, y), size),
                CornerRadius::ZERO,
                theme::DOT,
            ));
            x += spacing;
        }
        y += spacing;
    }
    painter.extend(shapes);
}

fn draw(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    res: &mut Resources,
    layout: &CanvasLayout,
    detailed: bool,
    zoom: f32,
) {
    dots(ui.painter(), ui.clip_rect(), zoom);
    if layout.sessions.is_empty() {
        let c = layout.bounds.center();
        let painter = ui.painter();
        painter.text(
            c - vec2(0.0, 14.0),
            Align2::CENTER_CENTER,
            "Nenhuma orquestração ainda",
            FontId::proportional(20.0),
            theme::TEXT,
        );
        painter.text(
            c + vec2(0.0, 16.0),
            Align2::CENTER_CENTER,
            "Crie uma em “+ Nova orquestração” (Ctrl+Shift+N)",
            FontId::proportional(14.0),
            theme::TEXT_DIM,
        );
        return;
    }
    let visible = ui.clip_rect();
    for sl in &layout.sessions {
        let Some(session) = cx.model.session(sl.id) else {
            continue;
        };
        let dim = st.focus.is_some_and(|f| f != sl.id);
        let session_detailed = detailed || st.focus == Some(sl.id);
        ui.scope(|ui| {
            if dim {
                ui.multiply_opacity(0.3);
            }
            draw_session(
                ui,
                cx,
                st,
                res,
                session,
                sl,
                session_detailed && !dim,
                visible,
            );
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_session(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    res: &mut Resources,
    session: &SessionView,
    sl: &SessionLayout,
    detailed: bool,
    visible: Rect,
) {
    let painter = ui.painter().clone();
    let frame = sl.frame;
    painter.rect(
        frame,
        CornerRadius::same(12),
        theme::PANEL_ALT,
        Stroke::new(1.0, theme::BORDER),
        StrokeKind::Inside,
    );

    // Cabeçalho: nome, repositório e resumo. Clique foca a sessão.
    let header = Rect::from_min_size(frame.min, vec2(frame.width(), layout::HEADER_H - 8.0));
    let header_resp = ui.interact(
        header,
        Id::unique(("session-header", sl.id)),
        Sense::click(),
    );
    if header_resp.clicked() {
        st.focus = if st.focus == Some(sl.id) {
            None
        } else {
            Some(sl.id)
        };
    }
    let header_resp = header_resp.on_hover_text("Clique para focar esta orquestração");
    let _ = header_resp;
    let y = frame.top() + 22.0;
    let name_rect = painter.text(
        pos2(frame.left() + 18.0, y),
        Align2::LEFT_CENTER,
        &session.config.name,
        FontId::proportional(15.5),
        theme::TEXT,
    );
    let c = cx.model.counts(Some(sl.id));
    let (summary, kind) = cx.model.session_summary(sl.id);
    let right = frame.right() - 18.0;
    let summary_rect = painter.text(
        pos2(right, y),
        Align2::RIGHT_CENTER,
        &summary,
        FontId::proportional(13.0),
        kind.map_or(theme::TEXT_DIM, theme::status_color),
    );
    let count_rect = painter.text(
        pos2(summary_rect.left() - 4.0, y),
        Align2::RIGHT_CENTER,
        format!("{} · ", plural(c.workers, "agente", "agentes")),
        FontId::proportional(13.0),
        theme::TEXT_DIM,
    );
    let repo = session.config.repo.display().to_string();
    let repo = repo.replace(&std::env::var("HOME").unwrap_or_default(), "~");
    paint_elided(
        &painter,
        pos2(name_rect.right() + 14.0, y + 1.0),
        Align2::LEFT_CENTER,
        &repo,
        FontId::proportional(12.5),
        theme::TEXT_DIM,
        (count_rect.left() - name_rect.right() - 30.0).max(0.0),
    );

    if sl.nodes.is_empty() {
        painter.text(
            pos2(frame.left() + 18.0, frame.top() + layout::HEADER_H + 24.0),
            Align2::LEFT_CENTER,
            "Sem agentes ainda.",
            FontId::proportional(13.0),
            theme::TEXT_DIM,
        );
    }

    // Arestas de baixo para cima: o trecho do tronco fica com a cor do filho mais próximo.
    for edge in sl.edges.iter().rev() {
        if let Some(child) = cx.model.agent(edge.child) {
            draw_edge(&painter, edge, child);
        }
    }
    for (id, rect) in &sl.nodes {
        let Some(agent) = cx.model.agent(*id) else {
            continue;
        };
        if !visible.intersects(*rect) {
            continue;
        }
        if agent.is_orchestrator() {
            orchestrator_card(ui, cx, st, agent, session, *rect);
        } else {
            worker_card(ui, cx, st, res, agent, *rect, detailed);
        }
    }
}

fn draw_edge(painter: &egui::Painter, edge: &Edge, child: &AgentView) {
    let kind = child.kind();
    let color = theme::status_color(kind);
    let [a, b, c, d] = edge.points;
    let end = d - vec2(7.0, 0.0);
    let pts = vec![a, b, c, end];
    let stroke = Stroke::new(1.6, color.gamma_multiply(0.85));
    if matches!(kind, StatusKind::Queued | StatusKind::Approval) {
        painter.extend(Shape::dashed_line(&pts, stroke, 6.0, 4.0));
    } else {
        painter.add(Shape::line(pts, stroke));
    }
    theme::paint_arrow_head(painter, d - vec2(1.0, 0.0), 8.0, color);
    paint_elided(
        painter,
        edge.label_at + vec2(0.0, -5.0),
        Align2::LEFT_BOTTOM,
        &child.config.name,
        FontId::proportional(11.5),
        theme::TEXT_DIM,
        edge.label_width,
    );
}

fn card_frame(painter: &egui::Painter, rect: Rect, agent: &AgentView, selected: bool) {
    let kind = agent.kind();
    painter.rect_filled(rect, CornerRadius::same(10), theme::CARD);
    let border = match kind {
        StatusKind::Waiting => theme::WAITING.gamma_multiply(0.8),
        StatusKind::Approval => theme::APPROVAL.gamma_multiply(0.7),
        StatusKind::Failed => theme::FAILED.gamma_multiply(0.45),
        _ => theme::BORDER,
    };
    if matches!(kind, StatusKind::Queued | StatusKind::Starting) && !selected {
        widgets::dashed_rect(painter, rect.shrink(0.5), theme::IDLE.gamma_multiply(0.7));
    } else {
        painter.rect_stroke(
            rect,
            CornerRadius::same(10),
            Stroke::new(1.0, border),
            StrokeKind::Inside,
        );
    }
    if kind == StatusKind::Waiting {
        painter.rect_filled(
            Rect::from_min_size(rect.min + vec2(0.0, 10.0), vec2(3.0, rect.height() - 20.0)),
            CornerRadius::same(2),
            theme::WAITING,
        );
    }
    if selected {
        painter.rect_stroke(
            rect.expand(2.0),
            CornerRadius::same(12),
            Stroke::new(2.0, theme::ACCENT),
            StrokeKind::Outside,
        );
    }
}

/// Linha de contexto do card: última linha do terminal ou o motivo da espera.
fn context_line(cx: &Ctx, agent: &AgentView) -> (String, Color32) {
    match &agent.status {
        AgentStatus::Queued {
            reason: QueueReason::Capacity,
        } => ("aguardando uma vaga livre".to_owned(), theme::TEXT_DIM),
        AgentStatus::Queued {
            reason: QueueReason::Approval,
        } => (
            format!(
                "quer criar: {}",
                agent
                    .config
                    .initial_prompt
                    .as_deref()
                    .unwrap_or(&agent.config.name)
            ),
            theme::APPROVAL,
        ),
        AgentStatus::Failed { reason } => {
            let last = term_tail(cx, agent, 1).pop();
            (last.unwrap_or_else(|| reason.clone()), theme::FAILED)
        }
        _ => match term_tail(cx, agent, 1).pop() {
            Some(line) => (line, Color32::from_rgb(0xb4, 0xba, 0xc4)),
            None => (
                agent
                    .config
                    .initial_prompt
                    .clone()
                    .unwrap_or_else(|| "iniciando…".to_owned()),
                theme::TEXT_DIM,
            ),
        },
    }
}

fn term_tail(cx: &Ctx, agent: &AgentView, n: usize) -> Vec<String> {
    cx.shared
        .term(agent.id)
        .map(|t| lock(&t).tail(n))
        .unwrap_or_default()
}

fn card_header(painter: &egui::Painter, rect: Rect, agent: &AgentView, title: &str, star: bool) {
    let kind = agent.kind();
    let y = rect.top() + ANCHOR_Y;
    let x = rect.left() + 16.0;
    let mut name_x = x + 18.0;
    if star {
        theme::paint_star(painter, pos2(x + 5.0, y), 7.0, theme::WAITING);
        name_x += 4.0;
    } else {
        theme::paint_status_icon(painter, pos2(x + 5.0, y), 5.0, kind);
    }
    let status = painter.text(
        pos2(rect.right() - 14.0, y),
        Align2::RIGHT_CENTER,
        status_label(&agent.status),
        FontId::proportional(12.0),
        theme::status_color(kind),
    );
    if star {
        theme::paint_status_icon(painter, pos2(status.left() - 10.0, y), 4.0, kind);
    }
    paint_elided(
        painter,
        pos2(name_x, y),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(14.5),
        theme::TEXT,
        status.left() - name_x - 22.0,
    );
}

fn worker_card(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    res: &mut Resources,
    agent: &AgentView,
    rect: Rect,
    detailed: bool,
) {
    let response = ui
        .interact(rect, Id::unique(("card", agent.id)), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        st.select(agent.id);
    }
    let painter = ui.painter().clone();
    card_frame(&painter, rect, agent, st.selected == Some(agent.id));
    card_header(&painter, rect, agent, &agent.config.name, false);

    // Tipo e badge do navegador.
    let x = rect.left() + 16.0;
    let y2 = rect.top() + 47.0;
    let kind_rect = painter.text(
        pos2(x, y2),
        Align2::LEFT_CENTER,
        kind_label(agent.config.kind),
        FontId::proportional(12.0),
        theme::TEXT_DIM,
    );
    if has_browser(agent) {
        let label = match agent.browser.port() {
            Some(port) => format!("navegador :{port}"),
            None => "navegador".to_owned(),
        };
        let galley = painter.layout_no_wrap(label, FontId::proportional(11.0), theme::ACCENT);
        let badge = Rect::from_min_size(
            pos2(kind_rect.right() + 10.0, y2 - 9.0),
            galley.size() + vec2(12.0, 4.0),
        );
        painter.rect(
            badge,
            CornerRadius::same(255),
            theme::ACCENT.gamma_multiply(0.1),
            Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.35)),
            StrokeKind::Inside,
        );
        painter.galley(badge.min + vec2(6.0, 2.0), galley, theme::ACCENT);
    }

    let (line, color) = context_line(cx, agent);
    paint_elided(
        &painter,
        pos2(x, rect.top() + 68.0),
        Align2::LEFT_CENTER,
        &line,
        FontId::monospace(11.5),
        color,
        rect.width() - 30.0,
    );

    let mut y = rect.top() + BASE_H;
    if detailed {
        let tail_rect = Rect::from_min_size(
            pos2(rect.left() + 10.0, y),
            vec2(rect.width() - 20.0, TAIL_H - 8.0),
        );
        painter.rect_filled(tail_rect, CornerRadius::same(6), theme::TERM_BG);
        // A última linha já aparece acima; aqui vêm as anteriores.
        let mut lines = term_tail(cx, agent, 5);
        lines.pop();
        for (i, l) in lines.iter().enumerate() {
            paint_elided(
                &painter,
                tail_rect.min + vec2(8.0, 6.0 + i as f32 * 12.5),
                Align2::LEFT_TOP,
                l,
                FontId::monospace(10.5),
                Color32::from_rgb(0x9a, 0xa2, 0xad),
                tail_rect.width() - 16.0,
            );
        }
        y += TAIL_H;
        if has_browser(agent) {
            let thumb = Rect::from_min_size(
                pos2(rect.left() + 10.0, y),
                vec2(rect.width() - 20.0, THUMB_H - 8.0),
            );
            thumbnail(ui, cx, st, res, agent, thumb);
            y += THUMB_H;
        }
        if agent.kind() == StatusKind::Waiting {
            reply_row(ui, cx, st, agent, pos2(rect.left() + 12.0, y));
            y += ROW_H;
        }
    }
    if agent.kind() == StatusKind::Approval {
        approval_row(ui, cx, agent, pos2(rect.left() + 12.0, y));
    }
}

fn thumbnail(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    res: &mut Resources,
    agent: &AgentView,
    rect: Rect,
) {
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(6), theme::TERM_BG);
    if agent.browser.port().is_some() {
        st.thumb_browsers.push(agent.id);
    }
    if let Some(img) = cx.shared.frames.take_thumb(agent.id) {
        match res.thumbs.get_mut(&agent.id) {
            Some(tex) => tex.set(img, egui::TextureOptions::LINEAR),
            None => {
                let tex = ui.ctx().load_texture(
                    format!("thumb-{}", agent.id.short()),
                    img,
                    egui::TextureOptions::LINEAR,
                );
                res.thumbs.insert(agent.id, tex);
            }
        }
    }
    match res.thumbs.get(&agent.id) {
        Some(tex) => {
            let r = fit_rect(rect.shrink(2.0), tex.size());
            painter.image(
                tex.id(),
                r,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        None => {
            let text = match &agent.browser {
                crate::model::BrowserState::Launching => "iniciando navegador…".to_owned(),
                crate::model::BrowserState::Ready { port } => format!("navegador :{port}"),
                crate::model::BrowserState::Failed(_) => "navegador falhou".to_owned(),
                _ => "sem navegador aberto".to_owned(),
            };
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                text,
                FontId::proportional(12.0),
                theme::TEXT_DIM,
            );
        }
    }
}

fn reply_row(ui: &mut Ui, cx: &Ctx, st: &mut UiState, agent: &AgentView, at: Pos2) {
    let (yes, no) = actions::quick_replies(agent.config.kind);
    let size = vec2(64.0, 24.0);
    let r1 = Rect::from_min_size(at, size);
    let r2 = r1.translate(vec2(72.0, 0.0));
    let r3 = Rect::from_min_size(r2.right_top() + vec2(8.0, 0.0), vec2(110.0, 24.0));
    if ui
        .put(r1, widgets::tinted_button("Sim", theme::DONE))
        .clicked()
    {
        cx.send(Command::SendInput {
            id: agent.id,
            data: yes,
        });
    }
    if ui
        .put(r2, widgets::tinted_button("Não", theme::FAILED))
        .clicked()
    {
        cx.send(Command::SendInput {
            id: agent.id,
            data: no,
        });
    }
    if ui
        .put(
            r3,
            egui::Button::new(RichText::new("Abrir terminal").size(12.5)),
        )
        .clicked()
    {
        st.select(agent.id);
    }
}

fn approval_row(ui: &mut Ui, cx: &Ctx, agent: &AgentView, at: Pos2) {
    let r1 = Rect::from_min_size(at, vec2(84.0, 24.0));
    let r2 = r1.translate(vec2(92.0, 0.0));
    if ui
        .put(r1, widgets::tinted_button("Aprovar", theme::DONE))
        .clicked()
    {
        cx.send(Command::ApproveSpawn { id: agent.id });
    }
    if ui
        .put(r2, widgets::tinted_button("Recusar", theme::FAILED))
        .clicked()
    {
        cx.send(Command::RejectSpawn { id: agent.id });
    }
}

fn orchestrator_card(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    agent: &AgentView,
    session: &SessionView,
    rect: Rect,
) {
    let response = ui
        .interact(rect, Id::unique(("card", agent.id)), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        st.select(agent.id);
    }
    let painter = ui.painter().clone();
    card_frame(&painter, rect, agent, st.selected == Some(agent.id));
    card_header(&painter, rect, agent, "orquestrador", true);
    let x = rect.left() + 16.0;
    let w = rect.width() - 32.0;
    painter.text(
        pos2(x, rect.top() + 47.0),
        Align2::LEFT_CENTER,
        kind_label(agent.config.kind),
        FontId::proportional(12.0),
        theme::TEXT_DIM,
    );

    // Objetivo em até duas linhas.
    painter.text(
        pos2(x, rect.top() + 66.0),
        Align2::LEFT_CENTER,
        "Objetivo",
        FontId::proportional(11.0),
        theme::TEXT_DIM,
    );
    let mut job = egui::text::LayoutJob::simple(
        session.config.goal.clone(),
        FontId::proportional(13.0),
        theme::TEXT,
        w,
    );
    job.wrap.max_rows = 2;
    job.wrap.overflow_character = Some('…');
    let galley = painter.layout_job(job);
    painter.galley(pos2(x, rect.top() + 76.0), galley, theme::TEXT);

    // Progresso.
    let (done, total) = cx.model.progress(agent.id);
    let py = rect.top() + 124.0;
    let label = if total == 0 {
        "nenhum agente criado".to_owned()
    } else {
        format!("{done} de {total} prontas")
    };
    let label_rect = painter.text(
        pos2(rect.right() - 16.0, py),
        Align2::RIGHT_CENTER,
        label,
        FontId::proportional(12.0),
        theme::TEXT_DIM,
    );
    let bar = Rect::from_min_size(
        pos2(x, py - 3.0),
        vec2((label_rect.left() - x - 12.0).max(10.0), 6.0),
    );
    painter.rect_filled(bar, CornerRadius::same(3), theme::BORDER);
    if total > 0 {
        let mut fill = bar;
        fill.set_width(bar.width() * done as f32 / total as f32);
        painter.rect_filled(fill, CornerRadius::same(3), theme::DONE);
    }

    let (line, color) = context_line(cx, agent);
    paint_elided(
        &painter,
        pos2(x, rect.top() + 146.0),
        Align2::LEFT_CENTER,
        &line,
        FontId::monospace(11.5),
        color,
        w,
    );

    // Campo de conversa: texto + Enter vai para o PTY do orquestrador.
    let field = Rect::from_min_size(pos2(x, rect.top() + 164.0), vec2(w - 38.0, 28.0));
    let send = Rect::from_min_size(pos2(field.right() + 6.0, field.top()), vec2(32.0, 28.0));
    let text = st.chat.entry(agent.id).or_default();
    let edit = ui.put(
        field,
        TextEdit::singleline(text)
            .hint_text("Fale com o orquestrador…")
            .margin(Margin::symmetric(8, 5)),
    );
    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
    let clicked = ui
        .put(send, egui::Button::new("↵"))
        .on_hover_text("Enviar")
        .clicked();
    if (enter || clicked) && !text.trim().is_empty() {
        cx.send(Command::SendInput {
            id: agent.id,
            data: actions::message(text),
        });
        text.clear();
        if enter {
            edit.request_focus();
        }
    }
}
