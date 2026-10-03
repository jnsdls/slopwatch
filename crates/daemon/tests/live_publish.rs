//! The calls publishing makes, against the real GitHub API: creating and
//! moving a branch, `createCommitOnBranch` with `expectedHeadOid`, and
//! opening and finding a PR. Ignored by default because they need the
//! network and a `gh` login, and because they leave a PR open. Run them with
//!
//! ```sh
//! SLOPWATCH_LIVE_REPO=owner/repo \
//!     cargo test -p slopwatch-daemon --test live_publish -- --ignored
//! ```
//!
//! The first test commits one file under `ticket-81/` to a new branch named
//! `ticket-81/publish-<time>` and opens a PR from it into the default
//! branch. The second publishes a draft twice, to `slopwatch/pipeline`, so
//! the repo must have no such branch or PR yet. It doesn't merge. Close the
//! PRs and delete the branches afterwards.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use slopwatch_core::Edit;
use slopwatch_daemon::auth::GhToken;
use slopwatch_daemon::clones::Clones;
use slopwatch_daemon::drafts::Drafts;
use slopwatch_daemon::github::api::Api;
use slopwatch_daemon::github::{GitHub, NewCommit, NewPr, PIPELINE_BRANCH};
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::secrets::MemoryKeychain;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::{Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::RepoName;

fn repo() -> RepoName {
    std::env::var("SLOPWATCH_LIVE_REPO")
        .expect("set SLOPWATCH_LIVE_REPO")
        .parse()
        .unwrap()
}

#[tokio::test]
#[ignore = "needs the network, a gh login and SLOPWATCH_LIVE_REPO; leaves a PR open"]
async fn publishing_calls_work_against_github() {
    let repo = repo();
    let api = Api::new(Arc::new(GhToken::default()));
    let remote = api.git_remote(&repo).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let clones = Clones::new(dir.path());
    let (base, main, _) = clones.default_pipeline(&repo, &remote).await.unwrap();
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let branch = format!("ticket-81/publish-{time}");
    let path = format!("ticket-81/publish-{time}.yml");

    // Created, then moved to where it already is, which takes the other path.
    api.set_branch(&repo, &branch, &main.sha).await.unwrap();
    api.set_branch(&repo, &branch, &main.sha).await.unwrap();
    assert_eq!(
        clones
            .tip(&repo, &remote, &branch)
            .await
            .unwrap()
            .as_deref(),
        Some(main.sha.as_str())
    );

    let commit = |expected: &str, text: &'static str| {
        let (branch, path, expected) = (branch.clone(), path.clone(), expected.to_owned());
        let api = &api;
        let repo = &repo;
        async move {
            api.commit_files(
                repo,
                &NewCommit {
                    branch: &branch,
                    expected_head: &expected,
                    headline: "Check publishing from slopwatch",
                    body: "Made by crates/daemon/tests/live_publish.rs.",
                    files: &[(&path, text)],
                },
            )
            .await
        }
    };
    let sha = commit(&main.sha, "version: 1\n").await.unwrap();
    assert_eq!(
        clones
            .tip(&repo, &remote, &branch)
            .await
            .unwrap()
            .as_deref(),
        Some(sha.as_str())
    );
    let stale = commit(&main.sha, "version: 2\n").await;
    assert!(
        stale.is_err(),
        "a moved branch refuses the commit: {stale:?}"
    );

    let opened = api
        .create_pr(
            &repo,
            &NewPr {
                head: &branch,
                base: &base,
                title: "Check publishing from slopwatch (ticket 81)",
                body: "Opened by crates/daemon/tests/live_publish.rs. Close it.",
            },
        )
        .await
        .unwrap();
    assert_eq!(opened.head_sha, sha);
    let found = api.open_pr_from(&repo, &branch).await.unwrap();
    assert_eq!(found, Some(opened.clone()));
    assert_eq!(
        api.open_pr_from(&repo, "ticket-81/nothing").await.unwrap(),
        None
    );
    eprintln!("opened {}", opened.url);
}

#[tokio::test]
#[ignore = "needs the network, a gh login and SLOPWATCH_LIVE_REPO; leaves a PR open"]
async fn publishing_a_draft_opens_the_pipeline_pr_and_then_updates_it() {
    let repo = repo();
    let api: Arc<dyn GitHub> = Arc::new(Api::new(Arc::new(GhToken::default())));
    let data = tempfile::tempdir().unwrap();
    // The repo goes straight into the store, so no poll starts Runs on
    // its PRs.
    let store = Store::in_memory();
    store.add_repo(&repo).unwrap();
    let exe = env!("CARGO_BIN_EXE_slopwatchd");
    let library = Arc::new(Library::open(data.path().join("steps")).unwrap());
    let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&api)).unwrap());
    let runs = Runs::start(
        store.clone(),
        Arc::clone(&api),
        watching,
        RunsConfig {
            data_dir: data.path().to_owned(),
            plugins: Plugins::new(exe, Arc::clone(&library)),
            login_path: None,
            retention: Retention::default(),
            keychain: Arc::new(MemoryKeychain::default()),
        },
    )
    .unwrap();
    let drafts = Drafts::new(store, Arc::clone(runs.plugins()) as _, library, runs);
    let seen = drafts.open(&repo).await.unwrap().edits.len();
    let step = serde_json::from_value(json!({ "uses": "ci" })).unwrap();
    let edits = [
        Edit::AddStep {
            id: "ticket-81-ci".into(),
            step,
        },
        Edit::AddGateTerm {
            term: "ticket-81-ci".into(),
        },
    ];
    drafts.edit(&repo, seen, &edits, &BTreeMap::new()).unwrap();

    drafts.publish(&repo, seen + 2).await.unwrap();
    let first = drafts.view(&repo).unwrap().published.unwrap();
    let timeout = Edit::SetKey {
        step: "ticket-81-ci".into(),
        key: "timeout".into(),
        value: "20m".into(),
    };
    let seen = drafts.view(&repo).unwrap().edits.len();
    drafts
        .edit(&repo, seen, &[timeout], &BTreeMap::new())
        .unwrap();
    drafts.publish(&repo, seen + 1).await.unwrap();

    let second = drafts.view(&repo).unwrap().published.unwrap();
    assert_eq!(second.number, first.number, "the same PR");
    assert_ne!(second.head, first.head);
    // The PR's head follows the branch a moment after the commit.
    let mut head = None;
    for _ in 0..30 {
        let open = api.open_pr_from(&repo, PIPELINE_BRANCH).await.unwrap();
        head = open.map(|pr| pr.head_sha);
        if head.as_ref() == Some(&second.head) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    assert_eq!(head, Some(second.head.clone()));
    eprintln!("published {}", second.url);
}
