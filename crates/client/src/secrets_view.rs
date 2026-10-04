//! The Secrets list: every Secret by name, whether it's set and when, and
//! which Plugins it's granted to, with a masked field to set or rotate one.
//! The daemon never sends a value back, so the list can't show one.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_protocol::{Command, SecretInfo};

use crate::outbox::Outbox;
use crate::outbox_view::{Pending, loading, refusal};
use crate::secrets::{SecretsList, granted_line, needed, state_line};

/// What the view's commands go out for ([`Outbox`]).
const LIST: &str = "secrets-list";
const SET: &str = "secret-set";

fn delete_action(name: &str) -> String {
    format!("secret-delete-{name}")
}

pub struct SecretsView {
    list: SecretsList,
    name: Entity<InputState>,
    value: Entity<InputState>,
    outbox: Outbox,
    _subscriptions: Vec<Subscription>,
}

impl SecretsView {
    pub fn new(outbox: Outbox, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("NEW_SECRET_NAME"));
        let value = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Paste the value")
                .masked(true)
        });
        let _subscriptions =
            vec![
                cx.subscribe_in(&value, window, |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.set(window, cx);
                    }
                }),
            ];
        Self {
            list: SecretsList::default(),
            name,
            value,
            outbox,
            _subscriptions,
        }
    }

    /// How many Secrets a Plugin needs that aren't set, for the sources
    /// pane.
    pub fn unset(&self) -> usize {
        self.list.unset()
    }

    /// Asks the daemon for the list. The answer comes to [`Self::listed`].
    pub fn refresh(&self) {
        self.outbox.load(LIST, Command::ListSecrets);
    }

    pub fn listed(&mut self, secrets: Vec<SecretInfo>, cx: &mut Context<Self>) {
        self.list.listed(secrets);
        cx.notify();
    }

    /// Picks the Secret the value field sets, as from a missing Secret's
    /// Inbox entry.
    pub fn choose(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.list.choose(name);
        self.name
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    /// Sends the value, clears the field, then lists again, so the row
    /// shows when it was set.
    fn set(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().to_string();
        let value = self.value.read(cx).value().to_string();
        let Some(command) = self.list.set(&name, &value) else {
            return;
        };
        if !self.outbox.press(SET, command) {
            return;
        }
        self.value
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.name
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.refresh();
    }

    fn delete(&self, name: &str) {
        let command = Command::DeleteSecret {
            secret: name.to_owned(),
        };
        if self.outbox.press(&delete_action(name), command) {
            self.refresh();
        }
    }

    fn rows(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs() as i64);
        let chosen = self.list.chosen().map(str::to_owned);
        let mut rows = div().flex().flex_col().gap_1();
        if !self.list.loaded() {
            return rows.child(loading(&self.outbox, LIST, "the Secrets", theme));
        }
        if self.list.secrets().is_empty() {
            rows = rows.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No Secrets yet. Plugins that need one list it here."),
            );
        }
        for secret in self.list.secrets() {
            let name = secret.name.clone();
            let selected = chosen.as_deref() == Some(name.as_str());
            let state = state_line(secret, now);
            let missing = needed(secret);
            let delete = name.clone();
            rows = rows.child(
                div()
                    .id(SharedString::from(format!("secret-{name}")))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .when(selected, |this| this.bg(theme.list_active))
                    .hover(|this| this.bg(theme.list_hover))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(div().text_sm().font_family("Menlo").child(name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(granted_line(secret)),
                            )
                            .children(refusal(&self.outbox, &delete_action(&name), theme)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if missing {
                                theme.danger
                            } else {
                                theme.muted_foreground
                            })
                            .child(state),
                    )
                    .when(secret.is_set(), |this| {
                        this.child(
                            Button::new(SharedString::from(delete_action(&name)))
                                .label("Delete")
                                .small()
                                .ghost()
                                .pending(self.outbox.waiting(&delete_action(&name)))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                                    this.delete(&delete);
                                })),
                        )
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.choose(&name, window, cx);
                    })),
            );
        }
        rows
    }

    fn form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let target = match self.list.chosen() {
            Some(name) => format!("Set or rotate `{name}`, or type another name"),
            None => "Pick a Secret above, or type a name".to_owned(),
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(target),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(div().w(px(220.)).child(Input::new(&self.name).small()))
                    .child(div().flex_1().child(Input::new(&self.value).small()))
                    .child(
                        Button::new(SET)
                            .label("Set")
                            .small()
                            .primary()
                            .pending(self.outbox.waiting(SET))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.set(window, cx);
                            })),
                    ),
            )
            .children(refusal(&self.outbox, SET, theme))
            .child(div().text_xs().text_color(theme.muted_foreground).child(
                "The value goes to the Keychain and is never shown again. Steps \
                         started from now on get it; running ones keep the old one.",
            ))
    }
}

impl Render for SecretsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .id("secrets")
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .overflow_y_scroll()
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("SECRETS"),
            )
            .child(self.rows(cx))
            .child(self.form(cx))
    }
}
