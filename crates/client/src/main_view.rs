//! The main window: the sources pane, then either the Watched PR list or
//! the Inbox, each with the PR pane, or the Library editor, or the Secrets
//! list, or a repo's Pipeline editor, or the link state while the daemon
//! isn't reachable.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use gpui_kit::component::Sizable;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_core::{GateState, WaiverCategory};
use slopwatch_protocol::{
    Answer, Cause, Command, Flavor, InboxEntry, LogLevel, LogSource, PrRef, PrStatus, PullRequest,
    Reply, RepoName, ResponseBody, RunView, Scope, StackParent, StepStatus, StepView, TopicUpdate,
};

use crate::agent::Agent;
use crate::components::{
    self, Ask, ButtonLooks, ChipKind, ask_card, chip, dot, gate_pill, mono, run_pill, section,
    segment, severity, strip, verdict_label, well,
};
use crate::dock;
use crate::inbox::{
    ANSWERS, InboxModel, actions, answer as answer_entry, badge, held_line, history_line, kicker,
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
use crate::run_graph_view::run_graph;
use crate::run_pane::{
    GRAPH_MODE_LIST_WIDTH, Look, PANE_PADDING, RunMode, RunPane, WaiveTarget, commit_line,
    end_label, run_look, split_at_gate, step_look, step_state, waiver_line,
};
use crate::secrets::missing_secret;
use crate::secrets_view::SecretsView;
use crate::settings_view::SettingsView;
use crate::step_log::{self, LogViewer, Row};
use crate::theme;

/// How see-through a PR list row is when it doesn't open.
const DIM: f32 = 0.55;
/// The prototype's column widths: the sources pane, then the PR list or
/// the Inbox. The PR pane takes the rest.
const SOURCES_WIDTH: f32 = 210.;
const LIST_WIDTH: f32 = 380.;
/// The PR list row under the mouse, which shows its Unwatch button.
const ROW_GROUP: &str = "pr-row";

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
    px(12.0 + 20.0 * row.depth() as f32)
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
        let entry = |id: SharedString, label: String, count: Option<Div>, selected: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .px(px(10.))
                .py(px(5.))
                .rounded(px(6.))
                .cursor_pointer()
                .when(selected, |this| this.bg(theme::CHOSEN))
                .when(!selected, |this| this.hover(|this| this.bg(theme::HOVER)))
                .child(div().truncate().child(label))
                .children(count)
        };
        let counted = |n: usize| (n > 0).then(|| components::count(n, false));
        let inbox = self.inbox.count();

        let mut pane = div()
            .id("sources")
            .w(px(SOURCES_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .px(px(6.))
            .py(px(10.))
            .bg(theme::PANEL)
            .border_r_1()
            .border_color(theme::LINE)
            .overflow_y_scroll()
            .child(
                entry(
                    "source-inbox".into(),
                    "Inbox".to_owned(),
                    (inbox > 0).then(|| components::count(inbox, true)),
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
                    counted(self.prs.count(&Source::Running)),
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
                    counted(self.prs.count(&Source::Ended)),
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
                    counted(self.secrets.read(cx).unset()),
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
                    counted(self.plugins.read(cx).waiting()),
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
                    .px(px(10.))
                    .pt(px(10.))
                    .pb(px(4.))
                    .text_size(theme::LABEL_SIZE)
                    .text_color(theme::MUTED)
                    .child("REPOS"),
            );

        for repo in self.prs.repos() {
            let selected = self.showing(&Source::Repo(repo.clone()));
            let chosen = repo.clone();
            pane = pane.child(
                entry(
                    format!("source-{repo}").into(),
                    repo.to_string(),
                    counted(self.prs.watched_in(repo)),
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
                        "Pipeline".to_owned(),
                        None,
                        false,
                    )
                    .pl(px(22.))
                    .text_color(theme::DIM)
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
                .px(px(10.))
                .text_xs()
                .text_color(theme::INC)
                .child(storage_line(warning))
        });
        let pane = pane.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .px(px(4.))
                .pt_2()
                .child(
                    Button::new(ADD_REPO)
                        .label("Add repo…")
                        .small()
                        .when(self.picker.is_some(), |button| button.accent())
                        .pending(self.outbox.waiting(ADD_REPO))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                            this.outbox.press(ADD_REPO, Command::ListAvailableRepos);
                        })),
                )
                .children(refusal(&self.outbox, ADD_REPO)),
        );
        pane.children(warning)
    }

    /// The repos the developer can add, on a card in the middle of the
    /// window: the tour's first step.
    fn repo_picker(&self, available: &[RepoName], cx: &mut Context<Self>) -> impl IntoElement {
        let mut card = components::card()
            .w(px(560.))
            .p(px(16.))
            .gap(px(4.))
            .child(
                div()
                    .text_size(theme::LABEL_SIZE)
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::ACCENT)
                    .child("ADD A REPO"),
            )
            .child(
                div()
                    .pb(px(6.))
                    .text_size(theme::HEADING_SIZE)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Which repo?"),
            );
        if available.is_empty() {
            card = card.child(div().text_color(theme::DIM).child("None left to add."));
        }
        for repo in available {
            let chosen = repo.clone();
            card = card.child(
                div()
                    .id(SharedString::from(format!("add-{repo}")))
                    .py(px(6.))
                    .border_b_1()
                    .border_color(theme::LINE)
                    .cursor_pointer()
                    .hover(|this| this.text_color(theme::ACCENT))
                    .child(mono(repo.to_string()))
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
        card = card
            .child(div().pt(px(6.)).text_xs().text_color(theme::DIM).child(
                "From gh: repos you can push to. The daemon makes its own blobless clone, \
                 never your checkout.",
            ))
            .child(
                div().pt(px(6.)).flex().child(
                    Button::new("close-picker")
                        .label("Cancel")
                        .small()
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.picker = None;
                            cx.notify();
                        })),
                ),
            );
        div()
            .relative()
            .flex_1()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .child(components::dot_grid())
            .child(card)
    }

    /// The PR list's or the Inbox's column: fixed, and narrower while the
    /// PR pane draws a graph.
    fn list_column(&self, id: &'static str) -> Stateful<Div> {
        let width = if self.run_pane.graph_shown() {
            GRAPH_MODE_LIST_WIDTH
        } else {
            LIST_WIDTH
        };
        div()
            .id(id)
            .w(px(width))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(theme::LINE)
            .overflow_y_scroll()
    }

    fn pr_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = self.list_column("pr-list");
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
            list = list.child(div().p(px(14.)).text_color(theme::DIM).child(empty));
        }
        for row in rows {
            list = match row {
                PrRow::Stack(stack) => list.child(self.stack_row(stack, cx)),
                PrRow::Pr { pr, .. } => list.child(self.pr_row(pr, row, cx)),
                PrRow::Parent { repo, parent, .. } => {
                    list.child(self.unlisted_parent_row(repo, parent, row))
                }
            };
        }
        list
    }

    /// A row of the PR list, with its left padding stepped in for a
    /// Stack's PRs.
    fn row(id: SharedString, row: PrRow<'_>) -> Stateful<Div> {
        div()
            .id(id)
            .flex()
            .flex_col()
            .gap(px(4.))
            .pr(px(12.))
            .pl(row_indent(row))
            .py(px(9.))
            .border_b_1()
            .border_color(theme::LINE)
    }

    /// A Stack's summary row. A click expands or collapses it.
    fn stack_row(&self, stack: Stack<'_>, cx: &mut Context<Self>) -> impl IntoElement {
        let tone = match stack.worst() {
            Some(PrState::NeedsYou) => theme::ASK,
            Some(PrState::Failed) => theme::ESCALATION_TEXT,
            Some(PrState::Passed) => theme::PASS,
            _ => theme::DIM,
        };
        // The Gate of the PR that needs attention, and why it does.
        let gate = stack
            .attention
            .and_then(|attention| attention.pr.runs.first())
            .map(run_pill);
        let attention = stack.attention.map(|attention| attention.line());
        let (repo, root) = (stack.repo.clone(), stack.root);
        Self::row(
            SharedString::from(format!("stack-{}-{}", stack.repo, stack.root)),
            PrRow::Stack(stack),
        )
        .cursor_pointer()
        .hover(|this| this.bg(theme::ROW_HOVER))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.prs.toggle(&repo, root);
            cx.notify();
        }))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme::DIM)
                        .child(if stack.expanded { "▾" } else { "▸" }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(stack.title()),
                )
                .children(gate),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme::DIM)
                .child(stack.size_line()),
        )
        .child(
            div()
                .flex()
                .gap_2()
                .text_xs()
                .child(
                    div()
                        .flex_none()
                        .text_color(tone)
                        .child(stack.status_line()),
                )
                .children(attention.map(|line| {
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(theme::DIM)
                        .child(line)
                })),
        )
    }

    /// A Stack parent that isn't one of the developer's PRs: dim, and it
    /// doesn't open.
    fn unlisted_parent_row(
        &self,
        repo: &RepoName,
        parent: &StackParent,
        row: PrRow<'_>,
    ) -> impl IntoElement {
        Self::row(
            SharedString::from(format!("parent-{repo}-{}", parent.number)),
            row,
        )
        .opacity(DIM)
        .child(
            div()
                .truncate()
                .font_weight(FontWeight::SEMIBOLD)
                .child(parent.title.clone()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme::DIM)
                .child(format!("{repo}#{} · not yours, not watched", parent.number)),
        )
    }

    fn pr_row(
        &self,
        pr: &PullRequest,
        stacked: PrRow<'_>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selectable = stacked.selectable();
        let latest = pr.runs.first().filter(|_| pr.watched());
        let (status, tone) = match self.prs.waiting_on_you(pr) {
            Some(waiting) if waiting.human => (waiting.line(), theme::ASK),
            Some(waiting) => (waiting.line(), theme::ESCALATION_TEXT),
            None if pr.blocked.is_some() || pr.status == PrStatus::Waiting => {
                (status_line(pr), theme::INC)
            }
            None => (status_line(pr), theme::DIM),
        };
        let watched = pr.watched();
        let (action, command) = toggle_watch(pr);
        let waiting = self.outbox.waiting(&action);
        let refused = refusal(&self.outbox, &action);
        // A watched row offers Unwatch only under the mouse.
        let quiet = watched && !waiting && refused.is_none();
        let toggle = Button::new(SharedString::from(action.clone()))
            .label(if watched { "Unwatch" } else { "Watch" })
            .xsmall()
            .when(!watched, |button| button.accent())
            .when(quiet, |button| {
                button
                    .invisible()
                    .group_hover(ROW_GROUP, |style| style.visible())
            })
            .pending(waiting)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                this.outbox.press(&action, command.clone());
            }));

        let selected = self.run_pane.selected() == Some(&(pr.repo.clone(), pr.number));
        let row = pr.clone();
        Self::row(
            SharedString::from(format!("pr-{}-{}", pr.repo, pr.number)),
            stacked,
        )
        .group(ROW_GROUP)
        .when(selected, |this| this.bg(theme::ROW_CHOSEN))
        .when(!selectable, |this| this.opacity(DIM))
        .when(selectable && !selected, |this| {
            this.hover(|this| this.bg(theme::ROW_HOVER))
        })
        .when(selectable, |this| {
            this.cursor_pointer()
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    for command in this.run_pane.select_pr(&row) {
                        this.send(command);
                    }
                    cx.notify();
                }))
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(pr.title.clone()),
                )
                .children(latest.map(run_pill)),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(theme::DIM)
                        .child(format!("{}#{}", pr.repo, pr.number)),
                )
                .children(latest.map(|run| strip(&run.strip))),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.))
                        .text_color(tone)
                        .child(status),
                )
                .child(toggle),
        )
        .children(refused)
    }

    /// Every open entry, oldest first. Clicking one opens its first PR in
    /// the PR pane.
    fn inbox_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = self.list_column("inbox-list").gap_2().p(px(12.));
        if let Some(text) = notice(SystemCenter::new(cx).permission()) {
            list = list.child(
                components::card()
                    .text_xs()
                    .text_color(theme::DIM)
                    .child(text)
                    .child(
                        div().child(
                            Button::new("notification-settings")
                                .label("Open System Settings")
                                .small()
                                .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                    cx.open_url(&settings_url(Flavor::CURRENT.bundle_id()));
                                })),
                        ),
                    ),
            );
        }
        if self.inbox.entries().is_empty() {
            list = list.child(div().text_color(theme::DIM).child("Nothing needs you."));
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
                        .text_color(theme::DIM)
                        .child(held_line(entry)),
                )
                .cursor_pointer()
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

    /// An open entry as the prototype's amber card for a Human Step, or red
    /// for an Escalation: its kicker, title, reasons and the buttons it
    /// offers. `with_answers` gives a Human Step its note field and
    /// answers, which only the PR pane draws.
    fn entry_card(
        &self,
        entry: &InboxEntry,
        prefix: &str,
        with_answers: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let human = matches!(entry.scope, Scope::Human { .. });
        let mut buttons = div().flex().flex_wrap().gap(px(6.)).mt(px(4.));
        let mut any_button = false;
        // What the entry's buttons sent, to show their refusals under it.
        let mut sent = Vec::new();
        if with_answers && human {
            for (label, answer) in ANSWERS {
                let id = SharedString::from(format!("{prefix}-{label}-{}", entry.id));
                let action = entry_action(label, entry);
                let entry = entry.clone();
                any_button = true;
                buttons = buttons.child(
                    Button::new(id)
                        .label(label)
                        .small()
                        .map(|button| match answer {
                            Answer::Approve => button.approve(),
                            Answer::Reject => button.reject(),
                        })
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
        for (label, command) in actions(entry) {
            let id = SharedString::from(format!("{prefix}-{label}-{}", entry.id));
            let action = entry_action(&label, entry);
            any_button = true;
            buttons = buttons.child(
                Button::new(id)
                    .label(label)
                    .small()
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
            any_button = true;
            buttons = buttons.child(Button::new(id).label("Settings").small().on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.show_settings(cx);
                }),
            ));
        }
        if let Some(name) = missing_secret(entry).map(str::to_owned) {
            let id = SharedString::from(format!("{prefix}-set-secret-{}", entry.id));
            any_button = true;
            buttons = buttons.child(Button::new(id).label("Set Secret").small().on_click(
                cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.pane = Pane::Secrets;
                    this.secrets
                        .update(cx, |view, cx| view.choose(&name, window, cx));
                    this.secrets.read(cx).refresh();
                    cx.notify();
                }),
            ));
        }
        if let Some(plugin) = unapproved_plugin(entry).map(str::to_owned) {
            let id = SharedString::from(format!("{prefix}-review-plugin-{}", entry.id));
            any_button = true;
            buttons = buttons.child(Button::new(id).label("Review Plugin").small().on_click(
                cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.pane = Pane::Plugins;
                    this.plugins
                        .update(cx, |view, cx| view.choose(&plugin, window, cx));
                    this.plugins.read(cx).refresh();
                    cx.notify();
                }),
            ));
        }
        let ask = if human { Ask::Human } else { Ask::Escalation };
        let mut card = ask_card(ask, &kicker(entry, now_millis() / 1000))
            .id(SharedString::from(format!("{prefix}-entry-{}", entry.id)))
            .child(div().child(entry.title.clone()));
        for reason in &entry.reasons {
            card = card.child(div().text_xs().text_color(theme::DIM).child(reason.clone()));
        }
        // One field, drawn once, for whichever Human Step gets answered.
        if with_answers && human {
            card = card.child(
                div()
                    .mt(px(4.))
                    .child(Input::new(&self.answer_note).small()),
            );
        }
        if any_button {
            card = card.child(buttons);
        }
        for action in &sent {
            card = card.children(refusal(&self.outbox, action));
        }
        card
    }

    /// The selected PR: its header and open entries, its Run history
    /// chips, newest first, then the Run shown as a Step list with the Gate
    /// as a row, or as a graph.
    fn pr_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut pane = div()
            .id("pr-pane")
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(PANE_PADDING))
            .pt(px(14.))
            .pb(px(24.))
            .overflow_y_scroll();
        let Some(pr) = self
            .run_pane
            .selected()
            .and_then(|(repo, number)| self.prs.pr(repo, *number))
        else {
            return pane.child(
                div()
                    .text_color(theme::DIM)
                    .child("Select a PR to see its Runs."),
            );
        };

        let shown = pr
            .runs
            .iter()
            .find(|run| Some(run.id) == self.run_pane.shown());
        let url = pr.url.clone();
        pane = pane.child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(theme::TITLE_SIZE)
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(pr.title.clone()),
                        )
                        .children(shown.map(run_pill)),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_xs()
                        .text_color(theme::DIM)
                        .child(format!("{}#{} ·", pr.repo, pr.number))
                        .child(components::link("pr-link", "PR on GitHub ↗", url)),
                ),
        );
        if let Some(blocked) = &pr.blocked {
            pane = pane.child(
                div()
                    .text_xs()
                    .text_color(theme::INC)
                    .child(blocked.clone()),
            );
        }
        // An open Human Step or Escalation, with its actions inline.
        let entries = self.inbox.for_pr(&pr.repo, pr.number);
        let first_human = entries
            .iter()
            .position(|entry| matches!(entry.scope, Scope::Human { .. }));
        for (index, entry) in entries.iter().enumerate() {
            let answers = Some(index) == first_human;
            pane = pane.child(self.entry_card(entry, "pr", answers, cx));
        }
        if pr.runs.is_empty() {
            return pane.child(div().text_color(theme::DIM).child("No Runs yet."));
        }

        let mut chips = div().flex().flex_wrap().gap(px(6.));
        for run in &pr.runs {
            let on = self.run_pane.shown() == Some(run.id);
            let (id, row) = (run.id, pr.clone());
            chips = chips.child(
                div()
                    .id(SharedString::from(format!("run-chip-{id}")))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .px(px(8.))
                    .py(px(2.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(if on { theme::ACCENT } else { theme::LINE })
                    .when(on, |this| this.bg(theme::ACCENT_BG))
                    .when(!on, |this| this.hover(|this| this.border_color(theme::DIM)))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .child(dot(run_look(run)))
                    .child(format!("Run {id}"))
                    .child(div().text_color(theme::DIM).child(match run.end {
                        Some(reason) => end_label(reason, run.waived),
                        None => "live".to_owned(),
                    }))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        for command in this.run_pane.select_run(id, &row) {
                            this.send(command);
                        }
                        cx.notify();
                    })),
            );
        }
        pane = pane.child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(section("Run history"))
                .child(chips),
        );

        let Some(view) = self.run_pane.view() else {
            return pane;
        };
        pane = pane.child(self.run_line(view, cx));
        pane = pane.children(refusal(&self.outbox, CANCEL_RUN));
        if let Some(viewer) = self.run_pane.viewer() {
            return pane.child(self.log_viewer(viewer, view, cx));
        }
        if self.run_pane.mode() == RunMode::Graph {
            let pane_view = cx.entity().downgrade();
            let on_select = Rc::new(move |step: &str, _: &mut Window, cx: &mut App| {
                let _ = pane_view.update(cx, |this, cx| {
                    for command in this.run_pane.toggle_step(step) {
                        this.send(command);
                    }
                    cx.notify();
                });
            });
            pane = pane.child(run_graph(view, self.run_pane.open_step(), on_select));
            let open = self.run_pane.open_step().and_then(|id| view.step(id));
            match open {
                Some(step) => pane = pane.child(self.step_row(step, view, true, cx)),
                None => {
                    pane = pane.child(
                        div()
                            .text_xs()
                            .text_color(theme::DIM)
                            .child("Click a Step for its Outcome."),
                    )
                }
            }
            if self.run_pane.can_override() {
                pane = pane.child(div().flex().child(self.override_button(cx)));
            }
        } else {
            pane = pane.child(self.step_list(view, cx));
        }
        pane = pane.children(refusal(&self.outbox, &waive_action(&WaiveTarget::Gate)));
        if self.run_pane.waiver_form().is_some() {
            pane = pane.child(self.waiver_form(cx));
        }
        if !view.inbox.is_empty() {
            let mut history = div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .text_xs()
                .text_color(theme::DIM)
                .child(section("Inbox history"));
            for entry in &view.inbox {
                history = history.child(history_line(entry));
            }
            pane = pane.child(history);
        }
        pane
    }

    /// One line about the shown Run: its number, head SHA, time, cost and
    /// where its Pipeline came from, then the List/Graph toggle and Cancel
    /// Run.
    fn run_line(&self, view: &RunView, cx: &mut Context<Self>) -> impl IntoElement {
        let short = |sha: &str| sha.chars().take(7).collect::<String>();
        let shown = self.run_pane.shown().map_or(0, |run| run.0);
        let mut facts = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .child(format!("Run {shown} ·"))
            .child(mono(short(&view.head_sha)).text_color(theme::TEXT));
        if let Some(time) = self.run_pane.run_time(now_millis()) {
            let so_far = if view.end.is_some() { "" } else { " so far" };
            facts = facts.child(format!("· {time}{so_far}"));
        }
        if let Some(cost) = view.cost() {
            facts = facts.child(format!("· {cost}"));
        }
        facts = facts
            .child(format!("· Pipeline from {} at", view.base))
            .child(mono(short(&view.base_sha)));
        let mode = self.run_pane.mode();
        let mut toggle = div().flex_none().flex();
        for (index, each) in RunMode::ALL.into_iter().enumerate() {
            toggle = toggle.child(
                segment(
                    SharedString::from(format!("run-mode-{}", each.label())),
                    each.label(),
                    mode == each,
                    index == 0,
                    index + 1 == RunMode::ALL.len(),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.run_pane.set_mode(each);
                    cx.notify();
                })),
            );
        }
        div()
            .flex()
            .items_center()
            .gap_2()
            .text_xs()
            .text_color(theme::DIM)
            .child(facts)
            .child(toggle)
            .when_some(self.run_pane.cancel(), |this, cancel| {
                this.child(
                    Button::new(CANCEL_RUN)
                        .label("Cancel Run")
                        .small()
                        .pending(self.outbox.waiting(CANCEL_RUN))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                            this.outbox.press(CANCEL_RUN, cancel.clone());
                        })),
                )
            })
    }

    fn override_button(&self, cx: &mut Context<Self>) -> Button {
        Button::new("override-gate")
            .label("Override Gate")
            .small()
            .pending(self.outbox.waiting(&waive_action(&WaiveTarget::Gate)))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.run_pane.start_waiver(WaiveTarget::Gate);
                cx.notify();
            }))
    }

    /// The Run as a Step list, with the Gate as a row between the Steps it
    /// reads and the Steps that run after it.
    fn step_list(&self, view: &RunView, cx: &mut Context<Self>) -> Div {
        let (before, after) = split_at_gate(&view.steps);
        let mut steps = div().flex().flex_col().gap(px(6.));
        for step in before {
            steps = steps.child(self.step_row(step, view, false, cx));
        }
        let gate = view.gate.unwrap_or(GateState::Pending);
        let reads: Vec<&str> = view
            .steps
            .iter()
            .filter(|step| step.info.gated)
            .map(|step| step.info.id.as_str())
            .collect();
        steps = steps.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px(px(10.))
                .py(px(7.))
                .rounded(px(8.))
                .border_2()
                .border_color(theme::gate_line(gate))
                .bg(theme::GATE_BG)
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::GATE_TEXT)
                        .child("GATE"),
                )
                .child(gate_pill(gate))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(theme::DIM)
                        .child(reads.join(" · ")),
                )
                .when(self.run_pane.can_override(), |this| {
                    this.child(self.override_button(cx))
                }),
        );
        for step in after {
            steps = steps.child(self.step_row(step, view, false, cx));
        }
        steps
    }

    /// The Waiver being filled in: a category, a reason, and the buttons
    /// that send or drop it. On an ended Run, sending starts a new Run on
    /// the same SHA.
    fn waiver_form(&self, cx: &mut Context<Self>) -> Div {
        let Some(form) = self.run_pane.waiver_form() else {
            return div();
        };
        let title = match &form.target {
            WaiveTarget::Step(step) => format!("Waive {step} for this head SHA"),
            WaiveTarget::Gate => "Override the Gate: waive every failing term".to_owned(),
        };
        let mut categories = div().flex().flex_wrap().gap(px(6.));
        for category in WaiverCategory::ALL {
            categories = categories.child(
                Button::new(SharedString::from(format!("waiver-category-{category:?}")))
                    .label(category.to_string())
                    .small()
                    .when(form.category == category, |button| button.accent())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.run_pane.pick_category(category);
                        cx.notify();
                    })),
            );
        }
        components::card()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(
                div()
                    .text_xs()
                    .text_color(theme::DIM)
                    .child("Counts as pass for this head SHA only. A push clears it."),
            )
            .child(categories)
            .child(Input::new(&self.waiver_reason).small())
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("waiver-cancel")
                            .label("Cancel")
                            .small()
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.run_pane.cancel_waiver();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("waiver-submit")
                            .label("Waive")
                            .small()
                            .accent()
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.submit_waiver(window, cx);
                            })),
                    ),
            )
    }

    /// A Step's row: its dot, name, chips, Verdict and time. Its evidence
    /// shows below while it's open, and on a Step that failed, errored or
    /// waits on the developer. `alone` when the graph shows the row under
    /// the canvas, always open.
    fn step_row(
        &self,
        step: &StepView,
        view: &RunView,
        alone: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let info = &step.info;
        let look = step_look(step);
        let open = alone || self.run_pane.open_step() == Some(info.id.as_str());
        let shows = open || matches!(look, Look::Fail | Look::Error | Look::Asking);
        let after_gate = split_at_gate(&view.steps)
            .1
            .iter()
            .any(|each| each.info.id == info.id);
        let advisory = !info.gated && !after_gate;
        let findings = match &step.status {
            StepStatus::Settled { outputs, .. } => outputs.findings.len(),
            _ => 0,
        };
        let skip_reason = match &step.status {
            StepStatus::Settled {
                verdict: slopwatch_core::Verdict::Skipped,
                reason: Some(reason),
                ..
            } => Some(reason.clone()),
            _ => None,
        };
        let label = verdict_text(step, look);
        let time = self.run_pane.step_time(&info.id, now_millis());
        let toggled = info.id.clone();
        let header = div()
            .id(SharedString::from(format!("step-head-{}", info.id)))
            .flex()
            .items_center()
            .gap_2()
            .px(px(10.))
            .py(px(7.))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                for command in this.run_pane.toggle_step(&toggled) {
                    this.send(command);
                }
                cx.notify();
            }))
            .child(dot(look))
            .child(
                div()
                    .flex_shrink(1.)
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(info.id.clone()),
            )
            .when(info.write, |this| {
                this.child(chip(ChipKind::Write, "commit ends Run").flex_none())
            })
            .when(advisory, |this| {
                this.child(chip(ChipKind::Advisory, "advisory").flex_none())
            })
            .when(findings > 0, |this| {
                let text = match findings {
                    1 => "1 finding".to_owned(),
                    n => format!("{n} findings"),
                };
                this.child(chip(ChipKind::Plain, text).flex_none())
            })
            .child(div().flex_1())
            .children(skip_reason.map(|reason| {
                div()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(theme::DIM)
                    .child(reason)
            }))
            .child(verdict_label(look, label))
            .child(
                div()
                    .flex_none()
                    .w(px(52.))
                    .flex()
                    .justify_end()
                    .text_xs()
                    .text_color(theme::DIM)
                    .children(time),
            );
        let row = div()
            .id(SharedString::from(format!("step-{}", info.id)))
            .flex()
            .flex_col()
            .rounded(px(8.))
            .border_1()
            .border_color(theme::LINE)
            .when(advisory, |this| this.border_dashed())
            .bg(theme::PANEL)
            .child(header);
        if !shows {
            return row;
        }
        row.child(self.evidence(step, view, look, open, cx))
    }

    /// What an open Step row shows: its Outcome's facts, Waiver, Findings
    /// and note, its log tail while it's the open Step, and the actions
    /// that apply to it.
    fn evidence(
        &self,
        step: &StepView,
        view: &RunView,
        look: Look,
        open: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let info = &step.info;
        let kv = |key: &str, value: AnyElement| {
            div()
                .flex()
                .gap(px(10.))
                .text_size(px(12.))
                .child(
                    div()
                        .flex_none()
                        .w(px(80.))
                        .text_color(theme::DIM)
                        .child(key.to_owned()),
                )
                .child(div().min_w_0().child(value))
        };
        let mut facts = div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(kv("Plugin", mono(info.plugin.clone()).into_any_element()))
            .child(kv(
                "Verdict",
                verdict_label(look, verdict_text(step, look)).into_any_element(),
            ));
        if let Some(time) = self.run_pane.step_time(&info.id, now_millis()) {
            let so_far = if matches!(step.status, StepStatus::Running) {
                " so far"
            } else {
                ""
            };
            facts = facts.child(kv("Time", format!("{time}{so_far}").into_any_element()));
        }
        if let Some(cost) = step.cost {
            facts = facts.child(kv("Cost", cost.to_string().into_any_element()));
        }
        if let Some(progress) = step
            .progress
            .as_ref()
            .filter(|_| matches!(step.status, StepStatus::Running))
        {
            facts = facts.child(kv("Progress", progress.clone().into_any_element()));
        }
        if !info.needs.is_empty() {
            facts = facts.child(kv("Reads", info.needs.join(", ").into_any_element()));
        }
        let role = if info.gated {
            "required".to_owned()
        } else if info.needs.iter().any(|need| need == slopwatch_core::GATE) {
            "runs after the Gate".to_owned()
        } else {
            "advisory".to_owned()
        };
        facts = facts.child(kv("Gate", role.into_any_element()));
        if let Some(condition) = &info.condition {
            facts = facts.child(kv("Condition", mono(condition.clone()).into_any_element()));
        }

        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .pl(px(27.))
            .pr(px(12.))
            .pt(px(6.))
            .pb(px(10.))
            .border_t_1()
            .border_color(theme::LINE)
            .child(facts);
        if let Some(line) = waiver_line(step) {
            body = body
                .child(section("Waiver").mt(px(6.)))
                .child(div().text_size(px(12.)).child(line));
        }
        if let Some(line) = commit_line(view, step) {
            body = body.child(div().text_size(px(12.)).text_color(theme::DIM).child(line));
        }
        if let StepStatus::Settled {
            outputs, reason, ..
        } = &step.status
        {
            if !outputs.findings.is_empty() {
                body = body.child(section("Findings").mt(px(6.)));
            }
            for finding in &outputs.findings {
                let place = match (&finding.file, finding.line) {
                    (Some(file), Some(line)) => Some(format!("{file}:{line}")),
                    (Some(file), None) => Some(file.clone()),
                    _ => None,
                };
                body = body.child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(6.))
                        .py(px(2.))
                        .text_size(px(12.))
                        .child(severity(finding.severity))
                        .child(
                            div()
                                .min_w_0()
                                .flex()
                                .flex_wrap()
                                .gap_x(px(6.))
                                .child(finding.message.clone())
                                .children(place.map(|place| mono(place).text_color(theme::DIM))),
                        ),
                );
            }
            let note = reason.as_ref().or(outputs.note.as_ref());
            if let Some(note) = note {
                let title = if reason.is_some() { "Reason" } else { "Note" };
                body = body
                    .child(section(title).mt(px(6.)))
                    .child(div().text_size(px(12.)).child(note.clone()));
            }
            if let StepStatus::Settled {
                reused_from: Some(run),
                ..
            } = step.status
            {
                body = body.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::DIM)
                        .child(format!("Reused from Run {run}")),
                );
            }
        }
        if open {
            body = body.child(self.step_log_tail(step, view, cx));
        }

        let retry_action = format!("retry-{}", info.id);
        let waive = waive_action(&WaiveTarget::Step(info.id.clone()));
        let mut actions = div().flex().flex_wrap().gap(px(6.)).mt(px(6.));
        let mut any = false;
        if let Some(retry) = self.run_pane.retry(&info.id) {
            any = true;
            actions = actions.child(
                Button::new(SharedString::from(retry_action.clone()))
                    .label("Retry")
                    .small()
                    .pending(self.outbox.waiting(&retry_action))
                    .on_click(cx.listener({
                        let action = retry_action.clone();
                        move |this, _: &ClickEvent, _, _| {
                            this.outbox.press(&action, retry.clone());
                        }
                    })),
            );
        }
        if self.run_pane.can_waive(&info.id) {
            any = true;
            let target = WaiveTarget::Step(info.id.clone());
            actions = actions.child(
                Button::new(SharedString::from(waive.clone()))
                    .label("Waive…")
                    .small()
                    .pending(self.outbox.waiting(&waive))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.run_pane.start_waiver(target.clone());
                        cx.notify();
                    })),
            );
        }
        if any {
            body = body.child(actions);
        }
        body.children(refusal(&self.outbox, &retry_action))
            .children(refusal(&self.outbox, &waive))
    }

    /// The open Step's last lines, with the way into its full log.
    fn step_log_tail(&self, step: &StepView, view: &RunView, cx: &mut Context<Self>) -> Div {
        let mut tail = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(section("Log tail").mt(px(6.)));
        if let Some(at) = view.pruned_at {
            return tail.child(
                div()
                    .text_xs()
                    .text_color(theme::DIM)
                    .child(format!("Log pruned on {}", step_log::date(at))),
            );
        }
        if let StepStatus::Settled {
            reused_from: Some(run),
            ..
        } = step.status
        {
            return tail.child(
                div().child(
                    Button::new("reused-log")
                        .label(format!("Full log from Run {run}"))
                        .small()
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            for command in this.run_pane.open_reused_log(run) {
                                this.send(command);
                            }
                            cx.notify();
                        })),
                ),
            );
        }
        if step.attempt == 0 {
            return tail.child(
                div()
                    .text_xs()
                    .text_color(theme::DIM)
                    .child("Not started yet."),
            );
        }
        let mut lines = well().max_h(px(140.)).overflow_hidden();
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
            lines = lines.child(div().text_color(theme::DIM).child(empty));
        }
        tail = tail.child(lines);
        let mut buttons = div().flex().flex_wrap().gap(px(6.));
        for attempt in (1..=step.attempt).rev() {
            let label = if step.attempt == 1 {
                "Open full log".to_owned()
            } else {
                format!("Full log, attempt {attempt}")
            };
            buttons = buttons.child(
                div()
                    .id(SharedString::from(format!("full-log-{attempt}")))
                    .text_xs()
                    .text_color(theme::LINK)
                    .cursor_pointer()
                    .hover(|this| this.underline())
                    .child(label)
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
        let attempts = view
            .step(&viewer.key.step)
            .map_or(viewer.key.attempt, |step| step.attempt.max(1));

        let mut header = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .child(
                Button::new("close-log")
                    .label("← Steps")
                    .small()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.run_pane.close_log();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(viewer.key.step.clone()),
            );
        if attempts > 1 {
            for attempt in (1..=attempts).rev() {
                let shown = attempt == viewer.key.attempt;
                header = header.child(
                    Button::new(SharedString::from(format!("attempt-{attempt}")))
                        .label(format!("Attempt {attempt}"))
                        .small()
                        .when(shown, |button| button.accent())
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
                .xsmall()
                .when(on, |button| button.accent())
        };
        let filter = &viewer.filter;
        let mut filters = div().flex().flex_wrap().gap(px(4.));
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
        filters = filters.child(Button::new("copy-log").label("Copy").xsmall().on_click(
            move |_: &ClickEvent, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
            },
        ));

        let mut lines = well();
        if viewer.has_older() {
            lines = lines.child(
                div().pb_1().child(
                    Button::new("log-older")
                        .label("Older")
                        .xsmall()
                        .pending(self.outbox.waiting(LOG))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.with_viewer(LogViewer::older);
                            cx.notify();
                        })),
                ),
            );
        }
        let refused = refusal(&self.outbox, LOG);
        let rows = viewer.rows(self.run_pane.events());
        if rows.is_empty() && refused.is_none() {
            let empty = if viewer.loading() {
                "Loading…"
            } else if filter.is_empty() {
                "The log is empty."
            } else {
                "No lines match."
            };
            lines = lines.child(div().text_color(theme::DIM).child(empty));
        }
        lines = lines.children(refused);
        for row in &rows {
            let line = div().child(step_log::row_text(row));
            lines = lines.child(match row {
                Row::Line(_) => line,
                Row::Truncated(_) => line.text_color(theme::INC),
                Row::Event { .. } => line.text_color(theme::RUN),
            });
        }
        if viewer.has_newer() {
            lines = lines.child(
                div()
                    .flex()
                    .gap_1()
                    .pt_1()
                    .child(
                        Button::new("log-newer")
                            .label("Newer")
                            .xsmall()
                            .pending(self.outbox.waiting(LOG))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.with_viewer(LogViewer::newer);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("log-follow")
                            .label("Latest")
                            .xsmall()
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
            lines = lines.child(div().text_color(theme::DIM).child("Following live…"));
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
        let repo = self.pipeline.read(cx).repo().cloned();
        let name = repo.as_ref().map_or_else(String::new, ToString::to_string);
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
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(8.))
                    .bg(theme::PANEL)
                    .border_b_1()
                    .border_color(theme::LINE)
                    .child(
                        Button::new("pipeline-back")
                            .label("← PRs")
                            .small()
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                let source = repo.clone().map_or(Source::All, Source::Repo);
                                this.show_prs(source);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(name))
                            .child(div().text_color(theme::DIM).child("› Pipeline")),
                    ),
            )
            .child(div().flex_1().min_h_0().flex().child(self.pipeline.clone()))
    }

    fn footer(&self) -> Option<impl IntoElement> {
        let (text, color) = match (&self.error, poll_line(self.prs.poll())) {
            (Some(error), _) => (error.clone(), theme::FAIL),
            (None, Some(poll)) => (poll, theme::DIM),
            (None, None) => return None,
        };
        Some(
            div()
                .px(px(14.))
                .py(px(4.))
                .bg(theme::PANEL)
                .border_t_1()
                .border_color(theme::LINE)
                .text_xs()
                .text_color(color)
                .child(text),
        )
    }
}

