//! The commit rules for write Steps end to end (ADR 0002, ADR 0008): the
//! daemon judges what a write Step leaves in its worktree, commits it
//! through the fake GitHub's `createCommitOnBranch`, and stops the Fix
//! loop. The rules key on `workspace: write`, so these use a shell-script
//! write Plugin, `writer`, rather than `fix`.
//!
//! `judge` reads the PR's head: it passes once `fixed.txt` is there, or
//! fails always while `<control>/judge.fail` exists. `writer` runs
//! `<control>/writer.sh` in its worktree, then reports pass.
//!
//! Each daemon life gets its own tokio runtime, and dropping the runtime
//! stands in for SIGKILL, as in `effects.rs`.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use slopwatch_core::{EndReason, Verdict};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::{FakeGitHub, Hold};
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::secrets::MemoryKeychain;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::{Manifest, Start};
use slopwatch_protocol::{InboxEntry, RepoName, RunEvent, RunId};

const WAIT: Duration = Duration::from_secs(30);

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

struct World {
    _reaper: support::Reaper,
    github: Arc<FakeGitHub>,
    data: tempfile::TempDir,
    control: tempfile::TempDir,
}

impl World {
    /// My PR 1, labelled, with a Pipeline where `writer` runs once the
    /// Gate on `judge` fails, and `writer.sh` as `writer`'s work.
    fn new(fix_rounds: Option<u32>, writer: &str) -> Self {
        let github = Arc::new(FakeGitHub::new("me"));
        github.add_repo(&repo());
        github.open_pr(&repo(), 1, "me", "Add the thing");
        github.push_file(&repo(), 1, "src/thing.txt", "A thing.\n");
        github.label_on_github(&repo(), 1, true);
        let rounds = fix_rounds.map_or(String::new(), |n| format!("fix_rounds: {n}\n"));
        github.set_pipeline(
            &repo(),
            "main",
            &format!(
                "version: 1\n{rounds}steps:\n  judge: {{ uses: judge }}\n  writer: {{ uses: \
                 writer, needs: [gate] }}\ngate: [judge]\n"
            ),
        );
        let control = tempfile::tempdir().unwrap();
        let world = Self {
            _reaper: support::Reaper::new(control.path()),
            github,
            data: tempfile::tempdir().unwrap(),
            control,
        };
        world.write_plugins();
        world.set_writer(writer);
        world
    }

    fn write_plugins(&self) {
        use std::os::unix::fs::PermissionsExt as _;
        let control = self.control.path().display();
        let judge = format!(
            r#"#!/bin/sh
read start
if [ ! -f '{control}/judge.fail' ] && [ -f fixed.txt ]; then
  echo '{{"type":"outcome","verdict":"pass"}}'
else
  echo '{{"type":"outcome","verdict":"fail","outputs":{{"findings":[{{"severity":"error","message":"fixed.txt is missing","file":"fixed.txt"}}]}}}}'
fi
"#
        );
        let writer = format!(
            r#"#!/bin/sh
read start
printf '%s\n' "$start" > '{control}/start.json'
sh '{control}/writer.sh'
printf '%s\n' '{{"type":"outcome","verdict":"pass","outputs":{{"note":"Add fixed.txt\n\nThe judge wants it."}}}}'
"#
        );
        for (name, script) in [("judge", judge), ("writer", writer)] {
            let path = self.control.path().join(name);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// What `writer` does in its worktree, as shell.
    fn set_writer(&self, script: &str) {
        std::fs::write(self.control.path().join("writer.sh"), script).unwrap();
    }

    /// Makes `judge` fail whatever the head holds.
    fn judge_always_fails(&self) {
        std::fs::write(self.control.path().join("judge.fail"), "").unwrap();
    }

    fn plugin(&self, name: &str) -> PathBuf {
        self.control.path().join(name)
    }

    fn store(&self) -> Store {
        Store::open(&self.data.path().join("state.db")).unwrap()
    }

    /// Runs one life of the daemon, which then dies with no warning.
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
            let mut plugins = Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library));
            for (name, workspace) in [("judge", "read"), ("writer", "write")] {
                let manifest: Manifest = serde_json::from_value(json!({
                    "id": name, "version": "1.0.0", "dialect": 1, "workspace": workspace,
                }))
                .unwrap();
                store
                    .put_approval(&slopwatch_daemon::approvals::Approval::granting(
                        &manifest,
                        slopwatch_protocol::Actor::Developer { via: "test".into() },
                        0,
                    ))
                    .unwrap();
                plugins = plugins.with_plugin(manifest, self.plugin(name), vec!["run".into()]);
            }
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

