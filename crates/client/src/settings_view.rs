//! The daemon settings screen: the daily Budget, which lives in the daemon
//! and never in a repo, and what Steps spent today; and the CLIs the
//! daemon runs, each with its executable, what the daemon resolved it to,
//! and for the agent CLIs, their `PATH` dirs, config directory and login.

use gpui_kit::component::Sizable;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_protocol::{Cents, Cli, CliListing, Command, DaemonSettings};

use crate::components::section;
use crate::outbox::Outbox;
use crate::outbox_view::{Pending, loading, refusal};
use crate::settings::{
    CliFields, SettingsModel, cli_fields, login_line, save_cli, save_daily, status_line,
};
use crate::theme;

/// What the view's commands go out for ([`Outbox`]).
const GET: &str = "settings-get";
const SAVE: &str = "settings-save";
const CLIS: &str = "settings-clis";

fn save_cli_action(cli: Cli) -> String {
    format!("cli-save-{cli}")
}

pub struct SettingsView {
    model: SettingsModel,
    daily: Entity<InputState>,
    /// Why the field can't be saved, until it's fixed.
    problem: Option<String>,
    /// The next settings to arrive fill the field. Others, such as after
    /// an Inbox change, leave what the developer is typing alone.
    fill: bool,
    /// What the field gets on the next draw, which has the window it needs.
    filling: Option<String>,
    clis: Vec<CliRow>,
    /// The next CLI listing fills the CLI fields, as `fill` does the daily
    /// Budget's.
    fill_clis: bool,
    outbox: Outbox,
    _subscriptions: Vec<Subscription>,
}

/// One CLI's fields. Only the agent CLIs have `PATH` dirs and a config
/// directory.
struct CliRow {
    cli: Cli,
    executable: Entity<InputState>,
    path: Option<Entity<InputState>>,
    config_dir: Option<Entity<InputState>>,
    /// Why the fields can't be saved, until they're fixed.
    problem: Option<String>,
    /// What the fields get on the next draw.
    filling: Option<CliFields>,
}

