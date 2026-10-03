//! Each Run's append-only event journal, and live delivery of what gets
//! appended. Appending and subscribing take the same lock, so a subscriber
//! gets every event exactly once: from the stored journal up to the moment
//! it subscribed, then from the broadcast.

use std::sync::Mutex;

use slopwatch_protocol::{RunEvent, RunId};
use tokio::sync::broadcast;

use super::now_ms;

use crate::store::{Store, StoreError};

/// Live events a slow subscriber may fall behind by. One that falls
/// further replays the rest from the stored journal.
const BACKLOG: usize = 1024;

pub type Live = broadcast::Receiver<(RunId, Journalled)>;

/// One event with its place in the journal.
#[derive(Debug, Clone, PartialEq)]
pub struct Journalled {
    pub seq: u64,
    /// When it was journalled, in milliseconds since the epoch.
    pub ts: i64,
    pub event: RunEvent,
}

pub struct Journal {
    store: Store,
    live: Mutex<broadcast::Sender<(RunId, Journalled)>>,
}

impl Journal {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            live: Mutex::new(broadcast::channel(BACKLOG).0),
        }
    }

    /// Appends `event` to the Run's journal and sends it to subscribers.
    pub fn append(&self, run: RunId, event: RunEvent) -> Result<u64, StoreError> {
        let live = self.live.lock().expect("no panics while appending");
        let text = serde_json::to_string(&event).expect("Run events always serialize");
        let ts = now_ms();
        let seq = self.store.append_event(run, ts, &text)?;
        // No subscribers is fine: the journal has it.
        let _ = live.send((run, Journalled { seq, ts, event }));
        Ok(seq)
    }

    /// Prunes an ended Run's journal down to the events that rebuild its
    /// record: the start, each Step's last settle, the last Gate, the
    /// Effects and the end. Then appends [`RunEvent::Pruned`], so a client that already
    /// had the Run learns its detail is gone. Folding what's left still
    /// gives the Run's final state, for a client starting from scratch or
    /// from any sequence number it had.
    pub fn prune(&self, run: RunId, at: i64) -> Result<(), StoreError> {
        let live = self.live.lock().expect("no panics while appending");
        let mut keep = Vec::new();
        let mut settled = std::collections::HashMap::new();
        let mut gate = None;
        for Journalled { seq, event, .. } in self.replay(run, 0)? {
            match event {
                // What a Step did on GitHub stays on record.
                RunEvent::Started { .. } | RunEvent::Ended { .. } | RunEvent::Effect { .. } => {
                    keep.push(seq)
                }
                RunEvent::StepSettled { step, .. } => {
                    settled.insert(step, seq);
                }
                RunEvent::Gate { .. } => gate = Some(seq),
                RunEvent::StepStarted { .. }
                | RunEvent::StepProgress { .. }
                | RunEvent::StepRetried { .. }
                | RunEvent::Pruned { .. } => {}
            }
        }
        keep.extend(settled.into_values());
        keep.extend(gate);
        let event = RunEvent::Pruned { at };
        let text = serde_json::to_string(&event).expect("Run events always serialize");
        let ts = now_ms();
        let seq = self.store.prune_journal(run, &keep, ts, &text, at)?;
        let _ = live.send((run, Journalled { seq, ts, event }));
        Ok(())
    }

    /// The Run's events after `after`, and a receiver for every event
    /// appended to any Run from now on.
    pub fn subscribe(&self, run: RunId, after: u64) -> Result<(Vec<Journalled>, Live), StoreError> {
        let live = self.live.lock().expect("no panics while appending");
        let stored = self.replay(run, after)?;
        Ok((stored, live.subscribe()))
    }

    pub fn has_run(&self, run: RunId) -> Result<bool, StoreError> {
        self.store.run_exists(run)
    }

    /// The Run's stored events after `after`.
    pub fn replay(&self, run: RunId, after: u64) -> Result<Vec<Journalled>, StoreError> {
        Ok(self
            .store
            .events_after(run, after)?
            .into_iter()
            .filter_map(|stored| match serde_json::from_str(&stored.event) {
                Ok(event) => Some(Journalled {
                    seq: stored.seq,
                    ts: stored.ts,
                    event,
                }),
                Err(error) => {
                    let seq = stored.seq;
                    eprintln!("slopwatchd: Run {run} event {seq} doesn't read back: {error}");
                    None
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_core::GateState;
    use slopwatch_protocol::RepoName;

    use crate::store::NewRun;

    fn run(store: &Store) -> RunId {
        let repo = RepoName::new("o", "r");
        store
            .insert_run(
                &NewRun {
                    repo: &repo,
                    number: 1,
                    head_sha: "abc",
                    base: "main",
                    base_sha: "def",
                    pipeline: "",
                    files: &[],
                    steps: vec![],
                },
                0,
            )
            .unwrap()
    }

    fn gate(state: GateState) -> RunEvent {
        RunEvent::Gate { state }
    }

    #[test]
    fn a_subscriber_gets_the_stored_events_then_the_live_ones() {
        let store = Store::in_memory();
        let journal = Journal::new(store.clone());
        let id = run(&store);
        journal.append(id, gate(GateState::Pending)).unwrap();
        journal.append(id, gate(GateState::Fail)).unwrap();

        let (stored, mut live) = journal.subscribe(id, 1).unwrap();
        journal.append(id, gate(GateState::Pass)).unwrap();

        assert_eq!(seqs(&stored), [(2, gate(GateState::Fail))]);
        let (live_run, live_event) = live.try_recv().unwrap();
        assert_eq!(
            (live_run, live_event.seq, live_event.event),
            (id, 3, gate(GateState::Pass))
        );
        assert!(live_event.ts > 0);
        assert!(live.try_recv().is_err());
    }

    fn seqs(events: &[Journalled]) -> Vec<(u64, RunEvent)> {
        events
            .iter()
            .map(|journalled| (journalled.seq, journalled.event.clone()))
            .collect()
    }

    fn settled(step: &str, verdict: slopwatch_core::Verdict) -> RunEvent {
        RunEvent::StepSettled {
            step: step.into(),
            verdict,
            reason: None,
            outputs: Default::default(),
            reused_from: None,
        }
    }

    #[test]
    fn a_pruned_journal_still_folds_into_the_final_run() {
        use slopwatch_core::{EndReason, Verdict};
        use slopwatch_protocol::RunView;

        let store = Store::in_memory();
        let journal = Journal::new(store.clone());
        let id = run(&store);
        let events = [
            RunEvent::Started {
                repo: RepoName::new("o", "r"),
                number: 1,
                head_sha: "abc".into(),
                base: "main".into(),
                base_sha: "def".into(),
                steps: vec![],
                gate: "[ci]".into(),
            },
            RunEvent::StepStarted {
                step: "ci".into(),
                attempt: 1,
            },
            RunEvent::StepProgress {
                step: "ci".into(),
                message: "waiting".into(),
            },
            settled("ci", Verdict::Error),
            gate(GateState::Fail),
            RunEvent::StepStarted {
                step: "ci".into(),
                attempt: 2,
            },
            settled("ci", Verdict::Pass),
            gate(GateState::Pass),
            RunEvent::Ended {
                reason: EndReason::Shippable,
            },
        ];
        let mut full = RunView::default();
        for event in events {
            let seq = journal.append(id, event.clone()).unwrap();
            full.apply(seq, event);
        }
        let (_, mut live) = journal.subscribe(id, 9).unwrap();

        journal.prune(id, 1234).unwrap();

        let left = journal.replay(id, 0).unwrap();
        assert_eq!(
            left.iter().map(|event| event.seq).collect::<Vec<_>>(),
            [1, 7, 8, 9, 10]
        );
        let mut pruned = RunView::default();
        for event in left {
            pruned.apply(event.seq, event.event);
        }
        assert_eq!(pruned.pruned_at, Some(1234));
        assert_eq!(pruned.gate, full.gate);
        assert_eq!(pruned.end, full.end);
        assert_eq!(
            live.try_recv().unwrap().1.event,
            RunEvent::Pruned { at: 1234 },
            "a subscriber hears about it"
        );
    }
}
