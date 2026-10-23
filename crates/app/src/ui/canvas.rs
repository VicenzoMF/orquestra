//! Canvas com pan/zoom (`egui::Scene`): uma moldura por sessão, onde a
//! pessoa a pôs (arrastando o cabeçalho), com quem pode delegar à esquerda e os
//! workers em coluna, ligados por linhas ortogonais na cor do status do
//! filho. O botão direito no fundo abre um menu; o rascunho de agente
//! (`draft.rs`) fica ancorado aqui. Por cima, fixos na tela, os painéis de
//! `overlay`.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::emath::TSTransform;
use egui::layers::ShapeIdx;
use egui::text::LayoutJob;
use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Id, Key, LayerId, Layout, Margin, Pos2, Rect,
    RichText, Scene, Sense, Shape, Stroke, StrokeKind, TextEdit, Ui, Vec2, pos2, vec2,
};
use orquestra_core::{AgentStatus, AppConfig, Command, QueueReason, SessionId};

use super::draft::{self, Draft};
use super::overlay::{self, INSET_BOTTOM, INSET_TOP};
use super::widgets::{self, kind_label, paint_elided};
use super::{Action, Ctx, Resources, UiState};
use crate::actions;
use crate::browser::fit_rect;
use crate::layout::{self, ANCHOR_Y, CanvasLayout, Edge, NodeSpec, SessionLayout, SessionSpec};
use crate::model::{AgentView, Model, SessionView, StatusKind, plural};
use crate::term::{Choice, Preview};
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
/// O ajuste automático não aproxima além do tamanho natural.
const FIT_MAX: f32 = 1.0;
/// Histerese do modo detalhado conforme o zoom.
const DETAIL_ON: f32 = 0.8;
const DETAIL_OFF: f32 = 0.65;
/// Abaixo deste zoom o rascunho abre aproximando a vista para 100%.
const DRAFT_MIN_ZOOM: f32 = 0.8;
/// Quanto a posição de uma sessão pedida ao hub espera ela chegar ao modelo.
const PENDING_TTL: Duration = Duration::from_secs(10);

pub struct CanvasState {
    pub scene_rect: Rect,
    /// Reajusta sozinho quando a estrutura muda, até o usuário mexer.
    auto_fit: bool,
    fit_now: bool,
    pub detailed: bool,
    pub zoom: f32,
    structure: u64,
    viewport: Rect,
    /// Rascunho de agente novo (um por vez).
    draft: Option<Draft>,
    /// Onde pôr o rascunho, resolvido no próximo frame depois do ajuste.
    place_draft: Option<Placement>,
    /// Ponto do último botão direito no fundo, em coordenadas do canvas.
    menu_at: Pos2,
    /// Canto da moldura de cada sessão, em coordenadas do canvas. Só no app:
    /// as sessões não sobrevivem a um reinício.
    positions: HashMap<SessionId, Pos2>,
    /// Sessões pedidas ao hub que ainda não chegaram ao modelo.
    pending: HashMap<SessionId, Instant>,
    /// Sessão arrastada pelo cabeçalho e onde a moldura estava ao começar.
    dragging: Option<(SessionId, Pos2)>,
}

/// O que as molduras das sessões pediram neste frame.
#[derive(Default)]
struct FrameInput {
    drag: Option<HeaderDrag>,
    /// O × ou "Fechar sessão" no menu do cabeçalho.
    close: Option<SessionId>,
}

/// O cabeçalho de uma moldura está sendo arrastado neste frame.
struct HeaderDrag {
    session: SessionId,
    /// Moldura como aparece agora (já com empurrões).
    frame_min: Pos2,
    /// Desde que o botão desceu, em coordenadas do canvas.
    total: Vec2,
}

#[derive(Clone, Copy)]
enum Placement {
    /// Centro da área visível.
    Center,
    /// Canto superior esquerdo neste ponto do canvas.
    At(Pos2),
}

/// Itens do menu do botão direito no fundo do canvas.
enum MenuChoice {
    NewAgent,
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
            draft: None,
            place_draft: None,
            menu_at: Pos2::ZERO,
            positions: HashMap::new(),
            pending: HashMap::new(),
            dragging: None,
        }
    }
}

