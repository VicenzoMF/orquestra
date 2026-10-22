//! Painéis que flutuam sobre o canvas, fixos na tela: o botão
//! "Sessões" (abre a lista, uma sessão de cada vez), "Precisa de
//! você" no canto de cima e os recursos, compactos, no de baixo.

use egui::{
    Align, Align2, Area, Color32, CornerRadius, FontId, Frame, Id, Layout, Margin, Order, Pos2,
    Rect, Response, RichText, ScrollArea, Sense, Shadow, Stroke, StrokeKind, Ui, UiBuilder, pos2,
    vec2,
};
use orquestra_core::{AgentStatus, Command};

use super::canvas::CanvasState;
use super::dialogs::{Confirm, Dialogs};
use super::widgets::{self, paint_elided};
use super::{Ctx, Resources, UiState};
use crate::model::{Attention, AttentionKind, SessionView, StatusKind, plural};
use crate::theme;

/// Distância dos painéis à borda do canvas.
const EDGE: f32 = 12.0;
const BUTTON_H: f32 = 30.0;
const CHIP_H: f32 = 28.0;
/// Faixas do canvas que os painéis cobrem (o enquadramento as evita).
pub const INSET_TOP: f32 = EDGE + BUTTON_H + 8.0;
pub const INSET_BOTTOM: f32 = EDGE + CHIP_H + 8.0;
const MENU_W: f32 = 300.0;
const MENU_CARD_H: f32 = 98.0;
const MENU_GAP: f32 = 8.0;
/// Cada card da lista entra este tempo depois do anterior...
const STAGGER: f64 = 0.06;
/// ...e leva este para aparecer.
const APPEAR: f64 = 0.2;
const ATTENTION_W: f32 = 340.0;

/// A lista "Sessões": aberta ou não, e desde quando (para a animação).
#[derive(Clone, Copy, Debug, Default)]
pub struct Menu {
    pub open: bool,
    since: f64,
}

impl Menu {
    pub fn set(&mut self, open: bool, now: f64) {
        if self.open != open {
            self.open = open;
            self.since = now;
        }
    }

    /// Quanto o card `i` de `n` já apareceu (0 a 1). Abrindo, de cima para
    /// baixo; fechando, de baixo para cima.
    fn progress(&self, now: f64, i: usize, n: usize) -> f32 {
        let order = if self.open { i } else { n - 1 - i };
        let t = ((now - self.since - order as f64 * STAGGER) / APPEAR).clamp(0.0, 1.0) as f32;
        let eased = 1.0 - (1.0 - t).powi(3);
        if self.open { eased } else { 1.0 - eased }
    }

    fn animating(&self, now: f64, n: usize) -> bool {
        now - self.since < APPEAR + n as f64 * STAGGER
    }
}

pub fn show(
    ui: &Ui,
    cx: &Ctx,
    st: &mut UiState,
    cs: &mut CanvasState,
    res: &mut Resources,
    dialogs: &mut Dialogs,
    canvas: Rect,
) {
    let ctx = ui.ctx().clone();
    orchestrations(&ctx, cx, st, cs, dialogs, canvas);
    attention(&ctx, cx, st, dialogs, canvas);
    resources(&ctx, cx, res, canvas);
}

fn shadow() -> Shadow {
    Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(90),
    }
}

fn floating() -> Frame {
    Frame::new()
        .fill(theme::PANEL)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(CornerRadius::same(10))
        .shadow(shadow())
}

fn area(id: &str, pivot: Align2, pos: Pos2) -> Area {
    Area::new(Id::unique(id))
        .order(Order::Middle)
        .pivot(pivot)
        .fixed_pos(pos)
        .constrain(false)
}

// --- Sessões --------------------------------------------------------------

