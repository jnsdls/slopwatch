//! The prototypes' small pieces, which every screen shares: the `.pill`
//! for the Gate and how a Run ended, the `.chip` for Conditions and
//! labels, the `.dot` for a Step's state and the strip of them on a PR
//! row, the amber `.ask` card for an open Human Step or Escalation, and
//! the section labels, wells and buttons around them.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_core::GateState;
use slopwatch_protocol::step::Severity;
use slopwatch_protocol::{RunSummary, StripMark};

use crate::run_pane::{Look, end_label};
use crate::theme;

/// What a pill says about the Gate or a Run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillTone {
    Pass,
    Fail,
    Pending,
    /// How a Run ended, in the dim text.
    Plain,
}

impl PillTone {
    pub fn of_gate(gate: GateState) -> PillTone {
        match gate {
            GateState::Pass => PillTone::Pass,
            GateState::Fail => PillTone::Fail,
            GateState::Pending => PillTone::Pending,
        }
    }
}

/// The prototype's `.pill`: a rounded label with a tinted fill.
pub fn pill(tone: PillTone, text: impl Into<SharedString>) -> Div {
    let (bg, fg, line) = match tone {
        PillTone::Pass => (theme::PASS_BG, theme::PASS, theme::PASS_LINE),
        PillTone::Fail => (theme::FAIL_BG, theme::FAIL, theme::FAIL_LINE),
        PillTone::Pending => (theme::INC_BG, theme::INC, theme::INC_LINE),
        PillTone::Plain => (Rgba { a: 0., ..theme::BG }, theme::DIM, theme::LINE),
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .px(px(8.))
        .py(px(1.))
        .rounded_full()
        .border_1()
        .border_color(line)
        .bg(bg)
        .text_color(fg)
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .whitespace_nowrap()
        .child(text.into())
}

/// The Gate's state as a pill, as the Gate row and the PR pane show it.
pub fn gate_pill(gate: GateState) -> Div {
    pill(PillTone::of_gate(gate), gate.to_string())
}

/// A Run on its PR's row: the Gate while it goes, how it ended once it has.
pub fn run_pill(run: &RunSummary) -> Div {
    match run.end {
        Some(reason) => pill(PillTone::Plain, end_label(reason, run.waived)),
        None => pill(PillTone::of_gate(run.gate), format!("Gate {}", run.gate)),
    }
}

/// What a chip marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    Plain,
    /// A Step outside the Gate: dashed.
    Advisory,
    /// A write Step: its commit ends the Run.
    Write,
    /// A Condition.
    Condition,
    /// A Gate term's "skip ok", or another setting that's on.
    Required,
    Ok,
    Bad,
    Warn,
}

/// The prototype's `.chip`: a small square label.
pub fn chip(kind: ChipKind, text: impl Into<SharedString>) -> Div {
    let (bg, fg, line) = match kind {
        ChipKind::Plain => (theme::CHIP_BG, theme::CHIP_TEXT, theme::LINE),
        ChipKind::Advisory => (Rgba { a: 0., ..theme::BG }, theme::DIM, theme::LINE),
        ChipKind::Write => (theme::WRITE_BG, theme::WRITE_TEXT, theme::WRITE_LINE),
        ChipKind::Condition => (theme::ACCENT_BG, theme::GATE_TEXT, theme::GATE_LINE),
        ChipKind::Required => (
            theme::REQUIRED_BG,
            theme::REQUIRED_TEXT,
            theme::REQUIRED_LINE,
        ),
        ChipKind::Ok => (theme::PASS_BG, theme::PASS, theme::PASS_LINE),
        ChipKind::Bad => (theme::FAIL_BG, theme::FAIL_TEXT, theme::FAIL_LINE),
        ChipKind::Warn => (theme::INC_BG, theme::INC, theme::INC_LINE),
    };
    div()
        .flex_shrink(1.)
        .min_w_0()
        .px(px(6.))
        .rounded(px(4.))
        .border_1()
        .border_color(line)
        .when(kind == ChipKind::Advisory, |this| this.border_dashed())
        .bg(bg)
        .text_color(fg)
        .text_xs()
        .truncate()
        .child(text.into())
}

