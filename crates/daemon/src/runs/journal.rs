//! Each Run's append-only event journal, and live delivery of what gets
//! appended. Appending and subscribing take the same lock, so a subscriber
//! gets every event exactly once: from the stored journal up to the moment
//! it subscribed, then from the broadcast.

use std::sync::Mutex;

use slopwatch_protocol::{RunEvent, RunId};
use tokio::sync::broadcast;

use crate::store::{Store, StoreError};

/// Live events a slow subscriber may fall behind by. One that falls
/// further replays the rest from the stored journal.
const BACKLOG: usize = 1024;

pub type Live = broadcast::Receiver<(RunId, u64, RunEvent)>;

pub struct Journal {
    store: Store,
    live: Mutex<broadcast::Sender<(RunId, u64, RunEvent)>>,
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
        let seq = self.store.append_event(run, &text)?;
        // No subscribers is fine: the journal has it.
        let _ = live.send((run, seq, event));
        Ok(seq)
    }

    /// The Run's events after `after`, and a receiver for every event
    /// appended to any Run from now on.
    pub fn subscribe(
        &self,
        run: RunId,
        after: u64,
    ) -> Result<(Vec<(u64, RunEvent)>, Live), StoreError> {
        let live = self.live.lock().expect("no panics while appending");
        let stored = self.replay(run, after)?;
        Ok((stored, live.subscribe()))
    }

    pub fn has_run(&self, run: RunId) -> Result<bool, StoreError> {
        self.store.run_exists(run)
    }

    /// The Run's stored events after `after`.
    pub fn replay(&self, run: RunId, after: u64) -> Result<Vec<(u64, RunEvent)>, StoreError> {
        Ok(self
            .store
            .events_after(run, after)?
            .into_iter()
            .filter_map(|(seq, text)| Some((seq, serde_json::from_str(&text).ok()?)))
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

        assert_eq!(stored, [(2, gate(GateState::Fail))]);
        assert_eq!(live.try_recv().unwrap(), (id, 3, gate(GateState::Pass)));
        assert!(live.try_recv().is_err());
    }
}
