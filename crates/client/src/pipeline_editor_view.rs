//! The Pipeline editor: the palette of Library Steps and Plugins on the
//! left, the draft on the node canvas, and the inspector on the right
//! (Prototype: Pipeline graph view and editor). Drag a palette item onto the
//! canvas, a Step or the Gate to add it; drag a node's port onto a Step, the
//! Gate or its any-of box to wire it; drag a node to move it. The daemon
//! refuses what would make the Pipeline invalid, and the reason shows under
//! the canvas.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::theme::Theme;
use gpui_kit::component::{ActiveTheme, Disableable, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_core::{GATE, GateRole, Target};
use slopwatch_protocol::pipeline::PipelineDraft;
use slopwatch_protocol::{Command, GateTerm, RepoName};

use crate::pipeline_editor::{DropOn, PipelineEditor, Selection};
use crate::run_graph::{
    self, ALL_OF_HEIGHT, ANY_OF_HEADER, EdgeKind, GATE_BORDER, GATE_PADDING, GATE_TITLE_HEIGHT,
    Layout, NodeId, Rect, Role, TERM_HEIGHT,
};
use crate::run_graph_view::{Colors, chip, dot, paint_edge};

const PALETTE_WIDTH: f32 = 220.;
const INSPECTOR_WIDTH: f32 = 300.;
const PORT: f32 = 14.;

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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        match &self.0 {
            Some(label) => div()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(theme.ring)
                .bg(theme.background)
                .text_xs()
                .child(label.clone()),
            None => div(),
        }
    }
}

pub struct PipelineEditorView {
    editor: PipelineEditor,
    commands: Sender<Command>,
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
    _subscriptions: Vec<Subscription>,
}

