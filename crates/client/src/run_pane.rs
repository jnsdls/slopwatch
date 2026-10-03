//! What the PR pane knows: the selected PR, which of its Runs it shows, and
//! that Run rebuilt from its `run/<id>` topic. No GPUI here, so it tests
//! without a window.
//!
//! The pane follows the PR's newest Run until the developer picks an older
//! one from the history chips. Each method returns the commands the link
//! should send to keep the subscriptions on what's shown: the Run, and the
//! Step log of the open Step row.

use slopwatch_core::{EndReason, GateState, Verdict};
use slopwatch_protocol::{
    Command, LogKey, LogRecord, PullRequest, RepoName, RunId, RunSummary, RunView, StepLogPage,
    StepStatus, StepView, Topic, TopicUpdate,
};

use crate::step_log::{LogTail, LogViewer, TimedEvent};

#[derive(Debug, Default)]
pub struct RunPane {
    selected: Option<(RepoName, u64)>,
    /// The Run the developer picked. `None` follows the newest.
    pinned: Option<RunId>,
    /// The Run subscribed to and shown.
    shown: Option<RunId>,
    view: RunView,
    /// The shown Run's events with their timestamps, for the log viewer.
    events: Vec<TimedEvent>,
    /// The Step row opened to show its log tail.
    open_step: Option<String>,
    /// The open Step's latest attempt, followed live.
    tail: Option<LogTail>,
    /// The full log of one of the open Step's attempts.
    viewer: Option<LogViewer>,
}

impl RunPane {
    pub fn selected(&self) -> Option<&(RepoName, u64)> {
        self.selected.as_ref()
    }

    pub fn shown(&self) -> Option<RunId> {
        self.shown
    }

    /// The Run shown, once its first event arrived.
    pub fn view(&self) -> Option<&RunView> {
        self.shown.filter(|_| self.view.seq > 0).map(|_| &self.view)
    }

    /// Shows `pr`'s newest Run.
    pub fn select_pr(&mut self, pr: &PullRequest) -> Vec<Command> {
        self.selected = Some((pr.repo.clone(), pr.number));
        self.pinned = None;
        self.retarget(&pr.runs)
    }

    /// Shows one of the selected PR's Runs. Picking the newest follows
    /// newer ones again.
    pub fn select_run(&mut self, run: RunId, pr: &PullRequest) -> Vec<Command> {
        self.pinned = (pr.runs.first().map(|newest| newest.id) != Some(run)).then_some(run);
        self.retarget(&pr.runs)
    }

    /// Follows the selected PR's row: a new Run, or the PR going away.
    pub fn pr_changed(&mut self, pr: Option<&PullRequest>) -> Vec<Command> {
        match pr {
            Some(pr) => self.retarget(&pr.runs),
            None => {
                self.selected = None;
                self.pinned = None;
                self.retarget(&[])
            }
        }
    }

    /// What cancels the Run shown, while it's still going.
    pub fn cancel(&self) -> Option<Command> {
        let view = self.view()?;
        let run = self.shown?;
        view.end.is_none().then_some(Command::CancelRun { run })
    }

    /// What retries `step` in the Run shown: only an errored Step, while
    /// the Run is still going.
    pub fn retry(&self, step: &str) -> Option<Command> {
        let view = self.view()?;
        let run = self.shown?;
        let errored = matches!(
            view.step(step)?.status,
            StepStatus::Settled {
                verdict: Verdict::Error,
                ..
            }
        );
        (view.end.is_none() && errored).then(|| Command::RetryStep {
            run,
            step: step.to_owned(),
        })
    }

    /// A new connection has no subscriptions, so it asks again for the Run
    /// shown, from the last event the pane has, and for the open log.
    pub fn reconnected(&self) -> Vec<Command> {
        let mut commands: Vec<Command> = self
            .shown
            .map(|run| Command::Subscribe {
                topic: Topic::Run(run),
                since: Some(self.view.seq),
            })
            .into_iter()
            .collect();
        commands.extend(self.tail.as_ref().map(LogTail::subscribe));
        commands
    }

