//! Lays a Pipeline out as a left-to-right graph: each Step in the column
//! after the Steps it needs, the Gate after the Steps it reads, and the
//! Steps that need the Gate (Merge, Fix) after it. No GPUI here, so the
//! layout tests without a window. The Run graph draws it today, and the
//! Pipeline editor's canvas builds on the same layout.

use std::collections::HashMap;

use slopwatch_core::GATE;
use slopwatch_protocol::{GateTerm, RunView, StepInfo};

pub const NODE_WIDTH: f32 = 164.;
pub const NODE_HEIGHT: f32 = 78.;
pub const GATE_WIDTH: f32 = 184.;
pub const COLUMN_GAP: f32 = 40.;
pub const ROW_GAP: f32 = 14.;
/// Room around the graph, so selection outlines and arrowheads aren't cut.
pub const MARGIN: f32 = 8.;
/// The Gate node's padding on each side.
pub const GATE_PADDING: f32 = 8.;
/// The Gate's title row plus its "ALL OF" label.
pub const GATE_HEADER: f32 = 40.;
pub const TERM_HEIGHT: f32 = 20.;
/// The "ANY OF" label atop an any-of group, plus the gap below the group.
pub const ANY_OF_HEADER: f32 = 20.;

/// In Graph mode the sources column collapses and the PR list narrows to
/// this, leaving the rest of the window to the PR pane.
pub const GRAPH_MODE_LIST_WIDTH: f32 = 300.;
/// The PR pane's padding on each side.
pub const PANE_PADDING: f32 = 16.;

/// The width the canvas gets in Graph mode in a window `window_width` wide.
pub fn canvas_width(window_width: f32) -> f32 {
    // The pane's left border takes one more pixel.
    window_width - GRAPH_MODE_LIST_WIDTH - 2. * PANE_PADDING - 1.
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeId {
    Step(String),
    Gate,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    fn left_middle(&self) -> Point {
        Point {
            x: self.x,
            y: self.y + self.height / 2.,
        }
    }

    fn right_middle(&self) -> Point {
        Point {
            x: self.x + self.width,
            y: self.y + self.height / 2.,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: NodeId,
    pub rect: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// A `needs` edge, drawn solid.
    Needs,
    /// The Gate reads the Step, drawn dashed.
    GateReads,
}

/// An edge from the right of `from` to the left of `to`.
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
    pub start: Point,
    pub end: Point,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub width: f32,
    pub height: f32,
}

impl Layout {
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.iter().find(|node| &node.id == id)
    }
}

/// What a Step is to the Gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The Gate reads it.
    Gated,
    /// It runs after the Gate, like Merge and Fix.
    AfterGate,
    /// Neither: it only advises, and draws dashed.
    Advisory,
}

/// Each Step's [`Role`], by id.
pub fn roles(steps: &[StepInfo]) -> HashMap<&str, Role> {
    let graph = Graph::new(steps);
    steps
        .iter()
        .map(|step| {
            let role = if step.gated {
                Role::Gated
            } else if graph.after_gate(&step.id) {
                Role::AfterGate
            } else {
                Role::Advisory
            };
            (step.id.as_str(), role)
        })
        .collect()
}

/// The Gate's terms. A Run from a daemon that predates them lists only
/// which Steps the Gate reads, so each becomes a plain term.
pub fn gate_terms(view: &RunView) -> Vec<GateTerm> {
    if !view.gate_terms.is_empty() {
        return view.gate_terms.clone();
    }
    view.steps
        .iter()
        .filter(|step| step.info.gated)
        .map(|step| GateTerm::Step {
            id: step.info.id.clone(),
            accepts_skipped: false,
        })
        .collect()
}

/// The Gate node's height for `terms`.
pub fn gate_height(terms: &[GateTerm]) -> f32 {
    let height = GATE_HEADER + 2. * GATE_PADDING + terms.iter().map(term_height).sum::<f32>();
    height.max(NODE_HEIGHT)
}