impl PipelineEditorView {
    pub fn new(commands: Sender<Command>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let with_input = cx.new(|cx| InputState::new(window, cx).placeholder("{ model: opus }"));
        let when_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("default: every upstream Step passed")
        });
        let _subscriptions = vec![
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
            commands,
            with_input,
            when_input,
            inputs_show: None,
            origin: Rc::default(),
            grab: Rc::default(),
            moving: None,
            wire: None,
            _subscriptions,
        }
    }

    pub fn repo(&self) -> Option<&RepoName> {
        self.editor.repo()
    }

    /// Shows `repo`'s draft.
    pub fn open(&mut self, repo: &RepoName, cx: &mut Context<Self>) {
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
            // The link thread only stops when the app quits.
            let _ = self.commands.send(command);
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
        let theme = cx.theme();
        let mut list = div()
            .id("palette")
            .w(px(PALETTE_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .border_r_1()
            .border_color(theme.border)
            .overflow_y_scroll()
            .child(
                div()
                    .px_1()
                    .pb_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("LIBRARY · drag onto the canvas, a Step or the Gate"),
            );
        let mut plugins_shown = false;
        for item in self.editor.palette() {
            if !item.uses.starts_with("lib/") && !plugins_shown {
                plugins_shown = true;
                list = list.child(
                    div()
                        .px_1()
                        .pt_2()
                        .pb_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("PLUGINS"),
                );
            }
            let uses = item.uses.clone();
            let label = SharedString::from(uses.clone());
            let detail = match &item.summary {
                Some(summary) => summary.clone(),
                None if item.write => format!("{} · terminal", item.plugin),
                None => item.plugin.clone(),
            };
            list = list.child(
                div()
                    .id(SharedString::from(format!("palette-{uses}")))
                    .flex()
                    .flex_col()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .cursor_grab()
                    .hover(|this| this.bg(theme.list_hover))
                    .child(div().text_sm().font_family("Menlo").child(label.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .line_clamp(2)
                            .child(detail),
                    )
                    .when(!item.installed, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(theme.warning)
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

    fn canvas(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let colors = Colors::new(&theme);
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
            .child(self.edges(&layout, colors).absolute().size_full())
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
                    .gate_node(
                        node.rect,
                        selected == Some(Selection::Gate),
                        &theme,
                        colors,
                        cx,
                    )
                    .into_any_element(),
                NodeId::Step(id) => {
                    let Some(step) = self.editor.draft().and_then(|draft| draft.step(id)) else {
                        continue;
                    };
                    let role = roles.get(id.as_str()).copied().unwrap_or(Role::Advisory);
                    let open = selected == Some(Selection::Step(id.clone()));
                    self.step_node(&step.info, role, open, node.rect, &theme, cx)
                        .into_any_element()
                }
            };
            inner = inner.child(element);
        }
        div()
            .id("editor-canvas-scroll")
            .flex_1()
            .min_h_0()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.muted)
            .overflow_scroll()
            .child(inner)
    }

    /// The edges, and the wire being drawn. Records where the canvas is, for
    /// drops that land on it.
    fn edges(&self, layout: &Layout, colors: Colors) -> Canvas<()> {
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
        canvas(
            |_, _, _| (),
            move |bounds, (), window, _| {
                origin.set(bounds.origin);
                for edge in &edges {
                    let (color, dashed) = match edge.kind {
                        EdgeKind::Needs => (colors.edge, false),
                        EdgeKind::GateReads => (colors.gate, true),
                    };
                    paint_edge(window, bounds.origin, edge.start, edge.end, color, dashed);
                }
                if let Some((start, end)) = wire {
                    paint_edge(window, bounds.origin, start, end, colors.gate, true);
                }
            },
        )
    }

    /// A node's output port: drag it to wire the node to another.
    fn port(&self, node: &str, theme: &Theme) -> impl IntoElement {
        div()
            .id(SharedString::from(format!("port-{node}")))
            .absolute()
            .right(px(-PORT / 2.))
            .top_1_2()
            .mt(px(-PORT / 2.))
            .size(px(PORT))
            .rounded_full()
            .border_2()
            .border_color(theme.ring)
            .bg(theme.background)
            .cursor_crosshair()
            .hover(|this| this.bg(theme.ring))
            .on_drag(WireDrag(node.to_owned()), |_, _, _, cx| {
                cx.new(|_| Ghost(None))
            })
    }

    fn step_node(
        &self,
        info: &slopwatch_protocol::StepInfo,
        role: Role,
        open: bool,
        rect: Rect,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = info.id.clone();
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
        let (select, wired, dropped) = (id.clone(), id.clone(), id.clone());
        div()
            .id(SharedString::from(format!("editor-node-{id}")))
            .absolute()
            .left(px(rect.x))
            .top(px(rect.y))
            .w(px(rect.width))
            .h(px(rect.height))
            .rounded_lg()
            .bg(theme.background)
            .border_1()
            .border_color(theme.border)
            .when(role == Role::Advisory, |this| this.border_dashed())
            .when(open, |this| this.border_2().border_color(theme.ring))
            .hover(|this| this.bg(theme.list_hover))
            .drag_over::<WireDrag>(|style, _, _, cx| style.border_2().border_color(cx.theme().ring))
            .drag_over::<PaletteDrag>(|style, _, _, cx| {
                style.border_2().border_color(cx.theme().ring)
            })
            .cursor_move()
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .px_2()
                    .py_1p5()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .child(dot(theme.muted_foreground))
                            .child(div().flex_1().text_sm().truncate().child(id.clone())),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(if info.write {
                                format!("{} · terminal", info.plugin)
                            } else {
                                info.plugin.clone()
                            }),
                    )
                    .child(chips),
            )
            .when(!info.write, |this| this.child(self.port(&id, theme)))
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
        theme: &Theme,
        colors: Colors,
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
            .rounded_xl()
            .bg(theme.background)
            .border(px(GATE_BORDER))
            .border_color(colors.gate)
            .when(open, |this| this.border_color(theme.ring))
            .drag_over::<WireDrag>(|style, _, _, cx| style.border_color(cx.theme().ring))
            .drag_over::<PaletteDrag>(|style, _, _, cx| style.border_color(cx.theme().ring))
            .cursor_move()
            .child(
                div()
                    .h(px(GATE_TITLE_HEIGHT))
                    .flex()
                    .items_center()
                    .text_xs()
                    .child(div().font_weight(FontWeight::BOLD).child("GATE")),
            )
            .child(
                div()
                    .h(px(ALL_OF_HEIGHT))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("ALL OF · drop a port here"),
            );
        for term in self.editor.gate_terms() {
            node = node.child(gate_term(term, theme, colors, cx));
        }
        if !self.editor.has_any_of() {
            node = node.child(any_of_box(Vec::new(), theme, colors, cx));
        }
        node.child(self.port(GATE, theme))
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
        let theme = cx.theme().clone();
        let pane = div()
            .id("inspector")
            .w(px(INSPECTOR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .border_l_1()
            .border_color(theme.border)
            .overflow_y_scroll()
            .text_sm();
        match self.editor.selected().cloned() {
            None => pane.child(
                div()
                    .text_color(theme.muted_foreground)
                    .child("Select a Step or the Gate to edit it."),
            ),
            Some(Selection::Gate) => pane.child(self.gate_inspector(&theme, cx)),
            Some(Selection::Step(id)) => pane.child(self.step_inspector(&id, &theme, cx)),
        }
    }

    fn step_inspector(&self, id: &str, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(step) = self
            .editor
            .draft()
            .and_then(|draft| draft.step(id))
            .cloned()
        else {
            return div();
        };
        let label = |text: &'static str| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let mut uses = div().flex().flex_wrap().gap_1();
        for item in self.editor.palette() {
            let (step_id, chosen) = (id.to_owned(), item.uses.clone());
            uses = uses.child(
                Button::new(SharedString::from(format!("uses-{}", item.uses)))
                    .label(item.uses.clone())
                    .xsmall()
                    .outline()
                    .when(item.uses == step.uses, |button| button.primary())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let commands = this.editor.set_uses(&step_id, &chosen);
                        this.send(commands);
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
            needs = needs.child(
                Button::new(SharedString::from(format!("need-{other}")))
                    .label(other.clone())
                    .xsmall()
                    .outline()
                    .when(step.info.needs.contains(&other), |button| button.primary())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let commands = this.editor.toggle_need(&step_id, &need);
                        this.send(commands);
                        cx.notify();
                    })),
            );
        }
        let role = self.editor.gate_role(id);
        let gate = match self.editor.gate_role_locked(id) {
            Some(reason) => div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(reason),
            None => {
                let step_id = id.to_owned();
                div().child(
                    ButtonGroup::new("gate-role")
                        .xsmall()
                        .outline()
                        .children(ROLES.map(|(each, name)| {
                            Button::new(SharedString::from(format!("role-{name}")))
                                .label(name)
                                .when(role == each, |button| button.primary())
                        }))
                        .on_click(cx.listener(move |this, clicked: &Vec<usize>, _, cx| {
                            if let Some(&(role, _)) = clicked.first().and_then(|&i| ROLES.get(i)) {
                                let commands = this.editor.set_gate_role(&step_id, role);
                                this.send(commands);
                                cx.notify();
                            }
                        })),
                )
            }
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
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::BOLD)
                            .child(id.to_owned()),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
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
                    .child(Input::new(&self.with_input).small().font_family("Menlo")),
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
                    .child(Input::new(&self.when_input).small().font_family("Menlo")),
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
                        .danger()
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            let commands = this.editor.remove_step(&removed);
                            this.send(commands);
                            cx.notify();
                        })),
                ),
            )
    }

    fn gate_inspector(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows = div().flex().flex_col().gap_2();
        for info in self.editor.infos() {
            let id = info.id.clone();
            let role = self.editor.gate_role(&id);
            let control = match self.editor.gate_role_locked(&id) {
                Some(_) => div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("after the Gate or terminal")
                    .into_any_element(),
                None => ButtonGroup::new(SharedString::from(format!("gate-role-{id}")))
                    .xsmall()
                    .outline()
                    .children(ROLES.map(|(each, name)| {
                        Button::new(SharedString::from(format!("gate-{id}-{name}")))
                            .label(name)
                            .when(role == each, |button| button.primary())
                    }))
                    .on_click(cx.listener(move |this, clicked: &Vec<usize>, _, cx| {
                        if let Some(&(role, _)) = clicked.first().and_then(|&i| ROLES.get(i)) {
                            let commands = this.editor.set_gate_role(&id, role);
                            this.send(commands);
                            cx.notify();
                        }
                    }))
                    .into_any_element(),
            };
            rows = rows.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().font_family("Menlo").text_xs().child(info.id))
                    .child(control),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::BOLD)
                    .child("Gate"),
            )
            .child(div().text_xs().text_color(theme.muted_foreground).child(
                "Shippable when every required term passes and one any-of term \
                         passes. Reads Verdicts only.",
            ))
            .child(rows)
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut footer = div().flex().flex_col().gap_1().text_xs();
        let legend = "Solid: needs · dashed: read by the Gate · dashed box: advisory · drag nodes \
                      to arrange, ports to wire";
        footer = footer.child(div().text_color(theme.muted_foreground).child(legend));
        if let Some(notice) = self.editor.notice() {
            footer = footer.child(
                div()
                    .text_color(theme.danger)
                    .child(format!("Refused: {notice}")),
            );
        }
        if let Some(draft) = self.editor.draft() {
            for problem in &draft.problems {
                footer = footer.child(div().text_color(theme.warning).child(problem.clone()));
            }
        }
        footer
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
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
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(status),
            )
            .child(
                Button::new("tidy")
                    .label("Tidy")
                    .small()
                    .ghost()
                    .disabled(!self.editor.arranged())
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        let commands = this.editor.tidy();
                        this.send(commands);
                        cx.notify();
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
        // A drag let go outside the canvas leaves things where they were.
        if !cx.has_active_drag() {
            self.moving = None;
            self.wire = None;
        }
        div()
            .flex_1()
            .h_full()
            .flex()
            .overflow_hidden()
            .child(self.palette(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .child(self.toolbar(cx))
                    .child(self.canvas(cx))
                    .child(self.footer(cx)),
            )
            .child(self.inspector(cx))
    }
}

