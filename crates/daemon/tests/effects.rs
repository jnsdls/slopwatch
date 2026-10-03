//! Effects end to end: Steps ask for them over the Step protocol, the
//! daemon carries them out against a fake GitHub, and the intent journal
//! carries them across a crash (ADR 0003, ADR 0009).
//!
//! Each daemon life gets its own tokio runtime, and dropping the runtime
//! stands in for SIGKILL, as in `runs.rs`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use slopwatch_core::{EndReason, Verdict};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::{CI_PIPELINE, FakeGitHub, Hold};
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::{
    Check, CheckState, Checks, ChecksState, Effect, EffectResult, Manifest,
};
use slopwatch_protocol::{RepoName, RunEvent, RunId};

const WAIT: Duration = Duration::from_secs(20);

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

/// My PR 1 in `jnsdls/app`, labelled, with `pipeline` on main, and a data
/// dir that outlives each daemon. Pipelines can use the `poster` Plugin,
/// whose Steps ask for Effects by Step name (see [`World::write_poster`]).
struct World {
    github: Arc<FakeGitHub>,
    data: tempfile::TempDir,
    bin: tempfile::TempDir,
}

impl World {
    fn new(pipeline: &str) -> Self {
        let github = Arc::new(FakeGitHub::new("me"));
        github.add_repo(&repo());
        github.open_pr(&repo(), 1, "me", "Add the thing");
        github.label_on_github(&repo(), 1, true);
        github.set_pipeline(&repo(), "main", pipeline);
        let world = Self {
            github,
            data: tempfile::tempdir().unwrap(),
            bin: tempfile::tempdir().unwrap(),
        };
        world.write_poster();
        world
    }

    /// The `poster` Plugin. Its Step asks for one Effect named by the Step
    /// id, then passes once it hears back. Step `late` asks for nothing
    /// until it's cancelled, and asks for a comment then. Step `after`
    /// passes first and asks for a comment after.
    fn write_poster(&self) {
        use std::os::unix::fs::PermissionsExt as _;
        let script = format!(
            r#"#!/bin/sh
ask() {{ printf '{{"type":"effect","id":"%s","effect":%s}}\n' "$1" "$2"; }}
case "$SLOPWATCH_STEP" in
  post) ask hello '{{"kind":"comment","body":"Hello from a Step"}}' ;;
  label) ask tag '{{"kind":"label","name":"reviewed"}}' ;;
  unwatch) ask off '{{"kind":"label","name":"slopwatch","remove":true}}' ;;
  deploy) ask go '{{"kind":"deploy"}}' ;;
  rerun) ask again '{{"kind":"rerun","check":"test","job":7}}' ;;
  after)
    echo '{{"type":"outcome","verdict":"pass"}}'
    ask after '{{"kind":"comment","body":"After the Outcome"}}' ;;
esac
while read line; do
  case "$line" in
    *'"effect_result"'*)
      echo "$SLOPWATCH_STEP $line" >> '{bin}/results'
      echo '{{"type":"outcome","verdict":"pass"}}'
      exit 0 ;;
    *'"cancel"'*)
      [ "$SLOPWATCH_STEP" = late ] && ask late '{{"kind":"comment","body":"Too late"}}'
      exit 0 ;;
  esac