/// One Gate term's height, as the Gate node lists it.
pub fn term_height(term: &GateTerm) -> f32 {
    match term {
        GateTerm::AnyOf { terms } => ANY_OF_HEADER + terms.iter().map(term_height).sum::<f32>(),
        GateTerm::Step { .. } | GateTerm::Other { .. } => TERM_HEIGHT,
    }
}

/// Places `steps`, in Pipeline order, and the Gate with `terms`.
pub fn layout(steps: &[StepInfo], terms: &[GateTerm]) -> Layout {
    let graph = Graph::new(steps);
    let roles = roles(steps);

    // Each column's nodes, top to bottom: the Gate, then the Steps the Gate
    // reads or that follow it, then advisory Steps, each in Pipeline order.
    let mut columns: Vec<Vec<(NodeId, f32, f32)>> = Vec::new();
    let mut place = |column: usize, node: (NodeId, f32, f32)| {
        if columns.len() <= column {
            columns.resize_with(column + 1, Vec::new);
        }
        columns[column].push(node);
    };
    place(
        graph.gate_column(),
        (NodeId::Gate, GATE_WIDTH, gate_height(terms)),
    );
    for advisory in [false, true] {
        for step in steps {
            if (roles[step.id.as_str()] == Role::Advisory) == advisory {
                let node = (NodeId::Step(step.id.clone()), NODE_WIDTH, NODE_HEIGHT);
                place(graph.column(&step.id), node);
            }
        }
    }

    let column_height = |nodes: &[(NodeId, f32, f32)]| {
        let heights: f32 = nodes.iter().map(|(_, _, height)| height).sum();
        heights + ROW_GAP * nodes.len().saturating_sub(1) as f32
    };
    let tallest = columns
        .iter()
        .map(|nodes| column_height(nodes))
        .fold(0., f32::max);
    let mut nodes = Vec::new();
    let mut x = MARGIN;
    for column in &columns {
        let mut y = MARGIN + (tallest - column_height(column)) / 2.;
        let mut width: f32 = 0.;
        for (id, node_width, height) in column {
            let rect = Rect {
                x,
                y,
                width: *node_width,
                height: *height,
            };
            nodes.push(Node {
                id: id.clone(),
                rect,
            });
            y += height + ROW_GAP;
            width = width.max(*node_width);
        }
        if !column.is_empty() {
            x += width + COLUMN_GAP;
        }
    }
    let width = x - COLUMN_GAP + MARGIN;

    let mut layout = Layout {
        nodes,
        edges: Vec::new(),
        width,
        height: tallest + 2. * MARGIN,
    };
    let mut edges = Vec::new();
    for step in steps {
        for need in &step.needs {
            let from = if need == GATE {
                NodeId::Gate
            } else {
                NodeId::Step(need.clone())
            };
            edges.push((from, NodeId::Step(step.id.clone()), EdgeKind::Needs));
        }
    }
    for step in steps.iter().filter(|step| step.gated) {
        edges.push((
            NodeId::Step(step.id.clone()),
            NodeId::Gate,
            EdgeKind::GateReads,
        ));
    }
    layout.edges = edges
        .into_iter()
        .filter_map(|(from, to, kind)| {
            let start = layout.node(&from)?.rect.right_middle();
            let end = layout.node(&to)?.rect.left_middle();
            Some(Edge {
                from,
                to,
                kind,
                start,
                end,
            })
        })
        .collect();
    layout
}

/// The Steps by id, for walking `needs`.
struct Graph<'a> {
    steps: HashMap<&'a str, &'a StepInfo>,
}

impl<'a> Graph<'a> {
    fn new(steps: &'a [StepInfo]) -> Self {
        Graph {
            steps: steps.iter().map(|step| (step.id.as_str(), step)).collect(),
        }
    }