impl CanvasState {
    pub fn request_fit(&mut self) {
        self.auto_fit = true;
        self.fit_now = true;
    }

    /// Põe a moldura de uma sessão com o canto em `at`.
    #[cfg(any(test, feature = "screenshot"))]
    pub fn set_position(&mut self, id: SessionId, at: Pos2) {
        self.positions.insert(id, at);
    }

    /// Posições de uma abertura anterior: valem quando as sessões reabertas
    /// chegarem (até lá, ficam como pendentes).
    pub fn restore_positions(&mut self, positions: &HashMap<SessionId, Pos2>) {
        let now = Instant::now();
        for (id, at) in positions {
            self.positions.insert(*id, *at);
            self.pending.insert(*id, now);
        }
    }

    /// Onde cada sessão está, para guardar; `None` no meio de um arrasto.
    pub fn settled_positions(&self) -> Option<&HashMap<SessionId, Pos2>> {
        self.dragging.is_none().then_some(&self.positions)
    }

    #[cfg(test)]
    pub fn position(&self, id: SessionId) -> Option<Pos2> {
        self.positions.get(&id).copied()
    }

    /// Moldura da sessão como o canvas a mostra (com foco nenhum).
    #[cfg(test)]
    pub fn session_frame(&self, model: &Model, id: SessionId) -> Option<Rect> {
        let detailed = self.detailed;
        let l = layout::layout(&specs(model, &self.positions, |_| detailed));
        l.session(id).map(|s| s.frame)
    }

    /// Esquece quem saiu do modelo e dá uma posição fixa, na primeira vez, a
    /// quem chegou sem uma (x = 0, abaixo da moldura mais baixa).
    fn sync_positions(&mut self, model: &Model, detailed: impl Fn(SessionId) -> bool) {
        let live: HashSet<SessionId> = model.sessions().iter().map(|s| s.id).collect();
        self.pending
            .retain(|id, since| !live.contains(id) && since.elapsed() < PENDING_TTL);
        self.positions
            .retain(|id, _| live.contains(id) || self.pending.contains_key(id));
        if live.iter().any(|id| !self.positions.contains_key(id)) {
            let l = layout::layout(&specs(model, &self.positions, detailed));
            for s in l.sessions {
                self.positions.entry(s.id).or_insert(s.frame.min);
            }
        }
    }

    /// Abre o rascunho no centro da área visível; se já há um, traz ele para
    /// lá com o texto.
    pub fn open_draft(&mut self, config: &AppConfig) {
        self.open_draft_with(config, None, Placement::Center);
    }

    /// Rascunho já com esta pasta (`orq <pasta>`): falta só a tarefa.
    pub fn open_draft_in(&mut self, config: &AppConfig, repo: &Path) {
        self.open_draft_with(config, Some(repo), Placement::Center);
    }

    fn open_draft_with(&mut self, config: &AppConfig, repo: Option<&Path>, place: Placement) {
        let draft = self
            .draft
            .get_or_insert_with(|| Draft::new(config, repo, self.auto_fit));
        if let Some(repo) = repo {
            draft.set_repo(repo);
        }
        draft.focus_task();
        self.place_draft = Some(place);
        // A vista não pula enquanto a pessoa digita.
        self.auto_fit = false;
    }

    /// Fecha o rascunho sem criar nada; devolve se havia um.
    pub fn cancel_draft(&mut self) -> bool {
        let Some(d) = self.draft.take() else {
            return false;
        };
        self.place_draft = None;
        if d.resume_fit {
            self.request_fit();
        }
        true
    }

    #[cfg(test)]
    pub fn draft(&self) -> Option<&Draft> {
        self.draft.as_ref()
    }

    #[cfg(any(test, feature = "screenshot"))]
    pub fn draft_mut(&mut self) -> Option<&mut Draft> {
        self.draft.as_mut()
    }

    /// Área do canvas que aparece na tela (a `Scene` centraliza `scene_rect`
    /// e sobra espaço num dos eixos).
    pub fn visible(&self) -> Rect {
        let zoom = effective_zoom(self.viewport, self.scene_rect);
        Rect::from_center_size(self.scene_rect.center(), self.viewport.size() / zoom)
    }

