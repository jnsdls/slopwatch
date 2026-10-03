//! The merge and branch-update calls against the real GitHub API. Ignored
//! by default because they need the network and a `gh` login, and because
//! they merge a PR. Run them with
//!
//! ```sh
//! SLOPWATCH_LIVE_REPO=owner/repo SLOPWATCH_LIVE_BEHIND_PR=8 SLOPWATCH_LIVE_CONFLICT_PR=7 \
//!     cargo test -p slopwatch-daemon --test live_merge -- --ignored
//! ```
//!
//! where both PRs are the developer's. The behind PR is a commit or more
//! behind its base without conflicting with it; the test brings it up to
//! date with a signed merge commit and then merges it, so it needs a fresh
//! PR each time. The conflict PR conflicts with its base and is left alone.

use std::sync::Arc;
use std::time::{Duration, Instant};

use slopwatch_daemon::auth::GhToken;
use slopwatch_daemon::github::api::Api;
use slopwatch_daemon::github::{GitHub, GitHubError, Merged};
use slopwatch_protocol::RepoName;
use slopwatch_protocol::step::{MergeMethod, MergeState, UpdateMethod};

fn repo() -> RepoName {
    std::env::var("SLOPWATCH_LIVE_REPO")
        .expect("set SLOPWATCH_LIVE_REPO")
        .parse()
        .unwrap()
}

fn pr(var: &str) -> u64 {
    std::env::var(var)
        .unwrap_or_else(|_| panic!("set {var}"))
        .parse()
        .unwrap()
}

/// The PR's head SHA as the poll sees it.
async fn head(api: &Api, repo: &RepoName, number: u64) -> String {
    let poll = api.poll(std::slice::from_ref(repo)).await.unwrap();
    let prs = poll.repos[0].prs.clone().unwrap();
    prs.into_iter()
        .find(|found| found.number == number)
        .expect("the PR is open and the developer's")
        .head_sha
}

/// The merge state once GitHub has worked out whether the PR conflicts.
async fn settled_state(api: &Api, repo: &RepoName, number: u64, head: &str) -> MergeState {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let state = api.merge_state(repo, number, head).await.unwrap();
        if state.status != slopwatch_protocol::step::MergeStatus::Unknown {
            return state;
        }
        assert!(Instant::now() < deadline, "GitHub never settled {number}");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

#[tokio::test]
#[ignore = "needs the network, a gh login and a scratch repo, and merges a PR"]
async fn a_behind_pr_is_brought_up_to_date_then_merges_only_at_the_head_it_names() {
    let (repo, number) = (repo(), pr("SLOPWATCH_LIVE_BEHIND_PR"));
    let api = Api::new(Arc::new(GhToken::default()));
    let old = head(&api, &repo, number).await;

    let state = settled_state(&api, &repo, number, &old).await;
    assert!(!state.merged);
    assert!(!state.conflicts, "{state:?}");
    assert!(
        state.behind_by.is_some_and(|behind| behind > 0),
        "{state:?}"
    );

    let stale = "0000000000000000000000000000000000000000";
    assert!(matches!(
        api.update_branch(&repo, number, stale, UpdateMethod::Merge)
            .await,
        Err(GitHubError::Unprocessable(_))
    ));
    api.update_branch(&repo, number, &old, UpdateMethod::Merge)
        .await
        .unwrap();

    // GitHub pushes the update a moment after it answers.
    let deadline = Instant::now() + Duration::from_secs(120);
    let new = loop {
        let now = head(&api, &repo, number).await;
        if now != old {
            break now;
        }
        assert!(Instant::now() < deadline, "the update never landed");
        tokio::time::sleep(Duration::from_secs(3)).await;
    };
    let state = settled_state(&api, &repo, number, &new).await;
    assert_eq!(state.behind_by, Some(0), "{state:?}");

    let moved = api
        .merge(&repo, number, &old, Some(MergeMethod::Squash))
        .await;
    assert!(
        matches!(moved, Err(GitHubError::Unprocessable(_))),
        "the judged head moved: {moved:?}"
    );
    assert_eq!(
        api.merge(&repo, number, &new, Some(MergeMethod::Squash))
            .await
            .unwrap(),
        Merged::Merged
    );
    assert_eq!(
        api.merge(&repo, number, &new, Some(MergeMethod::Squash))
            .await
            .unwrap(),
        Merged::Merged,
        "asking again, as after a crash, answers merged"
    );
    assert!(api.merge_state(&repo, number, &new).await.unwrap().merged);
}

#[tokio::test]
#[ignore = "needs the network, a gh login and a scratch repo"]
async fn a_conflicting_pr_reads_as_conflicting_and_wont_rebase() {
    let (repo, number) = (repo(), pr("SLOPWATCH_LIVE_CONFLICT_PR"));
    let api = Api::new(Arc::new(GhToken::default()));
    let head = head(&api, &repo, number).await;

    let state = settled_state(&api, &repo, number, &head).await;
    assert!(state.conflicts, "{state:?}");

    let refused = api
        .update_branch(&repo, number, &head, UpdateMethod::Rebase)
        .await;
    assert!(
        matches!(refused, Err(GitHubError::Unprocessable(_))),
        "{refused:?}"
    );
}