fn node_id(node: &str) -> NodeId {
    if node == GATE {
        NodeId::Gate
    } else {
        NodeId::Step(node.to_owned())
    }
}

fn gate_term(
    term: &GateTerm,
    theme: &Theme,
    colors: Colors,
    cx: &mut Context<PipelineEditorView>,
) -> AnyElement {
    match term {
        GateTerm::Step {
            id,
            accepts_skipped,
        } => div()
            .h(px(TERM_HEIGHT))
            .flex()
            .items_center()
            .gap_1p5()
            .text_xs()
            .child(dot(colors.gate))
            .child(div().truncate().child(id.clone()))
            .when(*accepts_skipped, |this| {
                this.child(
                    div()
                        .flex_none()
                        .text_color(theme.muted_foreground)
                        .child("pass · skipped"),
                )
            })
            .into_any_element(),
        GateTerm::AnyOf { terms } => {
            any_of_box(terms.clone(), theme, colors, cx).into_any_element()
        }
        GateTerm::Other { text } => div()
            .h(px(TERM_HEIGHT))
            .flex()
            .items_center()
            .text_xs()
            .truncate()
            .child(text.clone())
            .into_any_element(),
    }
}

/// The Gate's any-of group, where a port's wire can land to join it.
/// Without terms it's the empty box that starts one.
fn any_of_box(
    terms: Vec<GateTerm>,
    theme: &Theme,
    colors: Colors,
    cx: &mut Context<PipelineEditorView>,
) -> impl IntoElement + use<> {
    let empty = terms.is_empty();
    let mut group = div()
        .id(if empty { "any-of-empty" } else { "any-of" })
        .flex()
        .flex_col()
        .pl_2()
        .border_l_2()
        .border_color(colors.gate)
        .when(empty, |this| this.border_dashed())
        .drag_over::<WireDrag>(|style, _, _, cx| style.bg(cx.theme().list_hover))
        .child(
            div()
                .h(px(ANY_OF_HEADER))
                .flex()
                .items_center()
                .text_xs()
                .text_color(theme.muted_foreground)
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
        group = group.child(gate_term(term, theme, colors, cx));
    }
    group
}