    /// Ponto do canvas → ponto da tela.
    #[cfg(test)]
    pub fn to_screen(&self, p: Pos2) -> Pos2 {
        let zoom = effective_zoom(self.viewport, self.scene_rect);
        self.viewport.center() + (p - self.visible().center()) * zoom
    }

    /// Resolve `place` com a vista deste frame: aproxima se o zoom deixaria o
    /// texto miúdo (mantendo o ponto no mesmo lugar da tela) e mantém a
    /// janela dentro da área visível.
    fn place(&mut self, place: Placement) {
        let Some(size) = self.draft.as_ref().map(Draft::size) else {
            return;
        };
        let zoom = effective_zoom(self.viewport, self.scene_rect);
        let mut visible = self.visible();
        let anchor = match place {
            Placement::Center => visible.center(),
            Placement::At(p) => p,
        };
        if zoom < DRAFT_MIN_ZOOM {
            let center = anchor + (visible.center() - anchor) * zoom;
            visible = Rect::from_center_size(center, self.viewport.size());
            self.scene_rect = visible;
            self.update_detail(1.0);
        }
        let at = match place {
            Placement::Center => anchor - size / 2.0,
            Placement::At(p) => p,
        };
        let area = visible.shrink(16.0);
        if let Some(d) = self.draft.as_mut() {
            d.at = pos2(
                at.x.min(area.right() - size.x).max(area.left()),
                at.y.min(area.bottom() - size.y).max(area.top()),
            );
        }
    }