impl SettingsView {
    pub fn new(outbox: Outbox, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let daily = cx.new(|cx| InputState::new(window, cx).placeholder("Off"));
        let mut _subscriptions =
            vec![
                cx.subscribe_in(&daily, window, |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.save(cx);
                    }
                }),
            ];
        let mut clis = Vec::new();
        for (index, cli) in Cli::ALL.into_iter().enumerate() {
            let executable = cx.new(|cx| InputState::new(window, cx).placeholder(cli.name()));
            let mut agent = |placeholder: &'static str, cx: &mut Context<Self>| {
                cli.is_agent()
                    .then(|| cx.new(|cx| InputState::new(window, cx).placeholder(placeholder)))
            };
            let path = agent("Extra PATH dirs, such as /opt/tools/bin:/usr/local/bin", cx);
            let config_dir = agent("Config directory it logs in from, if not the usual", cx);
            for input in std::iter::once(&executable).chain(&path).chain(&config_dir) {
                _subscriptions.push(cx.subscribe_in(
                    input,
                    window,
                    move |this, _, event: &InputEvent, _, cx| {
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            this.save_cli(index, cx);
                        }
                    },
                ));
            }
            clis.push(CliRow {
                cli,
                executable,
                path,
                config_dir,
                problem: None,
                filling: None,
            });
        }
        Self {
            model: SettingsModel::default(),
            daily,
            problem: None,
            fill: true,
            filling: None,
            clis,
            fill_clis: true,
            outbox,
            _subscriptions,
        }
    }

    /// Asks the daemon for its settings. The answer comes to
    /// [`Self::listed`].
    pub fn refresh(&self) {
        self.outbox.load(GET, Command::GetSettings);
    }

    /// Asks for the settings and the CLIs, and fills the fields with them,
    /// as when the screen opens.
    pub fn reload(&mut self) {
        self.fill = true;
        self.fill_clis = true;
        self.refresh();
        self.outbox.load(CLIS, Command::ListClis);
    }

    /// The daemon's settings arrived.
    pub fn listed(&mut self, settings: DaemonSettings, spent_today: Cents, cx: &mut Context<Self>) {
        self.model.listed(settings, spent_today);
        if std::mem::take(&mut self.fill) {
            self.filling = Some(self.model.daily_field());
        }
        cx.notify();
    }

    /// The CLIs arrived, each checked by the daemon.
    pub fn clis_listed(&mut self, clis: Vec<CliListing>, cx: &mut Context<Self>) {
        self.model.clis_listed(clis);
        if std::mem::take(&mut self.fill_clis) {
            for row in &mut self.clis {
                if let Some(listing) = self.model.cli(row.cli) {
                    row.filling = Some(cli_fields(&listing.settings));
                }
            }
        }
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let field = self.daily.read(cx).value().to_string();
        match save_daily(&field) {
            Ok(command) => {
                self.problem = None;
                if self.outbox.press(SAVE, command) {
                    self.fill = true;
                    self.refresh();
                }
            }
            Err(problem) => self.problem = Some(problem),
        }
        cx.notify();
    }

    /// Saves the CLI in row `index`, and lists the CLIs again so its row
    /// shows what the daemon finds now.
    fn save_cli(&mut self, index: usize, cx: &mut Context<Self>) {
        let row = &mut self.clis[index];
        let value = |input: &Option<Entity<InputState>>| {
            input
                .as_ref()
                .map(|input| input.read(cx).value().to_string())
                .unwrap_or_default()
        };
        let fields = CliFields {
            executable: row.executable.read(cx).value().to_string(),
            path: value(&row.path),
            config_dir: value(&row.config_dir),
        };
        match save_cli(row.cli, &fields) {
            Ok(command) => {
                row.problem = None;
                if self.outbox.press(&save_cli_action(row.cli), command) {
                    self.outbox.load(CLIS, Command::ListClis);
                }
            }
            Err(problem) => row.problem = Some(problem),
        }
        cx.notify();
    }

    fn cli_row(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let row = &self.clis[index];
        let listing = self.model.cli(row.cli);
        let muted = |text: String| div().text_xs().text_color(theme::DIM).child(text);
        let status = match listing.map(|listing| status_line(&listing.status)) {
            None => match refusal(&self.outbox, CLIS) {
                Some(refused) if !self.outbox.waiting(CLIS) => refused,
                _ => muted("Checking…".to_owned()),
            },
            Some(Ok(line)) => muted(line),
            Some(Err(problem)) => div().text_xs().text_color(theme::FAIL).child(problem),
        };
        let login =
            listing
                .and_then(|listing| login_line(&listing.status))
                .map(|(line, logged_in)| {
                    div()
                        .text_xs()
                        .text_color(if logged_in { theme::PASS } else { theme::INC })
                        .child(line)
                });
        div()
            .flex()
            .flex_col()
            .gap_1()
            .pb_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(64.))
                            .text_sm()
                            .font_family(theme::MONO)
                            .child(row.cli.name()),
                    )
                    .child(div().flex_1().child(Input::new(&row.executable).small()))
                    .child(
                        Button::new(SharedString::from(save_cli_action(row.cli)))
                            .label("Save")
                            .small()
                            .pending(self.outbox.waiting(&save_cli_action(row.cli)))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.save_cli(index, cx)
                            })),
                    ),
            )
            .children(
                row.path
                    .as_ref()
                    .map(|path| div().pl(px(72.)).child(Input::new(path).small())),
            )
            .children(
                row.config_dir
                    .as_ref()
                    .map(|dir| div().pl(px(72.)).child(Input::new(dir).small())),
            )
            .child(
                div()
                    .pl(px(72.))
                    .flex()
                    .flex_col()
                    .child(status)
                    .children(login),
            )
            .when_some(row.problem.clone(), |this, problem| {
                this.child(
                    div()
                        .pl(px(72.))
                        .text_xs()
                        .text_color(theme::FAIL)
                        .child(problem),
                )
            })
            .children(refusal(&self.outbox, &save_cli_action(row.cli)).map(|line| line.pl(px(72.))))
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(field) = self.filling.take() {
            self.daily
                .update(cx, |input, cx| input.set_value(field, window, cx));
        }
        for row in &mut self.clis {
            let Some(fields) = row.filling.take() else {
                continue;
            };
            row.executable.update(cx, |input, cx| {
                input.set_value(fields.executable, window, cx)
            });
            if let Some(path) = &row.path {
                path.update(cx, |input, cx| input.set_value(fields.path, window, cx));
            }
            if let Some(dir) = &row.config_dir {
                dir.update(cx, |input, cx| {
                    input.set_value(fields.config_dir, window, cx)
                });
            }
        }
        let label = |text: &'static str| section(text);
        let rows: Vec<_> = (0..self.clis.len())
            .map(|index| self.cli_row(index, cx).into_any_element())
            .collect();
        div()
            .id("settings")
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .overflow_y_scroll()
            .child(label("SETTINGS"))
            .child(label("DAILY BUDGET (USD)"))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(div().w(px(160.)).child(Input::new(&self.daily).small()))
                    .child(
                        Button::new(SAVE)
                            .label("Save")
                            .small()
                            .pending(self.outbox.waiting(SAVE))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save(cx))),
                    ),
            )
            .children(refusal(&self.outbox, SAVE))
            .map(|this| {
                if self.model.loaded() {
                    this.child(div().text_sm().child(self.model.spent_line()))
                } else {
                    this.child(loading(&self.outbox, GET, "the settings"))
                }
            })
            .child(div().text_xs().text_color(theme::DIM).child(
                "What Steps may spend across every repo from local midnight on, at list price \
                 whatever you're billed. Once it's spent, Runs end over budget and every PR \
                 waits until midnight or a raise. Leave it blank to turn it off. PR and Step \
                 Budgets live in each Pipeline as budget_usd.",
            ))
            .when_some(self.problem.clone(), |this, problem| {
                this.child(div().text_xs().text_color(theme::FAIL).child(problem))
            })
            .child(div().pt_3().child(label("CLIS")))
            .child(div().text_xs().text_color(theme::DIM).child(
                "How the daemon finds each CLI it runs: a name on PATH or an absolute path. \
                 Blank runs the usual name, found on PATH or where Homebrew puts it. The \
                 review and fix Steps run the claude and codex set here, unless a Step names \
                 its own cli. These settings stay on this machine and never go in a repo.",
            ))
            .children(rows)
    }
}
