//! A real review: the built-in `claude` or `codex` Plugin runs the real
//! CLI over a real PR, in a worktree and with a diff the daemon's clone
//! code made. Ignored by default because it needs the network, a `gh`
//! login, the CLI logged in, and it spends a little. Run it with
//!
//! ```sh
//! SLOPWATCH_LIVE_REPO=owner/repo SLOPWATCH_LIVE_REVIEW_PR=30 \
//! SLOPWATCH_LIVE_REVIEW_PLUGIN=codex SLOPWATCH_LIVE_REVIEW_MODEL=gpt-5.6-luna \
//!     cargo test -p slopwatch-daemon --test live_review -- --ignored --nocapture
//! ```
//!
//! The PR must be the developer's. The Step gets only `PATH`, `HOME` and
//! `USER`, as under the daemon. `SLOPWATCH_LIVE_CONFIG_DIR` stands in
//! for the Plugin's config directory setting.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde_json::json;
use slopwatch_daemon::auth::GhToken;
use slopwatch_daemon::clones::Clones;
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::api::Api;
use slopwatch_protocol::RunId;
use slopwatch_protocol::step::{FromStep, PrSnapshot, Start, ToStep};

#[tokio::test]
#[ignore = "needs the network, a gh login, a logged-in agent CLI, and spends a little"]
async fn a_real_cli_reviews_a_real_pr() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
    let repo = var("SLOPWATCH_LIVE_REPO").parse().unwrap();
    let number: u64 = var("SLOPWATCH_LIVE_REVIEW_PR").parse().unwrap();
    let plugin = var("SLOPWATCH_LIVE_REVIEW_PLUGIN");
    let model = std::env::var("SLOPWATCH_LIVE_REVIEW_MODEL").ok();

    let api = Api::new(Arc::new(GhToken::default()));
    let poll = api.poll(std::slice::from_ref(&repo)).await.unwrap();
    let pr = poll.repos[0]
        .prs
        .as_ref()
        .unwrap()
        .iter()
        .find(|pr| pr.number == number)
        .expect("the PR is open and mine")
        .clone();
    let dir = tempfile::tempdir().unwrap();
    let clones = Clones::new(dir.path().join("repos"));
    let remote = api.git_remote(&repo).await.unwrap();
    let base = clones.pipeline_at(&repo, &remote, &pr.base).await.unwrap();
    let text = clones
        .diff(&repo, &remote, number, &base.sha, &pr.head_sha)
        .await
        .unwrap();
    let diff = dir.path().join("diffs/pr.diff");
    std::fs::create_dir_all(diff.parent().unwrap()).unwrap();
    std::fs::write(&diff, text).unwrap();
    let tree = dir.path().join("worktrees/1/review.1");
    clones
        .add_worktree(&repo, &remote, number, &pr.head_sha, &tree)
        .await
        .unwrap();

    let mut with =
        json!({ "prompt": "Look for bugs and anything that doesn't match the description." });
    if let Some(model) = model {
        with["model"] = json!(model);
    }
    let start = ToStep::Start(Start {
        run: RunId(1),
        step: "review".into(),
        config: with.as_object().unwrap().clone(),
        snapshot: PrSnapshot {
            repo,
            number,
            title: pr.title.clone(),
            body: pr.detail.body.clone(),
            url: pr.url.clone(),
            author: pr.detail.author.clone(),
            head_sha: pr.head_sha.clone(),
            base: pr.base.clone(),
            draft: pr.draft,
            labels: pr.detail.labels.clone(),
            checks: pr.detail.checks.clone(),
            merge: None,
            diff: Some(diff),
            linked_issues: vec![],
            stacked_on: None,
        },
        upstream: BTreeMap::new(),
        budget_usd: Some(0.5),
    });

    let mut step = Command::new(env!("CARGO_BIN_EXE_slopwatchd"))
        .args(["plugin", &plugin, "run"])
        .current_dir(&tree)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", std::env::var("HOME").unwrap())
        .env("USER", std::env::var("USER").unwrap())
        .envs(
            std::env::var("SLOPWATCH_LIVE_CONFIG_DIR")
                .ok()
                .map(|dir| (slopwatch_protocol::step::CONFIG_DIR_ENV, dir)),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = step.stdin.take().unwrap();
    writeln!(stdin, "{}", serde_json::to_string(&start).unwrap()).unwrap();
    let mut ended = None;
    for line in BufReader::new(step.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        eprintln!("step: {line}");
        let message = serde_json::from_str::<FromStep>(&line).unwrap();
        if matches!(message, FromStep::Outcome(_) | FromStep::Error { .. }) {
            ended = Some(message);
        }
    }
    drop(stdin);
    step.wait().unwrap();
    match ended.expect("the Step reported") {
        FromStep::Outcome(outcome) => {
            eprintln!("{outcome:#?}");
            assert!(
                !outcome.outputs.findings.is_empty(),
                "the fixture PR has a bug to find"
            );
        }
        FromStep::Error { reason } => panic!("the Step errored: {reason}"),
        _ => unreachable!(),
    }
}
