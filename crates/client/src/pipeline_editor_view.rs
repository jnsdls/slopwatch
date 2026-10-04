//! The Pipeline editor: the palette of Library Steps and Plugins on the
//! left, the draft on the node canvas, and the inspector on the right
//! (Prototype: Pipeline graph view and editor). Drag a palette item onto the
//! canvas, a Step or the Gate to add it; drag a node's port onto a Step, the
//! Gate or its any-of box to wire it; drag a node to move it. The daemon
//! refuses what would make the Pipeline invalid, and the reason shows under
//! the canvas.
//!
//! Onboarding happens here too: the palette offers the Starters above the
//! Library, a Step lacking a Secret or Plugin carries a red badge, and a
//! coach-mark tour dims all but one element at a time ([`onboarding`]).

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Disableable, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_core::STARTERS;
use slopwatch_core::{GATE, GateRole, Target};
use slopwatch_protocol::pipeline::PipelineDraft;
use slopwatch_protocol::{Command, GateTerm, PullRequest, RepoName, SecretValue};

use crate::components::{
    ButtonLooks, ChipKind, PillTone, chip, dot_grid, mono, pill, plain_dot, section, well,
};
use crate::onboarding::{Anchor, Stop, Tour, intersect, place_card, watch_line};
use crate::outbox::Outbox;
use crate::outbox_view::{Pending, refusal};
use crate::pipeline_editor::{DropOn, PipelineEditor, Selection};
use crate::prs::toggle_watch;
use crate::run_graph::{
    self, ALL_OF_HEIGHT, ANY_OF_HEADER, GATE_BORDER, GATE_PADDING, GATE_TITLE_HEIGHT, Layout,
    NodeId, Rect, Role, TERM_HEIGHT,
};
use crate::run_graph_view::{edge_look, paint_edge};
use crate::theme;

const PALETTE_WIDTH: f32 = 230.;
const INSPECTOR_WIDTH: f32 = 330.;
const PORT: f32 = 18.;
/// The cards' width, and the coach card's height until it's measured.
const CARD_WIDTH: f32 = 340.;
const CARD_HEIGHT: f32 = 200.;
/// Room the spotlight leaves around its anchor.
const SPOT_PAD: f32 = 6.;
/// The tour's shade over everything but its spotlight: the prototype's
/// `#000b`.
const TOUR_SHADE: f32 = 0.73;

/// What the editor's buttons send for ([`Outbox`]). The toolbar and the
/// tour share them, so either shows the other's wait.
const PUBLISH: &str = "publish-pipeline";
const MERGE: &str = "merge-pipeline";
const DISCARD: &str = "discard-draft";
const PASTE: &str = "paste-set";

/// The roles the inspector offers, in order.
const ROLES: [(GateRole, &str); 4] = [
    (GateRole::Advisory, "Advisory"),
    (GateRole::Required, "Pass"),
    (GateRole::RequiredOrSkipped, "Pass or skipped"),
    (GateRole::AnyOf, "Any of"),
];

/// A palette item being dragged, by what it `uses`.
struct PaletteDrag(String);
/// A node being moved, by its Step id or `gate`.
struct NodeDrag(String);
/// A wire from a node's port, by its Step id or `gate`.
struct WireDrag(String);

/// What follows the mouse while dragging.
struct Ghost(Option<SharedString>);

impl Render for Ghost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        match &self.0 {
            Some(label) => div()
                .px_2()
                .py_1()
                .rounded(px(6.))
                .bg(theme::ACCENT)
                .text_color(theme::BG)
                .text_xs()
                .child(label.clone()),
            None => div(),
        }
    }
}

