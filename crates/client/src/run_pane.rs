//! What the PR pane knows: the selected PR, which of its Runs it shows, and
//! that Run rebuilt from its `run/<id>` topic. No GPUI here, so it tests
//! without a window.
//!
//! The pane follows the PR's newest Run until the developer picks an older
//! one from the history chips. Each method returns the commands the link
//! should send to keep the subscription on the Run shown.

use slopwatch_core::{EndReason, GateState, Verdict};
use slopwatch_protocol::{
    Command, PullRequest, RepoName, RunId, RunSummary, RunView, StepStatus, StepView, Topic,
    TopicUpdate,
};

#[derive(Debug, Default)]
pub struct RunPane {
    selected: Option<(RepoName, u64)>,
    /// The Run the developer picked. `None` follows the newest.
    pinned: Option<RunId>,
    /// The Run subscribed to and shown.
    shown: Option<RunId>,
    view: RunView,
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

    /// A new connection has no subscriptions, so it asks again for the Run
    /// shown, from the last event the pane has.
    pub fn reconnected(&self) -> Vec<Command> {
        self.shown
            .map(|run| Command::Subscribe {
                topic: Topic::Run(run),
                since: Some(self.view.seq),
            })
            .into_iter()
            .collect()
    }

    pub fn apply(&mut self, update: TopicUpdate) {
        if let TopicUpdate::Run { id, seq, event } = update
            && Some(id) == self.shown
        {
            self.view.apply(seq, event);
        }
    }

    fn retarget(&mut self, runs: &[RunSummary]) -> Vec<Command> {
        let wanted = self
            .pinned
            .filter(|pinned| runs.iter().any(|run| run.id == *pinned))
            .or_else(|| runs.first().map(|run| run.id));
        if wanted == self.shown {
            return Vec::new();
        }
        let mut commands = Vec::new();
        if let Some(old) = self.shown.take() {
            commands.push(Command::Unsubscribe {
                topic: Topic::Run(old),
            });
        }
        self.view = RunView::default();
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
        StepStatus::Running => "running".to_owned(),
        StepStatus::Settled {
            verdict,
            reason,
            outputs,
        } => {
            let detail = reason.as_ref().or(outputs.note.as_ref());
            match detail {
                Some(detail) => format!("{verdict}: {detail}"),
                None => verdict.to_string(),
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
            },
        };
        assert_eq!(step_line(&step), "cancelled: the Run ended superseded");
    }
}
