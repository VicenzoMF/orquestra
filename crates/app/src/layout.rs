//! Layout do canvas. Função pura: recebe, por sessão, onde a moldura fica e
//! os nós com o tamanho de cada card; devolve molduras, retângulos (em árvore
//! dentro de cada moldura) e arestas ortogonais em coordenadas do canvas.
//!
//! Molduras nunca se sobrepõem: a que cruza outra mais acima desce só na
//! exibição, e volta quando a de cima encolhe.

use std::collections::HashMap;

use egui::{Pos2, Rect, Vec2, pos2, vec2};
use orquestra_core::{AgentId, SessionId};

/// Margem interna da moldura.
pub const FRAME_PAD: f32 = 20.0;
/// Altura do cabeçalho da moldura (nome, repositório, resumo).
pub const HEADER_H: f32 = 48.0;
/// Espaço horizontal entre colunas (onde passam tronco e ramos).
pub const COL_GAP: f32 = 150.0;
/// Espaço vertical entre cards de uma coluna.
pub const ROW_GAP: f32 = 14.0;
pub const SESSION_GAP: f32 = 36.0;
pub const MIN_FRAME_W: f32 = 760.0;
/// Altura do conteúdo de uma sessão sem agentes.
pub const EMPTY_H: f32 = 70.0;
/// Distância do topo do card até o ponto onde a aresta encosta.
pub const ANCHOR_Y: f32 = 24.0;
/// Distância da borda direita do pai até o tronco vertical.
pub const TRUNK_DX: f32 = 34.0;

#[derive(Clone, Debug)]
pub struct NodeSpec {
    pub id: AgentId,
    pub parent: Option<AgentId>,
    pub orchestrator: bool,
    pub size: Vec2,
}

#[derive(Clone, Debug)]
pub struct SessionSpec {
    pub id: SessionId,
    /// Canto superior esquerdo da moldura; `None` põe a sessão em x = 0, logo
    /// abaixo da moldura mais baixa.
    pub origin: Option<Pos2>,
    /// Em ordem de criação.
    pub nodes: Vec<NodeSpec>,
}

/// Aresta pai → filho: `points` é a polilinha ortogonal (saída do pai,
/// tronco, ramo até o filho). A seta fica no último ponto.
#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    pub parent: AgentId,
    pub child: AgentId,
    pub points: [Pos2; 4],
    /// Início do ramo do filho (onde o rótulo começa).
    pub label_at: Pos2,
    /// Largura disponível para o rótulo no ramo.
    pub label_width: f32,
}

#[derive(Clone, Debug)]
pub struct SessionLayout {
    pub id: SessionId,
    pub frame: Rect,
    pub nodes: Vec<(AgentId, Rect)>,
    pub edges: Vec<Edge>,
}

impl SessionLayout {
    pub fn node(&self, id: AgentId) -> Option<Rect> {
        self.nodes.iter().find(|(a, _)| *a == id).map(|(_, r)| *r)
    }

    fn translate(&mut self, delta: Vec2) {
        self.frame = self.frame.translate(delta);
        for (_, rect) in &mut self.nodes {
            *rect = rect.translate(delta);
        }
        for edge in &mut self.edges {
            for p in &mut edge.points {
                *p += delta;
            }
            edge.label_at += delta;
        }
    }
}

#[derive(Clone, Debug)]
pub struct CanvasLayout {
    pub sessions: Vec<SessionLayout>,
    /// União das molduras (para "Ajustar à tela").
    pub bounds: Rect,
}

impl CanvasLayout {
    pub fn session(&self, id: SessionId) -> Option<&SessionLayout> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn node(&self, id: AgentId) -> Option<Rect> {
        self.sessions.iter().find_map(|s| s.node(id))
    }
}