pub struct PipelineEditorView {
    editor: PipelineEditor,
    outbox: Outbox,
    with_input: Entity<InputState>,
    when_input: Entity<InputState>,
    /// The Step and the `with:` and Condition text the inputs were last
    /// given, so a new draft replaces them only when they change.
    inputs_show: Option<(String, String, String)>,
    /// The canvas's top left in the window, as last painted.
    origin: Rc<Cell<gpui_kit::Point<Pixels>>>,
    /// Where in a node the mouse grabbed it.
    grab: Rc<Cell<gpui_kit::Point<Pixels>>>,
    /// A node being moved, where it is now.
    moving: Option<(String, run_graph::Point)>,
    /// A wire being drawn from a port, to the mouse.
    wire: Option<(String, run_graph::Point)>,
    /// The onboarding tour over the editor.
    tour: Tour,
    /// The open repo's PRs, for the tour's Watch Stop.
    prs: Vec<PullRequest>,
    /// The Secret whose paste dialog is open.
    pasting: Option<String>,
    secret_input: Entity<InputState>,
    /// The editor's bounds in the window, as last painted.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The palette's Starters, as last painted, for the tour's spotlight.
    starters_at: Rc<Cell<Bounds<Pixels>>>,
    /// The canvas's visible part, as last painted. A spotlit node is cut
    /// to it.
    viewport: Rc<Cell<Bounds<Pixels>>>,
    canvas_scroll: ScrollHandle,
    /// The coach card, as last painted, so placing it knows its height.
    card_at: Rc<Cell<Bounds<Pixels>>>,
    /// This view, for the canvases that redraw it once they've measured.
    this: WeakEntity<Self>,
    /// The anchor the canvas last scrolled into sight for the tour, so it
    /// scrolls once per Stop and the developer can scroll away.
    revealed: Option<Anchor>,
    /// What the last gesture from a button went out for, whose refusal shows
    /// under the canvas until the next one.
    last_edit: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl PipelineEditorView {
    pub fn new(outbox: Outbox, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let with_input = cx.new(|cx| InputState::new(window, cx).placeholder("{ model: opus }"));
        let when_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("default: every upstream Step passed")
        });
        let secret_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Paste the value")
                .masked(true)
        });
        let _subscriptions = vec![
            cx.subscribe_in(
                &secret_input,
                window,
                |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.set_secret(cx);
                    }
                },
            ),
            cx.subscribe_in(
                &with_input,
                window,
                |this, input, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. })
                        && let Some(Selection::Step(id)) = this.editor.selected().cloned()
                    {
                        let text = input.read(cx).value().to_string();
                        let commands = this.editor.set_with_text(&id, &text);
                        this.send(commands);
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &when_input,
                window,
                |this, input, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. })
                        && let Some(Selection::Step(id)) = this.editor.selected().cloned()
                    {
                        let text = input.read(cx).value().to_string();
                        let commands = this.editor.set_condition_text(&id, &text);
                        this.send(commands);
                        cx.notify();
                    }
                },
            ),
        ];
        Self {
            editor: PipelineEditor::default(),
            outbox,
            with_input,
            when_input,
            inputs_show: None,
            origin: Rc::default(),
            grab: Rc::default(),
            moving: None,
            wire: None,
            tour: Tour::default(),
            prs: Vec::new(),
            pasting: None,
            secret_input,
            bounds: Rc::default(),
            starters_at: Rc::default(),
            viewport: Rc::default(),
            canvas_scroll: ScrollHandle::new(),
            card_at: Rc::default(),
            this: cx.entity().downgrade(),
            revealed: None,
            last_edit: None,
            _subscriptions,
        }
    }

    /// Opens the onboarding tour on the open repo.
    pub fn start_tour(&mut self, cx: &mut Context<Self>) {
        self.tour.start();
        self.revealed = None;
        cx.notify();
    }

    /// The open repo's PRs as the daemon last listed them.
    pub fn set_prs(&mut self, prs: Vec<PullRequest>, cx: &mut Context<Self>) {
        self.prs = prs;
        cx.notify();
    }

    /// Sends the pasted value for the Secret whose dialog is open. The
    /// dialog closes once the daemon has kept it, and the daemon sends the
    /// draft again, without that Step's badge.
    fn set_secret(&mut self, cx: &mut Context<Self>) {
        let value = self.secret_input.read(cx).value().to_string();
        let Some(secret) = self.pasting.clone() else {
            return;
        };
        if value.trim().is_empty() {
            return;
        }
        let command = Command::SetSecret {
            secret,
            value: SecretValue::new(value),
        };
        self.outbox.press(PASTE, command);
        cx.notify();
    }

    /// Opens the paste dialog on `secret`.
    fn paste(&mut self, secret: String, window: &mut Window, cx: &mut Context<Self>) {
        self.pasting = Some(secret);
        self.outbox.forget(PASTE);
        self.secret_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    /// What the editor waits on GitHub for, such as "Publishing…".
    fn busy(&self) -> Option<&'static str> {
        [
            (PUBLISH, "Publishing…"),
            (MERGE, "Merging…"),
            (DISCARD, "Starting over…"),
        ]
        .into_iter()
        .find(|(action, _)| self.outbox.waiting(action))
        .map(|(_, text)| text)
    }

    /// Whether another of the editor's GitHub actions is under way, which
    /// holds `action` back.
    fn held(&self, action: &str) -> bool {
        self.busy().is_some() && !self.outbox.waiting(action)
    }

    /// The refusal of the last publish, merge or discard.
    fn busy_refusal(&self) -> Option<Div> {
        [PUBLISH, MERGE, DISCARD]
            .into_iter()
            .find_map(|action| refusal(&self.outbox, action))
    }

    fn publish(&mut self, cx: &mut Context<Self>) {
        if self.busy().is_none()
            && let Some(command) = self.editor.publish()
        {
            self.outbox.press(PUBLISH, command);
        }
        cx.notify();
    }

    fn merge_now(&mut self, cx: &mut Context<Self>) {
        if self.busy().is_none()
            && let Some(command) = self.editor.merge_now()
        {
            self.outbox.press(MERGE, command);
        }
        cx.notify();
    }

    pub fn repo(&self) -> Option<&RepoName> {
        self.editor.repo()
    }

    /// Shows `repo`'s draft.
    pub fn open(&mut self, repo: &RepoName, cx: &mut Context<Self>) {
        if self.editor.repo() != Some(repo) {
            // The tour belongs to the repo it started on.
            self.tour.skip();
            self.pasting = None;
        }
        let commands = self.editor.open(repo);
        self.send(commands);
        cx.notify();
    }

    /// Follows the draft again on a new connection.
    pub fn reconnected(&self) {
        self.send(self.editor.reconnected());
    }

    pub fn apply(&mut self, draft: PipelineDraft, cx: &mut Context<Self>) {
        self.editor.apply(draft);
        cx.notify();
    }

    pub fn refused(&mut self, message: String, cx: &mut Context<Self>) {
        self.editor.refused(message);
        cx.notify();
    }

    fn send(&self, commands: Vec<Command>) {
        for command in commands {
            self.outbox.send(command);
        }
    }

    /// Sends a gesture's commands for the button `action`, unless its last
    /// one is still out.
    fn edit(&mut self, action: String, commands: Vec<Command>) {
        let mut commands = commands.into_iter();
        if let Some(first) = commands.next()
            && self.outbox.press(&action, first)
        {
            commands.for_each(|command| self.outbox.load(&action, command));
            self.last_edit = Some(action);
        }
    }

    /// Where the mouse is on the canvas.
    fn on_canvas(&self, position: gpui_kit::Point<Pixels>) -> run_graph::Point {
        let at = position - self.origin.get();
        run_graph::Point {
            x: f32::from(at.x),
            y: f32::from(at.y),
        }
    }

    fn palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let starters = self.starters(cx);
        let mut list = div()
            .id("palette")
            .w(px(PALETTE_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(10.))
            .bg(theme::PANEL)
            .border_r_1()
            .border_color(theme::LINE)
            .overflow_y_scroll()
            .child(starters)
            .child(section("Library · drag onto the canvas, a Step or the Gate").mt(px(8.)));
        let mut plugins_shown = false;
        for item in self.editor.palette() {
            if !item.uses.starts_with("lib/") && !plugins_shown {
                plugins_shown = true;
                list = list.child(section("Plugins").mt(px(8.)));
            }
            let uses = item.uses.clone();
            let label = SharedString::from(uses.clone());
            let detail = match &item.summary {
                Some(summary) => summary.clone(),
                None if item.write => format!("{} · terminal", item.plugin),
                None => item.plugin.clone(),
            };
            list = list.child(
                preset(SharedString::from(format!("palette-{uses}")))
                    .cursor_grab()
                    .child(mono(format!("+ {label}")).font_weight(FontWeight::SEMIBOLD))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::DIM)
                            .line_clamp(2)
                            .child(detail),
                    )
                    .when(!item.installed, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(theme::INC)
                                .child(format!("Plugin `{}` isn't installed", item.plugin)),
                        )
                    })
                    .on_drag(PaletteDrag(uses), move |_, _, _, cx| {
                        let label = label.clone();
                        cx.new(|_| Ghost(Some(label)))
                    }),
            );
        }
        list
    }

    /// The Starters, each a whole Pipeline that replaces the draft's Steps
    /// and Gate when picked.
    fn starters(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let mut list = div()
            .id("starters")
            .relative()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(section("Start from · replaces the Steps and Gate"));
        for starter in STARTERS {
            let key = starter.key;
            list = list.child(
                preset(SharedString::from(format!("starter-{key}")))
                    .cursor_pointer()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(starter.name))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::DIM)
                            .line_clamp(2)
                            .child(starter.blurb),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let commands = this.editor.pick_starter(key);
                        this.edit(format!("starter-{key}"), commands);
                        cx.notify();
                    })),
            );
        }
        list.child(bounds_of(&self.starters_at, &self.this))
    }

    fn canvas(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut layout = self.editor.layout();
        if let Some((node, at)) = &self.moving {
            let placed = [(node_id(node), *at)].into_iter().collect();
            layout = run_graph::place(layout, &placed);
        }
        let infos = self.editor.infos();
        let roles = run_graph::roles(&infos);
        let selected = self.editor.selected().cloned();

        let mut inner = div()
            .id("editor-canvas")
            .relative()
            .size_full()
            .min_w(px(layout.width + 200.))
            .min_h(px(layout.height + 120.))
            .child(self.edges(&layout).absolute().size_full())
            .on_drag_move::<NodeDrag>(cx.listener(
                |this, event: &DragMoveEvent<NodeDrag>, _, cx| {
                    let at = event.event.position - event.bounds.origin - this.grab.get();
                    let node = event.drag(cx).0.clone();
                    this.moving = Some((
                        node,
                        run_graph::Point {
                            x: f32::from(at.x),
                            y: f32::from(at.y),
                        },
                    ));
                    cx.notify();
                },
            ))
            .on_drag_move::<WireDrag>(cx.listener(
                |this, event: &DragMoveEvent<WireDrag>, _, cx| {
                    let at = event.event.position - event.bounds.origin;
                    let from = event.drag(cx).0.clone();
                    this.wire = Some((
                        from,
                        run_graph::Point {
                            x: f32::from(at.x),
                            y: f32::from(at.y),
                        },
                    ));
                    cx.notify();
                },
            ))
            .on_drop(cx.listener(|this, _: &NodeDrag, _, cx| {
                if let Some((node, at)) = this.moving.take() {
                    let commands = this.editor.move_node(&node, at);
                    this.send(commands);
                }
                cx.notify();
            }))
            .on_drop(cx.listener(|this, _: &WireDrag, _, cx| {
                this.wire = None;
                cx.notify();
            }))
            .on_drop(cx.listener(|this, dragged: &PaletteDrag, window, cx| {
                let at = this.on_canvas(window.mouse_position());
                // Center the new node under the mouse.
                let at = run_graph::Point {
                    x: at.x - run_graph::NODE_WIDTH / 2.,
                    y: at.y - run_graph::NODE_HEIGHT / 2.,
                };
                let commands = this.editor.drop_palette(&dragged.0, DropOn::Canvas(at));
                this.send(commands);
                cx.notify();
            }));
        for node in &layout.nodes {
            let element = match &node.id {
                NodeId::Gate => self
                    .gate_node(node.rect, selected == Some(Selection::Gate), cx)
                    .into_any_element(),
                NodeId::Step(id) => {
                    let Some(step) = self.editor.draft().and_then(|draft| draft.step(id)) else {
                        continue;
                    };
                    let role = roles.get(id.as_str()).copied().unwrap_or(Role::Advisory);
                    let open = selected == Some(Selection::Step(id.clone()));
                    self.step_node(step, role, open, node.rect, cx)
                        .into_any_element()
                }
            };
            inner = inner.child(element);
        }
        div()
            .flex_1()
            .min_h_0()
            .relative()
            .flex()
            .child(
                div()
                    .id("editor-canvas-scroll")
                    .relative()
                    .flex_1()
                    .bg(theme::BG)
                    .overflow_scroll()
                    .track_scroll(&self.canvas_scroll)
                    .child(dot_grid())
                    .child(inner),
            )
            .child(bounds_of(&self.viewport, &self.this))
    }

    /// The edges, and the wire being drawn. Records where the canvas is, for
    /// drops that land on it.
    fn edges(&self, layout: &Layout) -> Canvas<()> {
        let edges = layout.edges.clone();
        let wire = self.wire.as_ref().and_then(|(from, to)| {
            let start = layout.node(&node_id(from))?.rect;
            Some((
                run_graph::Point {
                    x: start.x + start.width,
                    y: start.y + start.height / 2.,
                },
                *to,
            ))
        });
        let origin = Rc::clone(&self.origin);
        let this = self.this.clone();
        canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| {
                if origin.get() != bounds.origin {
                    origin.set(bounds.origin);
                    // The tour's spotlight follows a scrolled canvas.
                    redraw(&this, cx);
                }
                for edge in &edges {
                    let (color, dashed) = edge_look(edge.kind);
                    paint_edge(window, bounds.origin, edge.start, edge.end, color, dashed);
                }
                if let Some((start, end)) = wire {
                    paint_edge(window, bounds.origin, start, end, theme::ACCENT, true);
                }
            },
        )
    }

    /// A node's output port: drag it to wire the node to another.
    fn port(&self, node: &str) -> impl IntoElement {
        div()
            .id(SharedString::from(format!("port-{node}")))
            .absolute()
            .right(px(-PORT / 2.))
            .top_1_2()
            .mt(px(-PORT / 2.))
            .size(px(PORT))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .border_2()
            .border_color(theme::ACCENT)
            .bg(theme::PANEL2)
            .text_color(theme::ACCENT)
            .text_size(px(13.))
            .line_height(px(13.))
            .font_weight(FontWeight::BOLD)
            .cursor_crosshair()
            .hover(|this| this.bg(theme::ACCENT).text_color(theme::BG))
            .child("+")
            .on_drag(WireDrag(node.to_owned()), |_, _, _, cx| {
                cx.new(|_| Ghost(None))
            })
    }

    fn step_node(
        &self,
        step: &slopwatch_protocol::pipeline::DraftStep,
        role: Role,
        open: bool,
        rect: Rect,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let info = &step.info;
        let badges = badges(step, cx);
        let id = info.id.clone();
        let mut chips = div().flex().gap_1().overflow_hidden();
        if let Some(condition) = &info.condition {
            chips = chips.child(chip(ChipKind::Condition, condition.clone()));
        }
        if role == Role::Advisory {
            chips = chips.child(chip(ChipKind::Advisory, "advisory"));
        }
        if info.write {
            chips = chips.child(chip(ChipKind::Write, "commit ends this Run"));
        }
        let (select, wired, dropped) = (id.clone(), id.clone(), id.clone());
        div()
            .id(SharedString::from(format!("editor-node-{id}")))
            .absolute()
            .left(px(rect.x))
            .top(px(rect.y))
            .w(px(rect.width))
            .h(px(rect.height))
            .rounded(px(10.))
            .when(role == Role::Advisory, |this| {
                this.bg(theme::ADVISORY_BG).border_dashed()
            })
            .when(role != Role::Advisory, |this| this.bg(theme::PANEL))
            .border(px(1.5))
            .border_color(
                if step.missing_secrets.is_empty() && step.missing_plugin.is_none() {
                    theme::LINE
                } else {
                    theme::FAIL_LINE
                },
            )
            .when(open, |this| this.border_2().border_color(theme::ACCENT))
            .drag_over::<WireDrag>(|style, _, _, _| style.border_2().border_color(theme::ACCENT))
            .drag_over::<PaletteDrag>(|style, _, _, _| style.border_2().border_color(theme::ACCENT))
            .cursor_move()
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .px(px(10.))
                    .py(px(8.))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(plain_dot(theme::MUTED))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(id.clone()),
                            ),
                    )
                    // What the Step lacks matters more than its Plugin's name.
                    .child(badges.unwrap_or_else(|| {
                        let uses = if step.uses == info.plugin {
                            info.plugin.clone()
                        } else {
                            format!("{} · {}", step.uses, info.plugin)
                        };
                        mono(if info.write {
                            format!("{uses} · terminal")
                        } else {
                            uses
                        })
                        .text_xs()
                        .text_color(theme::DIM)
                        .truncate()
                    }))
                    .child(chips),
            )
            .when(!info.write, |this| this.child(self.port(&id)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.editor.select(Some(Selection::Step(select.clone())));
                cx.notify();
            }))
            .on_drag(NodeDrag(id.clone()), {
                let grab = Rc::clone(&self.grab);
                move |_, offset, _, cx| {
                    grab.set(offset);
                    cx.new(|_| Ghost(None))
                }
            })
            .on_drop(cx.listener(move |this, dragged: &WireDrag, _, cx| {
                this.wire = None;
                let commands = this.editor.connect(&dragged.0, Target::Step(&wired));
                this.send(commands);
                cx.notify();
            }))
            .on_drop(cx.listener(move |this, dragged: &PaletteDrag, _, cx| {
                let commands = this.editor.drop_palette(&dragged.0, DropOn::Step(&dropped));
                this.send(commands);
                cx.notify();
            }))
    }

    fn gate_node(
        &self,
        rect: Rect,
        open: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let mut node = div()
            .id("editor-gate")
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
            .border_color(if open {
                theme::ACCENT
            } else {
                theme::GATE_LINE
            })
            .drag_over::<WireDrag>(|style, _, _, _| style.border_color(theme::ACCENT))
            .drag_over::<PaletteDrag>(|style, _, _, _| style.border_color(theme::ACCENT))
            .cursor_move()
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
                    .child(pill(PillTone::Plain, "not running")),
            )
            .child(
                div()
                    .h(px(ALL_OF_HEIGHT))
                    .text_size(theme::LABEL_SIZE)
                    .text_color(theme::DIM)
                    .child("ALL OF · drop a port here"),
            );
        for term in self.editor.gate_terms() {
            node = node.child(gate_term(term, cx));
        }
        if !self.editor.has_any_of() {
            node = node.child(any_of_box(Vec::new(), cx));
        }
        node.child(self.port(GATE))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.editor.select(Some(Selection::Gate));
                cx.notify();
            }))
            .on_drag(NodeDrag(GATE.to_owned()), {
                let grab = Rc::clone(&self.grab);
                move |_, offset, _, cx| {
                    grab.set(offset);
                    cx.new(|_| Ghost(None))
                }
            })
            .on_drop(cx.listener(|this, dragged: &WireDrag, _, cx| {
                this.wire = None;
                let commands = this.editor.connect(&dragged.0, Target::Gate);
                this.send(commands);
                cx.notify();
            }))
            .on_drop(cx.listener(|this, dragged: &PaletteDrag, _, cx| {
                let commands = this.editor.drop_palette(&dragged.0, DropOn::Gate);
                this.send(commands);
                cx.notify();
            }))
    }

    fn inspector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = div()
            .id("inspector")
            .w(px(INSPECTOR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(14.))
            .py(px(12.))
            .bg(theme::PANEL)
            .border_l_1()
            .border_color(theme::LINE)
            .overflow_y_scroll();
        match self.editor.selected().cloned() {
            None => pane.child(
                div()
                    .text_color(theme::DIM)
                    .child("Select a Step or the Gate to edit it."),
            ),
            Some(Selection::Gate) => pane.child(self.gate_inspector(cx)),
            Some(Selection::Step(id)) => pane.child(self.step_inspector(&id, cx)),
        }
    }

    fn step_inspector(&self, id: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(step) = self
            .editor
            .draft()
            .and_then(|draft| draft.step(id))
            .cloned()
        else {
            return div();
        };
        let label = |text: &'static str| div().text_xs().text_color(theme::DIM).child(text);
        let mut uses = div().flex().flex_wrap().gap_1();
        for item in self.editor.palette() {
            let (step_id, chosen) = (id.to_owned(), item.uses.clone());
            let action = format!("uses-{}", item.uses);
            uses = uses.child(
                Button::new(SharedString::from(action.clone()))
                    .label(item.uses.clone())
                    .xsmall()
                    .when(item.uses == step.uses, |button| button.accent())
                    .pending(self.outbox.waiting(&action))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let commands = this.editor.set_uses(&step_id, &chosen);
                        this.edit(action.clone(), commands);
                        cx.notify();
                    })),
            );
        }
        let mut needs = div().flex().flex_wrap().gap_1();
        let others = self
            .editor
            .infos()
            .into_iter()
            .map(|info| info.id)
            .filter(|other| other != id)
            .chain([GATE.to_owned()]);
        for other in others {
            let (step_id, need) = (id.to_owned(), other.clone());
            let action = format!("need-{other}");
            needs = needs.child(
                Button::new(SharedString::from(action.clone()))
                    .label(other.clone())
                    .xsmall()
                    .when(step.info.needs.contains(&other), |button| button.accent())
                    .pending(self.outbox.waiting(&action))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let commands = this.editor.toggle_need(&step_id, &need);
                        this.edit(action.clone(), commands);
                        cx.notify();
                    })),
            );
        }
        let role = self.editor.gate_role(id);
        let gate = match self.editor.gate_role_locked(id) {
            Some(reason) => div().text_xs().text_color(theme::DIM).child(reason),
            None => self.role_buttons("role", id, role, cx),
        };
        let removed = id.to_owned();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(theme::HEADING_SIZE)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(id.to_owned()),
                    )
                    .child(div().text_size(px(12.)).text_color(theme::DIM).child(
                        if step.info.write {
                            format!("Plugin {} · terminal", step.info.plugin)
                        } else {
                            format!("Plugin {}", step.info.plugin)
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(label("USES"))
                    .child(uses),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(label(
                        "WITH · overrides the Library Step key by key, Enter applies",
                    ))
                    .child(
                        Input::new(&self.with_input)
                            .small()
                            .font_family(theme::MONO),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(label("NEEDS · runs after these settle"))
                    .child(needs),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(label("CONDITION · Enter applies, empty for the default"))
                    .child(
                        Input::new(&self.when_input)
                            .small()
                            .font_family(theme::MONO),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(label("IN THE GATE"))
                    .child(gate),
            )
            .child(
                div().child(
                    Button::new("remove-step")
                        .label("Remove Step")
                        .small()
                        .reject()
                        .pending(self.outbox.waiting("remove-step"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            let commands = this.editor.remove_step(&removed);
                            this.edit("remove-step".to_owned(), commands);
                            cx.notify();
                        })),
                ),
            )
    }

    /// The Gate roles a Step can take, as a row of buttons with the one it
    /// has picked out. `prefix` keeps each row's actions apart.
    fn role_buttons(&self, prefix: &str, id: &str, role: GateRole, cx: &mut Context<Self>) -> Div {
        let mut row = div().flex().flex_wrap().gap(px(4.));
        for (each, name) in ROLES {
            let action = format!("{prefix}-{id}-{name}");
            let step_id = id.to_owned();
            row = row.child(
                Button::new(SharedString::from(action.clone()))
                    .label(name)
                    .xsmall()
                    .when(role == each, |button| button.accent())
                    .pending(self.outbox.waiting(&action))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let commands = this.editor.set_gate_role(&step_id, each);
                        this.edit(action.clone(), commands);
                        cx.notify();
                    })),
            );
        }
        row
    }

    fn gate_inspector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows = div().flex().flex_col();
        for info in self.editor.infos() {
            let id = info.id.clone();
            let role = self.editor.gate_role(&id);
            let control = match self.editor.gate_role_locked(&id) {
                Some(_) => div()
                    .text_xs()
                    .text_color(theme::DIM)
                    .child("after the Gate or terminal"),
                None => self.role_buttons("gate", &id, role, cx),
            };
            rows = rows.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .py(px(6.))
                    .border_b_1()
                    .border_color(theme::LINE)
                    .child(mono(info.id))
                    .child(control),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .text_size(theme::HEADING_SIZE)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Gate"),
            )
            .child(div().text_size(px(12.)).text_color(theme::DIM).child(
                "Shippable when every required term passes and one any-of term \
                         passes. Reads Verdicts only.",
            ))
            .child(rows)
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut footer = div()
            .flex()
            .flex_col()
            .gap_1()
            .px(px(14.))
            .py(px(6.))
            .border_t_1()
            .border_color(theme::LINE)
            .text_xs();
        let legend = "solid: needs · dashed: read by the Gate · dashed box: advisory · drag nodes \
                      to arrange, ports to wire";
        footer = footer.child(div().text_color(theme::DIM).child(legend));
        let notice = self.editor.notice().map(str::to_owned).or_else(|| {
            let action = self.last_edit.as_deref()?;
            self.outbox.error(action)
        });
        if let Some(notice) = notice {
            footer = footer.child(
                div()
                    .text_color(theme::FAIL)
                    .child(format!("Refused: {notice}")),
            );
        }
        if let Some(draft) = self.editor.draft() {
            for problem in &draft.problems {
                footer = footer.child(div().text_color(theme::INC).child(problem.clone()));
            }
            if !draft.conflicts.is_empty() {
                footer = footer.child(self.conflicts(draft, cx));
            }
        }
        footer
    }

    /// The nodes that stopped the last publish, each as the draft and the
    /// branch write it, and the way out: starting over from the branch.
    fn conflicts(&self, draft: &PipelineDraft, cx: &mut Context<Self>) -> impl IntoElement {
        let branch = &draft.base.branch;
        let version = |title: String, text: Option<&String>| {
            let lines = match text {
                Some(text) => text
                    .lines()
                    .map(|line| div().child(line.to_owned()))
                    .collect(),
                None => vec![div().text_color(theme::DIM).child("(removed)")],
            };
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_color(theme::DIM).child(title))
                .child(well().children(lines))
        };
        let mut panel = div()
            .flex()
            .flex_col()
            .gap_2()
            .p(px(10.))
            .rounded(px(8.))
            .border_1()
            .border_color(theme::FAIL_LINE)
            .bg(theme::ESCALATION_BG);
        for conflict in &draft.conflicts {
            panel = panel
                .child(
                    div()
                        .text_color(theme::FAIL_TEXT)
                        .child(format!("{} changed on {branch} too", conflict.node)),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(version("This draft".into(), conflict.draft.as_ref()))
                        .child(version(format!("{branch} now"), conflict.branch.as_ref())),
                );
        }
        panel.child(
            div().child(
                Button::new(DISCARD)
                    .label(format!("Start over from {branch}"))
                    .small()
                    .disabled(self.held(DISCARD))
                    .pending(self.outbox.waiting(DISCARD))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        if this.busy().is_none()
                            && let Some(command) = this.editor.discard()
                        {
                            this.outbox.press(DISCARD, command);
                        }
                        cx.notify();
                    })),
            ),
        )
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = match self.editor.draft() {
            None => "Loading the draft…".to_owned(),
            Some(draft) => {
                let edits = match draft.edits.len() {
                    0 => "No edits yet".to_owned(),
                    1 => "1 edit in the draft".to_owned(),
                    n => format!("{n} edits in the draft"),
                };
                let sha: String = draft.base.commit.chars().take(7).collect();
                format!("{edits} · from {}@{sha}", draft.base.branch)
            }
        };
        let published = self
            .editor
            .draft()
            .and_then(|draft| draft.published.clone());
        let busy = self.busy();
        div()
            .flex()
            .items_center()
            .gap_2()
            .px(px(14.))
            .py(px(8.))
            .border_b_1()
            .border_color(theme::LINE)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(theme::DIM)
                    .child(status),
            )
            .when_some(busy, |toolbar, busy| {
                toolbar.child(div().text_xs().text_color(theme::DIM).child(busy))
            })
            .children(self.busy_refusal())
            .when(!self.tour.on(), |toolbar| {
                toolbar.child(
                    Button::new("tour")
                        .label("?")
                        .small()
                        .tooltip("Take the tour again")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.start_tour(cx);
                        })),
                )
            })
            .child(
                Button::new("tidy")
                    .label("Tidy")
                    .small()
                    .disabled(!self.editor.arranged())
                    .pending(self.outbox.waiting("tidy"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        let commands = this.editor.tidy();
                        this.edit("tidy".to_owned(), commands);
                        cx.notify();
                    })),
            )
            .when_some(published, |toolbar, pr| {
                let url = pr.url.clone();
                toolbar
                    .child(
                        Button::new("pipeline-pr")
                            .label(format!("PR #{} ↗", pr.number))
                            .small()
                            .on_click(move |_: &ClickEvent, _, cx| cx.open_url(&url)),
                    )
                    .child(
                        Button::new("merge-pipeline")
                            .label("Merge it now")
                            .small()
                            .disabled(self.held(MERGE))
                            .pending(self.outbox.waiting(MERGE))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.merge_now(cx);
                            })),
                    )
            })
            .child(
                Button::new("publish-pipeline")
                    .label("Publish")
                    .small()
                    .accent()
                    .disabled(!self.editor.can_publish() || self.held(PUBLISH))
                    .pending(self.outbox.waiting(PUBLISH))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.publish(cx);
                    })),
            )
    }

    /// Gives the inspector's inputs the selected Step's values when those
    /// change, and leaves what the developer is typing alone otherwise.
    fn sync_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let show = match self.editor.selected() {
            Some(Selection::Step(id)) => Some((
                id.clone(),
                self.editor.with_text(id),
                self.editor.condition_text(id),
            )),
            _ => None,
        };
        if show == self.inputs_show {
            return;
        }
        if let Some((_, with, when)) = &show {
            self.with_input
                .update(cx, |input, cx| input.set_value(with.clone(), window, cx));
            self.when_input
                .update(cx, |input, cx| input.set_value(when.clone(), window, cx));
        }
        self.inputs_show = show;
    }
}