    pub fn apply(&mut self, update: TopicUpdate) -> Vec<Command> {
        match update {
            TopicUpdate::Run { id, seq, ts, event } if Some(id) == self.shown => {
                if seq <= self.view.seq {
                    return Vec::new();
                }
                self.events.push(TimedEvent {
                    ts,
                    event: event.clone(),
                });
                self.view.apply(seq, event);
                self.follow_open_step()
            }
            TopicUpdate::StepLog { key, records } => {
                if let Some(tail) = &mut self.tail
                    && tail.key == key
                {
                    tail.push(&records);
                }
                self.viewer
                    .as_mut()
                    .and_then(|viewer| viewer.live(&key, &records))
                    .into_iter()
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// Takes a page of a Step log the daemon sent.
    pub fn log_page(&mut self, page: StepLogPage) {
        if let Some(viewer) = &mut self.viewer {
            viewer.loaded(page);
        }
    }

    pub fn open_step(&self) -> Option<&str> {
        self.open_step.as_deref()
    }

    /// The open Step row's last lines.
    pub fn tail(&self) -> impl Iterator<Item = &LogRecord> {
        self.tail.iter().flat_map(LogTail::lines)
    }

    pub fn viewer(&self) -> Option<&LogViewer> {
        self.viewer.as_ref()
    }

    pub fn viewer_mut(&mut self) -> Option<&mut LogViewer> {
        self.viewer.as_mut()
    }

    /// The shown Run's events, timestamped.
    pub fn events(&self) -> &[TimedEvent] {
        &self.events
    }

    /// Opens a Step row to show its log tail, or closes the open one.
    pub fn toggle_step(&mut self, step: &str) -> Vec<Command> {
        let mut commands = self.close_step();
        if self.open_step.as_deref() == Some(step) {
            self.open_step = None;
            return commands;
        }
        self.open_step = Some(step.to_owned());
        commands.extend(self.follow_open_step());
        commands
    }

    /// Opens the full log of the open Step's `attempt`.
    pub fn open_log(&mut self, attempt: u32) -> Vec<Command> {
        let Some(step) = self.open_step.clone() else {
            return Vec::new();
        };
        let Some(run) = self.shown else {
            return Vec::new();
        };
        self.open_viewer(LogKey { run, step, attempt })
    }

    /// Opens the full log of the open Step in `run`, the earlier Run whose
    /// Outcome it reused, at that Run's latest attempt.
    pub fn open_reused_log(&mut self, run: RunId) -> Vec<Command> {
        let Some(step) = self.open_step.clone() else {
            return Vec::new();
        };
        self.open_viewer(LogKey {
            run,
            step,
            attempt: 0,
        })
    }

    fn open_viewer(&mut self, key: LogKey) -> Vec<Command> {
        let (mut viewer, command) = LogViewer::open(key);
        if let Some(old) = &self.viewer {
            viewer.events = old.events;
        }
        self.viewer = Some(viewer);
        vec![command]
    }

    pub fn close_log(&mut self) {
        self.viewer = None;
    }

    /// Keeps the tail subscription on the open Step's latest attempt.
    fn follow_open_step(&mut self) -> Vec<Command> {
        let (Some(run), Some(step)) = (self.shown, self.open_step.as_deref()) else {
            return Vec::new();
        };
        let wanted = self
            .view
            .step(step)
            .filter(|view| view.attempt > 0 && self.view.pruned_at.is_none())
            .map(|view| LogKey {
                run,
                step: step.to_owned(),
                attempt: view.attempt,
            });
        if self.tail.as_ref().map(|tail| &tail.key) == wanted.as_ref() {
            return Vec::new();
        }
        let mut commands = Vec::new();
        if let Some(old) = self.tail.take() {
            commands.push(Command::Unsubscribe {
                topic: Topic::StepLog(old.key),
            });
        }
        if self.view.pruned_at.is_some() {
            self.viewer = None;
        }
        if let Some(key) = wanted {
            let tail = LogTail::new(key);
            commands.push(tail.subscribe());
            self.tail = Some(tail);
        }
        commands
    }

    /// Drops the open Step's log subscription and viewer.
    fn close_step(&mut self) -> Vec<Command> {
        self.viewer = None;
        self.tail
            .take()
            .map(|tail| Command::Unsubscribe {
                topic: Topic::StepLog(tail.key),
            })
            .into_iter()
            .collect()
    }

    fn retarget(&mut self, runs: &[RunSummary]) -> Vec<Command> {
        let wanted = self
            .pinned
            .filter(|pinned| runs.iter().any(|run| run.id == *pinned))
            .or_else(|| runs.first().map(|run| run.id));
        if wanted == self.shown {
            return Vec::new();
        }
        let mut commands = self.close_step();
        self.open_step = None;
        if let Some(old) = self.shown.take() {
            commands.push(Command::Unsubscribe {
                topic: Topic::Run(old),
            });
        }
        self.view = RunView::default();
        self.events.clear();
        if let Some(run) = wanted {
            self.shown = Some(run);
            commands.push(Command::Subscribe {
                topic: Topic::Run(run),
                since: None,
            });
        }
        commands
    }
}

/// How a Run reads on its history chip and in the PR list.
pub fn run_label(run: &RunSummary) -> String {
    match run.end {
        Some(reason) => capitalized(&reason.to_string()),
        None => match run.gate {
            GateState::Pending => "Running".to_owned(),
            GateState::Pass => "Running, Gate passed".to_owned(),
            GateState::Fail => "Running, Gate failed".to_owned(),
        },
    }
}

/// Whether a Run reads as good, bad or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Bad,
    Neutral,
}

pub fn run_tone(run: &RunSummary) -> Tone {
    match (run.end, run.gate) {
        (Some(EndReason::Shippable | EndReason::Merged), _) => Tone::Good,
        (Some(EndReason::NotShippable | EndReason::OverBudget), _) => Tone::Bad,
        (Some(_), _) => Tone::Neutral,
        (None, GateState::Pass) => Tone::Good,
        (None, GateState::Fail) => Tone::Bad,
        (None, GateState::Pending) => Tone::Neutral,
    }
}

/// A Step's status as its row in the Step list says it.
pub fn step_line(step: &StepView) -> String {
    match &step.status {
        StepStatus::Pending => "pending".to_owned(),
        StepStatus::Running => match &step.progress {
            Some(progress) => format!("running: {progress}"),
            None => "running".to_owned(),
        },
        StepStatus::Settled {
            verdict,
            reason,
            outputs,
            reused_from,
        } => {
            let detail = reason.as_ref().or(outputs.note.as_ref());
            let line = match detail {
                Some(detail) => format!("{verdict}: {detail}"),
                None => verdict.to_string(),
            };
            match reused_from {
                Some(run) => format!("{line} (reused from Run {run})"),
                None => line,
            }
        }
    }
}

pub fn step_tone(step: &StepView) -> Tone {
    match step.status {
        StepStatus::Settled {
            verdict: Verdict::Pass,
            ..
        } => Tone::Good,
        StepStatus::Settled {
            verdict: Verdict::Fail | Verdict::Error,
            ..
        } => Tone::Bad,
        _ => Tone::Neutral,
    }
}

pub fn gate_tone(gate: GateState) -> Tone {
    match gate {
        GateState::Pass => Tone::Good,
        GateState::Fail => Tone::Bad,
        GateState::Pending => Tone::Neutral,
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_protocol::step::Outputs;
    use slopwatch_protocol::{PrStatus, RunEvent, StepInfo};

    fn summary(id: u64, end: Option<EndReason>) -> RunSummary {
        RunSummary {
            id: RunId(id),
            head_sha: format!("sha{id}"),
            gate: GateState::Pending,
            end,
        }
    }

    fn pr(runs: Vec<RunSummary>) -> PullRequest {
        PullRequest {
            repo: RepoName::new("o", "r"),
            number: 1,
            title: "Add the thing".into(),
            url: String::new(),
            draft: false,
            head_sha: "sha".into(),
            base: "main".into(),
            status: PrStatus::Ready,
            runs,
            blocked: None,
        }
    }

    fn subscribe(id: u64, since: Option<u64>) -> Command {
        Command::Subscribe {
            topic: Topic::Run(RunId(id)),
            since,
        }
    }

    fn unsubscribe(id: u64) -> Command {
        Command::Unsubscribe {
            topic: Topic::Run(RunId(id)),
        }
    }

    fn started(id: u64) -> TopicUpdate {
        TopicUpdate::Run {
            id: RunId(id),
            seq: 1,
            ts: 1,
            event: RunEvent::Started {
                repo: RepoName::new("o", "r"),
                number: 1,
                head_sha: format!("sha{id}"),
                base: "main".into(),
                base_sha: "base".into(),
                steps: vec![StepInfo {
                    id: "ci".into(),
                    plugin: "ci".into(),
                    needs: vec![],
                    gated: true,
                }],
                gate: "[ci]".into(),
            },
        }
    }

    #[test]
    fn selecting_a_pr_shows_its_newest_run_and_follows_newer_ones() {
        let mut pane = RunPane::default();

        assert_eq!(
            pane.select_pr(&pr(vec![summary(1, None)])),
            [subscribe(1, None)]
        );
        pane.apply(started(1));
        assert_eq!(pane.view().unwrap().head_sha, "sha1");

        let pushed = pr(vec![
            summary(2, None),
            summary(1, Some(EndReason::Superseded)),
        ]);
        assert_eq!(
            pane.pr_changed(Some(&pushed)),
            [unsubscribe(1), subscribe(2, None)]
        );
        assert!(pane.view().is_none(), "nothing shown until run 2's events");
        pane.apply(started(1));
        assert!(pane.view().is_none(), "events from other Runs are ignored");
    }

    #[test]
    fn a_picked_older_run_stays_shown_until_the_newest_is_picked() {
        let mut pane = RunPane::default();
        let row = pr(vec![
            summary(2, None),
            summary(1, Some(EndReason::Superseded)),
        ]);
        pane.select_pr(&row);

        assert_eq!(
            pane.select_run(RunId(1), &row),
            [unsubscribe(2), subscribe(1, None)]
        );
        let newer = pr(vec![
            summary(3, None),
            summary(2, Some(EndReason::Superseded)),
            summary(1, Some(EndReason::Superseded)),
        ]);
        assert!(pane.pr_changed(Some(&newer)).is_empty());

        assert_eq!(
            pane.select_run(RunId(3), &newer),
            [unsubscribe(1), subscribe(3, None)]
        );
        let newest = pr(vec![summary(4, None), summary(3, None)]);
        assert_eq!(
            pane.pr_changed(Some(&newest)),
            [unsubscribe(3), subscribe(4, None)]
        );
    }

    #[test]
    fn a_reconnect_resubscribes_from_the_last_event_seen() {
        let mut pane = RunPane::default();
        assert!(pane.reconnected().is_empty());
        pane.select_pr(&pr(vec![summary(5, None)]));
        pane.apply(started(5));

        assert_eq!(pane.reconnected(), [subscribe(5, Some(1))]);
    }

    fn settled(verdict: Verdict) -> TopicUpdate {
        TopicUpdate::Run {
            id: RunId(5),
            seq: 2,
            ts: 2,
            event: RunEvent::StepSettled {
                step: "ci".into(),
                verdict,
                reason: None,
                outputs: Outputs::default(),
                reused_from: None,
            },
        }
    }

    #[test]
    fn a_going_run_offers_cancel_and_a_retry_for_an_errored_step() {
        let mut pane = RunPane::default();
        pane.select_pr(&pr(vec![summary(5, None)]));
        assert_eq!(
            pane.cancel(),
            None,
            "nothing to cancel before the Run shows"
        );
        pane.apply(started(5));

        assert_eq!(pane.cancel(), Some(Command::CancelRun { run: RunId(5) }));
        assert_eq!(pane.retry("ci"), None, "ci hasn't errored");

        pane.apply(settled(Verdict::Error));
        assert_eq!(
            pane.retry("ci"),
            Some(Command::RetryStep {
                run: RunId(5),
                step: "ci".into(),
            })
        );

        pane.apply(TopicUpdate::Run {
            id: RunId(5),
            seq: 3,
            ts: 3,
            event: RunEvent::Ended {
                reason: EndReason::NotShippable,
            },
        });
        assert_eq!(pane.cancel(), None);
        assert_eq!(pane.retry("ci"), None, "an ended Run can't retry");
    }

    #[test]
    fn labels_say_where_a_run_and_its_steps_stand() {
        assert_eq!(run_label(&summary(1, None)), "Running");
        assert_eq!(
            run_label(&summary(1, Some(EndReason::NotShippable))),
            "Not shippable"
        );
        assert_eq!(
            run_tone(&summary(1, Some(EndReason::Shippable))),
            Tone::Good
        );

        let step = StepView {
            info: StepInfo {
                id: "ci".into(),
                plugin: "ci".into(),
                needs: vec![],
                gated: true,
            },
            status: StepStatus::Settled {
                verdict: Verdict::Cancelled,
                reason: Some("the Run ended superseded".into()),
                outputs: Outputs::default(),
                reused_from: None,
            },
            attempt: 1,
            progress: None,
        };
        assert_eq!(step_line(&step), "cancelled: the Run ended superseded");

        let reused = StepView {
            status: StepStatus::Settled {
                verdict: Verdict::Pass,
                reason: None,
                outputs: Outputs {
                    note: Some("1 check passed".into()),
                    ..Outputs::default()
                },
                reused_from: Some(RunId(3)),
            },
            ..step
        };
        assert_eq!(
            step_line(&reused),
            "pass: 1 check passed (reused from Run 3)"
        );
    }

    fn event(id: u64, seq: u64, event: RunEvent) -> TopicUpdate {
        TopicUpdate::Run {
            id: RunId(id),
            seq,
            ts: seq as i64 * 10,
            event,
        }
    }

    fn ci_log(run: u64, attempt: u32) -> LogKey {
        LogKey {
            run: RunId(run),
            step: "ci".into(),
            attempt,
        }
    }

    fn log_line(seq: u64, text: &str) -> LogRecord {
        LogRecord {
            seq,
            ts: seq as i64,
            source: slopwatch_protocol::LogSource::Stderr,
            level: None,
            text: text.into(),
        }
    }

    #[test]
    fn an_open_step_row_follows_the_log_of_its_latest_attempt() {
        let mut pane = RunPane::default();
        pane.select_pr(&pr(vec![summary(1, None)]));
        pane.apply(started(1));

        // Opened before it runs, it waits for the first attempt.
        assert!(pane.toggle_step("ci").is_empty());
        let commands = pane.apply(event(
            1,
            2,
            RunEvent::StepStarted {
                step: "ci".into(),
                attempt: 1,
            },
        ));
        assert_eq!(
            commands,
            [Command::Subscribe {
                topic: Topic::StepLog(ci_log(1, 1)),
                since: None,
            }]
        );
        pane.apply(TopicUpdate::StepLog {
            key: ci_log(1, 1),
            records: vec![log_line(1, "compiling"), log_line(2, "testing")],
        });
        let tail: Vec<&str> = pane.tail().map(|record| record.text.as_str()).collect();
        assert_eq!(tail, ["compiling", "testing"]);
        assert_eq!(
            pane.reconnected()[1],
            Command::Subscribe {
                topic: Topic::StepLog(ci_log(1, 1)),
                since: Some(2),
            },
            "a reconnect asks from the last line"
        );

        // A retry is a new attempt with its own log.
        let commands = pane.apply(event(
            1,
            3,
            RunEvent::StepStarted {
                step: "ci".into(),
                attempt: 2,
            },
        ));
        assert_eq!(
            commands,
            [
                Command::Unsubscribe {
                    topic: Topic::StepLog(ci_log(1, 1)),
                },
                Command::Subscribe {
                    topic: Topic::StepLog(ci_log(1, 2)),
                    since: None,
                },
            ]
        );
        assert_eq!(pane.tail().count(), 0);

        assert_eq!(
            pane.toggle_step("ci"),
            [Command::Unsubscribe {
                topic: Topic::StepLog(ci_log(1, 2)),
            }]
        );
        assert_eq!(pane.open_step(), None);
    }

    #[test]
    fn the_full_log_follows_live_records_and_sees_the_runs_events() {
        let mut pane = RunPane::default();
        pane.select_pr(&pr(vec![summary(1, None)]));
        pane.apply(started(1));
        pane.apply(event(
            1,
            2,
            RunEvent::StepStarted {
                step: "ci".into(),
                attempt: 1,
            },
        ));
        pane.toggle_step("ci");

        let commands = pane.open_log(1);
        let [Command::ReadStepLog { key, page, filter }] = &commands[..] else {
            panic!("the viewer reads the last page: {commands:?}");
        };
        assert_eq!(*key, ci_log(1, 1));
        pane.log_page(StepLogPage {
            key: key.clone(),
            page: *page,
            filter: filter.clone(),
            records: vec![log_line(1, "compiling")],
            more_before: false,
            more_after: false,
            truncated: None,
        });
        pane.apply(TopicUpdate::StepLog {
            key: ci_log(1, 1),
            records: vec![log_line(2, "testing")],
        });
        pane.apply(event(
            1,
            3,
            RunEvent::StepProgress {
                step: "ci".into(),
                message: "halfway".into(),
            },
        ));

        let viewer = pane.viewer().unwrap();
        assert_eq!(viewer.rows(pane.events()).len(), 2);
        pane.viewer_mut().unwrap().events = true;
        let rows: Vec<String> = pane
            .viewer()
            .unwrap()
            .rows(pane.events())
            .iter()
            .map(crate::step_log::row_text)
            .collect();
        assert_eq!(
            rows,
            [
                "compiling",
                "testing",
                "— attempt 1 started",
                "— progress: halfway"
            ]
        );
    }

    #[test]
    fn a_pruned_run_shows_no_log() {
        let mut pane = RunPane::default();
        pane.select_pr(&pr(vec![summary(1, Some(EndReason::Superseded))]));
        pane.apply(started(1));
        pane.apply(event(
            1,
            2,
            RunEvent::StepStarted {
                step: "ci".into(),
                attempt: 1,
            },
        ));
        pane.toggle_step("ci");
        assert!(pane.tail.is_some());

        let commands = pane.apply(event(1, 3, RunEvent::Pruned { at: 0 }));

        assert_eq!(
            commands,
            [Command::Unsubscribe {
                topic: Topic::StepLog(ci_log(1, 1)),
            }]
        );
        assert!(pane.viewer().is_none());
        assert_eq!(pane.view().unwrap().pruned_at, Some(0));
    }
}