fn orchestrations(
    ctx: &egui::Context,
    cx: &Ctx,
    st: &mut UiState,
    cs: &mut CanvasState,
    dialogs: &mut Dialogs,
    canvas: Rect,
) {
    let now = ctx.input(|i| i.time);
    let sessions = cx.model.sessions();
    if sessions.is_empty() {
        st.menu.set(false, now);
        return;
    }
    let origin = canvas.left_top() + vec2(EDGE, EDGE);
    let alert = sessions
        .iter()
        .any(|s| cx.model.counts(Some(s.id)).attention() > 0);
    area("orq-menu-button", Align2::LEFT_TOP, origin).show(ctx, |ui| {
        if menu_button(ui, st.menu.open, sessions.len(), alert).clicked() {
            st.menu.set(!st.menu.open, now);
        }
    });

    let n = sessions.len();
    let animating = st.menu.animating(now, n);
    if !st.menu.open && !animating {
        return;
    }
    if animating {
        ctx.request_repaint();
    }
    // Sem rolagem: cabem as que cabem, e o resto vira uma linha.
    let top = origin.y + BUTTON_H + MENU_GAP;
    let room = canvas.bottom() - EDGE - top;
    let fits = ((room + MENU_GAP) / (MENU_CARD_H + MENU_GAP)).floor() as usize;
    let shown = if n > fits {
        fits.saturating_sub(1).max(1)
    } else {
        n
    };
    area("orq-menu", Align2::LEFT_TOP, pos2(origin.x, top)).show(ctx, |ui| {
        ui.spacing_mut().item_spacing.y = MENU_GAP;
        for (i, session) in sessions.iter().take(shown).enumerate() {
            let (slot, _) = ui.allocate_exact_size(vec2(MENU_W, MENU_CARD_H), Sense::hover());
            let p = st.menu.progress(now, i, n);
            if p <= 0.0 {
                continue;
            }
            // Desce um pouco enquanto aparece.
            let rect = slot.translate(vec2(0.0, (p - 1.0) * 14.0));
            let mut card = ui.new_child(UiBuilder::new().max_rect(rect));
            card.multiply_opacity(p);
            session_card(&mut card, cx, st, cs, dialogs, session, rect, now);
        }
        // O rodapé entra junto com o último card.
        let p = st.menu.progress(now, shown - 1, n);
        if p > 0.0 {
            ui.scope(|ui| {
                ui.multiply_opacity(p);
                let rest = n - shown;
                if rest > 0 {
                    hint(
                        ui,
                        &format!("+{rest} que não cabem aqui: veja as abas no alto"),
                    );
                }
                hint(
                    ui,
                    "Botão direito no canvas ou Ctrl+Shift+N: novo orquestrador\nCtrl+Shift+↑/↓ agente · Ctrl+Shift+B navegador",
                );
            });
        }
    });
}

/// Linha discreta do tamanho dos cards.
fn hint(ui: &mut Ui, text: &str) {
    Frame::new()
        .fill(theme::PANEL.gamma_multiply(0.92))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.set_width(MENU_W - 20.0);
            ui.label(RichText::new(text).size(11.5).color(theme::TEXT_DIM));
        });
}

