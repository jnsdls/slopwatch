//! GitHub's API over HTTPS: GraphQL for polling, REST for labels.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use slopwatch_protocol::RepoName;

use slopwatch_protocol::step::{Check, CheckState, Checks, ChecksState};

use super::{
    GitHub, GitHubError, GitRemote, OpenPr, PIPELINE_PATH, Poll, PrDetail, RateLimit, RepoPoll,
    WATCH_LABEL,
};
use crate::auth::Credentials;

const API: &str = "https://api.github.com";
const TIMEOUT: Duration = Duration::from_secs(30);
/// How long to back off when GitHub rate-limits without saying how long.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(60);
/// GraphQL's largest page.
const PAGE: usize = 100;
/// Pages of repos to list in the add-repo picker.
const MAX_REPO_PAGES: usize = 10;
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
        let mut repos = Vec::new();
        let mut after = Value::Null;
        for _ in 0..MAX_REPO_PAGES {
            let data = self
                .graphql(AVAILABLE_REPOS, json!({ "after": after }))
                .await?;
            let page = read_available_repos(data)?;
            repos.extend(page.repos);
            match page.next {
                Some(cursor) => after = cursor.into(),
                None => break,
            }
        }
        Ok(repos)
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
            // A label by that name already exists.
            Err(GitHubError::Unprocessable(_)) => Ok(()),
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

    async fn git_remote(&self, repo: &RepoName) -> Result<GitRemote, GitHubError> {
        let token = self.credentials.token().await?;
        // git reads config from these variables, so the token stays off
        // the command line and out of the clone's config file.
        Ok(GitRemote {
            url: format!("https://github.com/{repo}.git"),
            env: vec![
                ("GIT_CONFIG_COUNT".into(), "1".into()),
                (
                    "GIT_CONFIG_KEY_0".into(),
                    "http.https://github.com/.extraheader".into(),
                ),
                ("GIT_CONFIG_VALUE_0".into(), basic_auth(&token)),
            ],
        })
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
            Duration::from_secs(reset.saturating_sub(unix_now())).max(DEFAULT_RETRY_AFTER)
        })
    });
    let url = response.url().path().to_owned();
    let body = response.text().await.unwrap_or_default();
    match (status, retry_after) {
        (StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS, Some(retry_after)) => {
            Err(GitHubError::RateLimited { retry_after })
        }
        (StatusCode::TOO_MANY_REQUESTS, None) => Err(GitHubError::RateLimited {
            retry_after: DEFAULT_RETRY_AFTER,
        }),
        (StatusCode::UNAUTHORIZED, _) => Err(GitHubError::Auth(body)),
        (StatusCode::NOT_FOUND, _) => Err(GitHubError::NotFound(url)),
        (StatusCode::UNPROCESSABLE_ENTITY, _) => Err(GitHubError::Unprocessable(body)),
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
query($after: String) {
  viewer {
    repositories(
      first: 100
      after: $after
      affiliations: [OWNER, COLLABORATOR, ORGANIZATION_MEMBER]
      ownerAffiliations: [OWNER, COLLABORATOR, ORGANIZATION_MEMBER]
      isArchived: false
      orderBy: { field: PUSHED_AT, direction: DESC }
    ) {
      nodes { nameWithOwner viewerPermission }
      pageInfo { hasNextPage endCursor }
    }
  }
}";

struct RepoPage {
    repos: Vec<RepoName>,
    /// The cursor of the next page, if there is one.
    next: Option<String>,
}

fn read_available_repos(data: Value) -> Result<RepoPage, GitHubError> {
    #[derive(Deserialize)]
    struct Data {
        viewer: Viewer,
    }
    #[derive(Deserialize)]
    struct Viewer {
        repositories: Repositories,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Repositories {
        nodes: Vec<Repo>,
        page_info: PageInfo,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PageInfo {
        has_next_page: bool,
        end_cursor: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Repo {
        name_with_owner: String,
        viewer_permission: Option<String>,
    }

    let Data { viewer } = parse(data)?;
    let Repositories { nodes, page_info } = viewer.repositories;
    Ok(RepoPage {
        repos: nodes
            .into_iter()
            .filter(|repo| {
                matches!(
                    repo.viewer_permission.as_deref(),
                    Some("ADMIN" | "MAINTAIN" | "WRITE")
                )
            })
            .filter_map(|repo| repo.name_with_owner.parse().ok())
            .collect(),
        next: page_info
            .has_next_page
            .then_some(page_info.end_cursor)
            .flatten(),
    })
}

/// One query for every repo in `repos`. Each repo gets an alias, `r0`, `r1`
/// and so on, that says whether it still exists, and one search finds the
/// developer's open PRs across all of them.
fn poll_query(repos: &[RepoName]) -> (String, Value) {
    let mut parameters = vec!["$search: String!".to_owned()];
    let mut fields = String::new();
    let mut search = "is:pr is:open author:@me".to_owned();
    let mut variables = serde_json::Map::new();
    for (index, repo) in repos.iter().enumerate() {
        parameters.push(format!("$o{index}: String!, $n{index}: String!"));
        variables.insert(format!("o{index}"), repo.owner.clone().into());
        variables.insert(format!("n{index}"), repo.name.clone().into());
        fields.push_str(&format!(
            "  r{index}: repository(owner: $o{index}, name: $n{index}) {{ nameWithOwner }}\n"
        ));
        search.push_str(&format!(" repo:{repo}"));
    }
    variables.insert("search".to_owned(), search.into());
    let query = format!(
        "query({parameters}) {{
  rateLimit {{ cost limit remaining resetAt }}
{fields}  prs: search(query: $search, type: ISSUE, first: {PAGE}) {{
    issueCount
    nodes {{
      ... on PullRequest {{
        number title body url isDraft headRefOid baseRefName
        author {{ login }}
        repository {{ nameWithOwner }}
        labels(first: {PAGE}) {{ nodes {{ name }} }}
        baseRef {{ target {{ oid ... on Commit {{ file(path: \"{PIPELINE_PATH}\") {{ oid }} }} }} }}
        commits(last: 1) {{ nodes {{ commit {{ oid statusCheckRollup {{
          state
          contexts(first: {PAGE}) {{ nodes {{
            __typename
            ... on CheckRun {{ name status conclusion detailsUrl }}
            ... on StatusContext {{ context state targetUrl }}
          }} }}
        }} }} }} }}
      }}
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
    #[serde(rename_all = "camelCase")]
    struct Repo {
        name_with_owner: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Search {
        issue_count: usize,
        nodes: Vec<Pr>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Pr {
        number: u64,
        title: String,
        #[serde(default)]
        body: String,
        url: String,
        is_draft: bool,
        head_ref_oid: String,
        base_ref_name: String,
        author: Option<Author>,
        repository: Repo,
        labels: Nodes<Label>,
        base_ref: Option<BaseRef>,
        commits: Option<Nodes<CommitNode>>,
    }
    #[derive(Deserialize)]
    struct Author {
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
        #[serde(default)]
        oid: String,
        file: Option<Value>,
    }
    #[derive(Deserialize)]
    struct CommitNode {
        commit: Commit,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Commit {
        oid: String,
        status_check_rollup: Option<Rollup>,
    }

    let rate: Option<Rate> = parse(data["rateLimit"].take())?;
    let search: Search = parse(data["prs"].take())?;
    // Missing PRs would read as closed, so a page that can't hold them all
    // fails the poll instead.
    if search.issue_count > search.nodes.len() {
        return Err(GitHubError::Other(format!(
            "{} open PRs across {} repos is more than one poll reads",
            search.issue_count,
            repos.len()
        )));
    }

    let mut polled = Vec::with_capacity(repos.len());
    for (index, repo) in repos.iter().enumerate() {
        let found: Option<Repo> = parse(data[format!("r{index}")].take())?;
        let prs = found.map(|found| {
            search
                .nodes
                .iter()
                .filter(|pr| {
                    pr.repository
                        .name_with_owner
                        .eq_ignore_ascii_case(&found.name_with_owner)
                })
                .map(|pr| {
                    let labels: Vec<String> =
                        pr.labels.nodes.iter().map(|l| l.name.clone()).collect();
                    let base = pr.base_ref.as_ref().and_then(|base| base.target.as_ref());
                    // Checks count only on the head the PR reports.
                    let checks = pr
                        .commits
                        .as_ref()
                        .and_then(|commits| commits.nodes.last())
                        .filter(|node| node.commit.oid == pr.head_ref_oid)
                        .and_then(|node| node.commit.status_check_rollup.as_ref())
                        .map(Rollup::checks)
                        .unwrap_or_default();
                    OpenPr {
                        number: pr.number,
                        title: pr.title.clone(),
                        url: pr.url.clone(),
                        draft: pr.is_draft,
                        head_sha: pr.head_ref_oid.clone(),
                        base: pr.base_ref_name.clone(),
                        labeled: labels.iter().any(|label| label == WATCH_LABEL),
                        base_has_pipeline: base
                            .and_then(|target| target.file.as_ref())
                            .is_some_and(|file| !file.is_null()),
                        detail: PrDetail {
                            body: pr.body.clone(),
                            author: pr
                                .author
                                .as_ref()
                                .map(|author| author.login.clone())
                                .unwrap_or_default(),
                            labels,
                            base_sha: base.map(|target| target.oid.clone()).unwrap_or_default(),
                            checks,
                        },
                    }
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
            resets_in: parse_utc(&rate.reset_at)
                .map(|at| Duration::from_secs(at.saturating_sub(unix_now())))
                .unwrap_or(Duration::from_secs(3600)),
        }),
    })
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

/// A commit's `statusCheckRollup`.
#[derive(Deserialize)]
struct Rollup {
    state: String,
    contexts: Nodes<Context>,
}

/// One check run or commit status. Other node types read as `Other`.
#[derive(Deserialize)]
#[serde(tag = "__typename")]
#[allow(clippy::enum_variant_names)] // GitHub's type names
enum Context {
    #[serde(rename_all = "camelCase")]
    CheckRun {
        name: String,
        status: String,
        conclusion: Option<String>,
        details_url: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    StatusContext {
        context: String,
        state: String,
        target_url: Option<String>,
    },
    #[serde(other)]
    Other,
}

impl Rollup {
    fn checks(&self) -> Checks {
        Checks {
            state: match self.state.as_str() {
                "SUCCESS" => ChecksState::Success,
                "FAILURE" | "ERROR" => ChecksState::Failure,
                _ => ChecksState::Pending,
            },
            runs: self
                .contexts
                .nodes
                .iter()
                .filter_map(|context| match context {
                    Context::CheckRun {
                        name,
                        status,
                        conclusion,
                        details_url,
                    } => Some(Check {
                        name: name.clone(),
                        state: match (status.as_str(), conclusion.as_deref()) {
                            ("COMPLETED", Some("SUCCESS")) => CheckState::Success,
                            ("COMPLETED", Some("NEUTRAL")) => CheckState::Neutral,
                            ("COMPLETED", Some("SKIPPED")) => CheckState::Skipped,
                            ("COMPLETED", _) => CheckState::Failure,
                            _ => CheckState::Pending,
                        },
                        url: details_url.clone(),
                    }),
                    Context::StatusContext {
                        context,
                        state,
                        target_url,
                    } => Some(Check {
                        name: context.clone(),
                        state: match state.as_str() {
                            "SUCCESS" => CheckState::Success,
                            "FAILURE" | "ERROR" => CheckState::Failure,
                            _ => CheckState::Pending,
                        },
                        url: target_url.clone(),
                    }),
                    Context::Other => None,
                })
                .collect(),
        }
    }
}

/// `Authorization` for git over HTTPS with a GitHub token.
fn basic_auth(token: &str) -> String {
    use base64::Engine as _;
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
    format!("AUTHORIZATION: basic {encoded}")
}

fn parse<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, GitHubError> {
    serde_json::from_value(value)
        .map_err(|error| GitHubError::Other(format!("unexpected GraphQL answer: {error}")))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
    fn the_poll_query_checks_every_repo_and_searches_them_all_at_once() {
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
            json!({
                "search": "is:pr is:open author:@me repo:o/a repo:p/b",
                "o0": "o", "n0": "a", "o1": "p", "n1": "b",
            })
        );
    }

    fn pr(repo: &str, number: u64, labels: &[&str], base: Value) -> Value {
        json!({
            "number": number, "title": format!("PR {number}"),
            "url": format!("https://github.com/{repo}/pull/{number}"),
            "isDraft": number == 9, "headRefOid": "abc", "baseRefName": "main",
            "repository": { "nameWithOwner": repo },
            "labels": { "nodes": labels.iter().map(|name| json!({ "name": name })).collect::<Vec<_>>() },
            "baseRef": base,
        })
    }

    #[test]
    fn a_poll_sorts_prs_into_their_repos_and_reads_label_and_pipeline() {
        let repos = [
            RepoName::new("o", "a"),
            RepoName::new("o", "b"),
            RepoName::new("o", "gone"),
        ];
        let with_pipeline = json!({ "target": { "file": { "oid": "def" } } });
        let without = json!({ "target": { "file": null } });
        let data = json!({
            "rateLimit": { "cost": 1, "limit": 5000, "remaining": 4990, "resetAt": "2100-01-01T00:00:00Z" },
            "r0": { "nameWithOwner": "o/a" },
            "r1": { "nameWithOwner": "O/B" },
            "r2": null,
            "prs": { "issueCount": 3, "nodes": [
                pr("o/a", 7, &["bug", "slopwatch"], with_pipeline),
                pr("O/B", 8, &[], without),
                pr("o/a", 9, &[], Value::Null),
            ] },
        });

        let poll = read_poll(&repos, data).unwrap();

        let rate = poll.rate.unwrap();
        assert_eq!((rate.cost, rate.limit, rate.remaining), (1, 5000, 4990));
        assert!(rate.resets_in > Duration::from_secs(3600));
        assert_eq!(poll.repos[2].prs, None);
        let summary = |index: usize| -> Vec<(u64, bool, bool, bool)> {
            poll.repos[index]
                .prs
                .as_ref()
                .unwrap()
                .iter()
                .map(|pr| (pr.number, pr.labeled, pr.base_has_pipeline, pr.draft))
                .collect()
        };
        assert_eq!(
            summary(0),
            [(7, true, true, false), (9, false, false, true)]
        );
        assert_eq!(summary(1), [(8, false, false, false)]);
    }

    #[test]
    fn a_poll_with_more_prs_than_one_page_fails_rather_than_dropping_some() {
        let repos = [RepoName::new("o", "a")];
        let data = json!({
            "rateLimit": null,
            "r0": { "nameWithOwner": "o/a" },
            "prs": { "issueCount": 101, "nodes": [] },
        });

        assert!(read_poll(&repos, data).is_err());
    }

    #[test]
    fn available_repos_are_the_ones_the_developer_can_push_to() {
        let data = json!({ "viewer": { "repositories": {
            "nodes": [
                { "nameWithOwner": "o/admin", "viewerPermission": "ADMIN" },
                { "nameWithOwner": "o/write", "viewerPermission": "WRITE" },
                { "nameWithOwner": "o/read", "viewerPermission": "READ" },
                { "nameWithOwner": "o/triage", "viewerPermission": "TRIAGE" },
            ],
            "pageInfo": { "hasNextPage": true, "endCursor": "abc" },
        } } });

        let page = read_available_repos(data).unwrap();

        assert_eq!(
            page.repos,
            [RepoName::new("o", "admin"), RepoName::new("o", "write")]
        );
        assert_eq!(page.next.as_deref(), Some("abc"));
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