    /// The Steps `id` needs, skipping the Gate and ids the Run doesn't list.
    fn needs(&self, id: &str) -> impl Iterator<Item = &'a StepInfo> {
        let needs = self.steps.get(id).map(|step| step.needs.as_slice());
        needs
            .unwrap_or_default()
            .iter()
            .filter_map(|need| self.steps.get(need.as_str()).copied())
    }

    fn needs_gate(&self, id: &str) -> bool {
        self.steps
            .get(id)
            .is_some_and(|step| step.needs.iter().any(|need| need == GATE))
    }

    /// Whether `id` runs after the Gate, directly or through a Step that
    /// does. A loaded Pipeline has no cycles, so the walk ends.
    fn after_gate(&self, id: &str) -> bool {
        self.needs_gate(id) || self.needs(id).any(|need| self.after_gate(&need.id))
    }

    /// The column after every Step `id` needs, and after the Gate when it
    /// needs the Gate.
    fn column(&self, id: &str) -> usize {
        let after_needs = self.needs(id).map(|need| self.column(&need.id) + 1);
        let after_gate = self.needs_gate(id).then(|| self.gate_column() + 1);
        after_needs.chain(after_gate).max().unwrap_or(0)
    }

    /// The column after every Step the Gate reads.
    fn gate_column(&self) -> usize {
        self.steps
            .values()
            .filter(|step| step.gated)
            .map(|step| self.column(&step.id) + 1)
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_protocol::RunEvent;

    fn step(id: &str, needs: &[&str], gated: bool) -> StepInfo {
        StepInfo {
            id: id.into(),
            plugin: id.into(),
            needs: needs.iter().map(|&n| n.into()).collect(),
            gated,
            write: false,
            condition: None,
        }
    }

    fn term(id: &str) -> GateTerm {
        GateTerm::Step {
            id: id.into(),
            accepts_skipped: false,
        }
    }

    fn id(step: &str) -> NodeId {
        NodeId::Step(step.into())
    }

    fn rect(layout: &Layout, node: &NodeId) -> Rect {
        layout.node(node).unwrap().rect
    }

    /// The Starter "Ask me, then merge", with the Human Step after the
    /// review: five columns, the widest Starter.
    fn ask_me_then_merge() -> (Vec<StepInfo>, Vec<GateTerm>) {
        let steps = vec![
            step("ci", &[], true),
            step("desc", &[], true),
            step("issue", &[], true),
            step("review", &["ci"], true),
            step("ship-it", &["review"], true),
            step("merge", &[GATE], false),
            StepInfo {
                write: true,
                ..step("fix", &[GATE], false)
            },
        ];
        let terms = vec![
            term("ci"),
            term("desc"),
            GateTerm::Step {
                id: "issue".into(),
                accepts_skipped: true,
            },
            term("review"),
            term("ship-it"),
        ];
        (steps, terms)
    }

    #[test]
    fn steps_sit_one_column_after_what_they_need_and_the_gate_after_what_it_reads() {
        let (steps, terms) = ask_me_then_merge();
        let layout = layout(&steps, &terms);
        let x = |node: NodeId| rect(&layout, &node).x;

        assert_eq!(x(id("ci")), x(id("desc")), "parallel Steps share a column");
        assert_eq!(x(id("ci")), x(id("issue")));
        assert!(x(id("ci")) < x(id("review")));
        assert!(x(id("review")) < x(id("ship-it")));
        assert!(x(id("ship-it")) < x(NodeId::Gate));
        assert!(x(NodeId::Gate) < x(id("merge")));
        assert_eq!(x(id("merge")), x(id("fix")));
    }

    #[test]
    fn nodes_in_a_column_dont_overlap_and_columns_center_on_one_line() {
        let (steps, terms) = ask_me_then_merge();
        let layout = layout(&steps, &terms);
        let [ci, desc, issue] = ["ci", "desc", "issue"].map(|s| rect(&layout, &id(s)));

        assert!(ci.y + ci.height + ROW_GAP <= desc.y);
        assert!(desc.y + desc.height + ROW_GAP <= issue.y);
        let middle = |r: Rect| r.y + r.height / 2.;
        let review = rect(&layout, &id("review"));
        assert_eq!(middle(review), middle(desc));
        assert_eq!(middle(rect(&layout, &NodeId::Gate)), middle(desc));
        for node in &layout.nodes {
            assert!(node.rect.x >= MARGIN && node.rect.y >= MARGIN);
            assert!(node.rect.x + node.rect.width + MARGIN <= layout.width);
            assert!(node.rect.y + node.rect.height + MARGIN <= layout.height);
        }
    }

    #[test]
    fn advisory_steps_sit_below_the_gated_ones_in_their_column() {
        let steps = vec![
            step("notes", &[], false),
            step("ci", &[], true),
            step("lint", &[], true),
        ];
        let layout = layout(&steps, &[term("ci"), term("lint")]);
        let y = |s| rect(&layout, &id(s)).y;

        assert!(y("ci") < y("lint"));
        assert!(y("lint") < y("notes"));
    }

    #[test]
    fn needs_edges_are_solid_and_gate_reads_are_dashed() {
        let steps = vec![
            step("ci", &[], true),
            step("review", &["ci"], true),
            step("notes", &["ci"], false),
            step("merge", &[GATE], false),
        ];
        let layout = layout(&steps, &[term("ci"), term("review")]);
        let edges: Vec<(NodeId, NodeId, EdgeKind)> = layout
            .edges
            .iter()
            .map(|e| (e.from.clone(), e.to.clone(), e.kind))
            .collect();

        assert_eq!(
            edges,
            [
                (id("ci"), id("review"), EdgeKind::Needs),
                (id("ci"), id("notes"), EdgeKind::Needs),
                (NodeId::Gate, id("merge"), EdgeKind::Needs),
                (id("ci"), NodeId::Gate, EdgeKind::GateReads),
                (id("review"), NodeId::Gate, EdgeKind::GateReads),
            ]
        );
        let review = &layout.edges[0];
        let (ci, to) = (rect(&layout, &id("ci")), rect(&layout, &id("review")));
        assert_eq!(review.start, ci.right_middle());
        assert_eq!(review.end, to.left_middle());
    }

    #[test]
    fn roles_tell_gated_advisory_and_after_the_gate_apart() {
        let steps = vec![
            step("ci", &[], true),
            step("notes", &["ci"], false),
            step("merge", &[GATE], false),
            step("announce", &["merge"], false),
        ];
        let roles = roles(&steps);

        assert_eq!(roles["ci"], Role::Gated);
        assert_eq!(roles["notes"], Role::Advisory);
        assert_eq!(roles["merge"], Role::AfterGate);
        assert_eq!(
            roles["announce"],
            Role::AfterGate,
            "after the Gate through merge"
        );
    }

    #[test]
    fn the_gate_grows_with_its_terms_and_any_of_groups() {
        let two = gate_height(&[term("ci"), term("lint")]);
        let three = gate_height(&[term("ci"), term("lint"), term("docs")]);
        let grouped = gate_height(&[
            term("ci"),
            term("lint"),
            GateTerm::AnyOf {
                terms: vec![term("review"), term("human")],
            },
        ]);

        assert_eq!(three - two, TERM_HEIGHT);
        assert_eq!(grouped - two, ANY_OF_HEADER + 2. * TERM_HEIGHT);
        assert_eq!(
            gate_height(&[term("ci")]),
            NODE_HEIGHT,
            "the Gate is never shorter than a Step"
        );
    }

    #[test]
    fn a_starter_sized_pipeline_fits_the_canvas_at_1440_px() {
        let (steps, terms) = ask_me_then_merge();

        assert!(layout(&steps, &terms).width <= canvas_width(1440.));
    }

    #[test]
    fn a_run_without_gate_terms_lists_the_steps_its_gate_reads() {
        let mut view = RunView::default();
        view.apply(
            1,
            RunEvent::Started {
                repo: slopwatch_protocol::RepoName::new("o", "r"),
                number: 1,
                head_sha: "abc".into(),
                base: "main".into(),
                base_sha: "def".into(),
                steps: vec![
                    step("ci", &[], true),
                    step("notes", &[], false),
                    step("lint", &[], true),
                ],
                gate: "[ci, lint]".into(),
                gate_terms: vec![],
            },
        );

        assert_eq!(gate_terms(&view), [term("ci"), term("lint")]);

        view.gate_terms = vec![GateTerm::AnyOf {
            terms: vec![term("ci"), term("lint")],
        }];
        assert_eq!(gate_terms(&view), view.gate_terms);
    }
}