/// Botão "≡ Sessões (n)". O badge âmbar avisa que alguma pede você.
fn menu_button(ui: &mut Ui, open: bool, count: usize, alert: bool) -> Response {
    let font = FontId::proportional(13.0);
    let label = ui
        .painter()
        .layout_no_wrap("Sessões".to_owned(), font, theme::TEXT);
    let badge =
        ui.painter()
            .layout_no_wrap(count.to_string(), FontId::proportional(11.5), theme::TEXT);
    let badge_w = (badge.size().x + 12.0).max(20.0);
    let width = 14.0 + 14.0 + 8.0 + label.size().x + 8.0 + badge_w + 10.0 + 10.0 + 12.0;
    let (rect, response) = ui.allocate_exact_size(vec2(width, BUTTON_H), Sense::click());
    let painter = ui.painter();
    let hovered = response.hovered();
    painter.add(shadow().as_shape(rect, CornerRadius::same(255)));
    let fill = if open {
        Color32::from_rgb(0x2a, 0x33, 0x40)
    } else if hovered {
        Color32::from_rgb(0x25, 0x2a, 0x32)
    } else {
        theme::PANEL
    };
    let stroke = if open {
        theme::ACCENT.gamma_multiply(0.7)
    } else {
        theme::BORDER
    };
    painter.rect(
        rect,
        CornerRadius::same(255),
        fill,
        Stroke::new(1.0, stroke),
        StrokeKind::Inside,
    );
    let cy = rect.center().y;
    let mut x = rect.left() + 14.0;
    // Três traços: a lista.
    for dy in [-4.0, 0.0, 4.0] {
        painter.line_segment(
            [pos2(x, cy + dy), pos2(x + 12.0, cy + dy)],
            Stroke::new(1.6, theme::TEXT_DIM),
        );
    }
    x += 14.0 + 8.0;
    let label_w = label.size().x;
    painter.galley(pos2(x, cy - label.size().y / 2.0), label, theme::TEXT);
    x += label_w + 8.0;
    let pill = Rect::from_min_size(pos2(x, cy - 9.0), vec2(badge_w, 18.0));
    let (pill_fill, text) = if alert {
        (theme::WAITING.gamma_multiply(0.22), theme::WAITING)
    } else {
        (theme::BORDER, theme::TEXT_DIM)
    };
    painter.rect_filled(pill, CornerRadius::same(255), pill_fill);
    painter.galley(pill.center() - badge.size() / 2.0, badge, text);
    x += badge_w + 10.0;
    chevron(painter, pos2(x + 5.0, cy), open, theme::TEXT_DIM);
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(if open {
            "Fechar a lista"
        } else {
            "Ver as sessões (orquestrações e agentes)"
        })
}

/// Seta ∨ (fechado) ou ∧ (aberto), centrada em `c`.
fn chevron(painter: &egui::Painter, c: Pos2, up: bool, color: Color32) {
    let dy = if up { -2.0 } else { 2.0 };
    painter.line(
        vec![c + vec2(-4.0, -dy), c + vec2(0.0, dy), c + vec2(4.0, -dy)],
        Stroke::new(1.6, color),
    );
}

