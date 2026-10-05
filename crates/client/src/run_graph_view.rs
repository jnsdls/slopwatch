//! Draws a Run on the node canvas: [`run_graph::layout`] places the nodes,
//! and this paints them with live Verdicts, the edges between them and the
//! Gate with its terms.

use std::rc::Rc;

use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_core::{GateState, Verdict};
use slopwatch_protocol::{GateTerm, RunView, StepStatus, StepView};

use crate::components::{ChipKind, chip, dot, dot_grid, gate_pill, truncated, verdict_label};
use crate::run_graph::{
    self, ALL_OF_HEIGHT, ANY_OF_HEADER, EdgeKind, GATE_BORDER, GATE_PADDING, GATE_TITLE_HEIGHT,
    Layout, NodeId, Rect, Role, TERM_HEIGHT,
};
use crate::run_pane::{GRAPH_BORDER, Look, step_look, step_state};
use crate::theme;

/// What a click on a Step's node does.
pub type OnSelect = Rc<dyn Fn(&str, &mut Window, &mut App)>;

/// The Run's graph. `open` is the Step whose evidence shows below it.
pub fn run_graph(view: &RunView, open: Option<&str>, on_select: OnSelect) -> impl IntoElement {
    let infos: Vec<_> = view.steps.iter().map(|step| step.info.clone()).collect();
    let terms = run_graph::gate_terms(view);
    let layout = run_graph::layout(&infos, &terms);
    let roles = run_graph::roles(&infos);

    let mut inner = div()
        .relative()
        .flex_none()
        .w(px(layout.width))
        .min_w_full()
        .h(px(layout.height))
        .child(dot_grid())
        .child(edges(&layout).absolute().size_full());
    for node in &layout.nodes {
        let element = match &node.id {
            NodeId::Gate => gate_node(view, &terms, node.rect).into_any_element(),
            NodeId::Step(id) => {
                let Some(step) = view.step(id) else {
                    continue;
                };
                let (on_select, picked) = (Rc::clone(&on_select), id.clone());
                step_node(step, roles[id.as_str()], open == Some(id), node.rect)
                    .on_click(move |_, window, cx| on_select(&picked, window, cx))
                    .into_any_element()
            }
        };
        inner = inner.child(element);
    }
    div()
        .id("run-graph")
        .relative()
        .w_full()
        .flex_none()
        .rounded(px(8.))
        .border(px(GRAPH_BORDER))
        .border_color(theme::LINE)
        .bg(theme::BG)
        // The pane scrolls up and down, and the canvas only sideways.
        .overflow_x_scroll()
        .restrict_scroll_to_axis()
        .child(inner)
}

/// Every edge as a curve from the right of one node to the left of the
/// next, with an arrowhead. Edges the Gate reads are dashed.
fn edges(layout: &Layout) -> Canvas<()> {
    let edges = layout.edges.clone();
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            for edge in &edges {
                let (color, dashed) = edge_look(edge.kind);
                paint_edge(window, bounds.origin, edge.start, edge.end, color, dashed);
            }
        },
    )
}

/// An edge's colour, and whether it's dashed: solid for `needs`, dashed
/// violet for what the Gate reads.
pub(crate) fn edge_look(kind: EdgeKind) -> (Rgba, bool) {
    match kind {
        EdgeKind::Needs => (theme::EDGE, false),
        EdgeKind::GateReads => (theme::GATE_LINE, true),
    }
}

/// Paints one edge as a curve from `start` to an arrowhead at `end`, both
/// relative to `origin`.
pub(crate) fn paint_edge(
    window: &mut Window,
    origin: gpui_kit::Point<Pixels>,
    start: run_graph::Point,
    end: run_graph::Point,
    color: Rgba,
    dashed: bool,
) {
    let at = |x: f32, y: f32| origin + point(px(x), px(y));
    // Stop the line at the arrowhead's base.
    let tip = end.x - 1.;
    let base = tip - 7.;
    let pull = ((base - start.x).abs() / 2.).max(16.);
    let mut line = PathBuilder::stroke(px(1.5));
    if dashed {
        line = line.dash_array(&[px(5.), px(4.)]);
    }
    line.move_to(at(start.x, start.y));
    line.cubic_bezier_to(
        at(base, end.y),
        at(start.x + pull, start.y),
        at(base - pull, end.y),
    );
    if let Ok(path) = line.build() {
        window.paint_path(path, color);
    }
    let mut head = PathBuilder::fill();
    head.move_to(at(tip, end.y));
    head.line_to(at(base, end.y - 4.));
    head.line_to(at(base, end.y + 4.));
    head.close();
    if let Ok(path) = head.build() {
        window.paint_path(path, color);
    }
}

