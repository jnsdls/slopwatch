//! The Merge Step end to end: the built-in `ci` and `merge` Plugins run as
//! real processes against the fake GitHub, which merges, queues and
//! updates branches in real git repos (ADR 0004, ADR 0011).

use std::sync::Arc;
use std::time::Duration;

use slopwatch_core::{EndReason, Verdict};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::secrets::MemoryKeychain;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::{
    Check, CheckState, Checks, ChecksState, Effect, EffectResult, MergeMethod, Outputs,
    UpdateMethod,
};
use slopwatch_protocol::{InboxEntry, RepoName, RunEvent, RunId, Scope};

const WAIT: Duration = Duration::from_secs(30);

const PIPELINE: &str = "version: 1
steps:
  ci: { uses: ci }
  merge: { uses: merge, needs: [gate] }
gate: [ci]
";

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

/// My PR 1 in `jnsdls/app`, labelled, with `pipeline` on main.
struct World {
    github: Arc<FakeGitHub>,
    data: tempfile::TempDir,
}

impl World {
    fn new(pipeline: &str) -> Self {
        let github = Arc::new(FakeGitHub::new("me"));
        github.add_repo(&repo());
        // The Pipeline goes on main first, so the PR starts up to date.
        github.set_pipeline(&repo(), "main", pipeline);
        github.open_pr(&repo(), 1, "me", "Add the thing");
        github.label_on_github(&repo(), 1, true);
        Self {
            github,
            data: tempfile::tempdir().unwrap(),
        }
    }

    fn store(&self) -> Store {
        Store::open(&self.data.path().join("state.db")).unwrap()
    }

    fn life<T>(&self, life: impl AsyncFnOnce(&Arc<Daemon>) -> T) -> T {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(async {
            let store = self.store();
            let github: Arc<dyn GitHub> = Arc::clone(&self.github) as Arc<dyn GitHub>;
            let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&github)).unwrap());
            let library = Arc::new(Library::open(self.data.path().join("steps")).unwrap());
            let plugins = Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library));
            let runs = Runs::start(
                store,
                github,
                Arc::clone(&watching),
                RunsConfig {
                    clis: Default::default(),
                    data_dir: self.data.path().to_owned(),
                    plugins,
                    login_path: None,
                    retention: Retention::default(),
                    keychain: Arc::new(MemoryKeychain::default()),
                },
            )
            .unwrap();
            if watching.prs().is_empty() {
                watching.add_repo(repo()).await.unwrap();
            }
            let daemon = Arc::new(Daemon::with_build_id("test", watching, library).with_runs(runs));
            life(&daemon).await
        });
        drop(runtime);
        result
    }

    /// The PR's Runs, oldest first.
    fn runs(&self) -> Vec<RunId> {
        let mut runs: Vec<RunId> = self
            .store()
            .run_summaries(&repo(), 1, 20)
            .unwrap()
            .iter()
            .map(|run| run.id)
            .collect();
        runs.reverse();
        runs
    }

    fn events(&self, run: RunId) -> Vec<RunEvent> {
        self.store()
            .events_after(run, 0)
            .unwrap()
            .into_iter()
            .map(|stored| serde_json::from_str(&stored.event).unwrap())
            .collect()
    }

    fn ended(&self, run: RunId) -> Option<EndReason> {
        self.events(run).iter().find_map(|event| match event {
            RunEvent::Ended { reason, .. } => Some(*reason),
            _ => None,
        })
    }

    /// The Verdict, reason and outputs the Run's events give `step`.
    fn settled(&self, run: RunId, step: &str) -> Option<(Verdict, Option<String>, Outputs)> {
        self.events(run)
            .into_iter()
            .rev()
            .find_map(|event| match event {
                RunEvent::StepSettled {
                    step: s,
                    verdict,
                    reason,
                    outputs,
                    ..
                } if s == step => Some((verdict, reason, outputs)),
                _ => None,
            })
    }

    fn effects(&self, run: RunId) -> Vec<(Effect, EffectResult)> {
        self.events(run)
            .into_iter()
            .filter_map(|event| match event {
                RunEvent::Effect { effect, result, .. } => Some((effect, result)),
                _ => None,
            })
            .collect()
    }

    /// The open PR entries in the Inbox.
    fn pr_entries(&self) -> Vec<InboxEntry> {
        self.store()
            .open_entries()
            .unwrap()
            .into_iter()
            .map(|(entry, _)| entry)
            .filter(|entry| entry.scope == Scope::Pr)
            .collect()
    }

    /// The one open PR entry, which says the PR couldn't merge, and its
    /// reasons.
    fn couldnt_merge(&self) -> Vec<String> {
        let entries = self.pr_entries();
        let [entry] = &entries[..] else {
            panic!("expected one PR entry, got {entries:#?}");
        };
        assert_eq!(entry.title, "Couldn't merge");
        entry.reasons.clone()
    }

    /// Makes the checks on the PR's current head pass.
    fn pass_checks(&self) {
        self.github.set_checks(
            &repo(),
            1,
            Checks {
                state: ChecksState::Success,
                runs: vec![Check {
                    name: "test".into(),
                    state: CheckState::Success,
                    url: None,
                    actions_job: None,
                }],
            },
        );
    }

    /// The `n`th Run, once it exists.
    async fn run(&self, daemon: &Daemon, n: usize) -> RunId {
        until(daemon, &format!("Run {n}"), || self.runs().len() >= n).await;
        self.runs()[n - 1]
    }

    async fn end(&self, daemon: &Daemon, run: RunId) -> EndReason {
        until(daemon, &format!("Run {run} to end"), || {
            self.ended(run).is_some()
        })
        .await;
        self.ended(run).unwrap()
    }

    /// Commits to main, which leaves the PR behind it.
    fn move_main(&self, n: usize) {
        self.github
            .commit_to(&repo(), "main", &format!("other-{n}.txt"), "Other work.\n");
    }
}

