//! The main window: the sources pane and the Watched PR list, or the link
//! state while the daemon isn't reachable.

use std::sync::mpsc::Sender;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use slopwatch_protocol::{Command, PrStatus, PullRequest, Reply, RepoName, ResponseBody};

use crate::link::{LinkEvent, LinkState};
use crate::link_view::LinkView;
use crate::prs::{Prs, Source, poll_line, status_line};

pub struct MainView {
    link: LinkState,
    link_view: Entity<LinkView>,
    prs: Prs,
    /// Repos the developer can add, while the picker is open.
    picker: Option<Vec<RepoName>>,
    /// The last command that failed, until the next one succeeds.
    error: Option<String>,
    commands: Sender<Command>,
}

impl MainView {
    pub fn new(commands: Sender<Command>, cx: &mut Context<Self>) -> Self {
        Self {
            link: LinkState::Connecting,
            link_view: cx.new(|_| LinkView::new()),
            prs: Prs::default(),
            picker: None,
            error: None,
            commands,
        }
    }

    pub fn handle(&mut self, event: LinkEvent, cx: &mut Context<Self>) {
        match event {
            LinkEvent::State(state) => {
                if !matches!(state, LinkState::Connected { .. }) {
                    self.picker = None;
                }
                self.link = state.clone();
                self.link_view
                    .update(cx, |view, cx| view.set_state(state, cx));
            }
            LinkEvent::Topic(update) => self.prs.apply(update),
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
                    self.prs.source == Source::All,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.prs.source = Source::All;
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
            let selected = self.prs.source == Source::Repo(repo.clone());
            let chosen = repo.clone();
            pane = pane.child(
                entry(
                    format!("source-{repo}").into(),
                    repo.to_string(),
                    Some(self.prs.watched_in(repo)),
                    selected,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.prs.source = Source::Repo(chosen.clone());
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
            PrStatus::Watched => theme.success,
        };
        let (repo, number) = (pr.repo.clone(), pr.number);
        let toggle = if pr.watched() {
            Button::new(SharedString::from(format!("unwatch-{repo}-{number}")))
                .label("Unwatch")
                .small()
                .ghost()
                .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                    this.send(Command::Unwatch {
                        repo: repo.clone(),
                        number,
                    });
                }))
        } else {
            Button::new(SharedString::from(format!("watch-{repo}-{number}")))
                .label("Watch")
                .small()
                .primary()
                .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                    this.send(Command::Watch {
                        repo: repo.clone(),
                        number,
                    });
                }))
        };

        div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
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
                    .child(self.pr_list(cx)),
            )
            .children(self.footer(cx))
    }
}
