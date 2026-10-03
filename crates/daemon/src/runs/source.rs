//! Runs read Pipelines through the daemon's clones, and drafts start from
//! the same clones. Publishing a draft commits to the `slopwatch/pipeline`
//! branch through the API and opens its PR (ADR 0007), and only the daemon
//! commits, journaled like an Effect (ADR 0002).

use std::time::Duration;

use async_trait::async_trait;
use slopwatch_protocol::RepoName;
use slopwatch_protocol::pipeline::{DraftBase, PipelinePr};
use slopwatch_protocol::step::{MergeMethod, UpdateMethod};

use super::Runs;
use crate::clones::GitError;
use crate::drafts::{BaseFile, Landed, PipelineSource, Publication};
use crate::github::{
    GitHubError, GitRemote, Merged, NewCommit, NewPr, PIPELINE_BRANCH, PIPELINE_PATH, PrLink,
};
use crate::store::StoreError;

/// How often, and how many times, to look for the commit a branch update
/// makes. GitHub pushes it a moment after it answers.
const UPDATE_POLL_EVERY: Duration = Duration::from_secs(1);
const UPDATE_POLLS: u32 = 30;

/// The merge methods "Merge it now" tries, in order, when the repo allows
/// more than one. Squash, as the Merge Step defaults to.
const METHODS: [MergeMethod; 3] = [MergeMethod::Squash, MergeMethod::Merge, MergeMethod::Rebase];

#[async_trait]
impl PipelineSource for Runs {
    async fn default_pipeline(&self, repo: &RepoName) -> Result<BaseFile, String> {
        let remote = self.remote(repo).await?;
        let (branch, at, blob) = self
            .clones
            .default_pipeline(repo, &remote)
            .await
            .map_err(|error| error.to_string())?;
        Ok(BaseFile {
            base: DraftBase {
                branch,
                commit: at.sha,
                blob,
            },
            text: at.text,
        })
    }

    async fn publish(
        &self,
        repo: &RepoName,
        publication: &Publication<'_>,
    ) -> Result<PipelinePr, String> {
        let remote = self.remote(repo).await?;
        let base = publication.base;
        let tip = self
            .clones
            .tip(repo, &remote, PIPELINE_BRANCH)
            .await
            .map_err(git)?;
        self.settle_commits(repo, &remote, tip.as_deref()).await?;
        let open = self
            .github
            .open_pr_from(repo, PIPELINE_BRANCH)
            .await
            .map_err(github)?;
        let head = match (&tip, &open) {
            (Some(tip), Some(pr)) => self.up_to_date(repo, &remote, base, tip, pr).await?,
            // A branch without an open PR is left from one that merged or
            // closed, so the new PR starts from the base again.
            _ => {
                self.github
                    .set_branch(repo, PIPELINE_BRANCH, &base.commit)
                    .await
                    .map_err(github)?;
                base.commit.clone()
            }
        };
        let at = self
            .clones
            .pipeline_at(repo, &remote, PIPELINE_BRANCH)
            .await
            .map_err(git)?;
        if at.sha != head {
            return Err(format!(
                "{PIPELINE_BRANCH} moved while publishing. Publish again."
            ));
        }
        let sha = if at.text.as_deref() == Some(publication.text) {
            head
        } else {
            self.commit(repo, &head, publication).await?
        };
        let pr = match open {
            Some(pr) => pr,
            None => self
                .github
                .create_pr(
                    repo,
                    &NewPr {
                        head: PIPELINE_BRANCH,
                        base: &base.branch,
                        title: publication.headline,
                        body: &pr_body(&base.branch),
                    },
                )
                .await
                .map_err(github)?,
        };
        Ok(PipelinePr {
            number: pr.number,
            url: pr.url,
            head: sha,
        })
    }

    async fn merge(&self, repo: &RepoName, pr: &PipelinePr) -> Result<Landed, String> {
        let open = self
            .github
            .open_pr_from(repo, PIPELINE_BRANCH)
            .await
            .map_err(github)?;
        if open.as_ref().map(|open| open.number) != Some(pr.number) {
            return Err(format!("PR #{} isn't open anymore", pr.number));
        }
        let state = self
            .github
            .merge_state(repo, pr.number, &pr.head)
            .await
            .map_err(github)?;
        // A merge queue goes by its own config.
        let method = if state.merge_queue {
            None
        } else {
            let allowed = METHODS.into_iter().find(|m| state.methods.contains(m));
            Some(allowed.ok_or("the repo allows no merge method")?)
        };
        match self
            .github
            .merge(repo, pr.number, &pr.head, method)
            .await
            .map_err(github)?
        {
            Merged::Merged => Ok(Landed::Merged),
            Merged::Enqueued => Ok(Landed::Enqueued),
        }
    }
}