/// The prototype's `.dot`: a state's colour, a ring for a Step that was
/// skipped or cancelled, and a dark dot for one that hasn't started.
pub fn dot(look: Look) -> Div {
    let dot = div().flex_none().size(px(9.)).rounded_full();
    match look {
        Look::Skipped | Look::Cancelled => dot.border(px(1.5)).border_color(theme::MUTED),
        Look::Waiting => dot.bg(theme::WAIT_DOT),
        _ => dot.bg(theme::color(look)),
    }
}

/// A dot in any colour, such as a Gate term's in the editor.
pub fn plain_dot(color: Rgba) -> Div {
    div().flex_none().size(px(9.)).rounded_full().bg(color)
}

/// A PR row's Step strip: a dot per Step and a tick where the Gate sits.
pub fn strip(marks: &[StripMark]) -> Div {
    let mut strip = div().flex_none().flex().items_center().gap(px(3.));
    for mark in marks {
        strip = strip.child(match Look::of_mark(*mark) {
            Some(look) => dot(look),
            None => div()
                .flex_none()
                .w(px(2.))
                .h(px(12.))
                .mx(px(2.))
                .bg(theme::GATE_LINE),
        });
    }
    strip
}

/// A Verdict or state in its colour: the prototype's `.vl`.
pub fn verdict_label(look: Look, text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::color(look))
        .child(text.into())
}

/// Small capitals over a section, such as "RUN HISTORY": the
/// prototype's `.sect`.
pub fn section(text: &str) -> Div {
    div()
        .text_size(theme::LABEL_SIZE)
        .text_color(theme::DIM)
        .child(text.to_uppercase())
}

/// Monospace text, for SHAs, paths, ids and logs.
pub fn mono(text: impl Into<SharedString>) -> Div {
    div()
        .font_family(theme::MONO)
        .text_size(theme::MONO_SIZE)
        .child(text.into())
}

/// The prototype's `.card`: a panel with a border.
pub fn card() -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p(px(12.))
        .rounded(px(10.))
        .border_1()
        .border_color(theme::LINE)
        .bg(theme::PANEL)
}

/// A row of a list such as the Secrets or the Plugins: a bordered panel
/// like a Step's row, picked out while `chosen`.
pub fn list_row(id: impl Into<ElementId>, chosen: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .px(px(10.))
        .py(px(7.))
        .rounded(px(8.))
        .border_1()
        .border_color(if chosen { theme::ACCENT } else { theme::LINE })
        .bg(if chosen {
            theme::ACCENT_BG
        } else {
            theme::PANEL
        })
        .cursor_pointer()
        .when(!chosen, |this| {
            this.hover(|this| this.border_color(theme::DIM))
        })
}

/// A dark well for logs and file text: the prototype's `.log`.
pub fn well() -> Div {
    div()
        .flex()
        .flex_col()
        .px(px(8.))
        .py(px(6.))
        .rounded(px(6.))
        .border_1()
        .border_color(theme::LINE)
        .bg(theme::WELL)
        .font_family(theme::MONO)
        .text_size(theme::MONO_SIZE)
}

/// What an open Inbox entry asks of the developer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// A Human Step's question: amber.
    Human,
    /// Something went wrong: red.
    Escalation,
}

