//! GitHub's API over HTTPS: GraphQL for polling, REST for labels.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use slopwatch_protocol::RepoName;

use super::{GitHub, GitHubError, OpenPr, PIPELINE_PATH, Poll, RateLimit, RepoPoll, WATCH_LABEL};
use crate::auth::Credentials;

const API: &str = "https://api.github.com";
const TIMEOUT: Duration = Duration::from_secs(30);
/// How long to back off when GitHub rate-limits without saying how long.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(60);
/// Open PRs read per repo, most recently updated first. Labelling a PR
/// bumps its update time, so a watch change is always among them.
const PRS_PER_REPO: usize = 100;
const LABEL_COLOR: &str = "6f42c1";
const LABEL_DESCRIPTION: &str = "Watched by slopwatch";

pub struct Api {
    http: reqwest::Client,
    credentials: Arc<dyn Credentials>,
}

impl Api {
    pub fn new(credentials: Arc<dyn Credentials>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("slopwatch/", env!("CARGO_PKG_VERSION")))
            .timeout(TIMEOUT)
            .build()
            .expect("build the HTTP client");
        Self { http, credentials }
    }

    /// Sends a request built by `build`, with the token, retrying once with
    /// a fresh token if GitHub rejects it.
    async fn send(
        &self,
        build: impl Fn(&reqwest::Client) -> RequestBuilder,
    ) -> Result<Response, GitHubError> {
        for attempt in 0..2 {
            let token = self.credentials.token().await?;
            let response = build(&self.http)
                .bearer_auth(token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .send()
                .await
                .map_err(|error| GitHubError::Other(format!("can't reach GitHub: {error}")))?;
            if response.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                self.credentials.forget();
                continue;
            }
            return check_status(response).await;
        }
        Err(GitHubError::Auth("GitHub rejected the token".into()))
    }

    async fn graphql(&self, query: &str, variables: Value) -> Result<Value, GitHubError> {
        let body = json!({ "query": query, "variables": variables });
        let response = self
            .send(|http| http.post(format!("{API}/graphql")).json(&body))
            .await?;
        let mut answer: Value = response
            .json()
            .await
            .map_err(|error| GitHubError::Other(format!("unreadable GraphQL answer: {error}")))?;
        let data = answer
            .get_mut("data")
            .map(Value::take)
            .unwrap_or(Value::Null);
        if data.is_null() {
            return Err(graphql_error(&answer));
        }
        Ok(data)
    }

    async fn rest(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(), GitHubError> {
        self.send(|http| {
            let request = http.request(method.clone(), format!("{API}{path}"));
            match &body {
                Some(body) => request.json(body),
                None => request,
            }
        })
        .await
        .map(drop)
    }
}

#[async_trait]
impl GitHub for Api {
    async fn available_repos(&self) -> Result<Vec<RepoName>, GitHubError> {
        let data = self.graphql(AVAILABLE_REPOS, json!({})).await?;
        read_available_repos(data)
    }

    async fn poll(&self, repos: &[RepoName]) -> Result<Poll, GitHubError> {
        let (query, variables) = poll_query(repos);
        let data = self.graphql(&query, variables).await?;
        read_poll(repos, data)
    }

    async fn create_label(&self, repo: &RepoName) -> Result<(), GitHubError> {
        let body = json!({
            "name": WATCH_LABEL,
            "color": LABEL_COLOR,
            "description": LABEL_DESCRIPTION,
        });
        match self
            .rest(Method::POST, &format!("/repos/{repo}/labels"), Some(body))
            .await
        {
            // 422 means a label by that name already exists.
            Err(GitHubError::Other(message)) if message.starts_with("422") => Ok(()),
            other => other,
        }
    }

    async fn set_label(&self, repo: &RepoName, number: u64, on: bool) -> Result<(), GitHubError> {
        let labels = format!("/repos/{repo}/issues/{number}/labels");
        if on {
            let body = json!({ "labels": [WATCH_LABEL] });
            return self.rest(Method::POST, &labels, Some(body)).await;
        }
        match self
            .rest(Method::DELETE, &format!("{labels}/{WATCH_LABEL}"), None)
            .await
        {
            // The label was already off.
            Err(GitHubError::NotFound(_)) => Ok(()),
            other => other,
        }
    }
}

