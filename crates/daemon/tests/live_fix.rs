//! A real fix, committed the way the daemon commits it: the built-in `fix`
//! Plugin runs the real Claude CLI over a real PR in a worktree the
//! daemon's clone code made, acting on a Finding. Its changes are read
//! from the worktree into a tree and committed through
//! `createCommitOnBranch` with `expectedHeadOid` (ADR 0002). Ignored by
//! default because it needs the network, a `gh` login and a logged-in
//! `claude`, spends a few cents, and pushes to the PR. Run it with
//!
//! ```sh
//! SLOPWATCH_LIVE_REPO=owner/repo SLOPWATCH_LIVE_FIX_PR=34 \
//!     cargo test -p slopwatch-daemon --test live_fix -- --ignored --nocapture
//! ```
//!
//! The PR must be the developer's, with `ticket-74/stats.py` whose `mean`
//! divides by zero on an empty list, an executable `ticket-74/run.sh` and
//! a `ticket-74/old.txt`. Besides the fix, the test changes `run.sh` and
//! deletes `old.txt` in the worktree, to check that GitHub keeps the mode
//! of a file it rewrites and makes the same tree the clone did, which is
//! how the daemon recognizes its commit after a crash.
//! `SLOPWATCH_LIVE_CONFIG_DIR` stands in for the Plugin's config directory
//! setting.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde_json::json;
use slopwatch_core::Verdict;
use slopwatch_daemon::auth::GhToken;
use slopwatch_daemon::clones::Clones;
use slopwatch_daemon::github::api::Api;
use slopwatch_daemon::github::{GitHub, GitHubError, NewCommit};
use slopwatch_protocol::RunId;
use slopwatch_protocol::step::{
    Finding, FromStep, Outcome, Outputs, PrSnapshot, Severity, Start, ToStep,
};

#[tokio::test]
#[ignore = "needs the network, a gh login and a logged-in claude; spends a little and pushes"]
async fn a_real_fix_is_committed_verified_with_the_tree_the_clone_made() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
    let repo = var("SLOPWATCH_LIVE_REPO").parse().unwrap();
    let number: u64 = var("SLOPWATCH_LIVE_FIX_PR").parse().unwrap();

    let api = Api::new(Arc::new(GhToken::default()));
    let head = api.pr_head(&repo, number).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let clones = Clones::new(dir.path().join("repos"));
    let remote = api.git_remote(&repo).await.unwrap();
    let tree = dir.path().join("worktrees/1/fix.1");
    clones
        .add_worktree(&repo, &remote, number, &head.sha, &tree)
        .await
        .unwrap();

    let review = Outcome {
        verdict: Verdict::Fail,
        outputs: Outputs {
            findings: vec![Finding {
                severity: Severity::Error,
                message: "`mean` divides by zero on an empty list; its docstring says it \
                          returns 0.0 then."
                    .into(),
                file: Some("ticket-74/stats.py".into()),
                line: Some(3),
            }],
            note: Some("1 finding".into()),
            ..Outputs::default()
        },
    };
    let start = ToStep::Start(Start {
        run: RunId(1),
        step: "fix".into(),
        config: json!({ "agent": "claude", "model": "haiku" })
            .as_object()
            .unwrap()
            .clone(),
        snapshot: PrSnapshot {
            repo: repo.clone(),
            number,
            title: "ticket-74: mean of a list".into(),
            body: String::new(),
            url: String::new(),
            author: String::new(),
            head_sha: head.sha.clone(),
            base: "main".into(),
            draft: false,
            labels: vec![],
            checks: Default::default(),
            merge: None,
            diff: None,
            linked_issues: vec![],
            stacked_on: None,
        },
        upstream: BTreeMap::from([("review".to_owned(), review)]),
        gate_failing: vec!["review".into()],
        ci_logs: vec![],
        budget_usd: Some(0.25),
    });

    let mut step = Command::new(env!("CARGO_BIN_EXE_slopwatchd"))
        .args(["plugin", "fix", "run"])
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
    let outcome = match ended.expect("the Step reported") {
        FromStep::Outcome(outcome) => outcome,
        FromStep::Error { reason } => panic!("the Step errored: {reason}"),
        _ => unreachable!(),
    };
    eprintln!("{outcome:#?}");
    assert_eq!(outcome.verdict, Verdict::Pass);

    // An executable rewritten in place, and a deletion.
    let run_sh = tree.join("ticket-74/run.sh");
    let mut script = std::fs::read_to_string(&run_sh).unwrap();
    script.push_str("python3 -c 'from stats import mean; print(mean([]))'\n");
    std::fs::write(&run_sh, script).unwrap();
    std::fs::remove_file(tree.join("ticket-74/old.txt")).unwrap();

    let index = dir.path().join("worktrees/1/fix.1.index");
    let changes = clones
        .worktree_changes(&repo, &remote, &tree, &head.sha, &index)
        .await
        .unwrap();
    eprintln!("{changes:#?}");
    let paths: Vec<&str> = changes.changes.iter().map(|c| c.path.as_str()).collect();
    assert!(paths.contains(&"ticket-74/stats.py"), "{paths:?}");
    let exe = changes
        .changes
        .iter()
        .find(|c| c.path == "ticket-74/run.sh")
        .unwrap();
    assert_eq!(
        (exe.old_mode.as_str(), exe.new_mode.as_str()),
        ("100755", "100755")
    );

    let mut files = Vec::new();
    let mut deletions = Vec::new();
    for change in &changes.changes {
        if change.status == 'D' {
            deletions.push(change.path.as_str());
        } else {
            let blob = clones.blob(&repo, &remote, &change.blob).await.unwrap();
            files.push((change.path.as_str(), blob));
        }
    }
    let additions: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_slice())).collect();
    let note = outcome.outputs.note.unwrap_or_default();
    let commit = NewCommit {
        branch: &head.branch,
        expected_head: &head.sha,
        headline: note.lines().next().unwrap_or("Fix from slopwatch"),
        body: "Slopwatch-Run: live\nCo-authored-by: slopwatch <noreply@slopwatch.invalid>",
        files: &additions,
        deletions: &deletions,
    };
    let sha = api.commit_files(&head.repo, &commit).await.unwrap();
    eprintln!("committed {sha}");

    // A second commit on the old head is refused as stale.
    let stale = api.commit_files(&head.repo, &commit).await;
    assert!(matches!(stale, Err(GitHubError::Stale(_))), "{stale:?}");

    let (parents, made) = clones
        .commit_of(&repo, &remote, number, &sha)
        .await
        .unwrap();
    assert_eq!(parents, std::slice::from_ref(&head.sha));
    assert_eq!(made, changes.tree, "GitHub made the tree the clone did");

    let verified = Command::new("gh")
        .args([
            "api",
            &format!("repos/{repo}/commits/{sha}"),
            "--jq",
            ".commit.verification.verified",
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&verified.stdout).trim(), "true");
}
