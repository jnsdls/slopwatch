//! What the window knows about the Inbox, kept from the `inbox` topic, and
//! what it shows for each entry. No GPUI here, so it tests without a
//! window.

use slopwatch_protocol::{
    Answer, Command, Inbox, InboxEntry, InboxUpdate, RepoName, Scope, TopicUpdate,
};

#[derive(Debug, Default)]
pub struct InboxModel {
    topic: Inbox,
    /// The sequence number of the last update applied. `None` until the
    /// first snapshot.
    seq: Option<u64>,
}

impl InboxModel {
    /// Applies an update from the daemon. A snapshot replaces everything,
    /// and a delta the snapshot already includes is dropped.
    pub fn apply(&mut self, update: TopicUpdate) {
        let TopicUpdate::Inbox { seq, update } = update else {
            return;
        };
        match update {
            InboxUpdate::Snapshot(snapshot) => self.topic = snapshot,
            InboxUpdate::Delta(delta) => {
                if self.seq.is_none_or(|last| seq <= last) {
                    return;
                }
                self.topic.apply(delta);
            }
        }
        self.seq = Some(seq);
    }

    /// The open entries, oldest first.
    pub fn entries(&self) -> &[InboxEntry] {
        &self.topic.entries
    }

    /// What the Dock badge and the sources pane count.
    pub fn count(&self) -> usize {
        self.topic.count()
    }

    /// The open entries that hold back one PR, for its pane.
    pub fn for_pr(&self, repo: &RepoName, number: u64) -> Vec<&InboxEntry> {
        self.topic.for_pr(repo, number).collect()
    }
}

/// The Dock badge's label: the open count, or none at zero.
pub fn badge(count: usize) -> Option<String> {
    (count > 0).then(|| count.to_string())
}

/// The buttons an open entry offers, with the command each sends. A Run
/// entry about a Step is answered by retrying it, and only a PR entry can
/// be dismissed. A spent Budget's entry offers to raise the Budget or run
/// anyway once. Any other cause offers nothing: it clears when its cause
/// does. A Human Step's answers need a note, so they come from [`answer`].
pub fn actions(entry: &InboxEntry) -> Vec<(String, Command)> {
    let mut actions = match &entry.scope {
        Scope::Run {
            run,
            step: Some(step),
        } => vec![(
            "Retry".to_owned(),
            Command::RetryStep {
                run: *run,
                step: step.clone(),
            },
        )],
        Scope::Pr => vec![(
            "Dismiss".to_owned(),
            Command::DismissEntry { entry: entry.id },
        )],
        Scope::Run { step: None, .. } | Scope::Cause { .. } | Scope::Human { .. } => Vec::new(),
    };
    if let Some(hit) = &entry.budget {
        let to = hit.raise_to();
        actions.splice(
            0..0,
            [
                (
                    format!("Raise to {to}"),
                    Command::RaiseBudget {
                        entry: entry.id,
                        to,
                    },
                ),
                (
                    "Run anyway once".to_owned(),
                    Command::RunAnywayOnce { entry: entry.id },
                ),
            ],
        );
    }
    actions
}

/// The ways a Human Step can be answered, with their button labels.
pub const ANSWERS: [(&str, Answer); 2] = [("Approve", Answer::Approve), ("Reject", Answer::Reject)];

/// The command that answers a Human Step entry with `note`, the text in
/// the note field. A blank note goes as none. `None` for any other entry.
pub fn answer(entry: &InboxEntry, answer: Answer, note: &str) -> Option<Command> {
    let Scope::Human { run, step } = &entry.scope else {
        return None;
    };
    let note = note.trim();
    Some(Command::AnswerStep {
        run: *run,
        step: step.clone(),
        answer,
        note: (!note.is_empty()).then(|| note.to_owned()),
    })
}

/// The line over an open entry's card: what kind it is, the Step a Human
/// Step entry is about, and how long ago it opened, as of `now` in
/// seconds since the Unix epoch.
pub fn kicker(entry: &InboxEntry, now: i64) -> String {
    let ago = ago(now - entry.raised_at);
    match &entry.scope {
        Scope::Human { step, .. } => format!("Human Step · {step} · {ago}"),
        _ => format!("Escalation · {ago}"),
    }
}

/// How long `seconds` is, as "just now", "11m ago", "2h ago" or "3d ago".
pub fn ago(seconds: i64) -> String {
    match seconds.max(0) {
        0..60 => "just now".to_owned(),
        seconds @ 60..3600 => format!("{}m ago", seconds / 60),
        seconds @ 3600..86400 => format!("{}h ago", seconds / 3600),
        seconds => format!("{}d ago", seconds / 86400),
    }
}

/// The PRs an entry holds back, as one line.
pub fn held_line(entry: &InboxEntry) -> String {
    let prs: Vec<String> = entry.prs.iter().map(ToString::to_string).collect();
    prs.join(", ")
}

