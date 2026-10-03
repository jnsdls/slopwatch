//! Stacks end to end (ADR 0011): the built-in `ci` and `merge` Plugins run
//! as real processes against the fake GitHub, whose PRs stack on each
//! other's head branches in real git repos.

use std::sync::Arc;
use std::time::Duration;

use slopwatch_core::{EndReason, Verdict};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::secrets::MemoryKeychain;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::{Check, CheckState, Checks, ChecksState, MergeMethod, UpdateMethod};
use slopwatch_protocol::{InboxEntry, PullRequest, RepoName, RunEvent, RunId, Scope};

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

/// `jnsdls/app` with `pipeline` on main and my PRs `prs`, all labelled.
/// Each PR after the first is stacked on the one before it.
struct World {
    github: Arc<FakeGitHub>,
    data: tempfile::TempDir,
    prs: Vec<u64>,
}

impl World {
    fn new(pipeline: &str, prs: &[u64], native: bool) -> Self {
        let github = Arc::new(FakeGitHub::new("me"));
        github.add_repo(&repo());
        github.set_pipeline(&repo(), "main", pipeline);
        github.open_pr(&repo(), prs[0], "me", &format!("PR {}", prs[0]));
        for pair in prs.windows(2) {
            let title = format!("PR {}", pair[1]);
            if native {
                github.open_native_stacked_pr(&repo(), pair[1], "me", &title, pair[0]);
            } else {
                github.open_stacked_pr(&repo(), pair[1], "me", &title, pair[0]);
            }
        }
        for &pr in prs {
            github.label_on_github(&repo(), pr, true);
        }
        Self {
            github,
            data: tempfile::tempdir().unwrap(),
            prs: prs.to_vec(),
        }
    }

    fn store(&self) -> Store {
        Store::open(&self.data.path().join("state.db")).unwrap()
    }