/// Turns an error status into a [`GitHubError`]. A rate limit shows up as
/// a 403 or 429 with `retry-after` or an empty `x-ratelimit-remaining`.
async fn check_status(response: Response) -> Result<Response, GitHubError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
    };
    let retry_after = header("retry-after").map(Duration::from_secs).or_else(|| {
        (header("x-ratelimit-remaining") == Some(0)).then(|| {
            let reset = header("x-ratelimit-reset").unwrap_or(0);
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            Duration::from_secs(reset.saturating_sub(now)).max(DEFAULT_RETRY_AFTER)
        })
    });
    let url = response.url().path().to_owned();
    let body = response.text().await.unwrap_or_default();
    match status {
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS if retry_after.is_some() => {
            Err(GitHubError::RateLimited {
                retry_after: retry_after.unwrap_or(DEFAULT_RETRY_AFTER),
            })
        }
        StatusCode::TOO_MANY_REQUESTS => Err(GitHubError::RateLimited {
            retry_after: DEFAULT_RETRY_AFTER,
        }),
        StatusCode::UNAUTHORIZED => Err(GitHubError::Auth(body)),
        StatusCode::NOT_FOUND => Err(GitHubError::NotFound(url)),
        _ => Err(GitHubError::Other(format!(
            "{} from {url}: {body}",
            status.as_u16()
        ))),
    }
}

fn graphql_error(answer: &Value) -> GitHubError {
    let errors = answer.get("errors").and_then(Value::as_array);
    let first = errors.and_then(|errors| errors.first());
    let kind = first
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str);
    let message = first
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("GraphQL answered without data")
        .to_owned();
    match kind {
        Some("RATE_LIMITED") => GitHubError::RateLimited {
            retry_after: DEFAULT_RETRY_AFTER,
        },
        Some("NOT_FOUND") => GitHubError::NotFound(message),
        _ => GitHubError::Other(message),
    }
}

const AVAILABLE_REPOS: &str = "
query {
  viewer {
    repositories(
      first: 100
      affiliations: [OWNER, COLLABORATOR, ORGANIZATION_MEMBER]
      ownerAffiliations: [OWNER, COLLABORATOR, ORGANIZATION_MEMBER]
      isArchived: false
      orderBy: { field: PUSHED_AT, direction: DESC }
    ) {
      nodes { nameWithOwner viewerPermission }
    }
  }
}";

