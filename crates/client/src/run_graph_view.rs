//! Draws a Run on the node canvas: [`run_graph::layout`] places the nodes,
//! and this paints them with live Verdicts, the edges between them and the
//! Gate with its terms.

use std::rc::Rc;

use gpui_kit::component::theme::Theme;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_core::{GateState, Verdict};
use slopwatch_protocol::{GateTerm, RunView, StepStatus, StepView};

use crate::run_graph::{
    self, ANY_OF_HEADER, EdgeKind, GATE_PADDING, Layout, NodeId, Rect, Role, TERM_HEIGHT,
};
use crate::run_pane::{Tone, gate_tone, step_state, step_tone};

/// What a click on a Step's node does.
pub type OnSelect = Rc<dyn Fn(&str, &mut Window, &mut App)>;

/// The Run's graph. `open` is the Step whose evidence shows below it.
pub fn run_graph(
    view: &RunView,
    open: Option<&str>,
    theme: &Theme,
    on_select: OnSelect,
) -> impl IntoElement {
    let infos: Vec<_> = view.steps.iter().map(|step| step.info.clone()).collect();
    let terms = run_graph::gate_terms(view);
    let layout = run_graph::layout(&infos, &terms);
    let roles = run_graph::roles(&infos);
    let colors = Colors::new(theme);

    let mut inner = div()
        .relative()
        .flex_none()
        .w(px(layout.width))
        .h(px(layout.height))
        .child(edges(&layout, colors).absolute().size_full());
    for node in &layout.nodes {
        let element = match &node.id {
            NodeId::Gate => gate_node(view, &terms, node.rect, theme, colors).into_any_element(),
            NodeId::Step(id) => {
                let Some(step) = view.step(id) else {
                    continue;
                };
                let (on_select, picked) = (Rc::clone(&on_select), id.clone());
                step_node(
                    step,
                    roles[id.as_str()],
                    open == Some(id),
                    node.rect,
                    theme,
                    colors,
                )
                .on_click(move |_, window, cx| on_select(&picked, window, cx))
                .into_any_element()
            }
        };
        inner = inner.child(element);
    }
    div()
        .id("run-graph")
        .w_full()
        .flex_none()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.muted)
        .overflow_x_scroll()
        .child(inner)
}

#[derive(Clone, Copy)]
struct Colors {
    good: Hsla,
    bad: Hsla,
    neutral: Hsla,
    running: Hsla,
    edge: Hsla,
    /// The Gate's edges and its any-of groups.
    gate: Hsla,
}

impl Colors {
    fn new(theme: &Theme) -> Self {
        Colors {
            good: theme.success,
            bad: theme.danger,
            neutral: theme.muted_foreground,
            running: theme.info,
            edge: theme.muted_foreground.opacity(0.7),
            gate: hsla(250. / 360., 0.5, 0.62, 1.),
        }
    }

    fn tone(&self, tone: Tone) -> Hsla {
        match tone {
            Tone::Good => self.good,
            Tone::Bad => self.bad,
            Tone::Neutral => self.neutral,
        }
    }

    fn step(&self, step: &StepView) -> Hsla {
        match step.status {
            StepStatus::Running => self.running,
            _ => self.tone(step_tone(step)),
        }
    }
}

/// Every edge as a curve from the right of one node to the left of the
/// next, with an arrowhead. Edges the Gate reads are dashed.
fn edges(layout: &Layout, colors: Colors) -> Canvas<()> {
    let edges = layout.edges.clone();
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let at = |x: f32, y: f32| bounds.origin + point(px(x), px(y));
            for edge in &edges {
                let (start, end) = (edge.start, edge.end);
                let (color, dashed) = match edge.kind {
                    EdgeKind::Needs => (colors.edge, false),
                    EdgeKind::GateReads => (colors.gate, true),
                };
                // Stop the line at the arrowhead's base.
                let tip = end.x - 1.;
                let base = tip - 7.;
                let pull = ((base - start.x) / 2.).max(16.);
                let mut line = PathBuilder::stroke(px(1.5));
                if dashed {
                    line = line.dash_array(&[px(4.), px(3.)]);
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
        },
    )
}

fn dot(color: Hsla) -> Div {
    div().flex_none().size(px(8.)).rounded_full().bg(color)
}

fn chip(text: impl Into<SharedString>, color: Hsla, theme: &Theme) -> Div {
    // Chips shrink and truncate, so a long Condition leaves room for the
    // chips after it.
    div()
        .flex_shrink(1.)
        .min_w_0()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(theme.border)
        .text_color(color)
        .truncate()
        .child(text.into())
}