fn step_node(step: &StepView, role: Role, open: bool, rect: Rect) -> Stateful<Div> {
    let info = &step.info;
    let look = step_look(step);
    let mut chips = div().flex().gap_1().overflow_hidden();
    if let Some(condition) = &info.condition {
        chips = chips.child(chip(ChipKind::Condition, condition.clone()));
    }
    if info.write {
        chips = chips.child(chip(ChipKind::Write, "commit ends Run"));
    }
    let findings = match &step.status {
        StepStatus::Settled { outputs, .. } => outputs.findings.len(),
        _ => 0,
    };
    let mut detail = div()
        .flex()
        .items_center()
        .gap_1()
        .text_xs()
        .overflow_hidden()
        .child(verdict_label(look, step_state(step)));
    if findings > 0 {
        detail = detail.child(div().text_color(theme::DIM).child(match findings {
            1 => "· 1 finding".to_owned(),
            n => format!("· {n} findings"),
        }));
    }
    if let Some(cost) = step.cost {
        detail = detail.child(div().text_color(theme::DIM).child(format!("· {cost}")));
    }

    div()
        .id(SharedString::from(format!("node-{}", info.id)))
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.width))
        .h(px(rect.height))
        .flex()
        .flex_col()
        .gap(px(3.))
        .px(px(10.))
        .py(px(7.))
        .rounded(px(10.))
        .bg(if role == Role::Advisory {
            theme::ADVISORY_BG
        } else {
            theme::PANEL
        })
        .border(px(1.5))
        .border_color(theme::node_line(look))
        .when(role == Role::Advisory, |this| this.border_dashed())
        .when(theme::faded(look), |this| this.opacity(0.6))
        .when(open, |this| this.border_2().border_color(theme::ACCENT))
        .cursor_pointer()
        .overflow_hidden()
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(dot(look))
                .child(
                    truncated("name", info.id.clone())
                        .flex_1()
                        .font_weight(FontWeight::SEMIBOLD),
                ),
        )
        .child(detail)
        .child(chips)
}

fn gate_node(view: &RunView, terms: &[GateTerm], rect: Rect) -> Div {
    let state = view.gate.unwrap_or(GateState::Pending);
    let mut node = div()
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.width))
        .h(px(rect.height))
        .flex()
        .flex_col()
        .p(px(GATE_PADDING))
        .rounded(px(14.))
        .bg(theme::GATE_BG)
        .border(px(GATE_BORDER))
        .border_color(theme::gate_line(state))
        .overflow_hidden()
        .child(
            div()
                .h(px(GATE_TITLE_HEIGHT))
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::GATE_TEXT)
                        .child("GATE"),
                )
                .child(gate_pill(state)),
        )
        .child(
            div()
                .h(px(ALL_OF_HEIGHT))
                .text_size(theme::LABEL_SIZE)
                .text_color(theme::DIM)
                .child("ALL OF"),
        );
    for term in terms {
        node = node.child(gate_term(view, term));
    }
    node
}

fn gate_term(view: &RunView, term: &GateTerm) -> Div {
    match term {
        GateTerm::Step {
            id,
            accepts_skipped,
        } => {
            let look = view.step(id).map_or(Look::Waiting, |step| {
                let skipped = matches!(
                    step.status,
                    StepStatus::Settled {
                        verdict: Verdict::Skipped,
                        ..
                    }
                );
                // A skip the term accepts counts as pass.
                if skipped && *accepts_skipped {
                    Look::Pass
                } else {
                    step_look(step)
                }
            });
            div()
                .h(px(TERM_HEIGHT))
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.))
                .child(dot(look))
                .child(truncated(
                    SharedString::from(format!("term-{id}")),
                    id.clone(),
                ))
                .when(*accepts_skipped, |this| {
                    this.child(chip(ChipKind::Required, "skip ok").flex_none())
                })
        }
        GateTerm::AnyOf { terms } => {
            let mut group = div()
                .flex()
                .flex_col()
                .pl(px(8.))
                .border_l_2()
                .border_color(theme::GATE_LINE)
                .child(
                    div()
                        .h(px(ANY_OF_HEADER))
                        .flex()
                        .items_center()
                        .text_size(theme::LABEL_SIZE)
                        .text_color(theme::DIM)
                        .child("ANY OF"),
                );
            for term in terms {
                group = group.child(gate_term(view, term));
            }
            group
        }
        GateTerm::Other { text } => div()
            .h(px(TERM_HEIGHT))
            .flex()
            .items_center()
            .text_size(px(12.))
            .child(truncated(
                SharedString::from(format!("term-{text}")),
                text.clone(),
            )),
    }
}
