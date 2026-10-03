//! The daemon settings screen: the daily Budget, which lives in the daemon
//! and never in a repo, and what Steps spent today.

use std::sync::mpsc::Sender;

use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_protocol::{Cents, Command, DaemonSettings};

use crate::settings::{SettingsModel, save_daily};

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
    commands: Sender<Command>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(commands: Sender<Command>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let daily = cx.new(|cx| InputState::new(window, cx).placeholder("Off"));
        let _subscriptions =
            vec![
                cx.subscribe_in(&daily, window, |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.save(cx);
                    }
                }),
            ];
        Self {
            model: SettingsModel::default(),
            daily,
            problem: None,
            fill: true,
            filling: None,
            commands,
            _subscriptions,
        }
    }

    /// Asks the daemon for its settings. The answer comes to
    /// [`Self::listed`].
    pub fn refresh(&self) {
        let _ = self.commands.send(Command::GetSettings);
    }

    /// Asks for the settings, and fills the field with them, as when the
    /// screen opens.
    pub fn reload(&mut self) {
        self.fill = true;
        self.refresh();
    }

    /// The daemon's settings arrived.
    pub fn listed(&mut self, settings: DaemonSettings, spent_today: Cents, cx: &mut Context<Self>) {
        self.model.listed(settings, spent_today);
        if std::mem::take(&mut self.fill) {
            self.filling = Some(self.model.daily_field());
        }
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let field = self.daily.read(cx).value().to_string();
        match save_daily(&field) {
            Ok(command) => {
                self.problem = None;
                let _ = self.commands.send(command);
                self.reload();
            }
            Err(problem) => self.problem = Some(problem),
        }
        cx.notify();
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(field) = self.filling.take() {
            self.daily
                .update(cx, |input, cx| input.set_value(field, window, cx));
        }
        let theme = cx.theme().clone();
        let label = |text: &'static str| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        div()
            .id("settings")
            .flex_1()
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
                        Button::new("settings-save")
                            .label("Save")
                            .small()
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save(cx))),
                    ),
            )
            .when(self.model.loaded(), |this| {
                this.child(div().text_sm().child(self.model.spent_line()))
            })
            .child(div().text_xs().text_color(theme.muted_foreground).child(
                "What Steps may spend across every repo from local midnight on, at list price \
                 whatever you're billed. Once it's spent, Runs end over budget and every PR \
                 waits until midnight or a raise. Leave it blank to turn it off. PR and Step \
                 Budgets live in each Pipeline as budget_usd.",
            ))
            .when_some(self.problem.clone(), |this, problem| {
                this.child(div().text_xs().text_color(theme.danger).child(problem))
            })
    }
}