/// How a Run's record lists an entry that touched it.
pub fn history_line(entry: &InboxEntry) -> String {
    match &entry.closed {
        Some(closed) => format!("{}: closed, {}", entry.title, closed.how),
        None => format!("{}: open", entry.title),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_protocol::{
        Actor, BudgetHit, BudgetKind, Cause, Cents, Closed, Closing, EntryId, InboxDelta, PrRef,
        RunId,
    };

    fn entry(id: u64, scope: Scope, prs: &[u64]) -> InboxEntry {
        InboxEntry {
            id: EntryId(id),
            scope,
            title: "Not shippable".into(),
            reasons: vec![],
            prs: prs
                .iter()
                .map(|&number| PrRef {
                    repo: RepoName::new("o", "r"),
                    number,
                })
                .collect(),
            raised_at: 0,
            budget: None,
            closed: None,
        }
    }

    fn delta(seq: u64, entry: InboxEntry) -> TopicUpdate {
        TopicUpdate::Inbox {
            seq,
            update: InboxUpdate::Delta(InboxDelta::Put {
                entry: Box::new(entry),
            }),
        }
    }

    #[test]
    fn deltas_after_the_snapshot_open_and_close_entries() {
        let mut inbox = InboxModel::default();
        inbox.apply(delta(1, entry(1, Scope::Pr, &[1])));
        assert_eq!(
            inbox.count(),
            0,
            "a delta before any snapshot means nothing"
        );

        inbox.apply(TopicUpdate::Inbox {
            seq: 3,
            update: InboxUpdate::Snapshot(Inbox {
                entries: vec![entry(2, Scope::Pr, &[1])],
            }),
        });
        inbox.apply(delta(3, entry(9, Scope::Pr, &[2])));
        assert_eq!(inbox.count(), 1, "the snapshot had delta 3");

        inbox.apply(delta(4, entry(5, Scope::Pr, &[2])));
        let mut closed = entry(2, Scope::Pr, &[1]);
        closed.closed = Some(Closed {
            at: 1,
            how: Closing::NextRunStarted,
        });
        inbox.apply(delta(5, closed));

        assert_eq!(inbox.count(), 1);
        assert_eq!(inbox.for_pr(&RepoName::new("o", "r"), 2).len(), 1);
        assert!(inbox.for_pr(&RepoName::new("o", "r"), 1).is_empty());
        assert_eq!(badge(inbox.count()).as_deref(), Some("1"));
        assert_eq!(badge(0), None);
    }

    #[test]
    fn a_run_entry_retries_its_step_a_pr_entry_dismisses_and_a_cause_offers_nothing() {
        let run = entry(
            1,
            Scope::Run {
                run: RunId(4),
                step: Some("ci".into()),
            },
            &[1],
        );
        let cause = entry(
            3,
            Scope::Cause {
                cause: Cause::InvalidPipeline {
                    repo: RepoName::new("o", "r"),
                    base: "main".into(),
                },
            },
            &[1, 2],
        );

        assert_eq!(
            actions(&run),
            [(
                "Retry".to_owned(),
                Command::RetryStep {
                    run: RunId(4),
                    step: "ci".into()
                }
            )]
        );
        assert_eq!(
            actions(&entry(2, Scope::Pr, &[1])),
            [(
                "Dismiss".to_owned(),
                Command::DismissEntry { entry: EntryId(2) }
            )]
        );
        assert!(actions(&cause).is_empty());
        assert_eq!(held_line(&cause), "o/r#1, o/r#2");
    }

    #[test]
    fn a_spent_budget_offers_a_raise_and_one_more_run() {
        let hit = |kind| {
            Some(BudgetHit {
                kind,
                spent: Cents(1040),
                budget: Cents(1000),
            })
        };
        let mut pr = entry(2, Scope::Pr, &[1]);
        pr.budget = hit(BudgetKind::Pr);
        let mut daily = entry(
            3,
            Scope::Cause {
                cause: Cause::DailyBudget,
            },
            &[1, 2],
        );
        daily.budget = hit(BudgetKind::Daily);

        assert_eq!(
            actions(&pr),
            [
                (
                    "Raise to $21".to_owned(),
                    Command::RaiseBudget {
                        entry: EntryId(2),
                        to: Cents(2100),
                    }
                ),
                (
                    "Run anyway once".to_owned(),
                    Command::RunAnywayOnce { entry: EntryId(2) }
                ),
                (
                    "Dismiss".to_owned(),
                    Command::DismissEntry { entry: EntryId(2) }
                ),
            ]
        );
        let labels: Vec<String> = actions(&daily)
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert_eq!(labels, ["Raise to $21", "Run anyway once"]);
    }

    #[test]
    fn a_human_step_is_approved_or_rejected_with_the_note_typed_in() {
        let human = entry(
            4,
            Scope::Human {
                run: RunId(7),
                step: "sign-off".into(),
            },
            &[1],
        );
        let command = |answer, note: Option<&str>| Command::AnswerStep {
            run: RunId(7),
            step: "sign-off".into(),
            answer,
            note: note.map(str::to_owned),
        };

        assert!(actions(&human).is_empty(), "its answers take a note");
        assert_eq!(
            answer(&human, Answer::Approve, "  rename the flag "),
            Some(command(Answer::Approve, Some("rename the flag")))
        );
        assert_eq!(
            answer(&human, Answer::Reject, " "),
            Some(command(Answer::Reject, None)),
            "a blank note is none"
        );
        assert_eq!(
            answer(&entry(2, Scope::Pr, &[1]), Answer::Approve, ""),
            None
        );
    }

    #[test]
    fn a_card_says_what_kind_of_entry_it_is_and_how_old() {
        let human = Scope::Human {
            run: RunId(3),
            step: "ship-it".into(),
        };
        assert_eq!(
            kicker(&entry(1, human, &[7]), 660),
            "Human Step · ship-it · 11m ago"
        );
        assert_eq!(
            kicker(&entry(2, Scope::Pr, &[7]), 30),
            "Escalation · just now"
        );
        assert_eq!(ago(7_200), "2h ago");
        assert_eq!(ago(3 * 86_400), "3d ago");
    }

    #[test]
    fn a_runs_record_says_how_each_entry_closed() {
        let mut dismissed = entry(2, Scope::Pr, &[1]);
        assert_eq!(history_line(&dismissed), "Not shippable: open");

        dismissed.closed = Some(Closed {
            at: 1,
            how: Closing::Dismissed {
                actor: Actor::Developer { via: "gui".into() },
            },
        });

        assert_eq!(history_line(&dismissed), "Not shippable: closed, dismissed");
    }
}
