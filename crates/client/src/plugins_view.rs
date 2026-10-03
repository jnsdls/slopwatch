//! The Plugins list: every Plugin, built-in and third-party, where it
//! stands, and for the one picked, what its manifest asks for, what its
//! Approval covers, the Approve button and its daemon settings (ADR 0012).

use std::sync::mpsc::Sender;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_protocol::{Command, PluginListing};

use crate::plugins::{
    PluginsList, approve, asks_more, grant_lines, save_settings, settings_fields, state_line,
};

pub struct PluginsView {
    list: PluginsList,
    path: Entity<InputState>,
    cap: Entity<InputState>,
    /// Why the settings fields can't be saved, until they're fixed.
    problem: Option<String>,
    commands: Sender<Command>,
}

impl PluginsView {
    pub fn new(commands: Sender<Command>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path =
            cx.new(|cx| InputState::new(window, cx).placeholder("/opt/tool/bin:/usr/local/bin"));
        let cap = cx.new(|cx| InputState::new(window, cx).placeholder("manifest's"));
        Self {
            list: PluginsList::default(),
            path,
            cap,
            problem: None,
            commands,
        }
    }

    /// How many Plugins wait for approval, for the sources pane.
    pub fn waiting(&self) -> usize {
        self.list.waiting()
    }

    /// Asks the daemon for the list. The answer comes to [`Self::listed`].
    pub fn refresh(&self) {
        let _ = self.commands.send(Command::ListPlugins);
    }

    pub fn listed(&mut self, plugins: Vec<PluginListing>, cx: &mut Context<Self>) {
        self.list.listed(plugins);
        cx.notify();
    }

    /// Picks the Plugin whose detail shows, as from its Inbox entry, and
    /// fills the settings fields with its settings.
    pub fn choose(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.list.choose(name);
        self.problem = None;
        let (path, cap) = self
            .list
            .chosen()
            .map(|plugin| settings_fields(&plugin.settings))
            .unwrap_or_default();
        self.path
            .update(cx, |input, cx| input.set_value(path, window, cx));
        self.cap
            .update(cx, |input, cx| input.set_value(cap, window, cx));
        cx.notify();
    }

    fn approve(&self) {
        if let Some(command) = self.list.chosen().and_then(approve) {
            let _ = self.commands.send(command);
            self.refresh();
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(plugin) = self.list.chosen().map(|plugin| plugin.name.clone()) else {
            return;
        };
        let path = self.path.read(cx).value().to_string();
        let cap = self.cap.read(cx).value().to_string();
        match save_settings(&plugin, &path, &cap) {
            Ok(command) => {
                self.problem = None;
                let _ = self.commands.send(command);
                self.refresh();
            }
            Err(problem) => self.problem = Some(problem),
        }
        cx.notify();
    }

    fn rows(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let chosen = self
            .list
            .chosen()
            .map(|plugin| (plugin.name.clone(), plugin.builtin));
        let mut rows = div().flex().flex_col().gap_1();
        for (index, plugin) in self.list.plugins().iter().enumerate() {
            let name = plugin.name.clone();
            let selected = chosen == Some((name.clone(), plugin.builtin));
            let attention = plugin.needs_approval() || plugin.problem.is_some();
            let detail = match (&plugin.version, &plugin.path) {
                (Some(version), Some(path)) => format!("{version} · {path}"),
                (Some(version), None) => version.clone(),
                (None, Some(path)) => path.clone(),
                (None, None) => String::new(),
            };
            rows = rows.child(
                div()
                    .id(SharedString::from(format!("plugin-{index}-{name}")))
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
                                    .child(detail),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if attention {
                                theme.danger
                            } else {
                                theme.muted_foreground
                            })
                            .child(state_line(plugin)),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.choose(&name, window, cx);
                    })),
            );
        }
        if self.list.loaded() && self.list.plugins().iter().all(|plugin| plugin.builtin) {
            rows = rows.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Put an executable, or a symlink to one, in the Plugins folder under your slopwatch config, and it shows here."),
            );
        }
        rows
    }

    fn detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut detail = div()
            .flex()
            .flex_col()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(theme.border);
        let Some(plugin) = self.list.chosen() else {
            return detail.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Pick a Plugin to see what it asks for."),
            );
        };
        let line = |text: String| div().text_xs().child(text);
        if let Some(asks) = &plugin.asks {
            detail = detail
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("ASKS FOR"),
                )
                .children(grant_lines(asks).into_iter().map(line));
        }
        let more = asks_more(plugin);
        if !more.is_empty() {
            detail = detail.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(format!("New since its Approval: {}", more.join(", "))),
            );
        }
        if let Some(approved) = &plugin.approved
            && !plugin.builtin
        {
            detail = detail
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("ITS APPROVAL COVERS"),
                )
                .children(grant_lines(approved).into_iter().map(line));
        }
        if approve(plugin).is_some() {
            detail = detail.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("plugin-approve")
                            .label("Approve")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _: &ClickEvent, _, _| this.approve())),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        "It runs with everything above. A rebuild keeps the Approval unless \
                         its manifest asks for more.",
                    )),
            );
        }
        detail
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("SETTINGS"),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.path).small()))
                    .child(div().w(px(90.)).child(Input::new(&self.cap).small()))
                    .child(
                        Button::new("plugin-save-settings")
                            .label("Save")
                            .small()
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save(cx))),
                    ),
            )
            .child(div().text_xs().text_color(theme.muted_foreground).child(
                "PATH dirs go in front of the PATH its describe and Steps get. The cap \
                 limits how many of its Steps run at once.",
            ))
            .when_some(self.problem.clone(), |this, problem| {
                this.child(div().text_xs().text_color(theme.danger).child(problem))
            })
    }
}

impl Render for PluginsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .id("plugins")
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
                    .child("PLUGINS"),
            )
            .child(self.rows(cx))
            .child(self.detail(cx))
    }
}