    fn zoom_by(&mut self, factor: f32) {
        let outer = self.viewport;
        let z = (effective_zoom(outer, self.scene_rect) * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        self.scene_rect = Rect::from_center_size(self.scene_rect.center(), outer.size() / z);
        self.auto_fit = false;
        self.update_detail(z);
    }

    /// Zoom fixo em torno do centro (ganchos de screenshot).
    #[cfg(feature = "screenshot")]
    pub fn zoom_to(&mut self, zoom: f32) {
        let z = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        self.scene_rect =
            Rect::from_center_size(self.scene_rect.center(), self.viewport.size() / z);
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

fn specs(
    model: &Model,
    positions: &HashMap<SessionId, Pos2>,
    detailed: impl Fn(SessionId) -> bool,
) -> Vec<SessionSpec> {
    model
        .sessions()
        .into_iter()
        .map(|s| {
            let d = detailed(s.id);
            SessionSpec {
                id: s.id,
                origin: positions.get(&s.id).copied(),
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
    // Enquadra na faixa livre entre o botão de cima e os recursos de baixo.
    let free = Rect::from_min_max(
        outer.min + vec2(0.0, INSET_TOP),
        outer.max - vec2(0.0, INSET_BOTTOM),
    );
    let target = |detailed: bool| {
        let l = layout::layout(&specs(model, &cs.positions, |s| {
            detailed || focus == Some(s)
        }));
        let rect = focus
            .and_then(|f| l.session(f).map(|s| s.frame))
            .unwrap_or(l.bounds)
            .expand(28.0);
        let zoom = effective_zoom(free, rect).min(FIT_MAX);
        // A área visível inteira nesse zoom (o `Scene` a estica até a tela),
        // com o conteúdo no centro da faixa livre.
        let center = rect.center() + vec2(0.0, (INSET_BOTTOM - INSET_TOP) / 2.0 / zoom);
        (Rect::from_center_size(center, outer.size() / zoom), zoom)
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

/// Desenha o canvas; devolve `Action::Created` quando o rascunho inicia.
pub fn show(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    cs: &mut CanvasState,
    res: &mut Resources,
    dialogs: &mut super::dialogs::Dialogs,
) -> Option<Action> {
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
    let focus = st.focus;
    let detailed = cs.detailed;
    cs.sync_positions(cx.model, |s| detailed || focus == Some(s));
    if cs.fit_now {
        fit(cs, cx.model, st.focus, outer);
    }
    if let Some(place) = cs.place_draft.take() {
        cs.place(place);
    }
    let detailed = cs.detailed;
    let layout = layout::layout(&specs(cx.model, &cs.positions, |s| {
        detailed || focus == Some(s)
    }));

    let mut scene_rect = cs.scene_rect;
    let zoom = cs.zoom;
    let draft = &mut cs.draft;
    let mut scene_layer = None;
    let scene = Scene::new()
        .zoom_range(ZOOM_MIN..=ZOOM_MAX)
        .max_inner_size(Vec2::splat(100_000.0))
        .show(ui, &mut scene_rect, |ui| {
            scene_layer = Some(ui.layer_id());
            let drag = draw(ui, cx, st, res, &layout, detailed, zoom, draft.is_none());
            // Depois dos cards: fica por cima deles no desenho e no clique.
            (drag, draft.as_mut().and_then(|d| d.show(ui, cx)))
        });
    if let Some(layer) = scene_layer
        && let Some(to_screen) = ui.ctx().layer_transform_to_global(layer)
    {
        sharpen_text(ui.ctx(), layer, to_screen.scaling);
    }
    let response = scene.response;
    let (frames, event) = scene.inner;
    cs.scene_rect = scene_rect;
    if let Some(id) = frames.close {
        dialogs.close_session(cx, st, id);
    }

    // A posição guardada parte de onde a moldura aparecia ao começar.
    match frames.drag {
        Some(d) => {
            let from = match cs.dragging {
                Some((id, from)) if id == d.session => from,
                _ => {
                    cs.auto_fit = false;
                    d.frame_min
                }
            };
            cs.dragging = Some((d.session, from));
            cs.positions.insert(d.session, from + d.total);
        }
        None => cs.dragging = None,
    }
    if response.changed() {
        cs.auto_fit = false;
        cs.update_detail(effective_zoom(outer, scene_rect));
    } else {
        cs.zoom = effective_zoom(outer, scene_rect);
    }
    if response.clicked() {
        st.selected = None;
        st.menu.set(false, ui.input(|i| i.time));
    }
    overlay::show(ui, cx, st, cs, res, dialogs, outer);

    // O fundo só recebe o botão direito fora dos cards e do rascunho.
    if response.secondary_clicked()
        && let Some(pos) = response.interact_pointer_pos()
    {
        cs.menu_at = pos;
    }
    let mut choice = None;
    response.context_menu(|ui| choice = background_menu(ui));
    if let Some(choice) = choice {
        match choice {
            MenuChoice::NewAgent => {
                let at = cs.menu_at;
                cs.open_draft_with(cx.config, None, Placement::At(at));
            }
        }
    }

    match event? {
        draft::Event::Cancel => {
            cs.cancel_draft();
            None
        }
        draft::Event::Start => {
            // A moldura da sessão nova fica onde estava a janela.
            let draft = cs.draft.as_ref()?;
            let at = draft.at;
            let (session, action) = draft.start(cx, st)?;
            cs.positions.insert(session, at);
            cs.pending.insert(session, Instant::now());
            cs.draft = None;
            cs.place_draft = None;
            Some(action)
        }
    }
}

fn background_menu(ui: &mut Ui) -> Option<MenuChoice> {
    let mut choice = None;
    if ui.button("Iniciar novo agente").clicked() {
        choice = Some(MenuChoice::NewAgent);
    }
    choice
}

/// Texto nítido em qualquer zoom. O `Scene` escala as formas da camada, e um
/// texto escalado é a mesma rasterização esticada (borrada ao aproximar,
/// serrilhada ao afastar). Aqui cada texto é refeito no tamanho em que vai
/// aparecer e encolhido de volta: a escala da camada o devolve a esse tamanho,
/// agora com glifos rasterizados nele.
fn sharpen_text(ctx: &egui::Context, layer: LayerId, zoom: f32) {
    // Degraus de 1/24 de oitava (erro abaixo de 1,5%): poucos tamanhos novos
    // no atlas de fontes enquanto o zoom anda.
    let k = ((zoom.log2() * 24.0).round() / 24.0).exp2();
    if !k.is_finite() || (k - 1.0).abs() < 1e-3 {
        return;
    }
    let jobs: Vec<(usize, Arc<LayoutJob>)> = ctx.graphics(|g| {
        g.get(layer).map_or_else(Vec::new, |list| {
            list.all_entries()
                .enumerate()
                .filter_map(|(i, clipped)| match &clipped.shape {
                    Shape::Text(text) => Some((i, text.galley.job.clone())),
                    _ => None,
                })
                .collect()
        })
    });
    if jobs.is_empty() {
        return;
    }
    let galleys: Vec<_> = ctx.fonts_mut(|fonts| {
        jobs.into_iter()
            .map(|(i, job)| (i, fonts.layout_job(scaled_job(&job, k))))
            .collect()
    });
    ctx.graphics_mut(|g| {
        let list = g.entry(layer);
        for (i, galley) in galleys {
            list.mutate_shape(ShapeIdx(i), |clipped| {
                if let Shape::Text(text) = &mut clipped.shape {
                    text.galley = galley;
                    text.underline.width *= k;
                    // Encolhe 1/k em torno da própria posição.
                    let pos = text.pos.to_vec2();
                    text.transform(TSTransform::new(pos * (1.0 - 1.0 / k), 1.0 / k));
                }
            });
        }
    });
}

/// O mesmo texto `k` vezes maior.
fn scaled_job(job: &LayoutJob, k: f32) -> LayoutJob {
    let mut job = job.clone();
    for section in &mut job.sections {
        section.leading_space *= k;
        let format = &mut section.format;
        format.font_id.size *= k;
        format.extra_letter_spacing *= k;
        format.line_height = format.line_height.map(|h| h * k);
        format.expand_bg *= k;
        format.underline.width *= k;
        format.strikethrough.width *= k;
    }
    job.wrap.max_width *= k;
    job.first_row_min_height *= k;
    job
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
                    .add_enabled(has_sessions, egui::Button::new("+ Agente na sessão"))
                    .on_hover_text("Mais um agente numa sessão aberta, sem criar outros")
                    .on_disabled_hover_text("Abra uma sessão primeiro");
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

#[allow(clippy::too_many_arguments)]
fn draw(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    res: &mut Resources,
    layout: &CanvasLayout,
    detailed: bool,
    zoom: f32,
    empty_hint: bool,
) -> FrameInput {
    let mut input = FrameInput::default();
    dots(ui.painter(), ui.clip_rect(), zoom);
    if layout.sessions.is_empty() {
        if !empty_hint {
            return input;
        }
        let c = layout.bounds.center();
        let painter = ui.painter();
        painter.text(
            c - vec2(0.0, 22.0),
            Align2::CENTER_CENTER,
            "Nada aberto ainda",
            FontId::proportional(20.0),
            theme::TEXT,
        );
        painter.text(
            c + vec2(0.0, 8.0),
            Align2::CENTER_CENTER,
            "Clique com o botão direito para iniciar um agente",
            FontId::proportional(14.0),
            theme::TEXT_DIM,
        );
        painter.text(
            c + vec2(0.0, 30.0),
            Align2::CENTER_CENTER,
            "ou use “+ Novo agente” (Ctrl+Shift+N)",
            FontId::proportional(12.5),
            theme::TEXT_DIM,
        );
        return input;
    }
    let visible = ui.clip_rect();
    for sl in &layout.sessions {
        let Some(session) = cx.model.session(sl.id) else {
            continue;
        };
        let dim = st.focus.is_some_and(|f| f != sl.id);
        let session_detailed = detailed || st.focus == Some(sl.id);
        let (dragged, close) = ui
            .scope(|ui| {
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
                )
            })
            .inner;
        input.drag = input.drag.or(dragged);
        if close {
            input.close = Some(sl.id);
        }
    }
    input
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
) -> (Option<HeaderDrag>, bool) {
    let painter = ui.painter().clone();
    let frame = sl.frame;
    painter.rect(
        frame,
        CornerRadius::same(12),
        theme::PANEL_ALT,
        Stroke::new(1.0, theme::BORDER),
        StrokeKind::Inside,
    );

    // Cabeçalho: nome, repositório e resumo. Clique foca a sessão; arrastar
    // move a moldura inteira (o canvas não faz pan); botão direito, o menu.
    let header = Rect::from_min_size(frame.min, vec2(frame.width(), layout::HEADER_H - 8.0));
    let header_resp = ui.interact(
        header,
        Id::unique(("session-header", sl.id)),
        Sense::click_and_drag(),
    );
    let focused = st.focus == Some(sl.id);
    if header_resp.clicked() {
        st.focus = if focused { None } else { Some(sl.id) };
    }
    let mut close = false;
    header_resp.context_menu(|ui| {
        if ui
            .button(if focused { "Ver todas" } else { "Focar" })
            .clicked()
        {
            st.focus = if focused { None } else { Some(sl.id) };
        }
        if ui.button("Fechar sessão").clicked() {
            close = true;
        }
    });
    let drag = header_resp.total_drag_delta().map(|total| HeaderDrag {
        session: sl.id,
        frame_min: frame.min,
        total,
    });
    if drag.is_some() {
        header_resp.on_hover_and_drag_cursor(egui::CursorIcon::Grabbing);
    } else {
        header_resp
            .on_hover_cursor(egui::CursorIcon::Grab)
            .on_hover_text("Clique para focar; arraste para mover");
    }
    let y = frame.top() + 22.0;
    // Depois do cabeçalho: fica por cima dele no clique.
    let close_rect = Rect::from_center_size(pos2(frame.right() - 22.0, y), Vec2::splat(22.0));
    let close_resp = ui
        .interact(
            close_rect,
            Id::unique(("session-close", sl.id)),
            Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Fechar sessão (a pasta não é tocada)");
    close_x(&painter, close_rect, close_resp.hovered());
    close |= close_resp.clicked();
    let name_rect = painter.text(
        pos2(frame.left() + 18.0, y),
        Align2::LEFT_CENTER,
        &session.config.name,
        FontId::proportional(15.5),
        theme::TEXT,
    );
    let c = cx.model.counts(Some(sl.id));
    let (summary, kind) = cx.model.session_summary(sl.id);
    let right = close_rect.left() - 6.0;
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
    (drag, close)
}

/// O × de fechar, com fundo ao passar o mouse.
fn close_x(painter: &egui::Painter, rect: Rect, hovered: bool) {
    if hovered {
        painter.rect_filled(rect, CornerRadius::same(5), theme::BORDER);
    }
    let color = if hovered {
        theme::TEXT
    } else {
        theme::TEXT_DIM
    };
    let c = rect.center();
    let d = 4.5;
    let stroke = Stroke::new(1.6, color);
    painter.line_segment([c + vec2(-d, -d), c + vec2(d, d)], stroke);
    painter.line_segment([c + vec2(-d, d), c + vec2(d, -d)], stroke);
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

fn card_frame(painter: &egui::Painter, rect: Rect, kind: StatusKind, selected: bool) {
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

/// Linha de contexto do card: última linha útil do terminal ou o motivo da espera.
fn context_line(agent: &AgentView, preview: &Preview) -> (String, Color32) {
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
            let last = preview.lines.last().cloned();
            (last.unwrap_or_else(|| reason.clone()), theme::FAILED)
        }
        _ => match preview.lines.last() {
            Some(line) => (line.clone(), Color32::from_rgb(0xb4, 0xba, 0xc4)),
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

/// Pergunta de sim/não na tela de um agente parado. Vale também para o
/// orquestrador que espera os filhos: a pergunta na tela é com você.
fn pending_choice<'a>(agent: &AgentView, preview: &'a Preview) -> Option<&'a Choice> {
    preview
        .choice
        .as_ref()
        .filter(|_| agent.status == AgentStatus::WaitingForInput)
}

/// Estado mostrado no card: o do agente, ou "aguardando você" se a tela pergunta.
fn shown_status(agent: &AgentView, asking: bool) -> (StatusKind, String) {
    if asking {
        (StatusKind::Waiting, StatusKind::Waiting.label().to_owned())
    } else {
        (agent.kind(), agent.status_text())
    }
}

fn card_header(
    painter: &egui::Painter,
    rect: Rect,
    (kind, label): (StatusKind, String),
    title: &str,
    star: bool,
) {
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
        label,
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
    let preview = cx.shared.preview(agent.id);
    let choice = pending_choice(agent, &preview);
    let shown = shown_status(agent, choice.is_some());
    card_frame(&painter, rect, shown.0, st.selected == Some(agent.id));
    card_header(&painter, rect, shown, &agent.config.name, false);

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

    let (line, color) = context_line(agent, &preview);
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
        let before = &preview.lines[..preview.lines.len().saturating_sub(1)];
        let lines = &before[before.len().saturating_sub(4)..];
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
            reply_row(ui, cx, st, agent, choice, pos2(rect.left() + 12.0, y));
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

/// Sim/Não só quando a tela pergunta (pedido de permissão, confirmação);
/// um agente que só terminou o turno ganha apenas o atalho para o terminal.
fn reply_row(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    agent: &AgentView,
    choice: Option<&Choice>,
    at: Pos2,
) {
    let mut x = at.x;
    if let Some(choice) = choice {
        choice_buttons(ui, cx, agent, choice, at);
        x += 144.0;
    }
    let open = Rect::from_min_size(pos2(x, at.y), vec2(110.0, 24.0));
    if ui
        .put(
            open,
            egui::Button::new(RichText::new("Abrir terminal").size(12.5)),
        )
        .clicked()
    {
        st.select(agent.id);
    }
}

/// Botões Sim (x..x+64) e Não (x+72..x+136) que mandam as teclas da pergunta.
fn choice_buttons(ui: &mut Ui, cx: &Ctx, agent: &AgentView, choice: &Choice, at: Pos2) {
    let size = vec2(64.0, 24.0);
    let yes = Rect::from_min_size(at, size);
    let no = yes.translate(vec2(72.0, 0.0));
    if ui
        .put(yes, widgets::tinted_button("Sim", theme::DONE))
        .clicked()
    {
        cx.send(Command::SendInput {
            id: agent.id,
            data: choice.yes.clone(),
        });
    }
    if ui
        .put(no, widgets::tinted_button("Não", theme::FAILED))
        .clicked()
    {
        cx.send(Command::SendInput {
            id: agent.id,
            data: choice.no.clone(),
        });
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
    let preview = cx.shared.preview(agent.id);
    let choice = pending_choice(agent, &preview);
    let shown = shown_status(agent, choice.is_some());
    card_frame(&painter, rect, shown.0, st.selected == Some(agent.id));
    card_header(&painter, rect, shown, &agent.config.name, true);
    let x = rect.left() + 16.0;
    let w = rect.width() - 32.0;
    painter.text(
        pos2(x, rect.top() + 47.0),
        Align2::LEFT_CENTER,
        kind_label(agent.config.kind),
        FontId::proportional(12.0),
        theme::TEXT_DIM,
    );

    // A tarefa (a primeira mensagem) em até duas linhas.
    painter.text(
        pos2(x, rect.top() + 66.0),
        Align2::LEFT_CENTER,
        "Tarefa",
        FontId::proportional(11.0),
        theme::TEXT_DIM,
    );
    let (task, color) = if session.config.goal.trim().is_empty() {
        (
            "sem tarefa: fale com ele abaixo".to_owned(),
            theme::TEXT_DIM,
        )
    } else {
        (session.config.goal.clone(), theme::TEXT)
    };
    let mut job = egui::text::LayoutJob::simple(task, FontId::proportional(13.0), color, w);
    job.wrap.max_rows = 2;
    job.wrap.overflow_character = Some('…');
    let galley = painter.layout_job(job);
    painter.galley(pos2(x, rect.top() + 76.0), galley, color);

    // Progresso.
    let (done, total) = cx.model.progress(agent.id);
    let py = rect.top() + 124.0;
    let label = if total == 0 {
        "ainda não criou agentes".to_owned()
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

    let (line, color) = context_line(agent, &preview);
    paint_elided(
        &painter,
        pos2(x, rect.top() + 146.0),
        Align2::LEFT_CENTER,
        &line,
        FontId::monospace(11.5),
        color,
        w,
    );

    // Pergunta na tela (permissão, confiança na pasta): Sim/Não no lugar do campo.
    if let Some(choice) = choice {
        reply_row(ui, cx, st, agent, Some(choice), pos2(x, rect.top() + 166.0));
        return;
    }

    // Campo de conversa: texto + Enter vai para o PTY do agente.
    let field = Rect::from_min_size(pos2(x, rect.top() + 164.0), vec2(w - 38.0, 28.0));
    let send = Rect::from_min_size(pos2(field.right() + 6.0, field.top()), vec2(32.0, 28.0));
    let text = st.chat.entry(agent.id).or_default();
    let edit = ui.put(
        field,
        TextEdit::singleline(text)
            .hint_text("Fale com o agente…")
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