    fn life<T>(&self, life: impl AsyncFnOnce(&Arc<Daemon>, &Watching) -> T) -> T {
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
            let daemon = Arc::new(
                Daemon::with_build_id("test", Arc::clone(&watching), library).with_runs(runs),
            );
            life(&daemon, &watching).await
        });
        drop(runtime);
        result
    }

    /// The PR's Runs, oldest first.
    fn runs(&self, pr: u64) -> Vec<RunId> {
        let mut runs: Vec<RunId> = self
            .store()
            .run_summaries(&repo(), pr, 20)
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

    /// The head SHA and root base a Run judged, and the Steps it ran.
    fn started(&self, run: RunId) -> (String, String, Vec<String>) {
        self.events(run)
            .into_iter()
            .find_map(|event| match event {
                RunEvent::Started {
                    head_sha,
                    base,
                    steps,
                    ..
                } => Some((
                    head_sha,
                    base,
                    steps.into_iter().map(|step| step.id).collect(),
                )),
                _ => None,
            })
            .unwrap()
    }

    /// The Verdict `step` settled with, and its first Finding.
    fn settled(&self, run: RunId, step: &str) -> Option<(Verdict, Option<String>)> {
        self.events(run)
            .into_iter()
            .rev()
            .find_map(|event| match event {
                RunEvent::StepSettled {
                    step: s,
                    verdict,
                    outputs,
                    ..
                } if s == step => {
                    Some((verdict, outputs.findings.first().map(|f| f.message.clone())))
                }
                _ => None,
            })
    }

    fn open_entries(&self) -> Vec<InboxEntry> {
        self.store()
            .open_entries()
            .unwrap()
            .into_iter()
            .map(|(entry, _)| entry)
            .collect()
    }

    /// Makes the checks on every PR's current head pass.
    fn pass_checks(&self) {
        for &pr in &self.prs {
            self.github.set_checks(
                &repo(),
                pr,
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
    }

    /// Polls until `done` holds, passing the checks on every head first,
    /// as CI would.
    async fn until(&self, daemon: &Daemon, what: &str, done: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            self.pass_checks();
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

    /// GitHub merged the PR, and its latest Run ended merged.
    fn landed(&self, pr: u64) -> bool {
        self.github.is_merged(&repo(), pr)
            && self
                .runs(pr)
                .last()
                .is_some_and(|&run| self.ended(run) == Some(EndReason::Merged))
    }

    async fn polls(&self, daemon: &Daemon, n: usize) {
        for _ in 0..n {
            let _ = daemon.poll().await;
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }
}

fn row(watching: &Watching, number: u64) -> PullRequest {
    watching
        .subscribe()
        .snapshot
        .prs
        .into_iter()
        .find(|pr| pr.number == number)
        .unwrap()
}

#[test]
fn a_three_pr_stack_runs_every_pr_at_once_from_the_root_bases_pipeline() {
    // CI never passes, so nothing merges and the Stack stays put.
    let world = World::new(PIPELINE, &[1, 2, 3], false);
    // A Pipeline edit in the bottom PR must not judge the PRs above it.
    world.github.set_pipeline(
        &repo(),
        "pr-1",
        "version: 1\nsteps:\n  lint: { uses: ci }\ngate: [lint]\n",
    );

    world.life(async |daemon, watching| {
        let deadline = tokio::time::Instant::now() + WAIT;
        while [1, 2, 3].iter().any(|&pr| world.runs(pr).is_empty()) {
            assert!(tokio::time::Instant::now() < deadline, "a Run each");
            daemon.poll().await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        let two = row(watching, 2).stack.unwrap();
        assert_eq!((two.parent.number, two.position), (1, None));
        assert_eq!(two.root_base, "main");
        assert_eq!(row(watching, 3).stack.unwrap().parent.number, 2);
        assert_eq!(row(watching, 3).root_base(), "main");
        assert_eq!(row(watching, 1).stack, None);
    });

    for (pr, base) in [(1, "main"), (2, "main"), (3, "main")] {
        let run = world.runs(pr)[0];
        let (head, root, steps) = world.started(run);
        assert_eq!(head, world.github.head_sha(&repo(), pr));
        assert_eq!(root, base, "PR {pr} reads the Pipeline on the root base");
        assert_eq!(steps, ["ci", "merge"]);
        let files = world.store().run(run).unwrap().unwrap().files;
        assert_eq!(
            files,
            [format!("change-{pr}.txt")],
            "PR {pr} is judged against its parent's head"
        );
    }
}

#[test]
fn a_non_native_stack_lands_bottom_up_with_no_manual_retargeting() {
    let world = World::new(PIPELINE, &[1, 2, 3], false);
    let heads: Vec<String> = [2, 3]
        .iter()
        .map(|&pr| world.github.head_sha(&repo(), pr))
        .collect();

    world.life(async |daemon, _| {
        world
            .until(daemon, "the whole Stack to merge", || {
                [1, 2, 3].iter().all(|&pr| world.landed(pr))
            })
            .await;
    });

    assert_eq!(
        world.github.retargets(),
        [(2, "main".to_owned()), (3, "main".to_owned())]
    );
    assert_eq!(
        world.github.updates(),
        [(2, UpdateMethod::Merge), (3, UpdateMethod::Merge)],
        "a squash-merged parent's child gets its base merged in"
    );
    let merged: Vec<u64> = world.github.merges().iter().map(|m| m.0).collect();
    assert_eq!(merged, [1, 2, 3], "bottom-up, one PR at a time");

    for (pr, old_head) in [(2, &heads[0]), (3, &heads[1])] {
        let runs = world.runs(pr);
        let first = runs[0];
        assert_eq!(world.started(first).0, *old_head);
        let (verdict, finding) = world.settled(first, "merge").unwrap();
        // The parent may merge first and end the Run as the daemon takes
        // its update on, before Merge settles.
        if world.ended(first) != Some(EndReason::Pushed) {
            assert_eq!(verdict, Verdict::Inconclusive);
            assert_eq!(finding.unwrap(), format!("Stacked on #{}", pr - 1));
        }
        let last = *runs.last().unwrap();
        assert_ne!(
            world.started(last).0,
            *old_head,
            "the last Run judged the update"
        );
        assert_eq!(world.ended(last), Some(EndReason::Merged));
        for run in &runs {
            let (head, root, _) = world.started(*run);
            assert!(
                head != *old_head || runs[0] == *run,
                "no Run on PR {pr}'s old head after the retarget"
            );
            assert_eq!(root, "main");
        }
    }
    assert!(
        world.open_entries().is_empty(),
        "a stacked Merge raises nothing: {:#?}",
        world.open_entries()
    );
}

#[test]
fn a_native_stack_lands_bottom_up_and_the_daemon_leaves_the_restacking_to_github() {
    let world = World::new(PIPELINE, &[1, 2], true);

    world.life(async |daemon, watching| {
        world
            .until(daemon, "PR 2's Run", || !world.runs(2).is_empty())
            .await;
        if !world.github.is_merged(&repo(), 1) {
            assert_eq!(row(watching, 2).stack.unwrap().position, Some(2));
        }
        world
            .until(daemon, "both PRs to merge", || {
                [1, 2].iter().all(|&pr| world.landed(pr))
            })
            .await;
    });

    assert!(world.github.retargets().is_empty());
    assert!(world.github.updates().is_empty());
    let merged: Vec<u64> = world.github.merges().iter().map(|m| m.0).collect();
    assert_eq!(merged, [1, 2]);
}

#[test]
fn a_crash_between_retarget_and_update_finishes_the_update_before_the_next_run() {
    let world = World::new(PIPELINE, &[1, 2], false);
    let old_head = world.github.head_sha(&repo(), 2);
    world.github.hold_updates(true);

    world.life(async |daemon, _| {
        world
            .until(daemon, "PR 2's retarget", || {
                world.github.is_merged(&repo(), 1) && !world.github.retargets().is_empty()
            })
            .await;
        // The update call hangs, and a few polls start nothing.
        world.polls(daemon, 5).await;
    });
    assert_eq!(world.github.base(&repo(), 2), "main");
    assert_eq!(world.github.head_sha(&repo(), 2), old_head);
    let before = world.runs(2);
    assert!(before.iter().all(|&run| world.started(run).0 == old_head));
    assert!(
        before.iter().all(|&run| world.ended(run).is_some()),
        "the retarget ended PR 2's Run"
    );
    world.github.hold_updates(false);

    world.life(async |daemon, _| {
        world
            .until(daemon, "PR 2 to merge", || world.landed(2))
            .await;
    });

    assert_eq!(world.github.updates(), [(2, UpdateMethod::Merge)]);
    let runs = world.runs(2);
    assert_eq!(runs.len(), before.len() + 1, "one Run after the update");
    let last = *runs.last().unwrap();
    assert_ne!(world.started(last).0, old_head);
    assert_eq!(world.ended(last), Some(EndReason::Merged));
}

#[test]
fn a_parent_merged_with_a_merge_commit_gets_its_child_rebased() {
    let world = World::new(
        "version: 1\nsteps:\n  ci: { uses: ci }\ngate: [ci]\n",
        &[1, 2],
        false,
    );

    world.life(async |daemon, _| {
        world
            .until(daemon, "both Runs", || {
                [1, 2].iter().all(|&pr| {
                    world
                        .runs(pr)
                        .first()
                        .is_some_and(|&run| world.ended(run).is_some())
                })
            })
            .await;
        world.github.merge_on_github(&repo(), 1, MergeMethod::Merge);
        world
            .until(daemon, "PR 2's update", || {
                !world.github.updates().is_empty()
            })
            .await;
    });

    assert_eq!(world.github.retargets(), [(2, "main".to_owned())]);
    assert_eq!(world.github.updates(), [(2, UpdateMethod::Rebase)]);
}

#[test]
fn a_parent_closed_unmerged_raises_one_entry_and_the_child_stays_put() {
    let world = World::new(
        "version: 1\nsteps:\n  ci: { uses: ci }\ngate: [ci]\n",
        &[1, 2],
        false,
    );

    world.life(async |daemon, _| {
        world
            .until(daemon, "PR 2's Run to end", || {
                world
                    .runs(2)
                    .first()
                    .is_some_and(|&run| world.ended(run).is_some())
            })
            .await;
        world.github.close_pr(&repo(), 1);
        world
            .until(daemon, "PR 2's entry", || !world.open_entries().is_empty())
            .await;
        world.polls(daemon, 5).await;

        let entries = world.open_entries();
        let [entry] = &entries[..] else {
            panic!("expected one entry, got {entries:#?}");
        };
        assert_eq!(entry.scope, Scope::Pr);
        assert_eq!(entry.title, "Parent closed");
        assert_eq!(entry.prs[0].number, 2);
        assert!(entry.reasons[0].contains("#1 closed without merging"));
        assert!(world.github.retargets().is_empty());
        assert_eq!(world.github.base(&repo(), 2), "pr-1");
        assert_eq!(world.runs(2).len(), 1, "no Run starts on its own");

        // The developer moves it by hand: a base change from outside, so a
        // new Run judges the same SHA, and that closes the entry.
        world.github.retarget_on_github(&repo(), 2, "main");
        world
            .until(daemon, "a Run on main", || world.runs(2).len() == 2)
            .await;
        assert!(world.open_entries().is_empty());
    });

    let runs = world.runs(2);
    assert_eq!(world.started(runs[0]).0, world.started(runs[1]).0);
}

#[test]
fn a_base_change_from_outside_ends_a_run_as_superseded() {
    // A Step that never settles keeps the Run going.
    let world = World::new(
        "version: 1\nsteps:\n  ci: { uses: ci }\ngate: [ci]\n",
        &[1, 2],
        false,
    );

    world.life(async |daemon, _| {
        let deadline = tokio::time::Instant::now() + WAIT;
        while world.runs(2).is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "PR 2's Run");
            daemon.poll().await.unwrap();
        }
        world.github.retarget_on_github(&repo(), 2, "main");
        let deadline = tokio::time::Instant::now() + WAIT;
        while world.runs(2).len() < 2 {
            assert!(tokio::time::Instant::now() < deadline, "a second Run");
            daemon.poll().await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    });

    let runs = world.runs(2);
    assert_eq!(world.ended(runs[0]), Some(EndReason::Superseded));
    assert_eq!(world.started(runs[0]).0, world.started(runs[1]).0);
}

#[test]
fn a_child_whose_parent_closed_never_merges_into_the_dead_branch() {
    let world = World::new(PIPELINE, &[1, 2], false);
    // A draft never merges, so #1 stays open until the test closes it.
    world.github.set_draft(&repo(), 1, true);

    world.life(async |daemon, _| {
        world
            .until(daemon, "PR 2's first Run to end", || {
                world
                    .runs(2)
                    .first()
                    .is_some_and(|&run| world.ended(run).is_some())
            })
            .await;
        world.github.close_pr(&repo(), 1);
        world
            .until(daemon, "PR 2's entry", || !world.open_entries().is_empty())
            .await;
        // A push to the orphan starts a Run, whose Merge still waits.
        world.github.push(&repo(), 2);
        world
            .until(daemon, "the Run on the push to end", || {
                let runs = world.runs(2);
                runs.len() == 2 && world.ended(runs[1]).is_some()
            })
            .await;
    });

    let runs = world.runs(2);
    let (verdict, finding) = world.settled(runs[1], "merge").unwrap();
    assert_eq!(verdict, Verdict::Inconclusive);
    assert_eq!(finding.unwrap(), "Stacked on #1");
    assert!(!world.github.is_merged(&repo(), 2));
    assert!(world.github.merges().is_empty());
    assert!(world.github.retargets().is_empty());
}

#[test]
fn a_child_whose_update_failed_waits_for_a_push_and_its_update_comes_first() {
    let world = World::new(PIPELINE, &[1, 2], false);
    world.github.set_conflicts(&repo(), 2, true);

    world.life(async |daemon, _| {
        world
            .until(daemon, "PR 2's entry", || {
                world
                    .open_entries()
                    .iter()
                    .any(|entry| entry.title == "Couldn't follow its parent")
            })
            .await;
        let before = world.runs(2).len();
        world.polls(daemon, 5).await;
        assert_eq!(world.runs(2).len(), before, "no Run on the retargeted diff");

        world.github.set_conflicts(&repo(), 2, false);
        let pushed = world.github.push(&repo(), 2);
        world
            .until(daemon, "PR 2 to merge", || world.landed(2))
            .await;
        for run in world.runs(2) {
            assert_ne!(
                world.started(run).0,
                pushed,
                "the push got its update first"
            );
        }
    });

    assert_eq!(world.github.retargets(), [(2, "main".to_owned())]);
    assert_eq!(world.github.updates(), [(2, UpdateMethod::Merge)]);
    assert!(
        world.open_entries().is_empty(),
        "the next Run closed the entry"
    );
}

#[test]
fn a_child_the_developer_moved_while_its_parent_was_open_is_left_alone() {
    let world = World::new(
        "version: 1\nsteps:\n  ci: { uses: ci }\ngate: [ci]\n",
        &[1, 2],
        false,
    );

    world.life(async |daemon, _| {
        world
            .until(daemon, "PR 2's Run", || !world.runs(2).is_empty())
            .await;
        world.github.retarget_on_github(&repo(), 2, "main");
        world.polls(daemon, 3).await;
        world
            .github
            .merge_on_github(&repo(), 1, MergeMethod::Squash);
        world.polls(daemon, 5).await;
    });

    assert!(world.github.retargets().is_empty());
    assert!(world.github.updates().is_empty());
}
