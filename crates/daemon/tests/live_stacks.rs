//! What the daemon does to a Stack child once its parent squash-merges,
//! against the real GitHub API (ADR 0011). Ignored by default because it
//! needs the network and a `gh` login, and because it merges a PR. Run it
//! with
//!
//! ```sh
//! SLOPWATCH_LIVE_REPO=owner/repo SLOPWATCH_LIVE_STACK=23,24 \
//!     cargo test -p slopwatch-daemon --test live_stacks -- --ignored
//! ```
//!
//! where both PRs are the developer's, the second is stacked on the first,
//! and the second changes nothing the first changes. The test squash-merges
//! the parent, so it needs a fresh pair each time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use slopwatch_core::EndReason;
use slopwatch_daemon::auth::GhToken;
use slopwatch_daemon::github::api::Api;
use slopwatch_daemon::github::{GitHub, Merged, OpenPr, PrFate};
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::secrets::MemoryKeychain;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::{MergeMethod, MergeStatus, UpdateMethod};
use slopwatch_protocol::{RepoName, RunEvent};

fn repo() -> RepoName {
    std::env::var("SLOPWATCH_LIVE_REPO")
        .expect("set SLOPWATCH_LIVE_REPO")
        .parse()
        .unwrap()
}

/// The PR as the poll sees it, once it's open.
async fn polled(api: &Api, repo: &RepoName, number: u64) -> Option<OpenPr> {
    let poll = api.poll(std::slice::from_ref(repo)).await.unwrap();
    poll.repos[0]
        .prs
        .clone()
        .unwrap()
        .into_iter()
        .find(|found| found.number == number)
}

#[tokio::test]
#[ignore = "needs the network, a gh login and a scratch repo, and merges a PR"]
async fn a_squash_merged_parents_child_is_retargeted_and_merges_its_base_in_cleanly() {
    let repo = repo();
    let stack = std::env::var("SLOPWATCH_LIVE_STACK").expect("set SLOPWATCH_LIVE_STACK");
    let (parent, child) = stack.split_once(',').expect("parent,child");
    let (parent, child): (u64, u64) = (parent.parse().unwrap(), child.parse().unwrap());
    let api = Api::new(Arc::new(GhToken::default()));

    let bottom = polled(&api, &repo, parent)
        .await
        .expect("the parent is open");
    assert_eq!(bottom.stack, None);
    let top = polled(&api, &repo, child).await.expect("the child is open");
    let link = top.stack.clone().expect("the child is stacked");
    assert_eq!(link.parent.number, parent);
    assert_eq!(link.parent.base, bottom.base);
    assert_eq!(top.root().name, bottom.base);
    assert_eq!(link.position, None, "a personal repo has no native stacks");
    assert_eq!(api.pr_fate(&repo, child).await.unwrap(), PrFate::Open);

    assert_eq!(
        api.merge(&repo, parent, &bottom.head_sha, Some(MergeMethod::Squash))
            .await
            .unwrap(),
        Merged::Merged
    );
    assert_eq!(
        api.pr_fate(&repo, parent).await.unwrap(),
        PrFate::Merged {
            into: bottom.base.clone(),
            kept_commits: false
        }
    );

    api.set_base(&repo, child, &bottom.base).await.unwrap();
    api.set_base(&repo, child, &bottom.base)
        .await
        .expect("retargeting onto the base it has succeeds, as after a crash");
    let state = api.merge_state(&repo, child, &top.head_sha).await.unwrap();
    assert!(
        state.behind_by.is_some_and(|behind| behind > 0),
        "{state:?}"
    );
    api.update_branch(&repo, child, &top.head_sha, UpdateMethod::Merge)
        .await
        .unwrap();

    // GitHub pushes the update a moment after it answers.
    let deadline = Instant::now() + Duration::from_secs(120);
    let moved = loop {
        let now = polled(&api, &repo, child).await.unwrap();
        if now.head_sha != top.head_sha {
            break now;
        }
        assert!(Instant::now() < deadline, "the update never landed");
        tokio::time::sleep(Duration::from_secs(3)).await;
    };
    assert_eq!(moved.base, bottom.base);
    assert_eq!(moved.stack, None);
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let state = api
            .merge_state(&repo, child, &moved.head_sha)
            .await
            .unwrap();
        if state.status != MergeStatus::Unknown {
            assert!(!state.conflicts, "{state:?}");
            assert_eq!(state.behind_by, Some(0), "{state:?}");
            break;
        }
        assert!(Instant::now() < deadline, "GitHub never settled {child}");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// The whole daemon lands a Stack on GitHub, bottom-up. Run it with
/// `SLOPWATCH_LIVE_LANDING=25,26,27`: the developer's PRs, each stacked on
/// the one before, with CI that passes and a Pipeline with `ci` and `merge`
/// on the bottom PR's base.
#[tokio::test]
#[ignore = "needs the network, a gh login and a scratch repo, and merges the Stack"]
async fn the_daemon_lands_a_stack_bottom_up() {
    let repo = repo();
    let prs: Vec<u64> = std::env::var("SLOPWATCH_LIVE_LANDING")
        .expect("set SLOPWATCH_LIVE_LANDING")
        .split(',')
        .map(|n| n.parse().unwrap())
        .collect();
    let data = tempfile::tempdir().unwrap();
    let store = Store::open(&data.path().join("state.db")).unwrap();
    let github: Arc<dyn GitHub> = Arc::new(Api::new(Arc::new(GhToken::default())));
    let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&github)).unwrap());
    let library = Arc::new(Library::open(data.path().join("steps")).unwrap());
    let plugins = Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library));
    let runs = Runs::start(
        store.clone(),
        Arc::clone(&github),
        Arc::clone(&watching),
        RunsConfig {
            data_dir: data.path().to_owned(),
            plugins,
            login_path: None,
            retention: Retention::default(),
            keychain: Arc::new(MemoryKeychain::default()),
        },
    )
    .unwrap();
    watching.add_repo(repo.clone()).await.unwrap();
    for &pr in &prs {
        watching.set_watched(repo.clone(), pr, true).await.unwrap();
    }
    let daemon = Daemon::with_build_id("live", Arc::clone(&watching), library).with_runs(runs);

    let deadline = Instant::now() + Duration::from_secs(30 * 60);
    loop {
        let _ = daemon.poll().await;
        let mut merged = 0;
        for &pr in &prs {
            if matches!(github.pr_fate(&repo, pr).await, Ok(PrFate::Merged { .. })) {
                merged += 1;
            }
        }
        eprintln!("{merged} of {} merged", prs.len());
        if merged == prs.len() {
            break;
        }
        assert!(Instant::now() < deadline, "the Stack never landed");
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    // The last Run ends once its Merge Step hears of the merge.
    for _ in 0..10 {
        let _ = daemon.poll().await;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    for &pr in &prs {
        let mut runs = store.run_summaries(&repo, pr, 20).unwrap();
        runs.reverse();
        for run in &runs {
            let started = store
                .events_after(run.id, 0)
                .unwrap()
                .into_iter()
                .map(|stored| serde_json::from_str::<RunEvent>(&stored.event).unwrap())
                .find_map(|event| match event {
                    RunEvent::Started { base, .. } => Some(base),
                    _ => None,
                })
                .unwrap();
            eprintln!(
                "#{pr} Run {}: head {} on {started}, ended {:?}",
                run.id, run.head_sha, run.end
            );
        }
        assert_eq!(runs.last().unwrap().end, Some(EndReason::Merged));
    }
}