impl Render for PipelineEditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_inputs(window, cx);
        if self.pasting.is_some() && self.outbox.done(PASTE) {
            self.pasting = None;
            self.secret_input
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        // A drag let go outside the canvas leaves things where they were.
        if !cx.has_active_drag() {
            self.moving = None;
            self.wire = None;
        }
        let overlay = self.overlay(cx);
        div()
            .flex_1()
            .h_full()
            .relative()
            .flex()
            .overflow_hidden()
            .child(bounds_of(&self.bounds, &self.this))
            .child(self.palette(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(self.toolbar(cx))
                    .child(self.canvas(cx))
                    .child(self.footer(cx)),
            )
            .child(self.inspector(cx))
            .children(overlay)
    }
}

impl PipelineEditorView {
    /// The editor's own bounds, from its top left.
    fn room(&self) -> run_graph::Rect {
        let size = self.bounds.get().size;
        run_graph::Rect {
            x: 0.,
            y: 0.,
            width: f32::from(size.width),
            height: f32::from(size.height),
        }
    }

    /// What sits over the editor: the paste dialog, or the tour's spotlight
    /// and card.
    fn overlay(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if let Some(secret) = self.pasting.clone() {
            return vec![self.paste_dialog(&secret, cx).into_any_element()];
        }
        let Some(draft) = self.editor.draft().cloned() else {
            return Vec::new();
        };
        let Some((place, count, stop)) = self.tour.current(&draft) else {
            return Vec::new();
        };
        let room = self.room();
        let layout = self.editor.layout();
        if self.revealed.as_ref() != Some(&stop.anchor()) {
            self.revealed = Some(stop.anchor());
            let node = match stop.anchor() {
                Anchor::Gate => layout.node(&NodeId::Gate),
                Anchor::Step(id) => layout.node(&NodeId::Step(id)),
                Anchor::Starters | Anchor::Center => None,
            };
            if let Some(node) = node {
                self.reveal(node.rect);
            }
        }
        let root = self.bounds.get();
        let origin = self.origin.get() - root.origin;
        let on_root = |rect: run_graph::Rect| run_graph::Rect {
            x: rect.x + f32::from(origin.x),
            y: rect.y + f32::from(origin.y),
            ..rect
        };
        let local = |at: Bounds<Pixels>| run_graph::Rect {
            x: f32::from(at.origin.x - root.origin.x),
            y: f32::from(at.origin.y - root.origin.y),
            width: f32::from(at.size.width),
            height: f32::from(at.size.height),
        };
        let viewport = local(self.viewport.get());
        let anchor = match stop.anchor() {
            Anchor::Center => None,
            Anchor::Starters => Some(local(self.starters_at.get())),
            // A node scrolled out of sight gets a card in the middle.
            Anchor::Gate => layout
                .node(&NodeId::Gate)
                .and_then(|node| intersect(on_root(node.rect), viewport)),
            Anchor::Step(id) => layout
                .node(&NodeId::Step(id))
                .and_then(|node| intersect(on_root(node.rect), viewport)),
        };
        let card = self.coach_card(place, count, &stop, &draft, cx);
        let Some(anchor) = anchor else {
            return vec![dim(room, None), centered(room, card).into_any_element()];
        };
        let keep_clear: Vec<run_graph::Rect> = layout
            .nodes
            .iter()
            .filter_map(|node| intersect(on_root(node.rect), viewport))
            .collect();
        let measured = f32::from(self.card_at.get().size.height);
        let height = if measured > 0. { measured } else { CARD_HEIGHT };
        let at = place_card(anchor, (CARD_WIDTH, height), room, &keep_clear);
        vec![
            dim(room, Some(anchor)),
            card.absolute()
                .left(px(at.x))
                .top(px(at.y))
                .into_any_element(),
        ]
    }