impl Runs {
    async fn remote(&self, repo: &RepoName) -> Result<GitRemote, String> {
        self.github.git_remote(repo).await.map_err(github)
    }

    /// The head of an open Pipeline PR's branch, at `tip` now. If the
    /// Pipeline file on the base changed since the branch forked, the base
    /// is merged in first, so the PR's diff shows only the draft's edits.
    async fn up_to_date(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
        base: &DraftBase,
        tip: &str,
        pr: &PrLink,
    ) -> Result<String, String> {
        // Fetches the branch, so both commits are in the clone.
        self.clones
            .pipeline_at(repo, remote, PIPELINE_BRANCH)
            .await
            .map_err(git)?;
        let moved = self
            .clones
            .pipeline_moved_since_fork(repo, remote, tip, &base.commit)
            .await
            .map_err(git)?;
        if !moved {
            return Ok(tip.to_owned());
        }
        self.github
            .update_branch(repo, pr.number, tip, UpdateMethod::Merge)
            .await
            .map_err(|error| {
                format!(
                    "can't bring {PIPELINE_BRANCH} up to date with {}: {error}",
                    base.branch
                )
            })?;
        for _ in 0..UPDATE_POLLS {
            let now = self
                .clones
                .tip(repo, remote, PIPELINE_BRANCH)
                .await
                .map_err(git)?;
            if let Some(now) = now.filter(|now| now != tip) {
                // GitHub made it at the daemon's asking (ADR 0004), so it's
                // slopwatch's own push.
                self.store.record_push(repo, &now).map_err(store)?;
                return Ok(now);
            }
            tokio::time::sleep(UPDATE_POLL_EVERY).await;
        }
        Err(format!(
            "GitHub hadn't brought {PIPELINE_BRANCH} up to date with {} after {}s",
            base.branch,
            (UPDATE_POLL_EVERY * UPDATE_POLLS).as_secs()
        ))
    }

    /// Commits the Pipeline file on top of `head`, journaled: the intent
    /// goes in the store before the call and the SHA after (ADR 0002). A
    /// call that fails leaves the intent open, since GitHub may have made
    /// the commit anyway, and the next publish settles it.
    async fn commit(
        &self,
        repo: &RepoName,
        head: &str,
        publication: &Publication<'_>,
    ) -> Result<String, String> {
        let intent = self
            .store
            .insert_pipeline_commit(repo, PIPELINE_BRANCH, head, publication.text)
            .map_err(store)?;
        let sha = self
            .github
            .commit_files(
                repo,
                &NewCommit {
                    branch: PIPELINE_BRANCH,
                    expected_head: head,
                    headline: publication.headline,
                    body: publication.body,
                    files: &[(PIPELINE_PATH, publication.text.as_bytes())],
                    deletions: &[],
                },
            )
            .await
            .map_err(github)?;
        self.store
            .finish_pipeline_commit(repo, intent, Some(&sha))
            .map_err(store)?;
        Ok(sha)
    }

    /// Settles the commits a crash or a failed call left open, with the
    /// branch at `tip`. If the tip is a commit on top of the head one
    /// expected, writing its file, that's the one GitHub made. Any other
    /// didn't happen. Each publish settles before it commits, so the one
    /// GitHub made is still the tip.
    async fn settle_commits(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
        tip: Option<&str>,
    ) -> Result<(), String> {
        let open = self.store.open_pipeline_commits(repo).map_err(store)?;
        if open.is_empty() {
            return Ok(());
        }
        let at = match tip {
            Some(_) => {
                let at = self
                    .clones
                    .pipeline_at(repo, remote, PIPELINE_BRANCH)
                    .await
                    .map_err(git)?;
                let parents = self
                    .clones
                    .parents(repo, remote, &at.sha)
                    .await
                    .map_err(git)?;
                Some((at, parents))
            }
            None => None,
        };
        for open in open {
            let made = at.as_ref().filter(|(at, parents)| {
                *parents == [open.expected_head.as_str()]
                    && at.text.as_deref() == Some(open.text.as_str())
            });
            self.store
                .finish_pipeline_commit(repo, open.id, made.map(|(at, _)| at.sha.as_str()))
                .map_err(store)?;
        }
        Ok(())
    }
}

fn pr_body(base: &str) -> String {
    format!(
        "Published from the slopwatch Pipeline editor. Once this merges, slopwatch judges \
         PRs on `{base}` with this Pipeline, and Watched PRs whose latest Run has ended get \
         a new Run on the same SHA."
    )
}

fn git(error: GitError) -> String {
    error.to_string()
}

fn github(error: GitHubError) -> String {
    error.to_string()
}

fn store(error: StoreError) -> String {
    format!("database error: {error}")
}