#[allow(clippy::too_many_arguments)]
fn session_card(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    cs: &mut CanvasState,
    dialogs: &mut Dialogs,
    session: &SessionView,
    rect: Rect,
    now: f64,
) {
    let focused = st.focus == Some(session.id);
    let background = ui
        .interact(rect, Id::unique(("orq-card", session.id)), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Clique para focar no canvas");
    let painter = ui.painter().clone();
    painter.add(shadow().as_shape(rect, CornerRadius::same(10)));
    let stroke = if focused {
        theme::ACCENT.gamma_multiply(0.7)
    } else if background.hovered() {
        Color32::from_rgb(0x3d, 0x44, 0x4f)
    } else {
        theme::BORDER
    };
    painter.rect(
        rect,
        CornerRadius::same(10),
        theme::PANEL,
        Stroke::new(1.0, stroke),
        StrokeKind::Inside,
    );

    let (summary, kind) = cx.model.session_summary(session.id);
    let c = cx.model.counts(Some(session.id));
    let x = rect.left() + 14.0;
    let right = rect.right() - 14.0;
    let y1 = rect.top() + 19.0;
    theme::paint_status_icon(
        &painter,
        pos2(x + 4.0, y1),
        4.5,
        kind.unwrap_or(StatusKind::Done),
    );
    paint_elided(
        &painter,
        pos2(x + 16.0, y1),
        Align2::LEFT_CENTER,
        &session.config.name,
        FontId::proportional(14.0),
        theme::TEXT,
        right - x - 16.0,
    );

    let y2 = rect.top() + 41.0;
    let progress = session.orchestrator.map(|o| cx.model.progress(o));
    let ready = progress.map(|(done, total)| {
        painter.text(
            pos2(right, y2),
            Align2::RIGHT_CENTER,
            format!("{done} de {total} prontas"),
            FontId::proportional(12.0),
            theme::TEXT_DIM,
        )
    });
    let agents = painter.text(
        pos2(x, y2),
        Align2::LEFT_CENTER,
        format!("{} · ", plural(c.workers, "agente", "agentes")),
        FontId::proportional(12.0),
        theme::TEXT_DIM,
    );
    paint_elided(
        &painter,
        pos2(agents.right(), y2),
        Align2::LEFT_CENTER,
        &summary,
        FontId::proportional(12.0),
        kind.map_or(theme::TEXT_DIM, theme::status_color),
        ready.map_or(right, |r| r.left() - 8.0) - agents.right(),
    );
    let bar = Rect::from_min_size(pos2(x, rect.top() + 54.0), vec2(right - x, 4.0));
    painter.rect_filled(bar, CornerRadius::same(2), theme::BORDER);
    if let Some((done, total)) = progress.filter(|(_, total)| *total > 0) {
        let mut fill = bar;
        fill.set_width(bar.width() * done as f32 / total as f32);
        painter.rect_filled(fill, CornerRadius::same(2), theme::DONE);
    }

    // Botões: [Focar][Orquestrador] ... [Encerrar…].
    let button = |from: f32, width: f32| {
        Rect::from_min_size(pos2(from, rect.top() + 66.0), vec2(width, 24.0))
    };
    let small = |text: &str| egui::Button::new(RichText::new(text).size(12.5));
    let focus_label = if focused { "Ver todas" } else { "Focar" };
    let mut close = false;
    if ui.put(button(x, 72.0), small(focus_label)).clicked() || background.clicked() {
        st.focus = if focused { None } else { Some(session.id) };
        cs.request_fit();
        close = true;
    }
    if let Some(orq) = session.orchestrator
        && ui
            .put(button(x + 78.0, 100.0), small("Orquestrador"))
            .on_hover_text("Abre o terminal do orquestrador")
            .clicked()
    {
        st.select(orq);
        close = true;
    }
    let end = button(right - 84.0, 84.0);
    if ui
        .put(end, widgets::tinted_button("Encerrar…", theme::FAILED))
        .clicked()
    {
        dialogs.confirm = Some(Confirm::close_session(session.id));
    }
    if close {
        st.menu.set(false, now);
    }
}

// --- Precisa de você -------------------------------------------------------

/// A fila do modelo, mais o orquestrador que espera os filhos mas tem uma
/// pergunta de sim/não na tela (o modelo não vê a tela).
fn attention_items(cx: &Ctx) -> Vec<Attention> {
    let mut items = Vec::new();
    for session in cx.model.sessions() {
        if let Some(orq) = session.orchestrator.and_then(|o| cx.model.agent(o))
            && orq.watching
            && cx.shared.preview(orq.id).choice.is_some()
        {
            items.push(Attention {
                agent: orq.id,
                session: session.id,
                kind: AttentionKind::Input,
            });
        }
    }
    items.extend(cx.model.attention());
    items
}

fn attention(ctx: &egui::Context, cx: &Ctx, st: &mut UiState, dialogs: &mut Dialogs, canvas: Rect) {
    let items = attention_items(cx);
    if items.is_empty() {
        return;
    }
    let pos = canvas.right_top() + vec2(-EDGE, EDGE);
    area("attention", Align2::RIGHT_TOP, pos).show(ctx, |ui| {
        floating()
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_width(ATTENTION_W);
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(vec2(10.0, 16.0), Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 4.0, theme::WAITING);
                    ui.label(
                        RichText::new("Precisa de você")
                            .size(13.5)
                            .strong()
                            .color(theme::TEXT),
                    );
                    ui.label(
                        RichText::new(items.len().to_string())
                            .size(13.0)
                            .color(theme::TEXT_DIM),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (rect, response) =
                            ui.allocate_exact_size(vec2(22.0, 20.0), Sense::click());
                        let color = if response.hovered() {
                            theme::TEXT
                        } else {
                            theme::TEXT_DIM
                        };
                        chevron(ui.painter(), rect.center(), !st.attention_collapsed, color);
                        let tip = if st.attention_collapsed {
                            "Mostrar"
                        } else {
                            "Recolher"
                        };
                        if response.on_hover_text(tip).clicked() {
                            st.attention_collapsed = !st.attention_collapsed;
                        }
                    });
                });
                if st.attention_collapsed {
                    return;
                }
                ui.add_space(2.0);
                // Numa `Area`, o espaço disponível é o tamanho do quadro
                // anterior: sem o tamanho explícito a lista nunca cresceria.
                let max = (canvas.height() * 0.6).max(160.0);
                ui.allocate_ui(vec2(ui.available_width(), max), |ui| {
                    ScrollArea::vertical()
                        .max_height(max)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for item in &items {
                                attention_card(ui, cx, st, dialogs, item);
                                ui.add_space(2.0);
                            }
                        });
                });
            });
    });
}