    /// Scrolls the canvas so `rect`, in canvas pixels, is in sight.
    fn reveal(&self, rect: run_graph::Rect) {
        const MARGIN: f32 = 40.;
        let size = self.viewport.get().size;
        let (width, height) = (f32::from(size.width), f32::from(size.height));
        let offset = self.canvas_scroll.offset();
        let axis = |offset: f32, start: f32, length: f32, room: f32| {
            let seen = -offset;
            if start + length + MARGIN > seen + room {
                -(start + length + MARGIN - room).min(start - MARGIN).max(0.)
            } else if start < seen {
                -(start - MARGIN).max(0.)
            } else {
                offset
            }
        };
        let to = gpui_kit::Point {
            x: px(axis(f32::from(offset.x), rect.x, rect.width, width)),
            y: px(axis(f32::from(offset.y), rect.y, rect.height, height)),
        };
        if to != offset {
            // The canvas then paints at its new origin, which redraws.
            self.canvas_scroll.set_offset(to);
        }
    }

    fn coach_card(
        &mut self,
        place: usize,
        count: usize,
        stop: &Stop,
        draft: &PipelineDraft,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let last = place + 1 == count;
        let mut card = div()
            .id("coach-card")
            .relative()
            .child(bounds_of(&self.card_at, &self.this))
            .occlude()
            .w(px(CARD_WIDTH))
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(14.))
            .rounded(px(12.))
            .border_1()
            .border_color(theme::ACCENT)
            .bg(theme::PANEL)
            .shadow_lg()
            .child(
                div()
                    .text_size(theme::LABEL_SIZE)
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::ACCENT)
                    .child(format!("STEP {} OF {count}", place + 1)),
            )
            .child(
                div()
                    .text_size(theme::HEADING_SIZE)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(stop.title()),
            )
            .child(
                div()
                    .text_color(theme::DIM)
                    .child(stop.body(&draft.base.branch)),
            );
        match stop {
            Stop::Publish => card = card.child(self.publish_actions(draft, cx)),
            Stop::Watch => card = card.child(self.watch_list(draft, cx)),
            _ => {}
        }
        let mut footer = div().flex().items_center().gap_2().mt(px(6.)).child(
            div()
                .id("skip-tour")
                .text_xs()
                .text_color(theme::DIM)
                .cursor_pointer()
                .hover(|this| this.underline())
                .child("Skip tour")
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.tour.skip();
                    cx.notify();
                })),
        );
        footer = footer.child(div().flex_1());
        if place > 0 {
            footer = footer.child(Button::new("tour-back").label("Back").small().on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.tour.back();
                    cx.notify();
                }),
            ));
        }
        if let Some(label) = stop.next_label(last, stop.done(draft)) {
            footer = footer.child(
                Button::new("tour-next")
                    .label(label)
                    .small()
                    .accent()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        if let Some(draft) = this.editor.draft().cloned() {
                            this.tour.next(&draft);
                        }
                        cx.notify();
                    })),
            );
        }
        card.child(footer)
    }

    /// Publish as a PR, then "Merge it now" (ADR 0007).
    fn publish_actions(&self, draft: &PipelineDraft, cx: &mut Context<Self>) -> Div {
        let busy = self.busy();
        let row = div().flex().items_center().gap_2();
        match &draft.published {
            None => row.child(
                Button::new("tour-publish")
                    .label("Publish as PR")
                    .small()
                    .accent()
                    .disabled(!self.editor.can_publish() || self.held(PUBLISH))
                    .pending(self.outbox.waiting(PUBLISH))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.publish(cx);
                    })),
            ),
            Some(pr) => row.child(format!("PR #{} is open.", pr.number)).child(
                Button::new("tour-merge")
                    .label("Merge it now")
                    .small()
                    .accent()
                    .disabled(self.held(MERGE))
                    .pending(self.outbox.waiting(MERGE))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.merge_now(cx);
                    })),
            ),
        }
        .children(busy.map(|busy| div().text_xs().text_color(theme::DIM).child(busy)))
        .children(self.busy_refusal())
    }

    /// The repo's open PRs, each with Watch or Unwatch.
    fn watch_list(&self, draft: &PipelineDraft, cx: &mut Context<Self>) -> Div {
        let mut list = div().flex().flex_col();
        if self.prs.is_empty() {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(theme::DIM)
                    .child("You have no open PRs here yet."),
            );
        }
        for pr in &self.prs {
            let (number, watched) = (pr.number, pr.watched());
            let (action, command) = toggle_watch(pr);
            let refused = refusal(&self.outbox, &action);
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py(px(6.))
                    .border_b_1()
                    .border_color(theme::LINE)
                    .text_xs()
                    .child(
                        Button::new(SharedString::from(format!("tour-watch-{number}")))
                            .label(if watched { "Unwatch" } else { "Watch" })
                            .xsmall()
                            .when(!watched, |button| button.accent())
                            .pending(self.outbox.waiting(&action))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                                this.outbox.press(&action, command.clone());
                            })),
                    )
                    .child(div().text_color(theme::DIM).child(format!("#{number}")))
                    .child(div().flex_1().min_w_0().truncate().child(pr.title.clone()))
                    .children(watch_line(pr).map(|line| div().text_color(theme::INC).child(line)))
                    .children(refused),
            );
        }
        if draft.published.is_some() {
            list = list.child(self.publish_actions(draft, cx).mt(px(8.)));
        }
        list
    }

    /// Where the developer pastes a Secret a Step lacks. The value goes to
    /// the daemon and never shows again.
    fn paste_dialog(&self, secret: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let room = self.room();
        let card = div()
            .id("paste-secret")
            .occlude()
            .w(px(CARD_WIDTH))
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(16.))
            .rounded(px(10.))
            .border_1()
            .border_color(theme::DIM)
            .bg(theme::PANEL)
            .shadow_lg()
            .child(mono(secret.to_owned()).font_weight(FontWeight::BOLD))
            .child(div().text_xs().text_color(theme::DIM).child(
                "The daemon keeps it in the Keychain and never shows it back. Every Step that needs it gets it.",
            ))
            .child(Input::new(&self.secret_input).small())
            .children(refusal(&self.outbox, PASTE))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("paste-cancel")
                            .label("Cancel")
                            .small()
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.pasting = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(PASTE)
                            .label("Set Secret")
                            .small()
                            .accent()
                            .pending(self.outbox.waiting(PASTE))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.set_secret(cx);
                            })),
                    ),
            );
        div()
            .absolute()
            .size_full()
            .child(dim(room, None))
            .child(centered(room, card))
    }
}

