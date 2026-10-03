//! The Effect calls against the real GitHub API. Ignored by default because
//! they need the network and a `gh` login. Run them with
//!
//! ```sh
//! SLOPWATCH_LIVE_REPO=owner/repo SLOPWATCH_LIVE_EFFECTS_PR=3 \
//!     cargo test -p slopwatch-daemon --test live_effects -- --ignored
//! ```
//!
//! where the PR is the developer's, and its head runs a GitHub Actions job
//! named `flaky` that fails on its first attempt and passes on a rerun.
//! The tests comment on the PR and add and remove a label on it. A rerun
//! uses the failure up, so push a new commit to the PR before running the
//! rerun test again.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use slopwatch_daemon::auth::GhToken;
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::api::Api;
use slopwatch_protocol::RepoName;
use slopwatch_protocol::step::{Check, CheckState};

const LABEL: &str = "ticket-63/effect";

fn fixture() -> (RepoName, u64) {
    let repo = std::env::var("SLOPWATCH_LIVE_REPO")
        .expect("set SLOPWATCH_LIVE_REPO")
        .parse()
        .unwrap();
    let pr = std::env::var("SLOPWATCH_LIVE_EFFECTS_PR")
        .expect("set SLOPWATCH_LIVE_EFFECTS_PR")
        .parse()
        .unwrap();
    (repo, pr)
}

#[tokio::test]
#[ignore = "needs the network, a gh login and a scratch repo"]
async fn a_comment_is_found_by_its_marker_and_labels_come_and_go() {
    let (repo, pr) = fixture();
    let api = Api::new(Arc::new(GhToken::default()));
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let marker = format!("<!-- slopwatch:effect=live-{stamp} -->");

    assert!(!api.has_comment(&repo, pr, &marker).await.unwrap());
    let body = format!("A live test of the comment Effect.\n\n{marker}");
    api.comment(&repo, pr, &body).await.unwrap();
    assert!(api.has_comment(&repo, pr, &marker).await.unwrap());

    api.set_label(&repo, pr, LABEL, true).await.unwrap();
    assert!(labels(&api, &repo, pr).await.contains(&LABEL.to_owned()));
    api.set_label(&repo, pr, LABEL, false).await.unwrap();
    api.set_label(&repo, pr, LABEL, false).await.unwrap();
    assert!(!labels(&api, &repo, pr).await.contains(&LABEL.to_owned()));
}

async fn labels(api: &Api, repo: &RepoName, pr: u64) -> Vec<String> {
    let poll = api.poll(std::slice::from_ref(repo)).await.unwrap();
    let prs = poll.repos[0].prs.clone().unwrap();
    let found = prs.into_iter().find(|found| found.number == pr).unwrap();
    found.detail.labels
}

/// The `flaky` check on the PR's head, once it isn't pending.
async fn flaky(api: &Api, repo: &RepoName, pr: u64) -> Check {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let poll = api.poll(std::slice::from_ref(repo)).await.unwrap();
        let prs = poll.repos[0].prs.clone().unwrap();
        let found = prs.into_iter().find(|found| found.number == pr).unwrap();
        let check = found
            .detail
            .checks
            .runs
            .into_iter()
            .find(|check| check.name == "flaky");
        if let Some(check) = check
            && check.state != CheckState::Pending
        {
            return check;
        }
        assert!(Instant::now() < deadline, "the flaky job never finished");
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

#[tokio::test]
#[ignore = "needs the network, a gh login and a scratch repo with a flaky Actions job"]
async fn a_failed_actions_job_reruns_by_its_check_run_id() {
    let (repo, pr) = fixture();
    let api = Api::new(Arc::new(GhToken::default()));

    let failed = flaky(&api, &repo, pr).await;
    assert_eq!(failed.state, CheckState::Failure, "{failed:?}");
    let job = failed.actions_job.expect("an Actions check run names its job");

    api.rerun_job(&repo, job).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(600);
    let rerun = loop {
        let check = flaky(&api, &repo, pr).await;
        if check.actions_job != Some(job) {
            break check;
        }
        assert!(Instant::now() < deadline, "the rerun never showed up");
        tokio::time::sleep(Duration::from_secs(10)).await;
    };

    assert_eq!(rerun.state, CheckState::Success, "{rerun:?}");
    assert!(rerun.actions_job.is_some());
}