    /// The `nth` Run's end, once there are that many and it has ended.
    fn nth_ended(&self, nth: usize) -> Option<EndReason> {
        self.runs().get(nth).and_then(|&run| self.ended(run))
    }

    fn settled(&self, run: RunId, step: &str) -> Option<(Verdict, Option<String>)> {
        self.events(run)
            .into_iter()
            .rev()
            .find_map(|event| match event {
                RunEvent::StepSettled {
                    step: s,
                    verdict,
                    reason,
                    ..
                } if s == step => Some((verdict, reason)),
                _ => None,
            })
    }

    fn committed(&self, run: RunId) -> Option<(String, Vec<String>)> {
        self.events(run).into_iter().find_map(|event| match event {
            RunEvent::Committed { sha, files, .. } => Some((sha, files)),
            _ => None,
        })
    }

    /// The open PR entry, if any.
    fn pr_entry(&self) -> Option<InboxEntry> {
        self.store()
            .open_entries()
            .unwrap()
            .into_iter()
            .map(|(entry, _)| entry)
            .find(|entry| entry.dismissable())
    }

    /// What `writer` was started with, last time.
    fn writer_start(&self) -> Start {
        let text = std::fs::read_to_string(self.control.path().join("start.json")).unwrap();
        match serde_json::from_str(&text).unwrap() {
            slopwatch_protocol::step::ToStep::Start(start) => start,
            other => panic!("expected a start, got {other:?}"),
        }
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

#[test]
fn a_passing_write_step_is_committed_and_the_next_run_judges_the_fix() {
    let world = World::new(None, "echo 'Fixed.' > fixed.txt\nrm src/thing.txt\n");
    let before = world.github.head_sha(&repo(), 1);

    world.life(async |daemon| {
        until(daemon, "the Run after the fix to end", || {
            world.nth_ended(1).is_some()
        })
        .await;
    });

    let runs = world.runs();
    assert_eq!(world.ended(runs[0]), Some(EndReason::Pushed));
    assert_eq!(world.settled(runs[0], "writer").unwrap().0, Verdict::Pass);
    let head = world.github.head_sha(&repo(), 1);
    assert_ne!(head, before);
    let (sha, files) = world.committed(runs[0]).expect("a committed event");
    assert_eq!(sha, head);
    assert_eq!(files, ["fixed.txt", "src/thing.txt"]);
    assert!(world.store().pushed_by_slopwatch(&repo(), &head).unwrap());
    assert_eq!(
        world.github.api_commits(),
        [("pr-1".to_owned(), head.clone())]
    );
    let message = &world.github.api_messages()[0];
    assert!(
        message.starts_with("Add fixed.txt\n\nThe judge wants it."),
        "{message}"
    );
    assert!(message.ends_with(&format!(
        "Slopwatch-Run: {}\nCo-authored-by: slopwatch <noreply@slopwatch.invalid>",
        runs[0]
    )));
    let files = world.github.head_files(&repo(), 1);
    assert!(
        files
            .iter()
            .any(|(_, path, text)| path == "fixed.txt" && text == "Fixed.\n")
    );
    assert!(!files.iter().any(|(_, path, _)| path == "src/thing.txt"));

    // The writer read the Finding behind the failing Gate term.
    let start = world.writer_start();
    assert_eq!(start.gate_failing, ["judge"]);
    assert_eq!(
        start.upstream["judge"].outputs.findings[0].message,
        "fixed.txt is missing"
    );

    // The next Run is on the fix, which passes the Gate.
    assert_eq!(world.ended(runs[1]), Some(EndReason::Shippable));
    assert_eq!(
        world
            .store()
            .fix_streak(&repo(), 1, &head, None)
            .unwrap()
            .rounds,
        1
    );
    assert!(world.pr_entry().is_none());
}

#[test]
fn a_guarded_path_refuses_the_whole_commit_and_raises_an_entry_listing_it() {
    let world = World::new(
        None,
        "echo 'Fixed.' > fixed.txt\nmkdir -p .github/workflows .slopwatch\necho 'on: push' > \
         .github/workflows/ci.yml\necho 'version: 1' > .slopwatch/pipeline.yml\n",
    );

    world.life(async |daemon| {
        until(daemon, "the Run to end", || world.nth_ended(0).is_some()).await;
    });

    let run = world.runs()[0];
    assert_eq!(world.ended(run), Some(EndReason::NotShippable));
    let (verdict, reason) = world.settled(run, "writer").unwrap();
    assert_eq!(verdict, Verdict::Error);
    let reason = reason.unwrap();
    assert!(reason.starts_with("error(guarded_path)"), "{reason}");
    assert!(reason.contains(".github/workflows/ci.yml"), "{reason}");
    assert!(reason.contains(".slopwatch/pipeline.yml"), "{reason}");
    assert!(!reason.contains("fixed.txt"), "{reason}");
    assert!(world.github.api_commits().is_empty(), "nothing committed");
    assert_eq!(world.committed(run), None);
    let entry = world.pr_entry().expect("a PR entry");
    assert!(
        entry
            .reasons
            .iter()
            .any(|r| r.contains(".github/workflows/ci.yml")),
        "{entry:?}"
    );
}

#[test]
fn modes_symlinks_and_submodules_fail_the_step_with_an_entry() {
    let world = World::new(
        None,
        "echo 'Fixed.' > fixed.txt\nln -s src/thing.txt link\nprintf '#!/bin/sh\\n' > run.sh\n\
         chmod +x run.sh\n",
    );

    world.life(async |daemon| {
        until(daemon, "the Run to end", || world.nth_ended(0).is_some()).await;
    });

    let run = world.runs()[0];
    let (verdict, reason) = world.settled(run, "writer").unwrap();
    assert_eq!(verdict, Verdict::Error);
    let reason = reason.unwrap();
    assert!(reason.starts_with("error(unsupported_change)"), "{reason}");
    assert!(reason.contains("link (symlink)"), "{reason}");
    assert!(reason.contains("run.sh (executable file)"), "{reason}");
    assert!(world.github.api_commits().is_empty());
    let entry = world.pr_entry().expect("a PR entry");
    assert!(entry.reasons.iter().any(|r| r.contains("link (symlink)")));
}

#[test]
fn a_branch_that_moved_during_the_step_refuses_the_commit_and_nothing_is_written() {
    let world = World::new(None, "echo 'Fixed.' > fixed.txt\n");
    world.github.push_before_next_commit(&repo(), 1);

    world.life(async |daemon| {
        until(daemon, "the first Run to end", || {
            world.nth_ended(0).is_some()
        })
        .await;
    });

    let run = world.runs()[0];
    assert_eq!(world.ended(run), Some(EndReason::Superseded));
    let (verdict, reason) = world.settled(run, "writer").unwrap();
    assert_eq!(verdict, Verdict::Error);
    assert!(reason.unwrap().contains("the branch moved"));
    assert!(world.github.api_commits().is_empty(), "nothing written");
    assert_eq!(world.committed(run), None);
    let head = world.github.head_sha(&repo(), 1);
    assert!(!world.store().pushed_by_slopwatch(&repo(), &head).unwrap());
}

#[test]
fn the_loop_stops_at_the_round_cap_with_its_own_entry() {
    let world = World::new(Some(1), "date +%s%N >> notes.txt\n");
    world.judge_always_fails();

    world.life(async |daemon| {
        until(daemon, "the capped Run to end", || {
            world.nth_ended(1).is_some()
        })
        .await;
    });

    let runs = world.runs();
    assert_eq!(world.ended(runs[0]), Some(EndReason::Pushed));
    assert_eq!(world.ended(runs[1]), Some(EndReason::NotShippable));
    assert_eq!(
        world.settled(runs[1], "writer"),
        Some((Verdict::Skipped, Some("round cap".to_owned())))
    );
    assert_eq!(world.github.api_commits().len(), 1);
    let entry = world.pr_entry().expect("a PR entry");
    assert_eq!(entry.title, "Fix stopped");
    assert!(entry.reasons[0].contains("round cap"), "{entry:?}");
    assert!(entry.reasons[0].contains("`fix_rounds` is 1"), "{entry:?}");
}

#[test]
fn an_empty_diff_stops_the_loop_as_nothing_actionable() {
    let world = World::new(None, "true\n");

    world.life(async |daemon| {
        until(daemon, "the Run to end", || world.nth_ended(0).is_some()).await;
    });

    let run = world.runs()[0];
    assert_eq!(world.ended(run), Some(EndReason::NotShippable));
    let (verdict, reason) = world.settled(run, "writer").unwrap();
    assert_eq!(verdict, Verdict::Pass);
    assert!(reason.unwrap().starts_with("nothing actionable"));
    assert!(world.github.api_commits().is_empty());
    let entry = world.pr_entry().expect("a PR entry");
    assert_eq!(entry.title, "Fix stopped");
    assert!(entry.reasons[0].contains("nothing actionable"), "{entry:?}");
    assert!(
        entry.reasons.iter().any(|r| r.contains("`judge`")),
        "the failing Gate term is listed too: {entry:?}"
    );
}

#[test]
fn a_repeated_tree_stops_the_loop_as_loop_detected() {
    // The writer adds a file, then takes it away again.
    let world = World::new(
        None,
        "if [ -f flip.txt ]; then rm flip.txt; else echo flip > flip.txt; fi\n",
    );
    world.judge_always_fails();

    world.life(async |daemon| {
        until(daemon, "the second Run to end", || {
            world.nth_ended(1).is_some()
        })
        .await;
    });

    let runs = world.runs();
    assert_eq!(world.ended(runs[0]), Some(EndReason::Pushed));
    assert_eq!(world.ended(runs[1]), Some(EndReason::NotShippable));
    let (_, reason) = world.settled(runs[1], "writer").unwrap();
    assert!(reason.unwrap().starts_with("loop detected"));
    assert_eq!(world.github.api_commits().len(), 1);
    let entry = world.pr_entry().expect("a PR entry");
    assert_eq!(entry.title, "Fix stopped");
    assert!(entry.reasons[0].contains("loop detected"), "{entry:?}");
}

#[test]
fn a_commit_a_crash_lost_is_reconciled_into_the_push_journal() {
    let world = World::new(None, "echo 'Fixed.' > fixed.txt\n");
    // GitHub makes the commit, and the daemon dies before it hears back.
    world.github.hold_commits(Some(Hold::AfterPosting));

    world.life(async |daemon| {
        until(daemon, "the commit to land", || {
            !world.github.api_commits().is_empty()
        })
        .await;
    });
    let head = world.github.head_sha(&repo(), 1);
    assert!(!world.store().pushed_by_slopwatch(&repo(), &head).unwrap());
    world.github.hold_commits(None);

    world.life(async |daemon| {
        until(daemon, "the second Run to end", || {
            world.nth_ended(1).is_some()
        })
        .await;
    });

    let runs = world.runs();
    assert_eq!(world.ended(runs[0]), Some(EndReason::Pushed));
    assert!(world.store().pushed_by_slopwatch(&repo(), &head).unwrap());
    assert_eq!(world.committed(runs[0]).unwrap().0, head);
    assert_eq!(world.ended(runs[1]), Some(EndReason::Shippable));
    assert_eq!(world.github.api_commits().len(), 1, "committed once");
}

#[test]
fn a_commit_that_never_reached_github_is_made_again_after_a_crash() {
    let world = World::new(None, "echo 'Fixed.' > fixed.txt\n");
    world.github.hold_commits(Some(Hold::BeforePosting));

    world.life(async |daemon| {
        until(daemon, "the commit call to go out", || {
            world.store().open_commits().unwrap().len() == 1
        })
        .await;
    });
    world.github.hold_commits(None);

    world.life(async |daemon| {
        until(daemon, "the second Run to end", || {
            world.nth_ended(1).is_some()
        })
        .await;
    });

    let runs = world.runs();
    assert_eq!(world.ended(runs[0]), Some(EndReason::Pushed));
    assert_eq!(world.github.api_commits().len(), 1);
    assert!(world.store().open_commits().unwrap().is_empty());
    assert_eq!(world.ended(runs[1]), Some(EndReason::Shippable));
}

/// A fake `claude` that records its arguments and prompt in
/// `<control>/claude.*`, writes `fixed.txt` where it runs, and answers as
/// `claude -p --output-format stream-json` does for a fix.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
out="$(dirname "$0")/claude"
printf '%s\n' "$@" > "$out.args"
cat > "$out.prompt"
echo 'Fixed.' > fixed.txt
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"structured_output":{"summary":"Add fixed.txt","fixed":true},"modelUsage":{"claude-haiku-4-5":{"inputTokens":10,"outputTokens":5,"costUSD":0.002}}}'
"#;

#[test]
fn the_fix_plugin_acts_on_findings_and_ci_logs_and_its_edits_are_committed() {
    use slopwatch_protocol::step::{Check, CheckState, Checks, ChecksState};
    use std::os::unix::fs::PermissionsExt as _;

    let world = World::new(None, "true\n");
    let cli = world.control.path().join("claude");
    std::fs::write(&cli, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    world.github.set_pipeline(
        &repo(),
        "main",
        &format!(
            "version: 1\nsteps:\n  judge: {{ uses: judge }}\n  fix: {{ uses: fix, needs: [gate], \
             with: {{ agent: claude, model: haiku, cli: {} }} }}\ngate: [judge]\n",
            cli.display()
        ),
    );
    world.github.set_checks(
        &repo(),
        1,
        Checks {
            state: ChecksState::Failure,
            runs: vec![Check {
                name: "test".into(),
                state: CheckState::Failure,
                url: None,
                actions_job: Some(77),
            }],
        },
    );
    world
        .github
        .set_job_log(77, "early line\nthread 'main' panicked: empty list\n");

    world.life(async |daemon| {
        until(daemon, "the Run after the fix to end", || {
            world.nth_ended(1).is_some()
        })
        .await;
    });

    let runs = world.runs();
    assert_eq!(world.ended(runs[0]), Some(EndReason::Pushed));
    assert_eq!(world.settled(runs[0], "fix").unwrap().0, Verdict::Pass);
    assert_eq!(world.ended(runs[1]), Some(EndReason::Shippable));
    let message = &world.github.api_messages()[0];
    assert!(
        message.starts_with("Add fixed.txt\n\nSlopwatch-Run: "),
        "{message}"
    );

    let prompt = std::fs::read_to_string(world.control.path().join("claude.prompt")).unwrap();
    assert!(prompt.contains("Step `judge` ended fail."), "{prompt}");
    assert!(
        prompt.contains("- error fixed.txt: fixed.txt is missing"),
        "{prompt}"
    );
    assert!(prompt.contains("The CI check `test` failed"), "{prompt}");
    assert!(
        prompt.contains("thread 'main' panicked: empty list"),
        "{prompt}"
    );
    let args = std::fs::read_to_string(world.control.path().join("claude.args")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert!(
        args.windows(2)
            .any(|w| w == ["--permission-mode", "acceptEdits"]),
        "{args:?}"
    );
    assert!(
        args.windows(2)
            .any(|w| w == ["--tools", "Read,Grep,Glob,Edit,Write"]),
        "{args:?}"
    );
    assert!(args.contains(&"--safe-mode"), "{args:?}");
    assert!(
        args.windows(2).any(|w| w == ["--model", "haiku"]),
        "{args:?}"
    );
}