/// An element that fills its parent and records the parent's bounds in the
/// window as it paints, for overlays placed against them.
fn bounds_of(
    cell: &Rc<Cell<Bounds<Pixels>>>,
    this: &WeakEntity<PipelineEditorView>,
) -> impl IntoElement {
    let (cell, this) = (Rc::clone(cell), this.clone());
    canvas(
        |_, _, _| (),
        move |bounds, (), _, cx| {
            if cell.get() != bounds {
                cell.set(bounds);
                // Overlays placed on the old bounds move on the next frame.
                redraw(&this, cx);
            }
        },
    )
    .absolute()
    .size_full()
}

/// Renders the editor again once this frame is done, since a view only
/// renders when notified.
fn redraw(this: &WeakEntity<PipelineEditorView>, cx: &mut App) {
    let this = this.clone();
    cx.defer(move |cx| {
        // Gone with its window is fine.
        let _ = this.update(cx, |_, cx| cx.notify());
    });
}

/// Dims `room` except a spotlight around `spot`, if any. It takes no
/// clicks, so the spotlit element stays usable.
fn dim(room: run_graph::Rect, spot: Option<run_graph::Rect>) -> AnyElement {
    let shade = Rgba {
        a: TOUR_SHADE,
        ..theme::WELL
    };
    let panel = |x: f32, y: f32, width: f32, height: f32| {
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(width.max(0.)))
            .h(px(height.max(0.)))
            .bg(shade)
    };
    let Some(spot) = spot else {
        return panel(room.x, room.y, room.width, room.height).into_any_element();
    };
    let (left, top) = (spot.x - SPOT_PAD, spot.y - SPOT_PAD);
    let (right, bottom) = (
        spot.x + spot.width + SPOT_PAD,
        spot.y + spot.height + SPOT_PAD,
    );
    div()
        .absolute()
        .size_full()
        .child(panel(0., 0., room.width, top))
        .child(panel(0., bottom, room.width, room.height - bottom))
        .child(panel(0., top, left, bottom - top))
        .child(panel(right, top, room.width - right, bottom - top))
        .child(
            div()
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(right - left))
                .h(px(bottom - top))
                .rounded(px(12.))
                .border_2()
                .border_color(theme::ACCENT),
        )
        .into_any_element()
}