/// Cada sessão na sua origem, com sua árvore. Em `sessions` fica a ordem
/// recebida.
///
/// As de origem conhecida entram de cima para baixo (y, depois x): uma
/// moldura que cruza outra já posta, com as faixas de x se cruzando, desce até
/// `SESSION_GAP` abaixo dela. As sem origem vêm depois, em x = 0, cada uma
/// abaixo da moldura mais baixa.
pub fn layout(sessions: &[SessionSpec]) -> CanvasLayout {
    let mut order: Vec<usize> = (0..sessions.len())
        .filter(|&i| sessions[i].origin.is_some())
        .collect();
    let origin = |i: usize| sessions[i].origin.unwrap_or_default();
    order.sort_by(|&a, &b| {
        let (pa, pb) = (origin(a), origin(b));
        pa.y.total_cmp(&pb.y)
            .then(pa.x.total_cmp(&pb.x))
            .then(a.cmp(&b))
    });

    let mut out: Vec<Option<SessionLayout>> = vec![None; sessions.len()];
    let mut frames: Vec<Rect> = Vec::with_capacity(sessions.len());
    for i in order {
        let mut session = layout_session(&sessions[i], origin(i));
        // Desce até não cruzar nenhuma: cada volta passa de vez uma moldura.
        while let Some(bottom) = frames
            .iter()
            .filter(|f| crosses(**f, session.frame))
            .map(|f| f.bottom())
            .max_by(f32::total_cmp)
        {
            let dy = bottom + SESSION_GAP - session.frame.top();
            session.translate(vec2(0.0, dy));
        }
        frames.push(session.frame);
        out[i] = Some(session);
    }

    let mut bottom = frames.iter().map(|f| f.bottom()).max_by(f32::total_cmp);
    for (i, spec) in sessions.iter().enumerate() {
        if spec.origin.is_none() {
            let y = bottom.map_or(0.0, |b| b + SESSION_GAP);
            let session = layout_session(spec, pos2(0.0, y));
            bottom = Some(session.frame.bottom());
            frames.push(session.frame);
            out[i] = Some(session);
        }
    }

    let bounds = frames
        .iter()
        .copied()
        .reduce(Rect::union)
        .unwrap_or_else(|| Rect::from_min_size(Pos2::ZERO, vec2(MIN_FRAME_W, 200.0)));
    CanvasLayout {
        sessions: out.into_iter().flatten().collect(),
        bounds,
    }
}

/// `b` encosta em `a`: as faixas de x se cruzam e as de y ficam a menos de
/// `SESSION_GAP` uma da outra.
fn crosses(a: Rect, b: Rect) -> bool {
    a.left() < b.right()
        && b.left() < a.right()
        && b.top() < a.bottom() + SESSION_GAP
        && a.top() < b.bottom() + SESSION_GAP
}

