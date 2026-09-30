# Watching GitHub PRs without webhooks

Research for [#5](https://github.com/jnsdls/slopwatch/issues/5), map [#1](https://github.com/jnsdls/slopwatch/issues/1). Checked 2026-09-30 against docs.github.com, the live GraphQL schema, the `gh` manual and `cli/cli` source at `fc4b137`. Timings and costs in the "measured" notes come from real calls made with my `gh` token.

## Answer

Polling is cheap if the daemon uses GraphQL for state and keeps queries small.

- One GraphQL query can return every signal slopwatch needs for 20 PRs (checks, statuses, reviews, review threads, labels, mergeability, auto-merge, merge queue). Measured cost is 1 point. At one sweep every 30s that is 120 points an hour out of 5,000.
- REST alone cannot cover the signals. Review thread resolution and merge queue entries are GraphQL-only. REST also needs about 4 calls per PR per poll, which blows through 5,000/hour at 30s for 20 PRs unless ETags turn most of them into free `304`s.
- GraphQL has no conditional requests. Every sweep costs points, but the cost is small enough not to matter.
- The daemon can reuse the user's `gh` token by running `gh auth token`. The catch is that the budget is per user, so the daemon shares 5,000 GraphQL points and 5,000 REST requests an hour with every other `gh` call the user and their agents make.
- Query latency is the real constraint, not cost. A sweep over 20 PRs in 5 large public repos took 5 to 8 seconds and a heavier variant hit GraphQL's 10 second timeout. Per-repo queries took about 1.2s.

## Rate limits

### Primary

| Pool | Limit for a user token | Source |
|---|---|---|
| REST (`x-ratelimit-resource: core`) | 5,000 requests/hour | [REST rate limits](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api) |
| GraphQL (`x-ratelimit-resource: graphql`) | 5,000 points/hour | [GraphQL limits](https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api) |

The two pools are separate: "The GraphQL API also has a separate primary rate limit." Enterprise Cloud raises REST to 15,000 and GraphQL to 10,000.

The budget belongs to the user, not the token. The REST doc says PATs, OAuth apps and GitHub Apps acting on the user's behalf "all count towards your personal rate limit of 5,000 requests per hour." A GitHub App installation token gets its own budget (at least 5,000/hour), which matters later for teams.

### GraphQL point cost

From the [GraphQL limits page](https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api): add up the requests each connection could need assuming the maximum `first`/`last`, divide by 100, round. Minimum 1. `first`/`last` must be 1 to 100 and a query may touch at most 500,000 nodes. Queries running over 10 seconds are killed with a `502` or `504`.

The `rateLimit { cost remaining resetAt nodeCount }` field reports cost per query, and `rateLimit(dryRun: true)` reports cost without spending points.

### Secondary

From the [REST rate limits page](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api):

- 100 concurrent requests, shared across REST and GraphQL.
- 900 points/minute per REST endpoint, 2,000 points/minute for GraphQL. A GET or a GraphQL query costs 1 point, a write or mutation costs 5.
- 90 seconds of CPU time per 60 seconds of real time.
- 80 content-creating requests per minute.

GitHub's [best practices](https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api) say to make requests serially, wait a second between writes, and on a secondary limit wait for `retry-after` or at least a minute. `GET /rate_limit` does not count against the primary limit.

## Conditional requests

REST supports `ETag`/`If-None-Match` and `Last-Modified`/`If-Modified-Since`. From [best practices](https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api): "Making a conditional request does not count against your primary rate limit if a `304` response is returned and the request was made while correctly authorized with an `Authorization` header." The same page asks clients to honor `x-poll-interval`.

Measured: `GET /repos/cli/cli/pulls` returned a weak ETag. Replaying it twice returned `304` and `x-ratelimit-remaining` stayed at 4980. It still counts toward secondary limits.

Two catches from the measured response:

- It carried `Cache-Control: private, max-age=60`. A client with an HTTP cache would serve 60-second-old data. The daemon should manage ETags itself and bypass any cache.
- GraphQL responses carry no `ETag`, so there is no free "nothing changed" check there.

The [Events API](https://docs.github.com/en/rest/activity/events) (`GET /repos/{owner}/{repo}/events`) supports ETags and `x-poll-interval`, but GitHub says it "is not built to serve real-time use cases" with latency "anywhere from 30s to 6h." It is not usable as a change feed.

## Where each signal lives

GraphQL field names below come from the live schema (introspection plus [schema.docs.graphql](https://docs.github.com/public/fpt/schema.docs.graphql)). None is deprecated or behind a preview header.

| Signal | GraphQL (`PullRequest`) | REST |
|---|---|---|
| Head SHA, draft, state | `headRefOid`, `isDraft`, `state`, `updatedAt` | `GET /repos/{o}/{r}/pulls/{n}` |
| Labels | `labels(first:)` | in the pull response |
| Check runs and commit statuses | `commits(last:1) { nodes { commit { statusCheckRollup { state contexts(first:) } } } }`. `contexts` is a union of `CheckRun` and `StatusContext` and also returns `checkRunCountsByState` and `statusContextCountsByState` | Two calls. [`GET /repos/{o}/{r}/commits/{ref}/check-runs`](https://docs.github.com/en/rest/checks/runs) and `GET /repos/{o}/{r}/commits/{ref}/status` |
| Required vs optional checks | `isRequired(pullRequestId:)` on `CheckRun` and `StatusContext` | not on these endpoints |
| Reviews | `reviewDecision` (`APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED`), `latestOpinionatedReviews`, `reviewRequests` | `GET /repos/{o}/{r}/pulls/{n}/reviews`, no rolled-up decision |
| Review threads | `reviewThreads(first:) { isResolved isOutdated path comments }` | none. `pulls/{n}/comments` has no resolved state |
| Mergeability | `mergeable` (`MERGEABLE`, `CONFLICTING`, `UNKNOWN`), `mergeStateStatus` (`BEHIND`, `BLOCKED`, `CLEAN`, `DIRTY`, `DRAFT`, `HAS_HOOKS`, `UNKNOWN`, `UNSTABLE`) | `mergeable`, `mergeable_state` on the single-PR endpoint only. The list endpoint omits them |
| Auto-merge | `autoMergeRequest { enabledAt enabledBy mergeMethod }`, `viewerCanEnableAutoMerge` | `auto_merge` on the pull |
| Merge queue | `isMergeQueueEnabled`, `isInMergeQueue`, `mergeQueueEntry { state position estimatedTimeToMerge headCommit }` with state `QUEUED`, `AWAITING_CHECKS`, `MERGEABLE`, `UNMERGEABLE`, `LOCKED` | no endpoint found in the REST reference |
| Stacks (public preview) | `stack`, `stackEntry` | `stack` object on pulls, plus a Stacks API ([docs](https://docs.github.com/en/pull-requests/reference/stacked-pull-requests-apis-and-webhooks)) |

Notes that affect the design:

- **Mergeability is lazy.** REST says a `null` `mergeable` means "GitHub has started a background job to compute the mergeability" ([pulls docs](https://docs.github.com/en/rest/pulls/pulls)). GraphQL reports `UNKNOWN`. After a push the first poll often sees unknown, and the next poll has the answer. The Gate has to treat unknown as "not yet", not as a failure.
- **Merge queue CI runs on a different commit.** GitHub builds a `merge_group` commit on a `gh-readonly-queue/{base_branch}` branch ([managing a merge queue](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue)). The PR head's `statusCheckRollup` does not show those checks. Read them from `mergeQueueEntry.headCommit.statusCheckRollup`.
- **The REST check-runs list caps at the 1,000 most recent check suites per ref.** That is irrelevant at our scale.
- **Finding Watched PRs.** Two options. `search(type: ISSUE, query: "is:pr is:open author:@me label:<opt-in>")` is one query across all repos. `repository { pullRequests(states: OPEN, labels: [...]) }` per repo with author filtered client-side reads the database directly. Search goes through the search index. I did not find a documented lag figure, so per-repo listing is the safer source of truth.

## Request budget: 5 repos, 20 Watched PRs

### GraphQL sweep (recommended)

Measured with `rateLimit`:

| Query | Cost | nodeCount | Wall time |
|---|---|---|---|
| `search` for 20 open PRs, all signals, 50 contexts each | 1 | 2,840 | not timed |
| 5 aliased repos x 4 PRs, all signals, 100 contexts, 100 threads | 1 | 4,840 | 6.4s to 7.6s |
| Same fields, 5 aliased repos x 20 PRs | 5 (dry run) | 24,200 | timed out after about 10.8s |
| 5 aliased repos x 4 PRs, light fields (rollup state only, no contexts) | 1 | 2,440 | 4.7s to 7.3s |
| 1 repo x 4 PRs, light fields | 1 | 488 | 1.2s |

These repos (cli/cli, rust-lang/rust, zed, next.js, react) have far more checks per PR than typical, so these are worst-case timings.

Budget, assuming one light sweep per repo per tick, plus a detail query for each PR whose fingerprint (head SHA, rollup state, thread count, labels, mergeability) changed:

| Interval | Sweeps/hour (5 repos) | Detail queries/hour (guess: 20 PRs, each changing every 3 min) | Points/hour | Share of 5,000 |
|---|---|---|---|---|
| 60s | 300 | 400 | about 700 | 14% |
| 30s | 600 | 400 | about 1,000 | 20% |
| 30s, single combined sweep | 120 | 400 | about 520 | 10% |

Secondary limits are nowhere near: about 20 points/minute against 2,000.

### REST only (for comparison)

Per PR per tick: pull, check-runs, combined status, reviews. That is 4 requests x 20 PRs = 80 per tick, plus 5 list calls.

| Interval | Requests/hour, no ETags | Share of 5,000 |
|---|---|---|
| 60s | 5,100 | 102% |
| 30s | 10,200 | 204% |

With ETags most of these return `304` and cost nothing, so steady-state spend is roughly the count of resources that actually changed. Check runs change on every CI step, so a PR under active CI costs about 1 request per poll per endpoint. Secondary load is 170/minute at 30s, fine against 900 per endpoint. REST still needs GraphQL for review threads and merge queue state, so it does not remove GraphQL. It only moves spend into a second pool.

A hybrid is possible. Use free REST `304`s as a change detector and spend GraphQL points only on PRs that changed. It is more code, and the GraphQL-only budget above already leaves 80% headroom.

## Reusing the `gh` token

- [`gh auth token`](https://cli.github.com/manual/gh_auth_token) prints the token for the active account (`--hostname`, `--user` to choose).
- Resolution order in `cli/cli` [`internal/config/config.go`](https://github.com/cli/cli/blob/fc4b137cdef0a6bd28fd461b7cf9c84a5812a8cd/internal/config/config.go#L257-L280) (`ActiveToken`): environment (`GH_TOKEN`, `GITHUB_TOKEN`), then plain-text config (`hosts.yml` `oauth_token`), then the keyring. The keyring service name is `gh:<hostname>`, for example `gh:github.com` ([line 577](https://github.com/cli/cli/blob/fc4b137cdef0a6bd28fd461b7cf9c84a5812a8cd/internal/config/config.go#L577-L579)).
- [`gh auth login`](https://cli.github.com/manual/gh_auth_login) stores the token "in the system credential store" and falls back to a plain-text file if that fails. Default scopes are `repo`, `read:org`, `gist`. On this machine `gh auth status` shows a `gho_` OAuth token from the keyring with `repo`, `read:org`, `gist`, `workflow`, `admin:public_key`. `repo` covers reading checks, statuses and PRs on private repos ([check runs docs](https://docs.github.com/en/rest/checks/runs)).

Recommendation: shell out to `gh auth token` instead of reading the Keychain item. It honors the user's env overrides and multi-account setup, and it is the documented interface. Reading `gh:github.com` directly from another binary will likely trigger a macOS Keychain access prompt, since `gh` created the item. I did not test that. Re-run `gh auth token` on a `401`, because `gh auth login` or `gh auth switch` replaces the token.

The shared budget is the real risk. Claude Code, Codex and the user's own `gh` calls all spend the same 5,000 points. The daemon should read `x-ratelimit-remaining` on every response (or `rateLimit` in the query), slow its interval as the budget drains, and surface that state in the UI. A GitHub App installation token would give slopwatch its own budget. That fits the later team story but needs an app install per org.

## Open questions

- Who owns the token when the daemon is hosted? The `gh` token only exists on the user's machine.
- Search index lag for the Watched PR query is undocumented. Worth measuring if we pick `search`.
- Stacked PRs are in public preview. They add `stack` fields in both APIs and a new async merge endpoint that the Merge Step would need for stacked PRs.