fn read_available_repos(data: Value) -> Result<Vec<RepoName>, GitHubError> {
    #[derive(Deserialize)]
    struct Data {
        viewer: Viewer,
    }
    #[derive(Deserialize)]
    struct Viewer {
        repositories: Nodes<Repo>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Repo {
        name_with_owner: String,
        viewer_permission: Option<String>,
    }

    let data: Data = parse(data)?;
    Ok(data
        .viewer
        .repositories
        .nodes
        .into_iter()
        .filter(|repo| {
            matches!(
                repo.viewer_permission.as_deref(),
                Some("ADMIN" | "MAINTAIN" | "WRITE")
            )
        })
        .filter_map(|repo| repo.name_with_owner.parse().ok())
        .collect())
}

/// One query for every repo in `repos`, aliased `r0`, `r1` and so on, with
/// the owners and names passed as variables.
fn poll_query(repos: &[RepoName]) -> (String, Value) {
    let mut parameters = Vec::new();
    let mut fields = String::new();
    let mut variables = serde_json::Map::new();
    for (index, repo) in repos.iter().enumerate() {
        parameters.push(format!("$o{index}: String!, $n{index}: String!"));
        variables.insert(format!("o{index}"), repo.owner.clone().into());
        variables.insert(format!("n{index}"), repo.name.clone().into());
        fields.push_str(&format!(
            "  r{index}: repository(owner: $o{index}, name: $n{index}) {{ ...Prs }}\n"
        ));
    }
    let query = format!(
        "query({parameters}) {{
  rateLimit {{ cost limit remaining resetAt }}
  viewer {{ login }}
{fields}}}
fragment Prs on Repository {{
  pullRequests(states: OPEN, first: {PRS_PER_REPO}, orderBy: {{ field: UPDATED_AT, direction: DESC }}) {{
    nodes {{
      number title url isDraft headRefOid baseRefName
      author {{ login }}
      labels(first: 20) {{ nodes {{ name }} }}
      baseRef {{ target {{ ... on Commit {{ file(path: \"{PIPELINE_PATH}\") {{ oid }} }} }} }}
    }}
  }}
}}",
        parameters = parameters.join(", "),
    );
    (query, Value::Object(variables))
}

fn read_poll(repos: &[RepoName], mut data: Value) -> Result<Poll, GitHubError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Rate {
        cost: u32,
        limit: u32,
        remaining: u32,
        reset_at: String,
    }
    #[derive(Deserialize)]
    struct Viewer {
        login: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Repo {
        pull_requests: Nodes<Pr>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Pr {
        number: u64,
        title: String,
        url: String,
        is_draft: bool,
        head_ref_oid: String,
        base_ref_name: String,
        author: Option<Login>,
        labels: Nodes<Label>,
        base_ref: Option<BaseRef>,
    }
    #[derive(Deserialize)]
    struct Login {
        login: String,
    }
    #[derive(Deserialize)]
    struct Label {
        name: String,
    }
    #[derive(Deserialize)]
    struct BaseRef {
        target: Option<Target>,
    }
    #[derive(Deserialize)]
    struct Target {
        file: Option<Value>,
    }

    let rate: Option<Rate> = parse(data["rateLimit"].take())?;
    let viewer: Viewer = parse(data["viewer"].take())?;
    let mut polled = Vec::with_capacity(repos.len());
    for (index, repo) in repos.iter().enumerate() {
        let found: Option<Repo> = parse(data[format!("r{index}")].take())?;
        let prs = found.map(|found| {
            found
                .pull_requests
                .nodes
                .into_iter()
                .filter(|pr| pr.author.as_ref().is_some_and(|a| a.login == viewer.login))
                .map(|pr| OpenPr {
                    number: pr.number,
                    title: pr.title,
                    url: pr.url,
                    draft: pr.is_draft,
                    head_sha: pr.head_ref_oid,
                    base: pr.base_ref_name,
                    labeled: pr
                        .labels
                        .nodes
                        .iter()
                        .any(|label| label.name == WATCH_LABEL),
                    base_has_pipeline: pr
                        .base_ref
                        .and_then(|base| base.target)
                        .and_then(|target| target.file)
                        .is_some_and(|file| !file.is_null()),
                })
                .collect()
        });
        polled.push(RepoPoll {
            repo: repo.clone(),
            prs,
        });
    }
    Ok(Poll {
        repos: polled,
        rate: rate.map(|rate| RateLimit {
            cost: rate.cost,
            limit: rate.limit,
            remaining: rate.remaining,
            resets_in: seconds_until(&rate.reset_at).unwrap_or(Duration::from_secs(3600)),
        }),
    })
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

fn parse<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, GitHubError> {
    serde_json::from_value(value)
        .map_err(|error| GitHubError::Other(format!("unexpected GraphQL answer: {error}")))
}

/// Seconds from now until an ISO 8601 UTC time like `2026-10-03T03:19:55Z`.
fn seconds_until(timestamp: &str) -> Option<Duration> {
    let at = parse_utc(timestamp)?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(Duration::from_secs(at.saturating_sub(now)))
}

/// Seconds since the Unix epoch for `YYYY-MM-DDTHH:MM:SSZ`.
fn parse_utc(timestamp: &str) -> Option<u64> {
    let (date, time) = timestamp.strip_suffix('Z')?.split_once('T')?;
    let mut date = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let mut time = time.split(':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (time.next()??, time.next()??, time.next()??);

    // Days from the civil date, after Howard Hinnant's `days_from_civil`.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;

    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_poll_query_asks_for_every_repo_under_its_own_alias() {
        let repos = [RepoName::new("o", "a"), RepoName::new("p", "b")];

        let (query, variables) = poll_query(&repos);

        assert!(
            query.contains("r0: repository(owner: $o0, name: $n0)"),
            "{query}"
        );
        assert!(
            query.contains("r1: repository(owner: $o1, name: $n1)"),
            "{query}"
        );
        assert_eq!(
            variables,
            json!({ "o0": "o", "n0": "a", "o1": "p", "n1": "b" })
        );
    }

    #[test]
    fn a_poll_keeps_the_viewers_prs_and_reads_label_and_pipeline() {
        let repos = [RepoName::new("o", "a"), RepoName::new("o", "gone")];
        let data = json!({
            "rateLimit": { "cost": 1, "limit": 5000, "remaining": 4990, "resetAt": "2100-01-01T00:00:00Z" },
            "viewer": { "login": "me" },
            "r0": { "pullRequests": { "nodes": [
                {
                    "number": 7, "title": "Mine, watched", "url": "https://github.com/o/a/pull/7",
                    "isDraft": false, "headRefOid": "abc", "baseRefName": "main",
                    "author": { "login": "me" },
                    "labels": { "nodes": [{ "name": "bug" }, { "name": "slopwatch" }] },
                    "baseRef": { "target": { "file": { "oid": "def" } } },
                },
                {
                    "number": 8, "title": "Someone else's", "url": "https://github.com/o/a/pull/8",
                    "isDraft": false, "headRefOid": "abd", "baseRefName": "main",
                    "author": { "login": "them" },
                    "labels": { "nodes": [] },
                    "baseRef": { "target": { "file": null } },
                },
                {
                    "number": 9, "title": "Mine, draft, base deleted", "url": "https://github.com/o/a/pull/9",
                    "isDraft": true, "headRefOid": "abe", "baseRefName": "old",
                    "author": { "login": "me" },
                    "labels": { "nodes": [] },
                    "baseRef": null,
                },
            ] } },
            "r1": null,
        });

        let poll = read_poll(&repos, data).unwrap();

        let rate = poll.rate.unwrap();
        assert_eq!((rate.cost, rate.limit, rate.remaining), (1, 5000, 4990));
        assert!(rate.resets_in > Duration::from_secs(3600));
        assert_eq!(
            poll.repos[1],
            RepoPoll {
                repo: repos[1].clone(),
                prs: None
            }
        );
        let prs = poll.repos[0].prs.as_ref().unwrap();
        assert_eq!(
            prs,
            &[
                OpenPr {
                    number: 7,
                    title: "Mine, watched".into(),
                    url: "https://github.com/o/a/pull/7".into(),
                    draft: false,
                    head_sha: "abc".into(),
                    base: "main".into(),
                    labeled: true,
                    base_has_pipeline: true,
                },
                OpenPr {
                    number: 9,
                    title: "Mine, draft, base deleted".into(),
                    url: "https://github.com/o/a/pull/9".into(),
                    draft: true,
                    head_sha: "abe".into(),
                    base: "old".into(),
                    labeled: false,
                    base_has_pipeline: false,
                },
            ]
        );
    }

    #[test]
    fn available_repos_are_the_ones_the_developer_can_push_to() {
        let data = json!({ "viewer": { "repositories": { "nodes": [
            { "nameWithOwner": "o/admin", "viewerPermission": "ADMIN" },
            { "nameWithOwner": "o/write", "viewerPermission": "WRITE" },
            { "nameWithOwner": "o/read", "viewerPermission": "READ" },
            { "nameWithOwner": "o/triage", "viewerPermission": "TRIAGE" },
        ] } } });

        let repos = read_available_repos(data).unwrap();

        assert_eq!(
            repos,
            [RepoName::new("o", "admin"), RepoName::new("o", "write")]
        );
    }

    #[test]
    fn utc_timestamps_parse_to_epoch_seconds() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_utc("2026-10-03T03:19:55Z"), Some(1_790_997_595));
        assert_eq!(parse_utc("not a time"), None);
    }

    #[test]
    fn a_rate_limited_graphql_answer_backs_off() {
        let answer =
            json!({ "errors": [{ "type": "RATE_LIMITED", "message": "API rate limit exceeded" }] });

        assert!(matches!(
            graphql_error(&answer),
            GitHubError::RateLimited { .. }
        ));
    }
}