/// `card` in the middle of `room`.
fn centered(room: run_graph::Rect, card: impl IntoElement) -> Div {
    div()
        .absolute()
        .left(px(room.x))
        .top(px(room.y))
        .w(px(room.width))
        .h(px(room.height))
        .flex()
        .items_center()
        .justify_center()
        .child(card)
}

/// The red badges on a Step that lacks something: one per Secret, which
/// opens the paste dialog, and one for a Plugin that isn't installed.
fn badges(
    step: &slopwatch_protocol::pipeline::DraftStep,
    cx: &mut Context<PipelineEditorView>,
) -> Option<Div> {
    if step.missing_secrets.is_empty() && step.missing_plugin.is_none() {
        return None;
    }
    let mut row = div().flex().gap_1().overflow_hidden();
    for secret in &step.missing_secrets {
        let name = secret.clone();
        row = row.child(
            div()
                .id(SharedString::from(format!(
                    "badge-{}-{secret}",
                    step.info.id
                )))
                .min_w_0()
                .cursor_pointer()
                .child(chip(ChipKind::Bad, format!("set {secret}")))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.paste(name.clone(), window, cx);
                })),
        );
    }
    if let Some(plugin) = &step.missing_plugin {
        row = row.child(chip(ChipKind::Bad, format!("{plugin} isn't installed")));
    }
    Some(row)
}

