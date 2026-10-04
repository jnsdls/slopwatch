//! The main window: the sources pane, then either the Watched PR list or
//! the Inbox, each with the PR pane, or the Library editor, or the Secrets
//! list, or a repo's Pipeline editor, or the link state while the daemon
//! isn't reachable.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Selectable, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_core::WaiverCategory;
use slopwatch_protocol::{
    Cause, Command, Flavor, InboxEntry, LogLevel, LogSource, PrRef, PrStatus, PullRequest, Reply,
    RepoName, ResponseBody, RunView, Scope, StackParent, StepStatus, StepView, TopicUpdate,
};

use crate::agent::Agent;
use crate::dock;
use crate::inbox::{
    ANSWERS, InboxModel, actions, answer as answer_entry, badge, held_line, history_line,
};
use crate::library_view::LibraryView;
use crate::link::{LinkEvent, LinkState};
use crate::link_view::LinkView;
use crate::notifications::{
    NotificationCenter, Permission, Poster, SystemCenter, notice, settings_url,
};
use crate::onboarding::Landing;
use crate::outbox::Outbox;
use crate::outbox_view::{Pending, refusal};
use crate::pipeline_editor_view::PipelineEditorView;
use crate::plugins::unapproved_plugin;
use crate::plugins_view::PluginsView;
use crate::prs::{
    PrState, Prs, Row as PrRow, Source, Stack, poll_line, status_line, storage_line, toggle_watch,
};
use crate::run_graph_view::{run_graph, tone_color};
use crate::run_pane::{
    GRAPH_MODE_LIST_WIDTH, PANE_PADDING, RunMode, RunPane, WaiveTarget, commit_line, end_label,
    finding_line, gate_tone, run_label, run_tone, step_line, step_tone, waiver_line,
};
use crate::secrets::missing_secret;
use crate::secrets_view::SecretsView;
use crate::settings_view::SettingsView;
use crate::step_log::{self, LogViewer, Row};

/// How see-through a PR list row is when it doesn't open.
const DIM: f32 = 0.55;

/// What the window's own commands go out for ([`Outbox`]).
const ADD_REPO: &str = "add-repo";
const CANCEL_RUN: &str = "cancel-run";
/// Every page the full-log viewer reads.
const LOG: &str = "step-log";

/// What an Inbox entry's button sends for. The Inbox and the PR pane share
/// it, so a press in one shows in the other.
fn entry_action(label: &str, entry: &InboxEntry) -> String {
    format!("entry-{label}-{}", entry.id)
}

/// What a Waiver sends for: the Waive or Override Gate button that opened
/// its form, which shows the wait once the form closes.
fn waive_action(target: &WaiveTarget) -> String {
    match target {
        WaiveTarget::Step(step) => format!("waive-{step}"),
        WaiveTarget::Gate => "override-gate".to_owned(),
    }
}

/// A PR list row's left padding: a Stack's PRs step in one level per
/// parent.
fn row_indent(row: PrRow<'_>) -> Pixels {
    px(16.0 + 20.0 * row.depth() as f32)
}

/// What fills the window right of the sources pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Prs,
    Inbox,
    Library,
    Secrets,
    /// The Pipeline editor, on the repo it has open.
    Pipeline,
    Plugins,
    /// The daemon's own settings, such as the daily Budget.
    Settings,
}

