//! The real GitHub API against a scratch repo. Ignored by default because it
//! needs the network and a `gh` login. Run it with
//!
//! ```sh
//! SLOPWATCH_LIVE_REPO=owner/repo SLOPWATCH_LIVE_PRS=1,2 \
//!     cargo test -p slopwatch-daemon --test live_github -- --ignored
//! ```
//!
//! where the first PR is the developer's and targets a branch that has a
//! Pipeline file, and the second is the developer's and targets one that
//! doesn't. The test adds and removes the `slopwatch` label on both.

use std::sync::Arc;

use slopwatch_daemon::auth::GhToken;
use slopwatch_daemon::clones::Clones;
use slopwatch_daemon::github::api::Api;
use slopwatch_daemon::github::{GitHub, WATCH_LABEL};
use slopwatch_protocol::RepoName;

fn fixture() -> (RepoName, u64, u64) {
    let repo = std::env::var("SLOPWATCH_LIVE_REPO")
        .expect("set SLOPWATCH_LIVE_REPO")
        .parse()
        .unwrap();
    let prs: Vec<u64> = std::env::var("SLOPWATCH_LIVE_PRS")
        .expect("set SLOPWATCH_LIVE_PRS")
        .split(',')
        .map(|number| number.trim().parse().unwrap())
        .collect();
    (repo, prs[0], prs[1])
}

#[tokio::test]
#[ignore = "needs the network, a gh login and a scratch repo"]
async fn the_api_lists_labels_and_polls_a_real_repo() {
    let (repo, on_pipeline, on_bare) = fixture();
    let api = Api::new(Arc::new(GhToken::default()));

    let available = api.available_repos().await.unwrap();
    assert!(available.contains(&repo), "{available:?}");

    let poll = api.poll(std::slice::from_ref(&repo)).await.unwrap();
    let rate = poll.rate.unwrap();
    eprintln!(
        "one poll of one repo cost {} of {} points",
        rate.cost, rate.limit
    );
    let prs = poll.repos[0].prs.clone().unwrap();
    let pr = |number| prs.iter().find(|pr| pr.number == number).unwrap();
    assert!(pr(on_pipeline).base_has_pipeline);
    assert!(!pr(on_bare).base_has_pipeline);

    api.create_label(&repo).await.unwrap();
    api.create_label(&repo).await.unwrap();
    api.set_label(&repo, on_pipeline, WATCH_LABEL, true)
        .await
        .unwrap();
    api.set_label(&repo, on_bare, WATCH_LABEL, false)
        .await
        .unwrap();
    api.set_label(&repo, on_bare, WATCH_LABEL, false)
        .await
        .unwrap();

    let poll = api.poll(std::slice::from_ref(&repo)).await.unwrap();
    let prs = poll.repos[0].prs.clone().unwrap();
    let labeled = |number| prs.iter().find(|pr| pr.number == number).unwrap().labeled;
    assert!(labeled(on_pipeline));
    assert!(!labeled(on_bare));

    api.set_label(&repo, on_pipeline, WATCH_LABEL, false)
        .await
        .unwrap();
    let missing = RepoName::new(repo.owner.clone(), "slopwatch-no-such-repo");
    let poll = api.poll(&[repo.clone(), missing]).await.unwrap();
    assert!(
        !poll.repos[0]
            .prs
            .as_ref()
            .unwrap()
            .iter()
            .any(|pr| pr.labeled)
    );
    assert_eq!(poll.repos[1].prs, None);
}

#[tokio::test]
#[ignore = "needs the network, a gh login and a scratch repo"]
async fn a_blobless_clone_reads_the_pipeline_from_a_real_base_branch() {
    let (repo, on_pipeline, _) = fixture();
    let api = Api::new(Arc::new(GhToken::default()));
    let poll = api.poll(std::slice::from_ref(&repo)).await.unwrap();
    let pr = poll.repos[0]
        .prs
        .as_ref()
        .unwrap()
        .iter()
        .find(|pr| pr.number == on_pipeline)
        .unwrap()
        .clone();
    let dir = tempfile::tempdir().unwrap();
    let clones = Clones::new(dir.path());
    let remote = api.git_remote(&repo).await.unwrap();

    let read = clones.pipeline_at(&repo, &remote, &pr.base).await.unwrap();

    assert_eq!(read.sha, pr.detail.base_sha);
    assert!(read.text.is_some());
    let files = clones
        .changed_files(&repo, &remote, pr.number, &read.sha, &pr.head_sha)
        .await
        .unwrap();
    assert!(!files.is_empty(), "the PR's head changes something");
    // The blobless clone fetches the blobs the diff needs, with the token.
    let diff = clones
        .diff(&repo, &remote, pr.number, &read.sha, &pr.head_sha)
        .await
        .unwrap();
    for file in &files {
        assert!(diff.contains(&format!(" b/{file}")), "{file} in {diff}");
    }
    let config = std::fs::read_to_string(clones.path(&repo).join("config")).unwrap();
    assert!(
        !config.contains("AUTHORIZATION"),
        "the token stays out of the clone"
    );
}

/// Needs `SLOPWATCH_LIVE_LINKED=<pr>:<issue>` too: a PR on the default
/// branch whose description closes the issue. GitHub links closing
/// keywords only on PRs into the default branch.
#[tokio::test]
#[ignore = "needs the network, a gh login and a scratch repo"]
async fn reads_the_issues_a_pr_closes() {
    let (repo, _, _) = fixture();
    let linked = std::env::var("SLOPWATCH_LIVE_LINKED").expect("set SLOPWATCH_LIVE_LINKED");
    let (pr, issue) = linked.split_once(':').expect("<pr>:<issue>");
    let api = Api::new(Arc::new(GhToken::default()));

    let issues = api.linked_issues(&repo, pr.parse().unwrap()).await.unwrap();

    let issue: u64 = issue.parse().unwrap();
    let found = issues
        .iter()
        .find(|linked| linked.number == issue)
        .unwrap_or_else(|| panic!("#{issue} isn't in {issues:?}"));
    assert_eq!(found.repo, repo);
    assert!(!found.title.is_empty());
    assert!(found.url.ends_with(&format!("/issues/{issue}")));
}