fn attention_card(
    ui: &mut Ui,
    cx: &Ctx,
    st: &mut UiState,
    dialogs: &mut Dialogs,
    item: &Attention,
) {
    let (Some(agent), Some(session)) = (cx.model.agent(item.agent), cx.model.session(item.session))
    else {
        return;
    };
    Frame::new()
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let kind = match item.kind {
                AttentionKind::Input => StatusKind::Waiting,
                AttentionKind::Approval => StatusKind::Approval,
                AttentionKind::Failure => StatusKind::Failed,
            };
            ui.horizontal(|ui| {
                widgets::status_icon(ui, kind, 14.0);
                let name = if agent.is_orchestrator() {
                    "orquestrador"
                } else {
                    agent.config.name.as_str()
                };
                ui.label(RichText::new(name).strong().color(theme::TEXT));
                let painter = ui.painter().clone();
                let font = FontId::proportional(12.0);
                let max = ui.available_width();
                let text = widgets::elide(&painter, &session.config.name, &font, max);
                ui.label(RichText::new(text).font(font).color(theme::TEXT_DIM));
            });
            let preview = cx.shared.preview(agent.id);
            let detail = match item.kind {
                // Numa pergunta, as linhas de cima dizem o que está em jogo
                // (o comando, o arquivo); senão basta a última.
                AttentionKind::Input => {
                    let n = if preview.choice.is_some() { 3 } else { 1 };
                    let lines = &preview.lines[preview.lines.len().saturating_sub(n)..];
                    if lines.is_empty() {
                        "Aguardando sua resposta".to_owned()
                    } else {
                        lines.join("\n")
                    }
                }
                AttentionKind::Approval => format!(
                    "Quer criar este agente: {}",
                    agent.config.initial_prompt.as_deref().unwrap_or("")
                ),
                AttentionKind::Failure => match &agent.status {
                    AgentStatus::Failed { reason } => reason.clone(),
                    _ => String::new(),
                },
            };
            let color = match item.kind {
                AttentionKind::Input => Color32::from_rgb(0xc9, 0xcd, 0xd4),
                AttentionKind::Approval => theme::APPROVAL,
                AttentionKind::Failure => theme::FAILED,
            };
            ui.label(RichText::new(detail).monospace().size(11.5).color(color));
            ui.horizontal(|ui| match item.kind {
                AttentionKind::Input => {
                    // Sim/Não só quando a tela pergunta; senão, o terminal.
                    if let Some(choice) = &preview.choice {
                        if ui.add(widgets::tinted_button("Sim", theme::DONE)).clicked() {
                            cx.send(Command::SendInput {
                                id: agent.id,
                                data: choice.yes.clone(),
                            });
                        }
                        if ui
                            .add(widgets::tinted_button("Não", theme::FAILED))
                            .clicked()
                        {
                            cx.send(Command::SendInput {
                                id: agent.id,
                                data: choice.no.clone(),
                            });
                        }
                    }
                    if ui.button("Abrir terminal").clicked() {
                        st.select(agent.id);
                    }
                }
                AttentionKind::Approval => {
                    if ui
                        .add(widgets::tinted_button("Aprovar", theme::DONE))
                        .clicked()
                    {
                        cx.send(Command::ApproveSpawn { id: agent.id });
                    }
                    if ui
                        .add(widgets::tinted_button("Recusar", theme::FAILED))
                        .clicked()
                    {
                        cx.send(Command::RejectSpawn { id: agent.id });
                    }
                }
                AttentionKind::Failure => {
                    if ui.button("Abrir terminal").clicked() {
                        st.select(agent.id);
                    }
                    if ui.button("Remover…").clicked() {
                        dialogs.confirm = Some(Confirm::remove(agent.id));
                    }
                }
            });
        });
}

