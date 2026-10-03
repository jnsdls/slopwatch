//! Runs read Pipelines through the daemon's clones, and drafts start from
//! the same clones.

use async_trait::async_trait;
use slopwatch_protocol::RepoName;
use slopwatch_protocol::pipeline::DraftBase;

use super::Runs;
use crate::drafts::{BaseFile, PipelineSource};

#[async_trait]
impl PipelineSource for Runs {
    async fn default_pipeline(&self, repo: &RepoName) -> Result<BaseFile, String> {
        let remote = self
            .github
            .git_remote(repo)
            .await
            .map_err(|error| error.to_string())?;
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
}