fn step_node(
    step: &StepView,
    role: Role,
    open: bool,
    rect: Rect,
    theme: &Theme,
    colors: Colors,
) -> Stateful<Div> {
    let info = &step.info;
    let color = colors.step(step);
    let faded = matches!(
        step.status,
        StepStatus::Pending
            | StepStatus::Settled {
                verdict: Verdict::Skipped | Verdict::Cancelled,
                ..
            }
    );
    let mut chips = div().flex().gap_1().overflow_hidden().text_xs();
    if let Some(condition) = &info.condition {
        chips = chips.child(chip(
            format!("when {condition}"),
            theme.muted_foreground,
            theme,
        ));
    }
    if role == Role::Advisory {
        chips = chips.child(chip("advisory", theme.muted_foreground, theme));
    }
    if info.write {
        chips = chips.child(chip("commit ends this Run", theme.warning, theme));
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
        .gap_0p5()
        .px_2()
        .py_1p5()
        .rounded_lg()
        .bg(theme.background)
        .border_1()
        .border_color(if faded { theme.border } else { color })
        .when(role == Role::Advisory, |this| this.border_dashed())
        .when(open, |this| this.border_2().border_color(theme.ring))
        .when(faded, |this| this.text_color(theme.muted_foreground))
        .hover(|this| this.bg(theme.list_hover))
        .overflow_hidden()
        .child(
            div()
                .flex()
                .items_center()
                .gap_1p5()
                .child(dot(color))
                .child(div().flex_1().text_sm().truncate().child(info.id.clone()))
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(color)
                        .child(step_state(step)),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .truncate()
                .child(info.plugin.clone()),
        )
        .child(chips)
}

fn gate_node(view: &RunView, terms: &[GateTerm], rect: Rect, theme: &Theme, colors: Colors) -> Div {
    let state = view.gate.unwrap_or(GateState::Pending);
    let color = match state {
        GateState::Pending => theme.warning,
        _ => colors.tone(gate_tone(state)),
    };
    let mut node = div()
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.width))
        .h(px(rect.height))
        .flex()
        .flex_col()
        .p(px(GATE_PADDING))
        .rounded_xl()
        .bg(theme.background)
        .border_2()
        .border_color(color)
        .overflow_hidden()
        .child(
            div()
                .h(px(22.))
                .flex()
                .items_center()
                .justify_between()
                .text_xs()
                .child(div().font_weight(FontWeight::BOLD).child("GATE"))
                .child(
                    div()
                        .px_1p5()
                        .rounded_md()
                        .border_1()
                        .border_color(color)
                        .text_color(color)
                        .child(state.to_string()),
                ),
        )
        .child(
            div()
                .h(px(18.))
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("ALL OF"),
        );
    for term in terms {
        node = node.child(gate_term(view, term, theme, colors));
    }
    node
}

fn gate_term(view: &RunView, term: &GateTerm, theme: &Theme, colors: Colors) -> Div {
    match term {
        GateTerm::Step {
            id,
            accepts_skipped,
        } => {
            let color = view.step(id).map_or(colors.neutral, |step| {
                let skipped = matches!(
                    step.status,
                    StepStatus::Settled {
                        verdict: Verdict::Skipped,
                        ..
                    }
                );
                if skipped && *accepts_skipped {
                    colors.good
                } else {
                    colors.step(step)
                }
            });
            div()
                .h(px(TERM_HEIGHT))
                .flex()
                .items_center()
                .gap_1p5()
                .text_xs()
                .child(dot(color))
                .child(div().truncate().child(id.clone()))
                .when(*accepts_skipped, |this| {
                    this.child(
                        div()
                            .flex_none()
                            .text_color(theme.muted_foreground)
                            .child("pass · skipped"),
                    )
                })
        }
        GateTerm::AnyOf { terms } => {
            let mut group = div()
                .flex()
                .flex_col()
                .pl_2()
                .border_l_2()
                .border_color(colors.gate)
                .child(
                    div()
                        .h(px(ANY_OF_HEADER))
                        .flex()
                        .items_center()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("ANY OF"),
                );
            for term in terms {
                group = group.child(gate_term(view, term, theme, colors));
            }
            group
        }
        GateTerm::Other { text } => div()
            .h(px(TERM_HEIGHT))
            .flex()
            .items_center()
            .text_xs()
            .truncate()
            .child(text.clone()),
    }
}
