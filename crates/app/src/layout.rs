//! Layout automático do canvas, em árvore. Função pura: recebe os nós com o
//! tamanho de cada card e devolve molduras, retângulos e arestas ortogonais
//! em coordenadas do canvas.

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

/// Empilha as sessões na vertical, cada uma com sua árvore.
pub fn layout(sessions: &[SessionSpec]) -> CanvasLayout {
    let mut out = Vec::with_capacity(sessions.len());
    let mut y = 0.0;
    let mut bounds = Rect::NOTHING;
    for spec in sessions {
        let session = layout_session(spec, pos2(0.0, y));
        y = session.frame.bottom() + SESSION_GAP;
        bounds = bounds.union(session.frame);
        out.push(session);
    }
    if out.is_empty() {
        bounds = Rect::from_min_size(Pos2::ZERO, vec2(MIN_FRAME_W, 200.0));
    }
    CanvasLayout {
        sessions: out,
        bounds,
    }
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

    #[test]
    fn sessions_stack_without_overlap() {
        let a = ids(3);
        let specs = [
            SessionSpec {
                id: SessionId::generate(),
                nodes: vec![node(a[0], None, true), node(a[1], Some(a[0]), false)],
            },
            SessionSpec {
                id: SessionId::generate(),
                nodes: vec![],
            },
            SessionSpec {
                id: SessionId::generate(),
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