done
"#,
            bin = self.bin.path().display(),
        );
        let path = self.poster();
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn poster(&self) -> PathBuf {
        self.bin.path().join("poster.sh")
    }

    /// The effect results each `poster` Step heard, as `step json` lines.
    fn results(&self) -> Vec<String> {
        std::fs::read_to_string(self.bin.path().join("results"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
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
            let manifest: Manifest = serde_json::from_value(json!({
                "id": "poster", "version": "1.0.0", "dialect": 1, "workspace": "none",
                "effects": ["comment", "label"],
            }))
            .unwrap();
            let plugins = Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library))
                .with_plugin(manifest, self.poster(), vec!["run".into()]);
            let runs = Runs::start(
                store,
                github,
                Arc::clone(&watching),
                RunsConfig {
                    data_dir: self.data.path().to_owned(),
                    plugins,
                    login_path: None,
                    retention: Retention::default(),
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

    fn runs(&self) -> Vec<RunId> {
        let mut runs: Vec<RunId> = self
            .store()
            .run_summaries(&repo(), 1, 10)
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

    /// The Effects the Run's events record, as `(step, effect, result)`.
    fn effects(&self, run: RunId) -> Vec<(String, Effect, EffectResult)> {
        self.events(run)
            .into_iter()
            .filter_map(|event| match event {
                RunEvent::Effect {
                    step,
                    effect,
                    result,
                } => Some((step, effect, result)),
                _ => None,
            })
            .collect()
    }

    /// The Verdict and reason the Run's events give `step`.
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

    fn starts(&self, run: RunId, step: &str) -> usize {
        self.events(run)
            .iter()
            .filter(|event| matches!(event, RunEvent::StepStarted { step: s, .. } if s == step))
            .count()
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

fn pipeline(steps: &[&str]) -> String {
    let mut text = "version: 1\nsteps:\n".to_owned();
    for step in steps {
        text.push_str(&format!("  {step}: {{ uses: poster }}\n"));
    }
    text.push_str(&format!("gate: [{}]\n", steps.join(", ")));
    text
}

#[test]
fn a_comment_lands_once_with_its_marker_and_the_step_hears_it_did() {
    let world = World::new(&pipeline(&["post"]));

    world.life(async |daemon| {
        until(daemon, "the Run to end", || {
            world
                .runs()
                .first()
                .is_some_and(|&run| world.ended(run).is_some())
        })
        .await;
    });

    let run = world.runs()[0];
    assert_eq!(world.ended(run), Some(EndReason::Shippable));
    let comments = world.github.comments(&repo(), 1);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].starts_with("Hello from a Step"));
    assert!(
        comments[0].contains("<!-- slopwatch:effect="),
        "{}",
        comments[0]
    );
    assert_eq!(
        world.effects(run),
        [(
            "post".to_owned(),
            Effect::Comment {
                body: "Hello from a Step".into()
            },
            EffectResult::Done
        )]
    );
    assert_eq!(world.results().len(), 1);
    assert!(world.results()[0].contains(r#""status":"done""#));
}

#[test]
fn labels_go_on_but_the_slopwatch_label_is_refused() {
    let world = World::new(&pipeline(&["label", "unwatch"]));

    world.life(async |daemon| {
        until(daemon, "the Run to end", || {
            world
                .runs()
                .first()
                .is_some_and(|&run| world.ended(run).is_some())
        })
        .await;
    });

    let run = world.runs()[0];
    let labels = world.github.labels(&repo(), 1);
    assert!(labels.contains("reviewed"), "{labels:?}");
    assert!(labels.contains("slopwatch"), "{labels:?}");
    let effects = world.effects(run);
    assert!(
        effects
            .iter()
            .any(|(step, _, result)| step == "label" && *result == EffectResult::Done),
        "{effects:?}"
    );
    assert!(
        effects
            .iter()
            .any(|(step, _, result)| step == "unwatch"
                && matches!(result, EffectResult::Refused { .. })),
        "{effects:?}"
    );
}

#[test]
fn an_effect_outside_the_list_or_the_manifest_is_a_protocol_error() {
    let world = World::new(&pipeline(&["deploy", "rerun"]));

    world.life(async |daemon| {
        until(daemon, "the Run to end", || {
            world
                .runs()
                .first()
                .is_some_and(|&run| world.ended(run).is_some())
        })
        .await;
    });

    let run = world.runs()[0];
    for step in ["deploy", "rerun"] {
        let (verdict, reason) = world.settled(run, step).unwrap();
        assert_eq!(verdict, Verdict::Error, "{step}");
        let reason = reason.unwrap_or_default();
        assert!(reason.starts_with("error(protocol)"), "{step}: {reason}");
    }
    assert!(
        world
            .settled(run, "rerun")
            .unwrap()
            .1
            .unwrap()
            .contains("doesn't declare"),
        "an undeclared Effect names itself"
    );
    assert!(world.github.reruns().is_empty());
    assert!(world.effects(run).is_empty());
}

#[test]
fn an_effect_requested_after_the_run_ended_is_dropped_and_shows_in_its_events() {
    let world = World::new(&pipeline(&["late"]));

    world.life(async |daemon| {
        until(daemon, "the Run to start", || !world.runs().is_empty()).await;
        let first = world.runs()[0];
        until(daemon, "late to start", || world.starts(first, "late") == 1).await;
        world.github.push(&repo(), 1);
        until(daemon, "the drop to show", || {
            !world.effects(first).is_empty()
        })
        .await;

        assert_eq!(world.ended(first), Some(EndReason::Superseded));
        let effects = world.effects(first);
        let [(step, effect, EffectResult::Dropped { reason })] = &effects[..] else {
            panic!("expected one dropped Effect, got {effects:?}");
        };
        assert_eq!(step, "late");
        assert_eq!(
            *effect,
            Effect::Comment {
                body: "Too late".into()
            }
        );
        assert!(reason.contains("ended"), "{reason}");
    });

    assert!(world.github.comments(&repo(), 1).is_empty());
}

#[test]
fn an_effect_requested_after_the_step_settled_is_dropped_while_the_run_goes_on() {
    let world = World::new(&pipeline(&["after", "late"]));

    world.life(async |daemon| {
        until(daemon, "the drop to show", || {
            world
                .runs()
                .first()
                .is_some_and(|&run| !world.effects(run).is_empty())
        })
        .await;
        let run = world.runs()[0];

        assert_eq!(world.ended(run), None, "late keeps the Run going");
        assert_eq!(world.settled(run, "after").unwrap().0, Verdict::Pass);
        let effects = world.effects(run);
        let [(step, _, EffectResult::Dropped { reason })] = &effects[..] else {
            panic!("expected one dropped Effect, got {effects:?}");
        };
        assert_eq!(step, "after");
        assert!(reason.contains("settled"), "{reason}");
    });

    assert!(world.github.comments(&repo(), 1).is_empty());
}

/// The first life: the `post` Step asks for its comment, and the daemon
/// dies inside the call at `hold`.
fn crash_inside_the_comment_call(world: &World, hold: Hold) -> RunId {
    world.github.hold_comments(Some(hold));
    world.life(async |daemon| {
        until(daemon, "the comment call", || {
            world.github.comment_calls() == 1
        })
        .await;
    });
    world.github.hold_comments(None);
    world.runs()[0]
}

/// The second life finishes the Run.
fn restart_and_finish(world: &World, run: RunId) {
    world.life(async |daemon| {
        until(daemon, "the Run to end", || world.ended(run).is_some()).await;
    });
    assert_eq!(world.ended(run), Some(EndReason::Shippable));
    assert_eq!(world.starts(run, "post"), 2, "post started again");
    assert_eq!(world.runs(), [run], "the same Run");
}

#[test]
fn a_crash_before_the_comment_posted_posts_it_once_after_the_restart() {
    let world = World::new(&pipeline(&["post"]));
    let run = crash_inside_the_comment_call(&world, Hold::BeforePosting);
    assert!(world.github.comments(&repo(), 1).is_empty());

    restart_and_finish(&world, run);

    assert_eq!(world.github.comments(&repo(), 1).len(), 1);
    assert_eq!(
        world.github.comment_calls(),
        2,
        "the reconcile posted it, and the respawned Step's request didn't"
    );
}

#[test]
fn a_crash_after_the_comment_posted_finds_it_and_doesnt_post_again() {
    let world = World::new(&pipeline(&["post"]));
    let run = crash_inside_the_comment_call(&world, Hold::AfterPosting);
    assert_eq!(world.github.comments(&repo(), 1).len(), 1);

    restart_and_finish(&world, run);

    assert_eq!(world.github.comments(&repo(), 1).len(), 1);
    assert_eq!(world.github.comment_calls(), 1);
    assert_eq!(
        world.effects(run),
        [(
            "post".to_owned(),
            Effect::Comment {
                body: "Hello from a Step".into()
            },
            EffectResult::Done
        )],
        "one Effect, done once"
    );
}

#[test]
fn an_open_comment_whose_run_ended_while_the_daemon_was_down_is_dropped() {
    let world = World::new(&pipeline(&["post"]));
    let run = crash_inside_the_comment_call(&world, Hold::BeforePosting);
    world.github.push(&repo(), 1);

    world.life(async |daemon| {
        until(daemon, "the open Effect to close", || {
            !world.effects(run).is_empty()
        })
        .await;
    });

    assert_eq!(world.ended(run), Some(EndReason::Superseded));
    assert!(matches!(
        world.effects(run)[..],
        [(_, _, EffectResult::Dropped { .. })]
    ));
    assert!(world.github.comments(&repo(), 1).is_empty());
}

fn failed(name: &str, job: Option<u64>) -> Checks {
    Checks {
        state: ChecksState::Failure,
        runs: vec![Check {
            name: name.into(),
            state: CheckState::Failure,
            url: None,
            actions_job: job,
        }],
    }
}

fn passed(name: &str, job: u64) -> Checks {
    Checks {
        state: ChecksState::Success,
        runs: vec![Check {
            name: name.into(),
            state: CheckState::Success,
            url: None,
            actions_job: Some(job),
        }],
    }
}

/// A CI-only Pipeline whose `test` Actions job (job 7) failed, after the
/// daemon asked GitHub to rerun it.
async fn ci_reran(world: &World, daemon: &Daemon) -> RunId {
    until(daemon, "the rerun", || world.github.reruns() == [7]).await;
    world.runs()[0]
}

#[test]
fn a_failed_actions_job_is_rerun_once_and_ci_passes_if_the_rerun_does() {
    let world = World::new(CI_PIPELINE);
    world.github.set_checks(&repo(), 1, failed("test", Some(7)));

    world.life(async |daemon| {
        let run = ci_reran(&world, daemon).await;
        // The fake, like GitHub, runs the rerun as a new job.
        world.github.set_checks(&repo(), 1, passed("test", 1_000));
        until(daemon, "the Run to end", || world.ended(run).is_some()).await;

        assert_eq!(world.ended(run), Some(EndReason::Shippable));
        assert_eq!(world.settled(run, "ci").unwrap().0, Verdict::Pass);
        assert!(matches!(
            world.effects(run)[..],
            [(_, Effect::Rerun { job: 7, .. }, EffectResult::Done)]
        ));
    });
    assert_eq!(world.github.reruns(), [7]);
}

#[test]
fn ci_fails_only_once_the_rerun_fails_too() {
    let world = World::new(CI_PIPELINE);
    world.github.set_checks(&repo(), 1, failed("test", Some(7)));

    world.life(async |daemon| {
        let run = ci_reran(&world, daemon).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(world.ended(run), None, "ci waits for the rerun");

        world
            .github
            .set_checks(&repo(), 1, failed("test", Some(1_000)));
        until(daemon, "the Run to end", || world.ended(run).is_some()).await;

        assert_eq!(world.ended(run), Some(EndReason::NotShippable));
        assert_eq!(world.settled(run, "ci").unwrap().0, Verdict::Fail);
    });
    assert_eq!(world.github.reruns(), [7], "no second rerun");
}

#[test]
fn a_failed_check_outside_actions_is_never_rerun() {
    let world = World::new(CI_PIPELINE);
    world.github.set_checks(&repo(), 1, failed("vercel", None));

    world.life(async |daemon| {
        until(daemon, "the Run to end", || {
            world
                .runs()
                .first()
                .is_some_and(|&run| world.ended(run).is_some())
        })
        .await;
    });

    let run = world.runs()[0];
    assert_eq!(world.settled(run, "ci").unwrap().0, Verdict::Fail);
    assert!(world.github.reruns().is_empty());
    assert!(world.effects(run).is_empty());
}
