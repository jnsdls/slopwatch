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
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::api::Api;
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
    api.set_label(&repo, on_pipeline, true).await.unwrap();
    api.set_label(&repo, on_bare, false).await.unwrap();
    api.set_label(&repo, on_bare, false).await.unwrap();

    let poll = api.poll(std::slice::from_ref(&repo)).await.unwrap();
    let prs = poll.repos[0].prs.clone().unwrap();
    let labeled = |number| prs.iter().find(|pr| pr.number == number).unwrap().labeled;
    assert!(labeled(on_pipeline));
    assert!(!labeled(on_bare));

    api.set_label(&repo, on_pipeline, false).await.unwrap();
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