/// The prototype's `.ask` card, with its kicker such as "HUMAN STEP ·
/// 11M AGO" on top.
pub fn ask_card(ask: Ask, kicker: &str) -> Div {
    let (bg, line, fg) = match ask {
        Ask::Human => (theme::ASK_BG, theme::INC_LINE, theme::ASK),
        Ask::Escalation => (theme::ESCALATION_BG, theme::FAIL_LINE, theme::FAIL),
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .px(px(10.))
        .py(px(8.))
        .rounded(px(8.))
        .border_1()
        .border_color(line)
        .bg(bg)
        .child(
            div()
                .text_size(theme::LABEL_SIZE)
                .font_weight(FontWeight::BOLD)
                .text_color(fg)
                .child(kicker.to_uppercase()),
        )
}

/// A Finding's severity: the prototype's `.sev`.
pub fn severity(severity: Severity) -> Div {
    let (bg, fg, text) = match severity {
        Severity::Error => (theme::FAIL_BG, theme::FAIL, "HIGH"),
        Severity::Warning => (theme::INC_BG, theme::INC, "MEDIUM"),
        Severity::Info => (theme::CHIP_BG, theme::DIM, "LOW"),
    };
    div()
        .flex_none()
        .px(px(4.))
        .rounded(px(3.))
        .bg(bg)
        .text_color(fg)
        .text_size(theme::LABEL_SIZE)
        .font_weight(FontWeight::BOLD)
        .child(text)
}

/// A count in the sources pane. `hot` draws it as the red Inbox badge.
pub fn count(n: usize, hot: bool) -> Div {
    let count = div().flex_none().text_xs().child(n.to_string());
    if hot {
        count
            .px(px(6.))
            .rounded_full()
            .bg(theme::BADGE)
            .text_color(theme::WHITE)
            .font_weight(FontWeight::BOLD)
    } else {
        count.text_color(theme::DIM)
    }
}

/// A link out, such as to the PR on GitHub.
pub fn link(id: impl Into<ElementId>, text: impl Into<SharedString>, url: String) -> Stateful<Div> {
    div()
        .id(id)
        .text_color(theme::LINK)
        .cursor_pointer()
        .hover(|this| this.underline())
        .child(text.into())
        .on_click(move |_, _, cx| cx.open_url(&url))
}

/// One option of a segmented control, such as List or Graph: the
/// prototype's `.seg`. `first` and `last` round its outer corners.
pub fn segment(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on: bool,
    first: bool,
    last: bool,
) -> Stateful<Div> {
    let radius = px(6.);
    div()
        .id(id)
        .px(px(10.))
        .py(px(2.))
        .border_1()
        .border_color(if on { theme::DIM } else { theme::LINE })
        .bg(if on { theme::CHOSEN } else { theme::PANEL2 })
        .text_color(if on { theme::TEXT } else { theme::DIM })
        .text_xs()
        .when(!first, |this| this.ml(px(-1.)))
        .when(first, |this| this.rounded_l(radius))
        .when(last, |this| this.rounded_r(radius))
        .cursor_pointer()
        .hover(|this| this.text_color(theme::TEXT))
        .child(label.into())
}

/// The prototype's button looks beyond the plain bordered one, which is
/// gpui-kit's default once [`theme::install`] has run.
pub trait ButtonLooks {
    /// The violet call to action, such as Publish.
    fn accent(self) -> Self;
    /// The green Approve.
    fn approve(self) -> Self;
    /// The red Reject, Cancel Run or Remove.
    fn reject(self) -> Self;
}

impl ButtonLooks for Button {
    fn accent(self) -> Self {
        self.primary().border_1().border_color(theme::ACCENT)
    }

    fn approve(self) -> Self {
        self.success().border_1().border_color(theme::PASS_LINE)
    }

    fn reject(self) -> Self {
        self.danger().border_1().border_color(theme::FAIL_LINE)
    }
}

/// The canvas's dotted backdrop: a dot every 18px, like the prototype's
/// `radial-gradient`. It fills its parent.
pub fn dot_grid() -> impl IntoElement {
    const STEP: f32 = 18.;
    canvas(
        |_, _, _| (),
        |bounds, (), window, _| {
            let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            let mut y = STEP / 2.;
            while y < height {
                let mut x = STEP / 2.;
                while x < width {
                    let at = bounds.origin + point(px(x - 1.), px(y - 1.));
                    window.paint_quad(fill(
                        Bounds::new(at, size(px(2.), px(2.))),
                        theme::CANVAS_DOT,
                    ));
                    x += STEP;
                }
                y += STEP;
            }
        },
    )
    .absolute()
    .size_full()
}
