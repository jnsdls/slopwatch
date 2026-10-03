//! The main window: the sources pane, then either the Watched PR list with
//! the PR pane or the Library editor, or the link state while the daemon
//! isn't reachable.

use std::sync::Arc;
use std::sync::mpsc::Sender;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_protocol::{
    Command, PrStatus, PullRequest, Reply, RepoName, ResponseBody, TopicUpdate,
};

use crate::agent::Agent;
use crate::library_view::LibraryView;
use crate::link::{LinkEvent, LinkState};
use crate::link_view::LinkView;
use crate::prs::{Prs, Source, poll_line, status_line};
use crate::run_pane::{RunPane, Tone, gate_tone, run_label, run_tone, step_line, step_tone};

/// What fills the window right of the sources pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Prs,
    Library,
}

pub struct MainView {
    link: LinkState,
    link_view: Entity<LinkView>,
    pane: Pane,
    prs: Prs,
    library: Entity<LibraryView>,
    run_pane: RunPane,
    /// Repos the developer can add, while the picker is open.
    picker: Option<Vec<RepoName>>,
    /// The last command that failed, until the next one succeeds.
    error: Option<String>,
    commands: Sender<Command>,
}

impl MainView {
    /// `agent` is `None` when the GUI runs outside its bundle. `reregister`
    /// asks the link to unregister and register the agent.
    pub fn new(
        commands: Sender<Command>,
        agent: Option<Arc<dyn Agent>>,
        reregister: Sender<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            link: LinkState::Connecting,
            link_view: cx.new(|_| LinkView::new(agent, reregister)),
            pane: Pane::Prs,
            prs: Prs::default(),
            library: cx.new(|cx| LibraryView::new(commands.clone(), window, cx)),
            run_pane: RunPane::default(),
            picker: None,
            error: None,
            commands,
        }
    }

    pub fn handle(&mut self, event: LinkEvent, cx: &mut Context<Self>) {
        match event {
            LinkEvent::State(state) => {
                let connected = matches!(state, LinkState::Connected { .. });
                if !connected {
                    self.picker = None;
                } else if self.pane == Pane::Library {
                    self.library.read(cx).refresh();
                }
                if connected && !matches!(self.link, LinkState::Connected { .. }) {
                    // The new connection subscribes to `watched_prs` itself.
                    for command in self.run_pane.reconnected() {
                        self.send(command);
                    }
                }
                self.link = state.clone();
                self.link_view
                    .update(cx, |view, cx| view.set_state(state, cx));
            }
            LinkEvent::Topic(update @ TopicUpdate::Run { .. }) => self.run_pane.apply(update),
            LinkEvent::Topic(update) => {
                self.prs.apply(update);
                if let Some((repo, number)) = self.run_pane.selected().cloned() {
                    let commands = self.run_pane.pr_changed(self.prs.pr(&repo, number));
                    for command in commands {
                        self.send(command);
                    }
                }
            }
            LinkEvent::Response(response) => match response.result {
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
                ResponseBody::Ok(_) => self.error = None,
                ResponseBody::Error(error) => self.error = Some(error.message),
            },
        }
        cx.notify();
    }

    fn send(&self, command: Command) {
        // The link thread only stops when the app quits.
        let _ = self.commands.send(command);
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
        }

        match &self.picker {
            None => pane.child(
                div().px_1().pt_2().child(
                    Button::new("add-repo")
                        .label("Add repo…")
                        .small()
                        .ghost()
                        .on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                            this.send(Command::ListAvailableRepos);
                        })),
                ),
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
                                this.show_prs(Source::Repo(chosen.clone()));
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
        }
    }

    fn pr_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut list = div()
            .id("pr-list")
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .overflow_y_scroll();

        let rows: Vec<&PullRequest> = self.prs.rows().collect();
        if rows.is_empty() {
            let empty = if !self.prs.loaded() {
                "Loading…"
            } else if self.prs.repos().is_empty() {
                "Add a repo to see your open PRs."
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
        for pr in rows {
            list = list.child(self.pr_row(pr, cx));
        }
        list
    }

    fn pr_row(&self, pr: &PullRequest, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let tone = match pr.status {
            PrStatus::NotWatched => theme.muted_foreground,
            PrStatus::Waiting => theme.warning,
            PrStatus::Ready => theme.success,
        };
        let (repo, number) = (pr.repo.clone(), pr.number);
        let watched = pr.watched();
        let toggle = Button::new(SharedString::from(format!("toggle-{repo}-{number}")))
            .label(if watched { "Unwatch" } else { "Watch" })
            .small()
            .when(watched, |button| button.ghost())
            .when(!watched, |button| button.primary())
            .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                let (repo, number) = (repo.clone(), number);
                this.send(if watched {
                    Command::Unwatch { repo, number }
                } else {
                    Command::Watch { repo, number }
                });
            }));

        let selected = self.run_pane.selected() == Some(&(pr.repo.clone(), pr.number));
        let row = pr.clone();
        div()
            .id(SharedString::from(format!("pr-{}-{}", pr.repo, pr.number)))
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .when(selected, |this| this.bg(theme.list_active))
            .hover(|this| this.bg(theme.list_hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                for command in this.run_pane.select_pr(&row) {
                    this.send(command);
                }
                cx.notify();
            }))
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
                    ),
            )
            .child(toggle)
    }

    /// The selected PR: its Run history chips, newest first, then the Run
    /// shown as a Step list with the Gate as its last row.
    fn pr_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let color = |tone: Tone| match tone {
            Tone::Good => theme.success,
            Tone::Bad => theme.danger,
            Tone::Neutral => theme.muted_foreground,
        };
        let mut pane = div()
            .id("pr-pane")
            .w(px(400.))
            .h_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
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

        pane = pane.child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(div().text_sm().child(pr.title.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{}#{}", pr.repo, pr.number)),
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
                        Button::new("cancel-run")
                            .label("Cancel Run")
                            .small()
                            .ghost()
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                                this.send(cancel.clone());
                            })),
                    )
                }),
        );
        let mut steps = div()
            .flex()
            .flex_col()
            .border_1()
            .border_color(theme.border)
            .rounded_md();
        for step in &view.steps {
            let mut row = div()
                .flex()
                .flex_col()
                .gap_0p5()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(theme.border)
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
                                Button::new(SharedString::from(format!("retry-{}", step.info.id)))
                                    .label("Retry")
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                                        this.send(retry.clone());
                                    })),
                            )
                        }),
                );
            if let slopwatch_protocol::StepStatus::Settled { outputs, .. } = &step.status {
                for finding in &outputs.findings {
                    row = row.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("• {}", finding.message)),
                    );
                }
            }
            steps = steps.child(row);
        }
        let gate = view.gate.unwrap_or(slopwatch_core::GateState::Pending);
        steps = steps.child(
            div()
                .flex()
                .justify_between()
                .px_3()
                .py_2()
                .text_sm()
                .child(format!("Gate {}", view.gate_text))
                .child(
                    div()
                        .text_color(color(gate_tone(gate)))
                        .child(gate.to_string()),
                ),
        );
        pane = pane.child(steps);
        if let Some(end) = view.end {
            pane = pane.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("Ended: {end}")),
            );
        }
        pane
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
                    .child(self.sources(cx))
                    .map(|this| match self.pane {
                        Pane::Prs => this.child(self.pr_list(cx)).child(self.pr_pane(cx)),
                        Pane::Library => this.child(self.library.clone()),
                    }),
            )
            .children(self.footer(cx))
    }
}