/// A Step's state in a word or two, as its row and evidence say it.
fn verdict_text(step: &StepView, look: Look) -> String {
    match look {
        Look::Asking => "waiting on you".to_owned(),
        _ => step_state(step),
    }
}

/// Now, in milliseconds since the Unix epoch, for how long ago things
/// happened.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64)
}

impl Render for MainView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::BG)
            .text_color(theme::TEXT)
            .text_sm()
            .line_height(relative(theme::LINE_HEIGHT));
        if !matches!(self.link, LinkState::Connected { .. }) {
            return root.child(self.link_view.clone());
        }
        root.child(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .overflow_hidden()
                .when(self.sources_shown(), |this| this.child(self.sources(cx)))
                .map(|this| match (&self.picker, self.pane) {
                    (Some(available), _) => this.child(self.repo_picker(available, cx)),
                    (None, pane) => match pane {
                        Pane::Prs => this.child(self.pr_list(cx)).child(self.pr_pane(cx)),
                        Pane::Inbox => this.child(self.inbox_list(cx)).child(self.pr_pane(cx)),
                        Pane::Library => this.child(self.library.clone()),
                        Pane::Secrets => this.child(self.secrets.clone()),
                        Pane::Pipeline => this.child(self.pipeline_pane(cx)),
                        Pane::Plugins => this.child(self.plugins.clone()),
                        Pane::Settings => this.child(self.settings.clone()),
                    },
                }),
        )
        .children(self.footer())
    }
}