fn layout_session(spec: &SessionSpec, origin: Pos2) -> SessionLayout {
    let index: HashMap<AgentId, usize> = spec
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id, i))
        .collect();
    // Pai fora da sessão (removido) = raiz sem seta.
    let parent_of = |n: &NodeSpec| n.parent.filter(|p| index.contains_key(p) && *p != n.id);
    let mut children: HashMap<AgentId, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (i, node) in spec.nodes.iter().enumerate() {
        match parent_of(node) {
            Some(p) => children.entry(p).or_default().push(i),
            None => roots.push(i),
        }
    }
    // Orquestrador primeiro; avulsos depois, na ordem de criação.
    roots.sort_by_key(|&i| (!spec.nodes[i].orchestrator, i));
    let has_orchestrator = roots.first().is_some_and(|&i| spec.nodes[i].orchestrator);

    // Coluna de cada nó: o orquestrador fica na 0; raízes avulsas ficam na
    // coluna dos workers quando há orquestrador.
    let mut column = vec![0usize; spec.nodes.len()];
    let mut visited = vec![false; spec.nodes.len()];
    let mut stack: Vec<(usize, usize)> = roots
        .iter()
        .map(|&i| {
            let base = usize::from(has_orchestrator && !spec.nodes[i].orchestrator);
            (i, base)
        })
        .collect();
    while let Some((i, col)) = stack.pop() {
        if std::mem::replace(&mut visited[i], true) {
            continue;
        }
        column[i] = col;
        for &c in children.get(&spec.nodes[i].id).into_iter().flatten() {
            stack.push((c, col + 1));
        }
    }
    // Nós em ciclo (não deveria acontecer) viram raízes na coluna 0.
    for (i, seen) in visited.iter().enumerate() {
        if !seen {
            roots.push(i);
        }
    }

    let columns = column.iter().copied().max().map_or(0, |m| m + 1);
    let mut col_w = vec![0.0f32; columns];
    for (i, node) in spec.nodes.iter().enumerate() {
        col_w[column[i]] = col_w[column[i]].max(node.size.x);
    }
    let content_min = origin + vec2(FRAME_PAD, HEADER_H);
    let mut col_x = Vec::with_capacity(columns);
    let mut x = content_min.x;
    for w in &col_w {
        col_x.push(x);
        x += w + COL_GAP;
    }

    // Colocação vertical: cada nó começa na altura pedida pelo pai, sem
    // invadir o que já está na coluna.
    let mut rects: Vec<Option<Rect>> = vec![None; spec.nodes.len()];
    let mut col_y = vec![content_min.y; columns];
    let mut placing = vec![false; spec.nodes.len()];
    for &root in &roots {
        place(
            root,
            content_min.y,
            spec,
            &children,
            &column,
            &col_x,
            &mut col_y,
            &mut rects,
            &mut placing,
        );
    }

    let mut nodes = Vec::with_capacity(spec.nodes.len());
    let mut content = Rect::NOTHING;
    for (i, node) in spec.nodes.iter().enumerate() {
        if let Some(rect) = rects[i] {
            content = content.union(rect);
            nodes.push((node.id, rect));
        }
    }

    let mut edges = Vec::new();
    for (i, node) in spec.nodes.iter().enumerate() {
        let (Some(parent), Some(child_rect)) = (parent_of(node), rects[i]) else {
            continue;
        };
        let Some(parent_rect) = rects[index[&parent]] else {
            continue;
        };
        let start = pos2(parent_rect.right(), parent_rect.top() + ANCHOR_Y);
        let trunk_x = parent_rect.right() + TRUNK_DX;
        let end = pos2(child_rect.left(), child_rect.top() + ANCHOR_Y);
        edges.push(Edge {
            parent,
            child: node.id,
            points: [start, pos2(trunk_x, start.y), pos2(trunk_x, end.y), end],
            label_at: pos2(trunk_x + 8.0, end.y),
            label_width: (end.x - trunk_x - 22.0).max(0.0),
        });
    }

    let (content_w, content_h) = if content.is_positive() {
        (content.right() - origin.x - FRAME_PAD, content.height())
    } else {
        (0.0, EMPTY_H)
    };
    let width = (content_w + 2.0 * FRAME_PAD).max(MIN_FRAME_W);
    let height = HEADER_H + content_h + FRAME_PAD;
    SessionLayout {
        id: spec.id,
        frame: Rect::from_min_size(origin, vec2(width, height)),
        nodes,
        edges,
    }
}