pub struct MainView {
    link: LinkState,
    link_view: Entity<LinkView>,
    pane: Pane,
    prs: Prs,
    inbox: InboxModel,
    /// Posts the daemon's notifications.
    poster: Poster,
    /// A PR a clicked banner asked for before the PR list had it.
    pending_reveal: Option<PrRef>,
    library: Entity<LibraryView>,
    secrets: Entity<SecretsView>,
    pipeline: Entity<PipelineEditorView>,
    plugins: Entity<PluginsView>,
    settings: Entity<SettingsView>,
    run_pane: RunPane,
    /// Repos the developer can add, while the picker is open.
    picker: Option<Vec<RepoName>>,
    /// A repo just added, until its draft says whether it has a Pipeline:
    /// without one it gets the tour, with one the developer goes to its
    /// PRs.
    onboarding: Option<RepoName>,
    /// The last command that failed with no control to show it, until the
    /// next one succeeds.
    error: Option<String>,
    outbox: Outbox,
    /// The log viewer's search field.
    log_search: Entity<InputState>,
    /// The Waiver form's reason field.
    waiver_reason: Entity<InputState>,
    /// The note that goes with an answer to a Human Step.
    answer_note: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl MainView {
    /// `agent` is `None` when the GUI runs outside its bundle. `reregister`
    /// asks the link to unregister and register the agent.
    pub fn new(
        outbox: Outbox,
        agent: Option<Arc<dyn Agent>>,
        reregister: Sender<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let log_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search the log"));
        let waiver_reason =
            cx.new(|cx| InputState::new(window, cx).placeholder("Why it doesn't block this PR"));
        let answer_note = cx
            .new(|cx| InputState::new(window, cx).placeholder("A note for later Steps (optional)"));
        let _subscriptions = vec![
            cx.subscribe_in(
                &log_search,
                window,
                |this, input, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        let search = input.read(cx).value().to_string();
                        this.with_viewer(|viewer| Some(viewer.set_search(&search)));
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &waiver_reason,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.submit_waiver(window, cx);
                    }
                },
            ),
            // The developer may have changed it in System Settings.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.refresh_permission(cx);
                }
            }),
        ];
        SystemCenter::new(cx).refresh();
        Self {
            log_search,
            waiver_reason,
            answer_note,
            _subscriptions,
            link: LinkState::Connecting,
            link_view: cx.new(|_| LinkView::new(agent, reregister)),
            pane: Pane::Prs,
            prs: Prs::default(),
            inbox: InboxModel::default(),
            poster: Poster::default(),
            pending_reveal: None,
            library: cx.new(|cx| LibraryView::new(outbox.clone(), window, cx)),
            secrets: cx.new(|cx| SecretsView::new(outbox.clone(), window, cx)),
            pipeline: cx.new(|cx| PipelineEditorView::new(outbox.clone(), window, cx)),
            plugins: cx.new(|cx| PluginsView::new(outbox.clone(), window, cx)),
            settings: cx.new(|cx| SettingsView::new(outbox.clone(), window, cx)),
            run_pane: RunPane::default(),
            picker: None,
            onboarding: None,
            error: None,
            outbox,
        }
    }

    pub fn handle(&mut self, event: LinkEvent, cx: &mut Context<Self>) {
        match event {
            LinkEvent::State(state) => {
                // Nothing sent before can be answered now.
                self.outbox.reset();
                let connected = matches!(state, LinkState::Connected { .. });
                if !connected {
                    self.picker = None;
                } else {
                    if self.pane == Pane::Library {
                        self.library.read(cx).refresh();
                    }
                    self.secrets.read(cx).refresh();
                    self.plugins.read(cx).refresh();
                    self.settings.read(cx).refresh();
                }
                if connected && !matches!(self.link, LinkState::Connected { .. }) {
                    // The new connection subscribes to its topics itself.
                    for command in self.run_pane.reconnected() {
                        self.send(command);
                    }
                    self.pipeline.read(cx).reconnected();
                }
                self.link = state.clone();
                self.link_view
                    .update(cx, |view, cx| view.set_state(state, cx));
            }
            LinkEvent::Topic(update @ TopicUpdate::Inbox { .. }) => {
                self.inbox.apply(update);
                self.prs.set_inbox(self.inbox.entries());
                dock::set_badge(badge(self.inbox.count()).as_deref());
                // A missing Secret's or an unapproved Plugin's entry
                // opening or closing changes those lists, and a spent
                // Budget's entry what today's spend reads.
                self.secrets.read(cx).refresh();
                self.plugins.read(cx).refresh();
                self.settings.read(cx).refresh();
            }
            LinkEvent::Topic(update @ TopicUpdate::Notifications { .. }) => {
                let center = SystemCenter::new(cx);
                if let Some(ack) = self.poster.apply(update, &center) {
                    self.send(ack);
                    // Posting may have asked for permission.
                    center.refresh();
                }
            }
            LinkEvent::Topic(TopicUpdate::Pipeline { draft }) => {
                if self.onboarding.as_ref() == Some(&draft.repo) {
                    self.onboarding = None;
                    // The first repo is where macOS asks, not launch.
                    self.ask_permission(cx);
                    match Landing::for_draft(&draft) {
                        Landing::Tour => {
                            self.pipeline.update(cx, |editor, cx| editor.start_tour(cx));
                        }
                        Landing::Watching => self.show_prs(Source::Repo(draft.repo.clone())),
                    }
                }
                self.pipeline
                    .update(cx, |editor, cx| editor.apply(*draft, cx));
            }
            LinkEvent::Topic(update @ (TopicUpdate::Run { .. } | TopicUpdate::StepLog { .. })) => {
                for command in self.run_pane.apply(update) {
                    self.send(command);
                }
            }
            LinkEvent::Topic(update) => {
                self.prs.apply(update);
                if let Some(repo) = self.pipeline.read(cx).repo().cloned() {
                    let prs = self.prs.in_repo(&repo);
                    self.pipeline
                        .update(cx, |editor, cx| editor.set_prs(prs, cx));
                }
                if let Some(pr) = self.pending_reveal.take() {
                    self.reveal(pr);
                }
                if let Some((repo, number)) = self.run_pane.selected().cloned() {
                    let commands = self.run_pane.pr_changed(self.prs.pr(&repo, number));
                    for command in commands {
                        self.send(command);
                    }
                }
            }
            LinkEvent::Response(response) => {
                let tracked = self.outbox.answered(&response);
                self.respond(response.result, tracked, cx);
            }
        }
        cx.notify();
    }

    /// Takes the daemon's answer to a command. `tracked` when a control
    /// waited on it.
    fn respond(&mut self, result: ResponseBody, tracked: bool, cx: &mut Context<Self>) {
        match result {
            // The control that sent it shows why.
            ResponseBody::Error(_) if tracked => {}
            ResponseBody::Ok(Reply::AvailableRepos { repos }) => {
                self.error = None;
                let added = self.prs.repos();
                self.picker = Some(
                    repos
                        .into_iter()
                        .filter(|repo| !added.contains(repo))
                        .collect(),
                );
            }
            ResponseBody::Ok(Reply::LibrarySteps { steps }) => {
                self.library
                    .update(cx, |library, cx| library.listed(steps, cx));
            }
            ResponseBody::Ok(Reply::Secrets { secrets }) => {
                self.secrets.update(cx, |view, cx| view.listed(secrets, cx));
            }
            ResponseBody::Ok(Reply::Plugins { plugins }) => {
                self.plugins.update(cx, |view, cx| view.listed(plugins, cx));
            }
            ResponseBody::Ok(Reply::Settings {
                settings,
                spent_today,
            }) => {
                self.settings
                    .update(cx, |view, cx| view.listed(settings, spent_today, cx));
            }
            ResponseBody::Ok(Reply::Clis { clis }) => {
                self.settings
                    .update(cx, |view, cx| view.clis_listed(clis, cx));
            }
            ResponseBody::Ok(Reply::StepLog(page)) => {
                self.error = None;
                self.run_pane.log_page(page);
            }
            ResponseBody::Ok(_) => self.error = None,
            // While the editor is open, its gestures are what get
            // refused, and it shows why next to the canvas.
            ResponseBody::Error(error) if self.pane == Pane::Pipeline => {
                // A repo whose draft won't open gets no tour, but the
                // first repo still asks for notifications.
                if self.onboarding.take().is_some() {
                    self.ask_permission(cx);
                }
                self.pipeline
                    .update(cx, |editor, cx| editor.refused(error.message, cx));
            }
            ResponseBody::Error(error) => self.error = Some(error.message),
        }
    }

    /// Shows `pr` in the PR pane, with its open Inbox entries above its
    /// latest Run, as a clicked banner asks. A PR the list doesn't have yet
    /// is shown once it arrives.
    pub fn reveal(&mut self, pr: PrRef) {
        match self.prs.pr(&pr.repo, pr.number).cloned() {
            Some(row) => {
                self.pane = Pane::Prs;
                self.prs.source = Source::All;
                self.prs.expand_to(&row.repo, row.number);
                for command in self.run_pane.select_pr(&row) {
                    self.send(command);
                }
            }
            None if !self.prs.loaded() => self.pending_reveal = Some(pr),
            // Gone since the banner posted, such as merged.
            None => {}
        }
    }

    /// Asks for notification permission at the first repo the developer
    /// adds, if they haven't been asked.
    fn ask_permission(&self, cx: &App) {
        let center = SystemCenter::new(cx);
        if center.permission() == Permission::NotAsked {
            center.ask();
        }
    }

    /// Shows `repo`'s Pipeline editor.
    fn open_pipeline(&mut self, repo: &RepoName, cx: &mut Context<Self>) {
        self.pane = Pane::Pipeline;
        let prs = self.prs.in_repo(repo);
        self.pipeline.update(cx, |editor, cx| {
            editor.open(repo, cx);
            editor.set_prs(prs, cx);
        });
    }

    /// Reads the permission again, and redraws once macOS has answered.
    fn refresh_permission(&self, cx: &mut Context<Self>) {
        SystemCenter::new(cx).refresh();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    /// Sends the Waiver form with the reason typed in, if it has one.
    fn submit_waiver(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let reason = self.waiver_reason.read(cx).value().to_string();
        let action = self
            .run_pane
            .waiver_form()
            .map(|form| waive_action(&form.target));
        if let Some(action) = action.filter(|action| !self.outbox.waiting(action))
            && let Some(command) = self.run_pane.submit_waiver(&reason)
        {
            self.outbox.press(&action, command);
            self.waiver_reason
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
    }

    fn send(&self, command: Command) {
        // A page of the open log shows it's loading, or why it didn't.
        if matches!(command, Command::ReadStepLog { .. }) {
            self.outbox.load(LOG, command);
        } else {
            self.outbox.send(command);
        }
    }

    /// Changes the open log viewer and sends the command it asks for.
    fn with_viewer(&mut self, change: impl FnOnce(&mut LogViewer) -> Option<Command>) {
        if let Some(command) = self.run_pane.viewer_mut().and_then(change) {
            self.send(command);
        }
    }

    /// Opens the daemon settings screen, with the settings as they are.
    fn show_settings(&mut self, cx: &mut Context<Self>) {
        self.pane = Pane::Settings;
        self.settings.update(cx, |view, _| view.reload());
        cx.notify();
    }

    fn show_prs(&mut self, source: Source) {
        self.pane = Pane::Prs;
        self.prs.source = source;
    }

    fn showing(&self, source: &Source) -> bool {
        self.pane == Pane::Prs && &self.prs.source == source
    }

    fn sources(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let entry = |id: SharedString, label: String, count: Option<usize>, selected: bool| {
            div()
                .id(id)
                .flex()
                .justify_between()
                .px_3()
                .py_1()
                .rounded_md()
                .text_sm()
                .when(selected, |this| this.bg(theme.list_active))
                .hover(|this| this.bg(theme.list_hover))
                .child(label)
                .children(count.filter(|&n| n > 0).map(|n| {
                    div()
                        .text_color(theme.muted_foreground)
                        .child(n.to_string())
                }))
        };

        let mut pane = div()
            .id("sources")
            .w(px(240.))
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .bg(theme.muted)
            .border_r_1()
            .border_color(theme.border)
            .overflow_y_scroll()
            .child(
                entry(
                    "source-inbox".into(),
                    "Inbox".to_owned(),
                    Some(self.inbox.count()),
                    self.pane == Pane::Inbox,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.pane = Pane::Inbox;
                    cx.notify();
                })),
            )
            .child(
                entry(
                    "source-all".into(),
                    "All PRs".to_owned(),
                    None,
                    self.showing(&Source::All),
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.show_prs(Source::All);
                    cx.notify();
                })),
            )
            .child(
                entry(
                    "source-running".into(),
                    "Running".to_owned(),
                    Some(self.prs.count(&Source::Running)),
                    self.showing(&Source::Running),
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.show_prs(Source::Running);
                    cx.notify();
                })),
            )
            .child(
                entry(
                    "source-ended".into(),
                    "Ended".to_owned(),
                    None,
                    self.showing(&Source::Ended),
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.show_prs(Source::Ended);
                    cx.notify();
                })),
            )
            .child(
                entry(
                    "source-library".into(),
                    "Library".to_owned(),
                    None,
                    self.pane == Pane::Library,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.pane = Pane::Library;
                    this.library.read(cx).refresh();
                    cx.notify();
                })),
            )
            .child(
                entry(
                    "source-secrets".into(),
                    "Secrets".to_owned(),
                    Some(self.secrets.read(cx).unset()),
                    self.pane == Pane::Secrets,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.pane = Pane::Secrets;
                    this.secrets.read(cx).refresh();
                    cx.notify();
                })),
            )
            .child(
                entry(
                    "source-plugins".into(),
                    "Plugins".to_owned(),
                    Some(self.plugins.read(cx).waiting()),
                    self.pane == Pane::Plugins,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.pane = Pane::Plugins;
                    this.plugins.read(cx).refresh();
                    cx.notify();
                })),
            )
            .child(
                entry(
                    "source-settings".into(),
                    "Settings".to_owned(),
                    None,
                    self.pane == Pane::Settings,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.show_settings(cx);
                })),
            )
            .child(
                div()
                    .px_3()
                    .pt_3()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("REPOS"),
            );

        for repo in self.prs.repos() {
            let selected = self.showing(&Source::Repo(repo.clone()));
            let chosen = repo.clone();
            pane = pane.child(
                entry(
                    format!("source-{repo}").into(),
                    repo.to_string(),
                    Some(self.prs.watched_in(repo)),
                    selected,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.show_prs(Source::Repo(chosen.clone()));
                    cx.notify();
                })),
            );
            if selected {
                let chosen = repo.clone();
                pane = pane.child(
                    entry(
                        format!("source-{repo}-pipeline").into(),
                        "    Pipeline".to_owned(),
                        None,
                        false,
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            this.open_pipeline(&chosen, cx);
                            cx.notify();
                        },
                    )),
                );
            }
        }

        let warning = self.prs.storage().map(|warning| {
            div()
                .mt_3()
                .px_3()
                .text_xs()
                .text_color(theme.warning)
                .child(storage_line(warning))
        });
        let pane = match &self.picker {
            None => pane.child(
                div()
                    .px_1()
                    .pt_2()
                    .child(
                        Button::new(ADD_REPO)
                            .label("Add repo…")
                            .small()
                            .ghost()
                            .pending(self.outbox.waiting(ADD_REPO))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                                this.outbox.press(ADD_REPO, Command::ListAvailableRepos);
                            })),
                    )
                    .children(refusal(&self.outbox, ADD_REPO, theme)),
            ),
            Some(available) => {
                let mut picker = div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .mt_2()
                    .p_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .child(
                        div()
                            .px_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Repos you can push to"),
                    );
                if available.is_empty() {
                    picker = picker.child(div().px_2().text_sm().child("None left to add."));
                }
                for repo in available {
                    let chosen = repo.clone();
                    picker = picker.child(
                        div()
                            .id(SharedString::from(format!("add-{repo}")))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_sm()
                            .hover(|this| this.bg(theme.list_hover))
                            .child(repo.to_string())
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.send(Command::AddRepo {
                                    repo: chosen.clone(),
                                });
                                this.open_pipeline(&chosen, cx);
                                this.onboarding = Some(chosen.clone());
                                this.prs.source = Source::Repo(chosen.clone());
                                this.picker = None;
                                cx.notify();
                            })),
                    );
                }
                picker = picker.child(
                    Button::new("close-picker")
                        .label("Cancel")
                        .small()
                        .ghost()
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.picker = None;
                            cx.notify();
                        })),
                );
                pane.child(picker)
            }
        };
        pane.children(warning)
    }

    fn pr_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut list = div()
            .id("pr-list")
            .map(|this| {
                if self.run_pane.graph_shown() {
                    this.flex_none().w(px(GRAPH_MODE_LIST_WIDTH))
                } else {
                    this.flex_1()
                }
            })
            .h_full()
            .flex()
            .flex_col()
            .overflow_y_scroll();

        let rows = self.prs.rows();
        if rows.is_empty() {
            let empty = if !self.prs.loaded() {
                "Loading…"
            } else if self.prs.repos().is_empty() {
                "Add a repo to see your open PRs."
            } else if self.prs.source == Source::Running {
                "No PR has a Run going."
            } else if self.prs.source == Source::Ended {
                "No PR's latest Run has ended."
            } else {
                "No open PRs by you."
            };
            list = list.child(
                div()
                    .p_6()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(empty),
            );
        }
        for row in rows {
            list = match row {
                PrRow::Stack(stack) => list.child(self.stack_row(stack, cx)),
                PrRow::Pr { pr, .. } => list.child(self.pr_row(pr, row, cx)),
                PrRow::Parent { repo, parent, .. } => {
                    list.child(self.unlisted_parent_row(repo, parent, row, cx))
                }
            };
        }
        list
    }

    /// A Stack's summary row. A click expands or collapses it.
    fn stack_row(&self, stack: Stack<'_>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let tone = match stack.worst() {
            Some(PrState::NeedsYou) => theme.warning,
            Some(PrState::Failed) => theme.danger,
            Some(PrState::Passed) => theme.success,
            _ => theme.muted_foreground,
        };
        // The Gate of the PR that needs attention, and why it does.
        let attention = stack.attention.map(|attention| {
            let gate = attention.pr.runs.first().map(|run| run.gate);
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_xs()
                .children(gate.map(|gate| {
                    div()
                        .px_2()
                        .rounded_md()
                        .border_1()
                        .border_color(tone_color(theme, gate_tone(gate)))
                        .text_color(tone_color(theme, gate_tone(gate)))
                        .child(format!("Gate {gate}"))
                }))
                .child(div().truncate().child(attention.line()))
        });
        let (repo, root) = (stack.repo.clone(), stack.root);
        div()
            .id(SharedString::from(format!(
                "stack-{}-{}",
                stack.repo, stack.root
            )))
            .flex()
            .items_center()
            .gap_3()
            .pr_4()
            .pl(row_indent(PrRow::Stack(stack)))
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .hover(|this| this.bg(theme.list_hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.prs.toggle(&repo, root);
                cx.notify();
            }))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if stack.expanded { "▾" } else { "▸" }),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .text_sm()
                            .child(div().truncate().child(stack.title()))
                            .child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(stack.size_line()),
                            ),
                    )
                    .child(div().text_xs().text_color(tone).child(stack.status_line())),
            )
            .children(attention)
    }

    /// A Stack parent that isn't one of the developer's PRs: dim, and it
    /// doesn't open.
    fn unlisted_parent_row(
        &self,
        repo: &RepoName,
        parent: &StackParent,
        row: PrRow<'_>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .id(SharedString::from(format!(
                "parent-{repo}-{}",
                parent.number
            )))
            .flex()
            .flex_col()
            .gap_0p5()
            .pr_4()
            .pl(row_indent(row))
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .opacity(DIM)
            .child(div().text_sm().truncate().child(parent.title.clone()))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{repo}#{} · not yours, not watched", parent.number)),
            )
    }

    fn pr_row(
        &self,
        pr: &PullRequest,
        stacked: PrRow<'_>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let selectable = stacked.selectable();
        let tone = match pr.status {
            PrStatus::NotWatched => theme.muted_foreground,
            PrStatus::Waiting => theme.warning,
            PrStatus::Ready => theme.success,
        };
        let watched = pr.watched();
        let (action, command) = toggle_watch(pr);
        let refused = refusal(&self.outbox, &action, theme);
        let toggle = Button::new(SharedString::from(action.clone()))
            .label(if watched { "Unwatch" } else { "Watch" })
            .small()
            .when(watched, |button| button.ghost())
            .when(!watched, |button| button.primary())
            .pending(self.outbox.waiting(&action))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                this.outbox.press(&action, command.clone());
            }));

        let selected = self.run_pane.selected() == Some(&(pr.repo.clone(), pr.number));
        let row = pr.clone();
        div()
            .id(SharedString::from(format!("pr-{}-{}", pr.repo, pr.number)))
            .flex()
            .items_center()
            .gap_3()
            .pr_4()
            .pl(row_indent(stacked))
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .when(selected, |this| this.bg(theme.list_active))
            .when(!selectable, |this| this.opacity(DIM))
            .when(selectable, |this| {
                this.hover(|this| this.bg(theme.list_hover))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        for command in this.run_pane.select_pr(&row) {
                            this.send(command);
                        }
                        cx.notify();
                    }))
            })
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .overflow_hidden()
                    .child(div().text_sm().truncate().child(pr.title.clone()))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .text_xs()
                            .child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{}#{}", pr.repo, pr.number)),
                            )
                            .child(div().text_color(tone).child(status_line(pr))),
                    )
                    .children(refused),
            )
            .child(toggle)
    }

    /// Every open entry, oldest first. Clicking one opens its first PR in
    /// the PR pane.
    fn inbox_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut list = div()
            .id("inbox-list")
            .map(|this| {
                if self.run_pane.graph_shown() {
                    this.flex_none().w(px(GRAPH_MODE_LIST_WIDTH))
                } else {
                    this.flex_1()
                }
            })
            .h_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .overflow_y_scroll();
        if let Some(text) = notice(SystemCenter::new(cx).permission()) {
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(text)
                    .child(
                        div().child(
                            Button::new("notification-settings")
                                .label("Open System Settings")
                                .small()
                                .ghost()
                                .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                    cx.open_url(&settings_url(Flavor::CURRENT.bundle_id()));
                                })),
                        ),
                    ),
            );
        }
        if self.inbox.entries().is_empty() {
            list = list.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Nothing needs you."),
            );
        }
        for entry in self.inbox.entries() {
            let held = entry
                .prs
                .first()
                .and_then(|pr| self.prs.pr(&pr.repo, pr.number))
                .cloned();
            let card = self
                .entry_card(entry, "inbox", false, cx)
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(held_line(entry)),
                )
                .hover(|this| this.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if let Some(pr) = &held {
                        this.prs.expand_to(&pr.repo, pr.number);
                        for command in this.run_pane.select_pr(pr) {
                            this.send(command);
                        }
                    }
                    cx.notify();
                }));
            list = list.child(card);
        }
        list
    }

    /// An open entry: its title, reasons and the buttons it offers.
    fn entry_card(
        &self,
        entry: &InboxEntry,
        prefix: &str,
        with_answers: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme().clone();
        let mut buttons = div().flex().gap_1();
        // What the entry's buttons sent, to show their refusals under it.
        let mut sent = Vec::new();
        for (label, command) in actions(entry) {
            let id = SharedString::from(format!("{prefix}-{label}-{}", entry.id));
            let action = entry_action(&label, entry);
            buttons = buttons.child(
                Button::new(id)
                    .label(label)
                    .small()
                    .ghost()
                    .pending(self.outbox.waiting(&action))
                    .on_click(cx.listener({
                        let action = action.clone();
                        move |this, _: &ClickEvent, _, _| {
                            this.outbox.press(&action, command.clone());
                        }
                    })),
            );
            sent.push(action);
        }
        if matches!(
            entry.scope,
            Scope::Cause {
                cause: Cause::DailyBudget
            }
        ) {
            let id = SharedString::from(format!("{prefix}-budget-settings-{}", entry.id));
            buttons = buttons.child(Button::new(id).label("Settings").small().ghost().on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.show_settings(cx);
                }),
            ));
        }
        if let Some(name) = missing_secret(entry).map(str::to_owned) {
            let id = SharedString::from(format!("{prefix}-set-secret-{}", entry.id));
            buttons = buttons.child(
                Button::new(id)
                    .label("Set Secret")
                    .small()
                    .ghost()
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.pane = Pane::Secrets;
                        this.secrets
                            .update(cx, |view, cx| view.choose(&name, window, cx));
                        this.secrets.read(cx).refresh();
                        cx.notify();
                    })),
            );
        }
        // A Human Step is answered next to the note field, which only the
        // PR pane draws.
        if with_answers && matches!(entry.scope, Scope::Human { .. }) {
            for (label, answer) in ANSWERS {
                let id = SharedString::from(format!("{prefix}-{label}-{}", entry.id));
                let action = entry_action(label, entry);
                let entry = entry.clone();
                buttons = buttons.child(
                    Button::new(id)
                        .label(label)
                        .small()
                        .ghost()
                        .pending(self.outbox.waiting(&action))
                        .on_click(cx.listener({
                            let action = action.clone();
                            move |this, _: &ClickEvent, window, cx| {
                                let note = this.answer_note.read(cx).value().to_string();
                                if let Some(command) = answer_entry(&entry, answer, &note)
                                    && this.outbox.press(&action, command)
                                {
                                    this.answer_note
                                        .update(cx, |input, cx| input.set_value("", window, cx));
                                }
                            }
                        })),
                );
                sent.push(action);
            }
        }
        if let Some(plugin) = unapproved_plugin(entry).map(str::to_owned) {
            let id = SharedString::from(format!("{prefix}-review-plugin-{}", entry.id));
            buttons = buttons.child(
                Button::new(id)
                    .label("Review Plugin")
                    .small()
                    .ghost()
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.pane = Pane::Plugins;
                        this.plugins
                            .update(cx, |view, cx| view.choose(&plugin, window, cx));
                        this.plugins.read(cx).refresh();
                        cx.notify();
                    })),
            );
        }
        let mut card = div()
            .id(SharedString::from(format!("{prefix}-entry-{}", entry.id)))
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(theme.warning)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(div().text_sm().child(entry.title.clone()))
                    .child(buttons),
            );
        for reason in &entry.reasons {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("• {reason}")),
            );
        }
        for action in &sent {
            card = card.children(refusal(&self.outbox, action, &theme));
        }
        card
    }

    /// The selected PR: its Run history chips, newest first, then the Run
    /// shown as a Step list with the Gate as its last row, or as a graph.
    fn pr_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let color = |tone| tone_color(&theme, tone);
        let mut pane = div()
            .id("pr-pane")
            .map(|this| {
                if self.run_pane.graph_shown() {
                    this.flex_1().min_w_0()
                } else {
                    this.w(px(400.))
                }
            })
            .h_full()
            .flex()
            .flex_col()
            .gap_3()
            .p(px(PANE_PADDING))
            .border_l_1()
            .border_color(theme.border)
            .overflow_y_scroll();
        let Some(pr) = self
            .run_pane
            .selected()
            .and_then(|(repo, number)| self.prs.pr(repo, *number))
        else {
            return pane.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Select a PR to see its Runs."),
            );
        };

        let mode = self.run_pane.mode();
        pane = pane.child(
            div()
                .flex()
                .items_start()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .min_w_0()
                        .child(div().text_sm().child(pr.title.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("{}#{}", pr.repo, pr.number)),
                        ),
                )
                .child(
                    ButtonGroup::new("run-mode")
                        .small()
                        .outline()
                        .children(RunMode::ALL.map(|each| {
                            Button::new(SharedString::from(format!("run-mode-{}", each.label())))
                                .label(each.label())
                                .selected(mode == each)
                        }))
                        .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                            if let Some(&mode) = clicked.first().and_then(|&i| RunMode::ALL.get(i))
                            {
                                this.run_pane.set_mode(mode);
                                cx.notify();
                            }
                        })),
                ),
        );
        if let Some(blocked) = &pr.blocked {
            pane = pane.child(
                div()
                    .text_xs()
                    .text_color(theme.warning)
                    .child(blocked.clone()),
            );
        }
        let entries = self.inbox.for_pr(&pr.repo, pr.number);
        for entry in &entries {
            pane = pane.child(self.entry_card(entry, "pr", true, cx));
        }
        // One field, drawn once, for whichever Human Step gets answered.
        if entries
            .iter()
            .any(|entry| matches!(entry.scope, Scope::Human { .. }))
        {
            pane = pane.child(Input::new(&self.answer_note).small());
        }
        if pr.runs.is_empty() {
            return pane.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No Runs yet."),
            );
        }

        let mut chips = div().flex().flex_wrap().gap_1();
        for run in &pr.runs {
            let shown = self.run_pane.shown() == Some(run.id);
            let (id, row) = (run.id, pr.clone());
            chips = chips.child(
                div()
                    .id(SharedString::from(format!("run-chip-{id}")))
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(if shown {
                        theme.foreground
                    } else {
                        theme.border
                    })
                    .text_xs()
                    .text_color(color(run_tone(run)))
                    .hover(|this| this.bg(theme.list_hover))
                    .child(format!("#{id} {}", run_label(run)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        for command in this.run_pane.select_run(id, &row) {
                            this.send(command);
                        }
                        cx.notify();
                    })),
            );
        }
        pane = pane.child(chips);

        let Some(view) = self.run_pane.view() else {
            return pane;
        };
        let short = |sha: &str| sha.chars().take(7).collect::<String>();
        pane = pane.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "Head {} · Pipeline from {} at {}",
                            short(&view.head_sha),
                            view.base,
                            short(&view.base_sha)
                        )),
                )
                .when_some(self.run_pane.cancel(), |this, cancel| {
                    this.child(
                        Button::new(CANCEL_RUN)
                            .label("Cancel Run")
                            .small()
                            .ghost()
                            .pending(self.outbox.waiting(CANCEL_RUN))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                                this.outbox.press(CANCEL_RUN, cancel.clone());
                            })),
                    )
                }),
        );
        pane = pane.children(refusal(&self.outbox, CANCEL_RUN, &theme));
        if let Some(viewer) = self.run_pane.viewer() {
            return pane.child(self.log_viewer(viewer, view, cx));
        }
        if mode == RunMode::Graph {
            let pane_view = cx.entity().downgrade();
            let on_select = Rc::new(move |step: &str, _: &mut Window, cx: &mut App| {
                let _ = pane_view.update(cx, |this, cx| {
                    for command in this.run_pane.toggle_step(step) {
                        this.send(command);
                    }
                    cx.notify();
                });
            });
            pane = pane
                .child(run_graph(
                    view,
                    self.run_pane.open_step(),
                    &theme,
                    on_select,
                ))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Solid: needs · dashed: read by the Gate · dashed box: advisory"),
                );
            let open = self.run_pane.open_step().and_then(|id| view.step(id));
            if let Some(step) = open {
                pane = pane.child(
                    div()
                        .flex()
                        .flex_col()
                        .border_1()
                        .border_color(theme.border)
                        .rounded_md()
                        .child(self.step_row(step, view, cx)),
                );
            }
            if self.run_pane.can_override() {
                pane = pane.child(
                    div().child(
                        Button::new("override-gate")
                            .label("Override Gate")
                            .small()
                            .ghost()
                            .pending(self.outbox.waiting(&waive_action(&WaiveTarget::Gate)))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.run_pane.start_waiver(WaiveTarget::Gate);
                                cx.notify();
                            })),
                    ),
                );
            }
        } else {
            pane = pane.child(self.step_list(view, cx));
        }
        pane = pane.children(refusal(
            &self.outbox,
            &waive_action(&WaiveTarget::Gate),
            &theme,
        ));
        if self.run_pane.waiver_form().is_some() {
            pane = pane.child(self.waiver_form(cx));
        }
        if let Some(cost) = view.cost() {
            pane = pane.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("Run cost: {cost}")),
            );
        }
        if let Some(end) = view.end {
            pane = pane.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("Ended: {}", end_label(end, view.waived))),
            );
        }
        if !view.inbox.is_empty() {
            let mut history = div()
                .flex()
                .flex_col()
                .gap_0p5()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Inbox history");
            for entry in &view.inbox {
                history = history.child(format!("• {}", history_line(entry)));
            }
            pane = pane.child(history);
        }
        pane
    }

    /// The Run as a Step list, with the Gate as its last row.
    fn step_list(&self, view: &RunView, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let color = |tone| tone_color(&theme, tone);
        let mut steps = div()
            .flex()
            .flex_col()
            .border_1()
            .border_color(theme.border)
            .rounded_md();
        for step in &view.steps {
            steps = steps.child(self.step_row(step, view, cx));
        }
        let gate = view.gate.unwrap_or(slopwatch_core::GateState::Pending);
        steps = steps.child(
            div()
                .flex()
                .justify_between()
                .px_3()
                .py_2()
                .text_sm()
                .items_center()
                .gap_2()
                .child(format!("Gate {}", view.gate_text))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .when(self.run_pane.can_override(), |this| {
                            this.child(
                                Button::new("override-gate")
                                    .label("Override Gate")
                                    .small()
                                    .ghost()
                                    .pending(self.outbox.waiting(&waive_action(&WaiveTarget::Gate)))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.run_pane.start_waiver(WaiveTarget::Gate);
                                        cx.notify();
                                    })),
                            )
                        })
                        .child(
                            div()
                                .text_color(color(gate_tone(gate)))
                                .child(gate.to_string()),
                        ),
                ),
        );
        steps
    }

    /// The Waiver being filled in: a category, a reason, and the buttons
    /// that send or drop it. On an ended Run, sending starts a new Run on
    /// the same SHA.
    fn waiver_form(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let Some(form) = self.run_pane.waiver_form() else {
            return div();
        };
        let title = match &form.target {
            WaiveTarget::Step(step) => format!("Waive `{step}` for this head SHA"),
            WaiveTarget::Gate => "Override the Gate: waive every failing term".to_owned(),
        };
        let mut categories = div().flex().flex_wrap().gap_1();
        for category in WaiverCategory::ALL {
            categories = categories.child(
                Button::new(SharedString::from(format!("waiver-category-{category:?}")))
                    .label(category.to_string())
                    .small()
                    .when(form.category == category, |button| button.primary())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.run_pane.pick_category(category);
                        cx.notify();
                    })),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .child(div().text_sm().child(title))
            .child(categories)
            .child(Input::new(&self.waiver_reason).small())
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("waiver-submit")
                            .label("Waive")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.submit_waiver(window, cx);
                            })),
                    )
                    .child(
                        Button::new("waiver-cancel")
                            .label("Cancel")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.run_pane.cancel_waiver();
                                cx.notify();
                            })),
                    ),
            )
    }

    /// A Step's row with its Verdict and reason, its Findings, and its log
    /// tail while open. The list shows one per Step, and the graph shows the open
    /// Step's below the canvas.
    fn step_row(&self, step: &StepView, view: &RunView, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = cx.theme().clone();
        let color = |tone| tone_color(&theme, tone);
        let open = self.run_pane.open_step() == Some(step.info.id.as_str());
        let toggled = step.info.id.clone();
        let retry_action = format!("retry-{}", step.info.id);
        let waive = waive_action(&WaiveTarget::Step(step.info.id.clone()));
        let mut row = div()
            .id(SharedString::from(format!("step-{}", step.info.id)))
            .flex()
            .flex_col()
            .gap_0p5()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .hover(|this| this.bg(theme.list_hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                for command in this.run_pane.toggle_step(&toggled) {
                    this.send(command);
                }
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .gap_2()
                    .text_sm()
                    .child(if step.info.gated {
                        step.info.id.clone()
                    } else {
                        format!("{} (advisory)", step.info.id)
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(step.info.plugin.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(color(step_tone(step)))
                            .child(step_line(step)),
                    )
                    .when_some(self.run_pane.retry(&step.info.id), |this, retry| {
                        this.child(
                            Button::new(SharedString::from(retry_action.clone()))
                                .label("Retry")
                                .small()
                                .ghost()
                                .pending(self.outbox.waiting(&retry_action))
                                .on_click(cx.listener({
                                    let action = retry_action.clone();
                                    move |this, _: &ClickEvent, _, _| {
                                        this.outbox.press(&action, retry.clone());
                                    }
                                })),
                        )
                    })
                    .when(self.run_pane.can_waive(&step.info.id), |this| {
                        let target = WaiveTarget::Step(step.info.id.clone());
                        this.child(
                            Button::new(SharedString::from(waive.clone()))
                                .label("Waive")
                                .small()
                                .ghost()
                                .pending(self.outbox.waiting(&waive))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.run_pane.start_waiver(target.clone());
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .children(waiver_line(step).map(|line| {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(line)
            }))
            .children(commit_line(view, step).map(|line| {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(line)
            }))
            .children(refusal(&self.outbox, &retry_action, &theme))
            .children(refusal(&self.outbox, &waive, &theme));
        if let slopwatch_protocol::StepStatus::Settled { outputs, .. } = &step.status {
            for finding in &outputs.findings {
                row = row.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(finding_line(finding)),
                );
            }
        }
        if open {
            row = row.child(self.step_log_tail(step, view, cx));
        }
        row
    }

    /// The open Step row's last lines, with the way into its full log.
    fn step_log_tail(&self, step: &StepView, view: &RunView, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mut tail = div().flex().flex_col().gap_1().pt_1();
        if let Some(at) = view.pruned_at {
            return tail.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("Log pruned on {}", step_log::date(at))),
            );
        }
        if let StepStatus::Settled {
            reused_from: Some(run),
            ..
        } = step.status
        {
            return tail.child(
                Button::new("reused-log")
                    .label(format!("Full log from Run {run}"))
                    .small()
                    .ghost()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        for command in this.run_pane.open_reused_log(run) {
                            this.send(command);
                        }
                        cx.notify();
                    })),
            );
        }
        if step.attempt == 0 {
            return tail.child(div().text_xs().text_color(muted).child("Not started yet."));
        }
        let mut lines = div()
            .flex()
            .flex_col()
            .p_2()
            .rounded_md()
            .bg(theme.muted)
            .font_family("Menlo")
            .text_xs();
        let mut any = false;
        for record in self.run_pane.tail() {
            any = true;
            lines = lines.child(div().child(step_log::line_text(record)));
        }
        if !any {
            let empty = if matches!(step.status, StepStatus::Running) {
                "Nothing written yet."
            } else {
                "The Step wrote nothing."
            };
            lines = lines.child(div().text_color(muted).child(empty));
        }
        tail = tail.child(lines);
        let mut buttons = div().flex().flex_wrap().gap_1();
        for attempt in (1..=step.attempt).rev() {
            let label = if step.attempt == 1 {
                "Full log".to_owned()
            } else {
                format!("Full log, attempt {attempt}")
            };
            buttons = buttons.child(
                Button::new(SharedString::from(format!("full-log-{attempt}")))
                    .label(label)
                    .small()
                    .ghost()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        for command in this.run_pane.open_log(attempt) {
                            this.send(command);
                        }
                        cx.notify();
                    })),
            );
        }
        tail.child(buttons)
    }

    /// The full-log viewer: attempt tabs, search, the source and level
    /// filters, the Events toggle, then one page of the log.
    fn log_viewer(&self, viewer: &LogViewer, view: &RunView, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let attempts = view
            .step(&viewer.key.step)
            .map_or(viewer.key.attempt, |step| step.attempt.max(1));

        let mut header = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .child(
                Button::new("close-log")
                    .label("← Steps")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.run_pane.close_log();
                        cx.notify();
                    })),
            )
            .child(div().text_sm().child(viewer.key.step.clone()));
        if attempts > 1 {
            for attempt in (1..=attempts).rev() {
                let shown = attempt == viewer.key.attempt;
                header = header.child(
                    Button::new(SharedString::from(format!("attempt-{attempt}")))
                        .label(format!("Attempt {attempt}"))
                        .small()
                        .when(shown, |button| button.primary())
                        .when(!shown, |button| button.ghost())
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            for command in this.run_pane.open_log(attempt) {
                                this.send(command);
                            }
                            cx.notify();
                        })),
                );
            }
        }

        let toggle = |id: &'static str, label: &'static str, on: bool| {
            Button::new(id)
                .label(label)
                .small()
                .when(on, |button| button.primary())
                .when(!on, |button| button.ghost())
        };
        let filter = &viewer.filter;
        let mut filters = div().flex().flex_wrap().gap_1();
        for (id, label, source) in [
            ("source-stderr", "stderr", LogSource::Stderr),
            ("source-log", "log", LogSource::Log),
        ] {
            filters = filters.child(
                toggle(id, label, step_log::shows(&filter.sources, &source)).on_click(cx.listener(
                    move |this, _: &ClickEvent, _, cx| {
                        this.with_viewer(|viewer| Some(viewer.toggle_source(source)));
                        cx.notify();
                    },
                )),
            );
        }
        for (id, label, level) in [
            ("level-debug", "debug", LogLevel::Debug),
            ("level-info", "info", LogLevel::Info),
            ("level-warn", "warn", LogLevel::Warn),
            ("level-error", "error", LogLevel::Error),
        ] {
            filters = filters.child(
                toggle(id, label, step_log::shows(&filter.levels, &level)).on_click(cx.listener(
                    move |this, _: &ClickEvent, _, cx| {
                        this.with_viewer(|viewer| Some(viewer.toggle_level(level)));
                        cx.notify();
                    },
                )),
            );
        }
        filters = filters.child(
            toggle("events", "Events", viewer.events).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.with_viewer(|viewer| {
                        viewer.events = !viewer.events;
                        None
                    });
                    cx.notify();
                },
            )),
        );
        let copied = viewer.text(self.run_pane.events());
        filters = filters.child(
            Button::new("copy-log")
                .label("Copy")
                .small()
                .ghost()
                .on_click(move |_: &ClickEvent, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                }),
        );

        let mut lines = div()
            .flex()
            .flex_col()
            .p_2()
            .rounded_md()
            .bg(theme.muted)
            .font_family("Menlo")
            .text_xs();
        if viewer.has_older() {
            lines = lines.child(
                Button::new("log-older")
                    .label("Older")
                    .small()
                    .ghost()
                    .pending(self.outbox.waiting(LOG))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.with_viewer(LogViewer::older);
                        cx.notify();
                    })),
            );
        }
        let refused = refusal(&self.outbox, LOG, theme);
        let rows = viewer.rows(self.run_pane.events());
        if rows.is_empty() && refused.is_none() {
            let empty = if viewer.loading() {
                "Loading…"
            } else if filter.is_empty() {
                "The log is empty."
            } else {
                "No lines match."
            };
            lines = lines.child(div().text_color(muted).child(empty));
        }
        lines = lines.children(refused);
        for row in &rows {
            let line = div().child(step_log::row_text(row));
            lines = lines.child(match row {
                Row::Line(_) => line,
                Row::Truncated(_) => line.text_color(theme.warning),
                Row::Event { .. } => line.text_color(theme.info),
            });
        }
        if viewer.has_newer() {
            lines = lines.child(
                div()
                    .flex()
                    .gap_1()
                    .child(
                        Button::new("log-newer")
                            .label("Newer")
                            .small()
                            .ghost()
                            .pending(self.outbox.waiting(LOG))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.with_viewer(LogViewer::newer);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("log-follow")
                            .label("Latest")
                            .small()
                            .ghost()
                            .pending(self.outbox.waiting(LOG))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.with_viewer(|viewer| Some(viewer.follow()));
                                cx.notify();
                            })),
                    ),
            );
        } else if viewer.following()
            && matches!(
                view.step(&viewer.key.step).map(|step| (&step.status, step.attempt)),
                Some((StepStatus::Running, attempt)) if attempt == viewer.key.attempt
            )
        {
            lines = lines.child(div().text_color(muted).child("Following live…"));
        }

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(header)
            .child(Input::new(&self.log_search).small())
            .child(filters)
            .child(lines)
    }

    /// The editor and Graph mode need the width, so they fold the sources
    /// column away.
    fn sources_shown(&self) -> bool {
        match self.pane {
            Pane::Library | Pane::Secrets | Pane::Plugins | Pane::Settings => true,
            Pane::Pipeline => false,
            Pane::Prs | Pane::Inbox => !self.run_pane.graph_shown(),
        }
    }

    /// The Pipeline editor, under a bar that leads back to the repo's PRs.
    fn pipeline_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let repo = self.pipeline.read(cx).repo().cloned();
        let title = repo
            .as_ref()
            .map_or_else(String::new, |repo| format!("Pipeline · {repo}"));
        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("pipeline-back")
                            .label("‹ PRs")
                            .small()
                            .ghost()
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                let source = repo.clone().map_or(Source::All, Source::Repo);
                                this.show_prs(source);
                                cx.notify();
                            })),
                    )
                    .child(div().text_sm().font_weight(FontWeight::BOLD).child(title)),
            )
            .child(div().flex_1().min_h_0().flex().child(self.pipeline.clone()))
    }

    fn footer(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let theme = cx.theme();
        let (text, color) = match (&self.error, poll_line(self.prs.poll())) {
            (Some(error), _) => (error.clone(), theme.danger),
            (None, Some(poll)) => (poll, theme.muted_foreground),
            (None, None) => return None,
        };
        Some(
            div()
                .px_4()
                .py_1()
                .border_t_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(color)
                .child(text),
        )
    }
}

impl Render for MainView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !matches!(self.link, LinkState::Connected { .. }) {
            return div().size_full().child(self.link_view.clone());
        }
        let theme = cx.theme();
        let background = theme.background;
        let foreground = theme.foreground;
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(background)
            .text_color(foreground)
            .child(
                div()
                    .flex_1()
                    .flex()
                    .overflow_hidden()
                    .when(self.sources_shown(), |this| this.child(self.sources(cx)))
                    .map(|this| match self.pane {
                        Pane::Prs => this.child(self.pr_list(cx)).child(self.pr_pane(cx)),
                        Pane::Inbox => this.child(self.inbox_list(cx)).child(self.pr_pane(cx)),
                        Pane::Library => this.child(self.library.clone()),
                        Pane::Secrets => this.child(self.secrets.clone()),
                        Pane::Pipeline => this.child(self.pipeline_pane(cx)),
                        Pane::Plugins => this.child(self.plugins.clone()),
                        Pane::Settings => this.child(self.settings.clone()),
                    }),
            )
            .children(self.footer(cx))
    }
}
