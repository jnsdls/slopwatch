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
/// be dismissed. A cause offers nothing: it clears when its cause does. A
/// Human Step's answers need a note, so they come from [`answer`].
pub fn actions(entry: &InboxEntry) -> Vec<(&'static str, Command)> {
    match &entry.scope {
        Scope::Run {
            run,
            step: Some(step),
        } => vec![(
            "Retry",
            Command::RetryStep {
                run: *run,
                step: step.clone(),
            },
        )],
        Scope::Pr => vec![("Dismiss", Command::DismissEntry { entry: entry.id })],
        Scope::Run { step: None, .. } | Scope::Cause { .. } | Scope::Human { .. } => Vec::new(),
    }
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
    use slopwatch_protocol::{Actor, Cause, Closed, Closing, EntryId, InboxDelta, PrRef, RunId};

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
                "Retry",
                Command::RetryStep {
                    run: RunId(4),
                    step: "ci".into()
                }
            )]
        );
        assert_eq!(
            actions(&entry(2, Scope::Pr, &[1])),
            [("Dismiss", Command::DismissEntry { entry: EntryId(2) })]
        );
        assert!(actions(&cause).is_empty());
        assert_eq!(held_line(&cause), "o/r#1, o/r#2");
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
