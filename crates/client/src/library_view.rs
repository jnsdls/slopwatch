//! The Library editor: the developer's Library Steps on the left, the open
//! one's file on the right. Saving sends the whole file to the daemon, which
//! refuses text that wouldn't load, and the next Run of every Pipeline that
//! uses the Step reads what was saved.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme, Disableable, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_protocol::{Command, LibraryStep};

use crate::library::{LibraryEditor, NEW_STEP_TEXT};
use crate::outbox::Outbox;
use crate::outbox_view::{Pending, loading, refusal};

/// What the view's commands go out for ([`Outbox`]).
const LIST: &str = "library-list";
const SAVE: &str = "library-save";
const DELETE: &str = "library-delete";
const NEW: &str = "library-new";

pub struct LibraryView {
    library: LibraryEditor,
    editor: Entity<TextareaState>,
    new_name: Entity<InputState>,
    /// Text for the editor that arrived outside a render. Rendering puts it
    /// in, because replacing an input's text needs the window.
    replace_with: Option<String>,
    outbox: Outbox,
    _subscriptions: Vec<Subscription>,
}

impl LibraryView {
    pub fn new(outbox: Outbox, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| TextareaState::new(window, cx));
        let new_name = cx.new(|cx| InputState::new(window, cx).placeholder("new-step-name"));
        let _subscriptions = vec![
            // Redraw on every edit, so the unsaved marker follows the text.
            cx.subscribe_in(&editor, window, |_, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &new_name,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.create(window, cx);
                    }
                },
            ),
        ];
        Self {
            library: LibraryEditor::default(),
            editor,
            new_name,
            replace_with: None,
            outbox,
            _subscriptions,
        }
    }

    /// Asks the daemon for the Library. The answer comes to [`Self::listed`].
    pub fn refresh(&self) {
        self.outbox.load(LIST, Command::ListLibrarySteps);
    }

    /// Takes the daemon's listing of the Library.
    pub fn listed(&mut self, steps: Vec<LibraryStep>, cx: &mut Context<Self>) {
        let editor = self.editor_text(cx);
        if let Some(text) = self.library.listed(steps, &editor) {
            self.replace_with = Some(text);
        }
        cx.notify();
    }

    fn editor_text(&self, cx: &App) -> String {
        self.replace_with
            .clone()
            .unwrap_or_else(|| self.editor.read(cx).value().to_string())
    }

    fn open(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(text) = self.library.open(name) {
            self.replace_with = Some(text);
            cx.notify();
        }
    }

    /// Saves the open Step, then lists the Library again, so the listing
    /// shows what the daemon kept. A refused save keeps the edits, and its
    /// reason shows by the Save button.
    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(step) = self.library.open_step() else {
            return;
        };
        let command = Command::SaveLibraryStep {
            step: step.name.clone(),
            text: self.editor_text(cx),
        };
        if self.outbox.press(SAVE, command) {
            self.refresh();
        }
    }

    fn revert(&mut self, cx: &mut Context<Self>) {
        if let Some(name) = self.library.open_step().map(|step| step.name.clone()) {
            self.open(&name, cx);
        }
    }

    fn delete(&mut self) {
        let Some(step) = self.library.open_step() else {
            return;
        };
        let command = Command::DeleteLibraryStep {
            step: step.name.clone(),
        };
        if self.outbox.press(DELETE, command) {
            self.refresh();
        }
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.new_name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            return;
        }
        if self.library.steps().iter().any(|step| step.name == name) {
            self.open(&name, cx);
        } else {
            let command = Command::SaveLibraryStep {
                step: name.clone(),
                text: NEW_STEP_TEXT.to_owned(),
            };
            if !self.outbox.press(NEW, command) {
                return;
            }
            self.library.open_when_listed(&name);
            self.refresh();
        }
        self.new_name
            .update(cx, |input, cx| input.set_value("", window, cx));
    }

    fn step_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let open = self.library.open_step().map(|step| step.name.clone());
        let mut list = div()
            .id("library-steps")
            .w(px(220.))
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .border_r_1()
            .border_color(theme.border)
            .overflow_y_scroll()
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("LIBRARY STEPS"),
            );
        if self.library.loaded() && self.library.steps().is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No Library Steps yet."),
            );
        }
        for step in self.library.steps() {
            let name = step.name.clone();
            let selected = open.as_deref() == Some(name.as_str());
            list = list.child(
                div()
                    .id(SharedString::from(format!("library-{name}")))
                    .flex()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .when(selected, |this| this.bg(theme.list_active))
                    .hover(|this| this.bg(theme.list_hover))
                    .child(format!("lib/{name}"))
                    .when(step.problem.is_some(), |this| {
                        this.child(div().text_color(theme.danger).child("invalid"))
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.open(&name, cx);
                    })),
            );
        }
        list.child(
            div()
                .mt_2()
                .flex()
                .gap_1()
                .child(div().flex_1().child(Input::new(&self.new_name).small()))
                .child(
                    Button::new(NEW)
                        .label("New")
                        .small()
                        .ghost()
                        .pending(self.outbox.waiting(NEW))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.create(window, cx);
                        })),
                ),
        )
        .children(refusal(&self.outbox, NEW, theme))
    }

    fn editor_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let Some(step) = self.library.open_step() else {
            let pane = div().flex_1().p_6();
            if !self.library.loaded() {
                return pane.child(loading(&self.outbox, LIST, "the Library", theme));
            }
            return pane
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Pick a Library Step, or create one.");
        };
        let unsaved = self.library.unsaved(&self.editor_text(cx));
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .child(format!("lib/{}", step.name))
                    .when(unsaved, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Unsaved changes"),
                        )
                    }),
            )
            .child(
                Button::new("library-revert")
                    .label("Revert")
                    .small()
                    .ghost()
                    .disabled(!unsaved)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.revert(cx))),
            )
            .child(
                Button::new(DELETE)
                    .label("Delete")
                    .small()
                    .ghost()
                    .pending(self.outbox.waiting(DELETE))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, _| this.delete())),
            )
            .child(
                Button::new(SAVE)
                    .label("Save")
                    .small()
                    .primary()
                    .disabled(!unsaved)
                    .pending(self.outbox.waiting(SAVE))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save(cx))),
            );
        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(header)
            .children(refusal(&self.outbox, SAVE, theme))
            .children(refusal(&self.outbox, DELETE, theme))
            .children(step.problem.as_ref().map(|problem| {
                div().text_xs().text_color(theme.danger).child(format!(
                    "Pipelines that use this Step won't load: {problem}"
                ))
            }))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Pipelines override these keys per repo with `with:`."),
            )
            .child(Textarea::new(&self.editor).flex_1().font_family("Menlo"))
    }
}

impl Render for LibraryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(text) = self.replace_with.take() {
            self.editor
                .update(cx, |editor, cx| editor.set_value(text, window, cx));
        }
        div()
            .flex_1()
            .h_full()
            .flex()
            .overflow_hidden()
            .child(self.step_list(cx))
            .child(self.editor_pane(cx))
    }
}