// --- Recursos --------------------------------------------------------------

fn resources(ctx: &egui::Context, cx: &Ctx, res: &mut Resources, canvas: Rect) {
    let used = cx.model.slots_used();
    let max = cx.model.max_workers.max(1);
    let queued = cx.model.counts(None).queued;
    let browsers = cx.model.browsers_open();
    let ram = res.ram.label();
    let pos = canvas.right_bottom() + vec2(-EDGE, -EDGE);
    area("resources", Align2::RIGHT_BOTTOM, pos).show(ctx, |ui| {
        let response = floating()
            .corner_radius(CornerRadius::same(255))
            .inner_margin(Margin::symmetric(12, 5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let small =
                        |text: String, color: Color32| RichText::new(text).size(12.0).color(color);
                    ui.label(small("vagas".to_owned(), theme::TEXT_DIM));
                    slots(ui, used, max);
                    ui.label(small(format!("{used}/{max}"), theme::TEXT));
                    if queued > 0 {
                        ui.label(small(format!("+{queued} na fila"), theme::IDLE));
                    }
                    ui.label(small("·".to_owned(), theme::TEXT_DIM));
                    ui.label(small(
                        plural(browsers, "navegador", "navegadores"),
                        theme::TEXT_DIM,
                    ));
                    ui.label(small("·".to_owned(), theme::TEXT_DIM));
                    ui.label(small(ram.clone(), theme::TEXT_DIM));
                });
            })
            .response;
        response.on_hover_text(format!(
            "Recursos\n{used} de {max} vagas de agentes em uso{}\n{} aberto(s)\nRAM do app: {ram}",
            if queued > 0 {
                format!(", {queued} na fila")
            } else {
                String::new()
            },
            plural(browsers, "navegador", "navegadores"),
        ));
    });
}

/// Uma casinha por vaga (ou uma barra, se forem muitas).
fn slots(ui: &mut Ui, used: u32, max: u32) {
    let full = used >= max;
    let on = if full { theme::WAITING } else { theme::ACCENT };
    if max > 16 {
        let (rect, _) = ui.allocate_exact_size(vec2(64.0, 14.0), Sense::hover());
        let bar = Rect::from_center_size(rect.center(), vec2(rect.width(), 5.0));
        ui.painter()
            .rect_filled(bar, CornerRadius::same(3), theme::BORDER);
        let mut fill = bar;
        fill.set_width(bar.width() * (used as f32 / max as f32).min(1.0));
        ui.painter().rect_filled(fill, CornerRadius::same(3), on);
        return;
    }
    let (w, gap) = (5.0, 2.0);
    let width = max as f32 * (w + gap) - gap;
    let (rect, _) = ui.allocate_exact_size(vec2(width, 14.0), Sense::hover());
    for i in 0..max {
        let cell = Rect::from_min_size(
            pos2(rect.left() + i as f32 * (w + gap), rect.center().y - 5.0),
            vec2(w, 10.0),
        );
        let color = if i < used { on } else { theme::BORDER };
        ui.painter().rect_filled(cell, CornerRadius::same(1), color);
    }
}