#[allow(clippy::too_many_arguments)]
fn place(
    i: usize,
    want_y: f32,
    spec: &SessionSpec,
    children: &HashMap<AgentId, Vec<usize>>,
    column: &[usize],
    col_x: &[f32],
    col_y: &mut [f32],
    rects: &mut [Option<Rect>],
    placing: &mut [bool],
) {
    if rects[i].is_some() || std::mem::replace(&mut placing[i], true) {
        return;
    }
    let col = column[i];
    let y = want_y.max(col_y[col]);
    let rect = Rect::from_min_size(pos2(col_x[col], y), spec.nodes[i].size);
    rects[i] = Some(rect);
    col_y[col] = rect.bottom() + ROW_GAP;
    for &c in children.get(&spec.nodes[i].id).into_iter().flatten() {
        place(c, y, spec, children, column, col_x, col_y, rects, placing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORQ: Vec2 = vec2(320.0, 220.0);
    const CARD: Vec2 = vec2(300.0, 90.0);

    fn node(id: AgentId, parent: Option<AgentId>, orchestrator: bool) -> NodeSpec {
        NodeSpec {
            id,
            parent,
            orchestrator,
            size: if orchestrator { ORQ } else { CARD },
        }
    }

    fn ids(n: usize) -> Vec<AgentId> {
        (0..n).map(|_| AgentId::generate()).collect()
    }

    #[test]
    fn orchestrator_left_and_workers_in_a_column() {
        let a = ids(4);
        let spec = SessionSpec {
            id: SessionId::generate(),
            origin: None,
            nodes: vec![
                node(a[0], None, true),
                node(a[1], Some(a[0]), false),
                node(a[2], Some(a[0]), false),
                node(a[3], Some(a[0]), false),
            ],
        };
        let l = layout(&[spec]);
        let s = &l.sessions[0];
        let orq = s.node(a[0]).unwrap();
        let workers: Vec<Rect> = a[1..].iter().map(|id| s.node(*id).unwrap()).collect();
        assert_eq!(orq.left(), FRAME_PAD);
        assert_eq!(orq.top(), HEADER_H);
        for (i, w) in workers.iter().enumerate() {
            assert_eq!(w.left(), orq.right() + COL_GAP);
            if i > 0 {
                assert_eq!(w.top(), workers[i - 1].bottom() + ROW_GAP);
            }
        }
        assert_eq!(workers[0].top(), orq.top());
        assert!(s.frame.contains_rect(orq));
        assert!(workers.iter().all(|w| s.frame.contains_rect(*w)));
    }

    #[test]
    fn edges_are_orthogonal_from_parent_to_child() {
        let a = ids(3);
        let spec = SessionSpec {
            id: SessionId::generate(),
            origin: None,
            nodes: vec![
                node(a[0], None, true),
                node(a[1], Some(a[0]), false),
                node(a[2], Some(a[0]), false),
            ],
        };
        let l = layout(&[spec]);
        let s = &l.sessions[0];
        assert_eq!(s.edges.len(), 2);
        let orq = s.node(a[0]).unwrap();
        for edge in &s.edges {
            let child = s.node(edge.child).unwrap();
            let [p0, p1, p2, p3] = edge.points;
            assert_eq!(p0, pos2(orq.right(), orq.top() + ANCHOR_Y));
            assert_eq!(p3, pos2(child.left(), child.top() + ANCHOR_Y));
            // Horizontal, vertical, horizontal.
            assert_eq!(p0.y, p1.y);
            assert_eq!(p1.x, p2.x);
            assert_eq!(p2.y, p3.y);
            assert!(edge.label_width > 50.0);
        }
        // Os ramos compartilham o mesmo tronco.
        assert_eq!(s.edges[0].points[1].x, s.edges[1].points[1].x);
    }

    #[test]
    fn solo_agents_have_no_edge_and_go_below_children() {
        let a = ids(3);
        let spec = SessionSpec {
            id: SessionId::generate(),
            origin: None,
            nodes: vec![
                node(a[0], None, true),
                node(a[1], None, false),
                node(a[2], Some(a[0]), false),
            ],
        };
        let l = layout(&[spec]);
        let s = &l.sessions[0];
        assert_eq!(s.edges.len(), 1);
        assert_eq!(s.edges[0].child, a[2]);
        let child = s.node(a[2]).unwrap();
        let solo = s.node(a[1]).unwrap();
        assert_eq!(solo.left(), child.left());
        assert!(solo.top() >= child.bottom());
    }

    #[test]
    fn session_without_orchestrator_starts_at_column_zero() {
        let a = ids(2);
        let spec = SessionSpec {
            id: SessionId::generate(),
            origin: None,
            nodes: vec![node(a[0], None, false), node(a[1], None, false)],
        };
        let l = layout(&[spec]);
        let s = &l.sessions[0];
        assert_eq!(s.node(a[0]).unwrap().left(), FRAME_PAD);
        assert_eq!(s.node(a[1]).unwrap().left(), FRAME_PAD);
        assert!(s.edges.is_empty());
    }

    #[test]
    fn grandchildren_go_one_column_further_and_missing_parent_is_root() {
        let a = ids(4);
        let gone = AgentId::generate();
        let spec = SessionSpec {
            id: SessionId::generate(),
            origin: None,
            nodes: vec![
                node(a[0], None, true),
                node(a[1], Some(a[0]), false),
                node(a[2], Some(a[1]), false),
                node(a[3], Some(gone), false),
            ],
        };
        let l = layout(&[spec]);
        let s = &l.sessions[0];
        let child = s.node(a[1]).unwrap();
        let grand = s.node(a[2]).unwrap();
        assert_eq!(grand.left(), child.right() + COL_GAP);
        assert_eq!(s.edges.len(), 2);
        assert!(s.edges.iter().all(|e| e.child != a[3]));
        assert!(s.frame.contains_rect(grand));
    }

    /// Sessão com `n` workers do orquestrador (altura cresce com `n`).
    fn session(origin: Option<Pos2>, workers: usize) -> SessionSpec {
        let a = ids(workers + 1);
        let mut nodes = vec![node(a[0], None, true)];
        nodes.extend(a[1..].iter().map(|id| node(*id, Some(a[0]), false)));
        SessionSpec {
            id: SessionId::generate(),
            origin,
            nodes,
        }
    }

    #[test]
    fn origin_is_respected() {
        let specs = [
            session(Some(pos2(0.0, 0.0)), 1),
            session(Some(pos2(1200.0, -40.0)), 3),
            session(Some(pos2(-300.0, 900.0)), 0),
        ];
        let l = layout(&specs);
        for (spec, s) in specs.iter().zip(&l.sessions) {
            assert_eq!(s.id, spec.id, "a ordem recebida se mantém");
            assert_eq!(s.frame.min, spec.origin.unwrap());
            assert!(s.nodes.iter().all(|(_, r)| s.frame.contains_rect(*r)));
        }
        let orq = l.sessions[1].nodes[0].1;
        assert_eq!(orq.min, pos2(1200.0 + FRAME_PAD, -40.0 + HEADER_H));
        let edge = &l.sessions[1].edges[0];
        assert_eq!(edge.points[0].x, orq.right());
        assert!(l.bounds.contains_rect(l.sessions[1].frame));
        assert!(l.bounds.contains_rect(l.sessions[2].frame));
    }

    #[test]
    fn overlapping_frame_is_pushed_below_only_for_display() {
        let upper = session(Some(pos2(0.0, 0.0)), 3);
        let lower = session(Some(pos2(200.0, 100.0)), 1);
        let l = layout(&[lower.clone(), upper.clone()]);
        let (lo, up) = (&l.sessions[0], &l.sessions[1]);
        assert_eq!(up.frame.min, pos2(0.0, 0.0));
        assert_eq!(lo.frame.left(), 200.0, "só desce, não anda de lado");
        assert_eq!(lo.frame.top(), up.frame.bottom() + SESSION_GAP);
        assert!(!lo.frame.intersects(up.frame));
        // Nós e arestas descem junto.
        let orq = lo.nodes[0].1;
        assert_eq!(orq.top(), lo.frame.top() + HEADER_H);
        assert_eq!(lo.edges[0].points[0].y, orq.top() + ANCHOR_Y);

        // Em cascata: a terceira cruza a segunda já empurrada.
        let third = session(Some(pos2(600.0, 150.0)), 0);
        let l = layout(&[upper.clone(), lower.clone(), third]);
        let [a, b, c] = [&l.sessions[0], &l.sessions[1], &l.sessions[2]];
        assert_eq!(b.frame.top(), a.frame.bottom() + SESSION_GAP);
        assert_eq!(c.frame.top(), b.frame.bottom() + SESSION_GAP);
    }

    #[test]
    fn pushed_frame_returns_when_the_upper_one_shrinks() {
        let lower_origin = pos2(0.0, 500.0);
        let tall = layout(&[session(Some(Pos2::ZERO), 6), session(Some(lower_origin), 0)]);
        assert!(tall.sessions[1].frame.top() > lower_origin.y);
        let short = layout(&[session(Some(Pos2::ZERO), 1), session(Some(lower_origin), 0)]);
        assert_eq!(short.sessions[1].frame.min, lower_origin);
    }

    #[test]
    fn side_by_side_frames_are_not_pushed() {
        let left = session(Some(pos2(0.0, 0.0)), 4);
        let width = layout(std::slice::from_ref(&left)).sessions[0]
            .frame
            .width();
        // Encostada à direita (faixas de x só se tocam) e mais abaixo.
        let right = session(Some(pos2(width, 60.0)), 1);
        let l = layout(&[left, right]);
        assert_eq!(l.sessions[1].frame.min, pos2(width, 60.0));
    }

    #[test]
    fn the_higher_origin_wins_and_ties_go_by_x() {
        // Quem tem a origem mais acima fica; a outra desce, mesmo vindo antes.
        let a = session(Some(pos2(100.0, 40.0)), 2);
        let b = session(Some(pos2(0.0, 0.0)), 2);
        let l = layout(&[a.clone(), b.clone()]);
        assert_eq!(l.sessions[1].frame.min, pos2(0.0, 0.0));
        assert_eq!(
            l.sessions[0].frame.top(),
            l.sessions[1].frame.bottom() + SESSION_GAP
        );
        let c = session(Some(pos2(300.0, 0.0)), 0);
        let l = layout(&[c, b]);
        assert_eq!(l.sessions[1].frame.min, pos2(0.0, 0.0));
        assert!(l.sessions[0].frame.top() > 0.0);
    }

    #[test]
    fn sessions_without_origin_go_below_the_lowest_frame() {
        let placed = session(Some(pos2(900.0, 300.0)), 2);
        let l = layout(&[session(None, 0), placed, session(None, 1)]);
        let low = l.sessions[1].frame;
        let first = l.sessions[0].frame;
        assert_eq!(first.min, pos2(0.0, low.bottom() + SESSION_GAP));
        assert_eq!(
            l.sessions[2].frame.min,
            pos2(0.0, first.bottom() + SESSION_GAP)
        );
        let alone = layout(&[session(None, 0)]);
        assert_eq!(alone.sessions[0].frame.min, Pos2::ZERO);
    }

    #[test]
    fn sessions_stack_without_overlap() {
        let a = ids(3);
        let specs = [
            SessionSpec {
                id: SessionId::generate(),
                origin: None,
                nodes: vec![node(a[0], None, true), node(a[1], Some(a[0]), false)],
            },
            SessionSpec {
                id: SessionId::generate(),
                origin: None,
                nodes: vec![],
            },
            SessionSpec {
                id: SessionId::generate(),
                origin: None,
                nodes: vec![node(a[2], None, true)],
            },
        ];
        let l = layout(&specs);
        assert_eq!(l.sessions.len(), 3);
        for pair in l.sessions.windows(2) {
            assert_eq!(pair[1].frame.top(), pair[0].frame.bottom() + SESSION_GAP);
        }
        assert_eq!(l.sessions[1].frame.height(), HEADER_H + EMPTY_H + FRAME_PAD);
        assert!(l.sessions.iter().all(|s| s.frame.width() >= MIN_FRAME_W));
        assert!(l.bounds.contains_rect(l.sessions[2].frame));
        assert_eq!(l.node(a[2]), l.sessions[2].node(a[2]));
    }
}