/// Polls until `done` holds.
async fn until(daemon: &Daemon, what: &str, done: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let _ = daemon.poll().await;
        if done() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

fn finding(world: &World, run: RunId, step: &str) -> String {
    let (_, _, outputs) = world.settled(run, step).unwrap();
    outputs.findings[0].message.clone()
}

#[test]
fn a_pr_whose_gate_passes_merges_at_the_judged_sha() {
    let world = World::new(PIPELINE);
    let head = world.github.head_sha(&repo(), 1);
    world.pass_checks();

    world.life(async |daemon| {
        let run = world.run(daemon, 1).await;
        assert_eq!(world.end(daemon, run).await, EndReason::Merged);
    });

    let run = world.runs()[0];
    assert_eq!(world.settled(run, "merge").unwrap().0, Verdict::Pass);
    assert!(world.github.is_merged(&repo(), 1));
    assert_eq!(
        world.github.merges(),
        [(1, head, Some(MergeMethod::Squash))],
        "one merge call, at the head the Gate judged"
    );
    assert!(world.pr_entries().is_empty());
    assert_eq!(
        world.effects(run),
        [(
            Effect::Merge {
                method: Some(MergeMethod::Squash)
            },
            EffectResult::Done
        )]
    );
}

#[test]
fn a_pr_on_a_base_with_a_merge_queue_is_enqueued_and_followed_to_merged() {
    let world = World::new(PIPELINE);
    world.github.set_merge_queue(&repo(), true);
    world.pass_checks();

    world.life(async |daemon| {
        let run = world.run(daemon, 1).await;
        until(daemon, "the PR to be queued", || {
            world.github.is_in_merge_queue(&repo(), 1)
        })
        .await;
        // The Step waits through the queue.
        for _ in 0..5 {
            daemon.poll().await.unwrap();
        }
        assert_eq!(world.ended(run), None);

        world.github.land_from_queue(&repo(), 1);
        assert_eq!(world.end(daemon, run).await, EndReason::Merged);
        assert_eq!(world.settled(run, "merge").unwrap().0, Verdict::Pass);
    });

    let run = world.runs()[0];
    assert!(
        matches!(world.github.merges()[..], [(1, _, None)]),
        "a queue uses its own method"
    );
    assert!(matches!(
        world.effects(run)[..],
        [(Effect::Merge { method: None }, EffectResult::Enqueued)]
    ));
}

#[test]
fn a_pr_the_merge_queue_ejects_fails_merge_and_ends_not_shippable() {
    let world = World::new(PIPELINE);
    world.github.set_merge_queue(&repo(), true);
    world.pass_checks();

    world.life(async |daemon| {
        let run = world.run(daemon, 1).await;
        until(daemon, "the PR to be queued", || {
            world.github.is_in_merge_queue(&repo(), 1)
        })
        .await;
        // The queue drops it before any merge state shows it queued.
        world.github.eject_from_queue(&repo(), 1);
        assert_eq!(world.end(daemon, run).await, EndReason::NotShippable);
    });

    let run = world.runs()[0];
    assert_eq!(world.settled(run, "merge").unwrap().0, Verdict::Fail);
    let message = finding(&world, run, "merge");
    assert!(message.contains("merge queue removed"), "{message}");
    assert_eq!(world.runs().len(), 1, "nothing starts on the same SHA");
    assert_eq!(world.couldnt_merge(), [format!("`merge`: {message}")]);
}

#[test]
fn a_behind_pr_is_rebased_and_the_next_run_lands_the_rebased_sha() {
    let world = World::new(PIPELINE);
    world.move_main(1);
    world.pass_checks();

    world.life(async |daemon| {
        let first = world.run(daemon, 1).await;
        assert_eq!(world.end(daemon, first).await, EndReason::Pushed);
        assert_eq!(world.settled(first, "merge").unwrap().0, Verdict::Cancelled);

        let second = world.run(daemon, 2).await;
        let rebased = world.github.head_sha(&repo(), 1);
        world.pass_checks();
        assert_eq!(world.end(daemon, second).await, EndReason::Merged);

        assert_eq!(world.github.updates(), [(1, UpdateMethod::Rebase)]);
        assert_eq!(
            world.github.merges(),
            [(1, rebased, Some(MergeMethod::Squash))],
            "only the rebased SHA the second Run judged lands"
        );
    });
}

#[test]
fn a_behind_pr_on_a_branch_that_requires_signed_commits_gets_its_base_merged_in() {
    let world = World::new(PIPELINE);
    world.github.set_requires_signatures(&repo(), true);
    world.move_main(1);
    world.pass_checks();

    world.life(async |daemon| {
        let first = world.run(daemon, 1).await;
        assert_eq!(world.end(daemon, first).await, EndReason::Pushed);
    });

    assert_eq!(world.github.updates(), [(1, UpdateMethod::Merge)]);
}

#[test]
fn a_draft_runs_the_pipeline_but_never_merges() {
    let world = World::new(PIPELINE);
    world.github.set_draft(&repo(), 1, true);
    world.pass_checks();

    world.life(async |daemon| {
        let run = world.run(daemon, 1).await;
        assert_eq!(world.end(daemon, run).await, EndReason::Shippable);
    });

    let run = world.runs()[0];
    assert_eq!(world.settled(run, "ci").unwrap().0, Verdict::Pass);
    assert_eq!(
        world.settled(run, "merge").unwrap().0,
        Verdict::Inconclusive
    );
    assert!(finding(&world, run, "merge").contains("draft"));
    assert!(world.github.merges().is_empty());
    assert!(!world.github.is_merged(&repo(), 1));
    assert!(world.pr_entries().is_empty(), "a draft needs nothing");
}

#[test]
fn a_conflicting_pr_fails_merge_and_ends_not_shippable() {
    let world = World::new(PIPELINE);
    world.move_main(1);
    world.github.set_conflicts(&repo(), 1, true);
    world.pass_checks();

    world.life(async |daemon| {
        let run = world.run(daemon, 1).await;
        assert_eq!(world.end(daemon, run).await, EndReason::NotShippable);
    });

    let run = world.runs()[0];
    assert_eq!(world.settled(run, "merge").unwrap().0, Verdict::Fail);
    assert_eq!(
        finding(&world, run, "merge"),
        "The PR conflicts with its base `main`"
    );
    assert_eq!(
        world.couldnt_merge(),
        ["`merge`: The PR conflicts with its base `main`"]
    );
    assert!(world.github.updates().is_empty());
    assert!(world.github.merges().is_empty());
}

#[test]
fn a_pr_github_blocks_past_merges_timeout_ends_not_shippable() {
    let world = World::new(
        "version: 1
steps:
  ci: { uses: ci }
  merge: { uses: merge, needs: [gate], timeout: 1s }
gate: [ci]
",
    );
    world.github.set_blocked(&repo(), 1, true);
    world.pass_checks();

    world.life(async |daemon| {
        let run = world.run(daemon, 1).await;
        assert_eq!(world.end(daemon, run).await, EndReason::NotShippable);
    });

    let run = world.runs()[0];
    let (verdict, reason, _) = world.settled(run, "merge").unwrap();
    assert_eq!(verdict, Verdict::Error);
    let reason = reason.unwrap();
    assert!(reason.starts_with("error(timeout)"), "{reason}");
    assert!(world.github.merges().is_empty());
    assert_eq!(world.couldnt_merge(), [format!("`merge`: {reason}")]);
}

#[test]
fn a_fourth_rebase_after_three_rebase_started_runs_is_refused() {
    let world = World::new(PIPELINE);
    world.pass_checks();

    world.life(async |daemon| {
        // Each Run finds main moved again and rebases, until the third
        // rebase-started Run.
        for n in 1..=4 {
            world.move_main(n);
            let run = world.run(daemon, n).await;
            world.pass_checks();
            let ended = world.end(daemon, run).await;
            if n < 4 {
                assert_eq!(ended, EndReason::Pushed, "Run {n}");
            } else {
                assert_eq!(ended, EndReason::NotShippable, "Run {n}");
            }
        }
    });

    let last = world.runs()[3];
    let message = finding(&world, last, "merge");
    assert!(message.contains("merge queue"), "{message}");
    assert!(matches!(
        world.effects(last)[..],
        [(Effect::Rebase { .. }, EffectResult::Refused { .. })]
    ));
    assert_eq!(world.github.updates().len(), 3);
    assert!(world.github.merges().is_empty());
    let reasons = world.couldnt_merge();
    assert!(reasons[0].contains("merge queue"), "{reasons:?}");
}