fn node_id(node: &str) -> NodeId {
    if node == GATE {
        NodeId::Gate
    } else {
        NodeId::Step(node.to_owned())
    }
}

fn gate_term(term: &GateTerm, cx: &mut Context<PipelineEditorView>) -> AnyElement {
    match term {
        GateTerm::Step {
            id,
            accepts_skipped,
        } => div()
            .h(px(TERM_HEIGHT))
            .flex()
            .items_center()
            .gap(px(6.))
            .text_size(px(12.))
            .child(plain_dot(theme::MUTED))
            .child(div().min_w_0().truncate().child(id.clone()))
            .when(*accepts_skipped, |this| {
                this.child(chip(ChipKind::Required, "skip ok").flex_none())
            })
            .into_any_element(),
        GateTerm::AnyOf { terms } => any_of_box(terms.clone(), cx).into_any_element(),
        GateTerm::Other { text } => div()
            .h(px(TERM_HEIGHT))
            .flex()
            .items_center()
            .text_size(px(12.))
            .truncate()
            .child(text.clone())
            .into_any_element(),
    }
}

/// The Gate's any-of group, where a port's wire can land to join it.
/// Without terms it's the empty box that starts one.
fn any_of_box(
    terms: Vec<GateTerm>,
    cx: &mut Context<PipelineEditorView>,
) -> impl IntoElement + use<> {
    let empty = terms.is_empty();
    let mut group = div()
        .id(if empty { "any-of-empty" } else { "any-of" })
        .flex()
        .flex_col()
        .pl(px(8.))
        .map(|this| {
            if empty {
                this.mt(px(4.))
                    .pr(px(8.))
                    .border_1()
                    .border_dashed()
                    .rounded(px(6.))
            } else {
                this.border_l_2()
            }
        })
        .border_color(theme::GATE_LINE)
        .drag_over::<WireDrag>(|style, _, _, _| style.bg(theme::ACCENT_BG))
        .child(
            div()
                .h(px(ANY_OF_HEADER))
                .flex()
                .items_center()
                .text_size(theme::LABEL_SIZE)
                .text_color(theme::DIM)
                .child(if empty {
                    "ANY OF · drop here"
                } else {
                    "ANY OF"
                }),
        )
        .on_drop(cx.listener(|this, dragged: &WireDrag, _, cx| {
            this.wire = None;
            let commands = this.editor.connect(&dragged.0, Target::GateAnyOf);
            this.send(commands);
            cx.notify();
        }));
    for term in &terms {
        group = group.child(gate_term(term, cx));
    }
    group
}

/// A card in the palette, a Starter or a Library Step: the prototype's
/// `.preset`.
fn preset(id: SharedString) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap(px(2.))
        .px(px(9.))
        .py(px(7.))
        .rounded(px(8.))
        .border_1()
        .border_color(theme::LINE)
        .bg(theme::PANEL)
        .hover(|this| this.border_color(theme::DIM))
}
