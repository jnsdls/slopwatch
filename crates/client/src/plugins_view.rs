//! The Plugins list: every Plugin, built-in and third-party, where it
//! stands, and for the one picked, what its manifest asks for, what its
//! Approval covers, the Approve button and its daemon settings (ADR 0012).

use gpui_kit::component::Sizable;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_protocol::{Command, PluginListing};

use crate::components::{ButtonLooks, ChipKind, chip, list_row, section};
use crate::outbox::Outbox;
use crate::outbox_view::{Pending, loading, refusal};
use crate::plugins::{
    Fields, PluginsList, approve, asks_more, grant_lines, save_settings, settings_fields,
    state_line,
};
use crate::theme;

/// What the view's commands go out for ([`Outbox`]).
const LIST: &str = "plugins-list";
const APPROVE: &str = "plugin-approve";
const SAVE: &str = "plugin-save-settings";

pub struct PluginsView {
    list: PluginsList,
    path: Entity<InputState>,
    cap: Entity<InputState>,
    config_dir: Entity<InputState>,
    /// Why the settings fields can't be saved, until they're fixed.
    problem: Option<String>,
    outbox: Outbox,
}

impl PluginsView {
    pub fn new(outbox: Outbox, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path =
            cx.new(|cx| InputState::new(window, cx).placeholder("/opt/tool/bin:/usr/local/bin"));
        let cap = cx.new(|cx| InputState::new(window, cx).placeholder("manifest's"));
        let config_dir = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Config directory for its CLI, if it takes one")
        });
        Self {
            list: PluginsList::default(),
            path,
            cap,
            config_dir,
            problem: None,
            outbox,
        }
    }

    /// How many Plugins wait for approval, for the sources pane.
    pub fn waiting(&self) -> usize {
        self.list.waiting()
    }

    /// Asks the daemon for the list. The answer comes to [`Self::listed`].
    pub fn refresh(&self) {
        self.outbox.load(LIST, Command::ListPlugins);
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
        let fields = self
            .list
            .chosen()
            .map(|plugin| settings_fields(&plugin.settings))
            .unwrap_or_default();
        self.path
            .update(cx, |input, cx| input.set_value(fields.path, window, cx));
        self.cap
            .update(cx, |input, cx| input.set_value(fields.cap, window, cx));
        self.config_dir.update(cx, |input, cx| {
            input.set_value(fields.config_dir, window, cx)
        });
        cx.notify();
    }

    fn approve(&self) {
        if let Some(command) = self.list.chosen().and_then(approve)
            && self.outbox.press(APPROVE, command)
        {
            self.refresh();
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some((plugin, agent)) = self
            .list
            .chosen()
            .map(|plugin| (plugin.name.clone(), plugin.runs_agent_cli()))
        else {
            return;
        };
        // An agent Plugin shows only its cap.
        let shown = |input: &Entity<InputState>| {
            if agent {
                String::new()
            } else {
                input.read(cx).value().to_string()
            }
        };
        let fields = Fields {
            path: shown(&self.path),
            cap: self.cap.read(cx).value().to_string(),
            config_dir: shown(&self.config_dir),
        };
        match save_settings(&plugin, &fields) {
            Ok(command) => {
                self.problem = None;
                if self.outbox.press(SAVE, command) {
                    self.refresh();
                }
            }
            Err(problem) => self.problem = Some(problem),
        }
        cx.notify();
    }

    fn rows(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let chosen = self
            .list
            .chosen()
            .map(|plugin| (plugin.name.clone(), plugin.builtin));
        let mut rows = div().flex().flex_col().gap(px(6.));
        if !self.list.loaded() {
            return rows.child(loading(&self.outbox, LIST, "the Plugins"));
        }
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
                list_row(
                    SharedString::from(format!("plugin-{index}-{name}")),
                    selected,
                )
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(div().text_sm().font_family(theme::MONO).child(name.clone()))
                        .child(div().text_xs().text_color(theme::DIM).child(detail)),
                )
                .child(
                    chip(
                        if attention {
                            ChipKind::Bad
                        } else {
                            ChipKind::Ok
                        },
                        state_line(plugin),
                    )
                    .flex_none(),
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
                    .text_color(theme::DIM)
                    .child("Put an executable, or a symlink to one, in the Plugins folder under your slopwatch config, and it shows here."),
            );
        }
        rows
    }

    fn detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut detail = div()
            .flex()
            .flex_col()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(theme::LINE);
        let Some(plugin) = self.list.chosen() else {
            return detail.child(
                div()
                    .text_xs()
                    .text_color(theme::DIM)
                    .child("Pick a Plugin to see what it asks for."),
            );
        };
        let line = |text: String| div().text_xs().child(text);
        if let Some(asks) = &plugin.asks {
            detail = detail
                .child(section("Asks for"))
                .children(grant_lines(asks).into_iter().map(line));
        }
        let more = asks_more(plugin);
        if !more.is_empty() {
            detail = detail.child(
                div()
                    .text_xs()
                    .text_color(theme::FAIL)
                    .child(format!("New since its Approval: {}", more.join(", "))),
            );
        }
        if let Some(approved) = &plugin.approved
            && !plugin.builtin
        {
            detail = detail
                .child(section("Its approval covers"))
                .children(grant_lines(approved).into_iter().map(line));
        }
        if approve(plugin).is_some() {
            detail = detail.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new(APPROVE)
                            .label("Approve")
                            .small()
                            .accent()
                            .pending(self.outbox.waiting(APPROVE))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, _| this.approve())),
                    )
                    .child(div().text_xs().text_color(theme::DIM).child(
                        "It runs with everything above. A rebuild keeps the Approval unless \
                         its manifest asks for more.",
                    )),
            );
        }
        detail = detail.children(refusal(&self.outbox, APPROVE));
        // An agent Plugin's PATH dirs and config directory are its CLI's,
        // under Settings, so only its cap is set here.
        let agent = plugin.runs_agent_cli();
        let save = Button::new(SAVE)
            .label("Save")
            .small()
            .pending(self.outbox.waiting(SAVE))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save(cx)));
        detail
            .child(section("Settings"))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .when(!agent, |this| {
                        this.child(div().flex_1().child(Input::new(&self.path).small()))
                    })
                    .child(div().w(px(90.)).child(Input::new(&self.cap).small()))
                    .child(save),
            )
            .when(!agent, |this| {
                this.child(Input::new(&self.config_dir).small()).child(
                    div().text_xs().text_color(theme::DIM).child(
                        "PATH dirs go in front of the PATH its describe and Steps get. The \
                         cap limits how many of its Steps run at once. The config directory \
                         goes to the CLI it runs.",
                    ),
                )
            })
            .when(agent, |this| {
                this.child(div().text_xs().text_color(theme::DIM).child(
                    "The cap limits how many of its Steps run at once. Which claude or \
                     codex it runs, with their PATH dirs and config directory, is set under \
                     Settings, CLIs.",
                ))
            })
            .when_some(self.problem.clone(), |this, problem| {
                this.child(div().text_xs().text_color(theme::FAIL).child(problem))
            })
            .children(refusal(&self.outbox, SAVE))
    }
}

impl Render for PluginsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("plugins")
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .overflow_y_scroll()
            .child(section("Plugins"))
            .child(self.rows(cx))
            .child(self.detail(cx))
    }
}
