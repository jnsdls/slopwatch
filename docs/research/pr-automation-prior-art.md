# PR automation prior art: merge conditions, fix loops, noise filtering

Researched 2026-09-30 against vendor docs and repos. Every claim links to the page it came from. Where I read a vendor's docs source (Mergify's `Mergifyio/docs` repo, GitHub's `github/docs` repo, `qodo-ai/pr-agent`, `chdsbd/kodiak`, `renovatebot/renovate`), the link points at the published page built from that source.

## Summary

- Nobody else has a Gate that mixes LLM verdicts with CI and human approval in one expression. Mergify comes closest on syntax (a condition list with implicit AND plus `or`/`and`/`not` blocks, `#` counts, `-` negation). Aviator Verify comes closest on semantics: per-criterion LLM verdicts, typed waivers, and one required GitHub check that flips when a waiver lands.
- Every tool that runs a fix loop caps it with a small hard number. CodeRabbit Auto-fix defaults to 3 repair rounds (max 10). Aviator Runbooks defaults to 2 CI rework attempts. Mergify `max_checks_retries` defaults to 0. Aviator retries a flaky check at most once. Copilot cloud agent has a 59-minute session ceiling. None of them loop until green.
- The tools most careful about agents say outright that "unknown" never authorizes an action. CodeRabbit Triage: "Missing, incomplete, or stale evidence stays unknown; it cannot authorize an action." Aviator flaky handling: "Uncertainty changes nothing." Aviator Verify treats a verifier that "couldn't confirm it should pass" as `fail`.
- Two vendors stop the fixer from touching the files that define the gate. CodeRabbit Fix CI will not commit CI config, lock files, or `.github/workflows/`. Copilot cloud agent PRs cannot run Actions workflows until a human with write access approves.
- Aviator Runbooks matches slopwatch's Run rule almost exactly. It stops watching CI when a commit it did not push lands, and it terminates the rework workflow when a new commit arrives mid-validation.
- Noise control is mostly prompt-side (path instructions, exclusions, learnings, profiles). Only PR-Agent documents a numeric score threshold, and only Graphite documents per-rule acceptance metrics you can use to prune rules.

## Mergify

### Merge conditions

Condition grammar, from [docs.mergify.com/configuration/conditions](https://docs.mergify.com/configuration/conditions/):

> `[ "-" ] [ "#" ] <attribute> [ <operator> <value> ]`

`-` negates and `#` takes the length of a list attribute, so `#approved-reviews-by >= 2` counts approvals. Operators are `=`/`:`, `!=`/`≠`, `~=` (regex), `*=` (glob), `>=`, `>`, `<=`, `<`. On list attributes, comparisons mean "any element matches", and `!=` means "no element matches" ([same page](https://docs.mergify.com/configuration/conditions/)).

Lines in a list are implicitly ANDed. `or`, `and`, and `not` group blocks ([same page](https://docs.mergify.com/configuration/conditions/)):

```yaml
success_conditions:
  - or:
    - "#approved-reviews-by >= 2"
    - "check-success = test job"
```

```yaml
success_conditions:
  - not:
      and:
        - "#approved-reviews-by >= 2"
        - "check-success = test job"
```

Check attributes are `check-success`, `check-failure`, `check-neutral`, `check-skipped`, `check-cancelled`, `check-timed-out`, `check-pending`, `check-stale`. Mergify also has a qualified form `@<github-app-slug>/<check-name>` because "Two GitHub Apps can publish a check with the same name ... `check-success = pep8` matches whichever app reported success" ([same page](https://docs.mergify.com/configuration/conditions/)). One legacy wart: "Cancelled checks are also matched by `check-failure` for backward compatibility", so a genuine-failure rule needs `- -check-cancelled = test` next to it.

Mergify separates activation from requirement in two places.

- Queue rules have `queue_conditions` (to enter the queue) and `merge_conditions` (to merge once at the front). The docs example queues on a label but still requires CI before merge ([docs.mergify.com/merge-queue/rules](https://docs.mergify.com/merge-queue/rules/)):

  ```yaml
  queue_rules:
    - name: urgent
      queue_conditions:
        - label = urgent
      merge_conditions:
        - check-success = myci
  ```

- Merge Protections rules have `if` and `success_conditions`. "If all `if` conditions are true, the rule becomes **active**", and "Inactive rules are ignored (not shown as failing)." A failing active rule fails the single `Mergify Merge Protections` check ([docs.mergify.com/merge-protections/custom-rules](https://docs.mergify.com/merge-protections/custom-rules/)):

  ```yaml
  name: Auth Security Review
  if:
    - files ~= ^auth/
  success_conditions:
    - label = security-reviewed
  ```

Mergify reads GitHub rulesets and branch protection itself and "injects the matching condition into the rules it evaluates", showing them in the check summary. Users cannot write those derived attributes (except `github-review-decision`) in their own conditions ([conditions page](https://docs.mergify.com/configuration/conditions/)).

### Fix loop (retries, not fixes)

Mergify does not write code. Its loop is retry and requeue, all documented on [docs.mergify.com/merge-queue/lifecycle](https://docs.mergify.com/merge-queue/lifecycle/):

- `max_checks_retries` "defaults to `0`, which disables retries". When set, "On each retry, Mergify recreates the draft pull request to trigger a fresh CI run" and the status shows "attempt 2/3".
- `checks_timeout` defaults to `auto`, computed from "the 95th-percentile duration of successful runs with a safety margin" over seven days, excluding retries and bisections. Until about twenty qualifying runs exist, `auto` "applies no timeout". A timeout dequeues with reason `checks-timeout` and lists checks that never reported, because "A check that never reports can never satisfy its merge condition, so that second list usually means a check name in your `merge_conditions` doesn't match what your CI publishes."
- A cancelled check dequeues as `checks-interrupted`, not `checks-failed`: "Nothing about the pull request needs fixing". "When a check fails and another is interrupted at the same time, the failure wins."
- Losing `queue_conditions` (for example a removed label) ejects the PR. A PR removed by `dequeue` "will not rejoin the queue automatically". Requeue is `@mergifyio queue`, which "will reset its status to a neutral state".

Batch failures split the batch into `max_parallel_checks` parts (minimum two) until a single-PR batch fails, which "is deemed to be the culprit". `batch_max_failure_resolution_attempts` bounds this ([docs.mergify.com/merge-queue/batches](https://docs.mergify.com/merge-queue/batches/)). I did not find its default value.

Flaky handling has two mechanisms. `skip_intermediate_results: true` treats an earlier failing batch as transient if a later passing batch contains the same PR, on the reasoning that "a real bug would also fail the larger batch that contains the same code" ([batches page](https://docs.mergify.com/merge-queue/batches/)). CI Insights Auto-Retry reruns GitHub Actions workflows on rule match, 1 to 10 retries per rule, and skips jobs on merge queue batch PRs ([docs.mergify.com/ci-insights/auto-retry](https://docs.mergify.com/ci-insights/auto-retry/)).

### Noise filtering

Not applicable. Mergify has no LLM findings. Its debugging aid is the check summary, which "shows which rules were evaluated and whether each condition was met" ([conditions page](https://docs.mergify.com/configuration/conditions/)).

## GitHub rulesets and merge queue

### Merge conditions

Rulesets are checkboxes, not expressions ([available rules for rulesets](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/available-rules-for-rulesets)). The pull request rule has these options:

- Required approval count.
- "dismiss stale pull request approvals when commits are pushed that affect the diff".
- Code owner review.
- "require an approval from someone other than the last person to push to a branch".
- Require conversation resolution.
- Allowed merge methods.
- Required reviewers per file pattern. Up to 15 teams, 0 to 10 approvals each, `.gitignore`-style patterns with `!` negation.
- "Require an additional approval for unattributed Copilot pull requests", on by default and in public preview. "When Copilot opens a pull request that isn't attributed to a person, the ruleset requires one more approval than the number you configured."

Required status checks can be pinned to a source GitHub App. They come in "strict" (branch must be up to date) and "loose" forms ([same page](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/available-rules-for-rulesets)). Code scanning and code quality rules block when a result at the configured severity exists, when analysis is still running, or when a required tool is not configured.

Skip semantics are inconsistent, and they bite. "A job that is skipped will report its status as 'Success'. It will not prevent a pull request from merging, even if it is a required check." But a workflow skipped by path or branch filtering leaves its checks "in a 'Pending' state", which blocks merging ([troubleshooting required status checks](https://docs.github.com/en/pull-requests/how-tos/merge-and-close-pull-requests/troubleshooting-required-status-checks)).

Merge queue settings ([managing a merge queue](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue)):

- Merge method.
- Build concurrency (1 to 100 `merge_group` webhooks).
- Min and max group size, plus a wait time for the minimum.
- "Only merge non-failing pull requests". The rulesets page calls this "Require all queue entries to pass required checks".
- Status check timeout, after which "checks that have not reported a conclusion will be assumed to have failed".

CI must trigger on `merge_group`, or on `gh-readonly-queue/{base_branch}` branches for third-party CI.

### Fix loop (removal, not fixes)

On failure, "the merge queue automatically removes pull request #1 from the merge queue" and rebuilds the groups behind it. The documented removal reasons are CI failure, timeout, a removal request, and "Branch protection failure that could not automatically be resolved" ([managing a merge queue](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue)). There is no built-in retry. The only flake mitigation is turning off "Only merge non-failing pull requests", which lets a failed PR ride along if the last PR in the group passes.

## GitHub Copilot: code review, Autofix, cloud agent

### Merge conditions

Copilot code review does not gate by default. "By default, Copilot's reviews do not count toward required approvals for the pull request." With Copilot approvals enabled at repository, organization, and enterprise level (public preview), "Copilot can submit an approving review that satisfies your repository's required-approval rule the same way a teammate's approval would. If new commits are pushed after Copilot approves, the approval is dismissed" ([about Copilot code review](https://docs.github.com/en/copilot/concepts/agents/code-review)).

The customization tutorial lists "Block a PR from merging unless all Copilot comments are addressed" as an unsupported instruction ([customize code review](https://docs.github.com/en/copilot/tutorials/customize-code-review)).

### Fix loop

- **Code review re-runs.** Without "Review new pushes", Copilot "will only review a pull request once". Review effort is Lite (default) or Balanced ([about Copilot code review](https://docs.github.com/en/copilot/concepts/agents/code-review)).
- **Cloud agent.** You trigger it with `@copilot` on a PR, or by batching review comments with "Add to batch". It "can only work on one branch at a time". "Each ... session has a maximum execution time of 59 minutes. This is a hard limit" ([about cloud agent](https://docs.github.com/en/copilot/concepts/agents/cloud-agent/about-cloud-agent)). No per-PR iteration count is documented. The agent "only responds to mentions in open pull requests" ([troubleshoot cloud agent](https://docs.github.com/en/copilot/how-tos/use-copilot-agents/cloud-agent/troubleshoot-cloud-agent)).
- **Cloud agent safeguards.** "The cloud agent only responds to interactions from users with repository write access. Actions workflows triggered by pull requests raised by the agent require approval from a user with write access before they will run." It "can only push to a single branch" and "cannot push directly to your default branch" ([responsible use, agents](https://docs.github.com/en/copilot/responsible-use/agents)).
- **Autofix for code scanning.** Non-agentic Autofix "generates a single suggested fix for an alert, which you review and apply yourself". Agentic autofix (public preview) "generates a fix, validates it (for example, by re-running CodeQL), and iterates until it opens a pull request". It "works on a best-effort basis" ([autofix for code scanning](https://docs.github.com/en/code-security/concepts/code-scanning/autofix-for-code-scanning)). I found no documented iteration cap for agentic autofix.

### Noise filtering

- Instructions live in `.github/copilot-instructions.md`, path-scoped `.github/instructions/**/*.instructions.md` with `applyTo`, and `AGENTS.md` ([about Copilot code review](https://docs.github.com/en/copilot/concepts/agents/code-review)). `excludeAgent: "code-review"` hides a file from the reviewer ([add repository instructions](https://docs.github.com/en/copilot/how-tos/copilot-on-github/customize-copilot/add-custom-instructions/add-repository-instructions)).
- Instructions may be ignored when the file is "too long (over 1,000 lines)". Unsupported instructions include formatting changes, overview-comment changes, following external links, and "Vague quality improvements" such as "Be more accurate" ([customize code review](https://docs.github.com/en/copilot/tutorials/customize-code-review)).
- Excluded files are dependency manifests and lockfiles, logs, and SVGs ([about Copilot code review](https://docs.github.com/en/copilot/concepts/agents/code-review)).
- Autofix suppresses its own bad output: "If no suggestion is available, or if a suggested fix fails internal testing, no suggestion is displayed." It also says "You must always review suggestions from Copilot Autofix and edit changes as needed before accepting them" ([security and quality AI features](https://docs.github.com/en/code-security/responsible-use/security-and-quality-ai-features)).
- Code scanning alerts close as `false-positive`, `used-in-tests`, `wont-fix`, or `fixed` ([overview dashboard filters](https://docs.github.com/en/code-security/reference/security-at-scale/overview-dashboard-filters)).
- Confidence thresholds exist only for cloud agent issue triage. The agent rates each change high, medium, or low, and a repository "automation level" (Full control, Cautious (default), Balanced, Full automation) holds anything below the threshold as a suggestion. GitHub adds: "Approvals are a workflow convenience, not a security control" ([rationale, confidence, and approvals](https://docs.github.com/en/copilot/concepts/agents/cloud-agent/about-automation-rationale-and-approvals)). This applies to issue fields only, not to PRs or pushes.

## CodeRabbit

CodeRabbit has no public docs repo that I could find. I read the published pages as Markdown from docs.coderabbit.ai.

### Merge conditions

Pre-Merge Checks are LLM pass/fail judgements, the nearest analogue to a Jev Step ([pre-merge checks](https://docs.coderabbit.ai/pr-reviews/pre-merge-checks)).

- There are four built-ins (docstring coverage, title, description, issue assessment) plus custom checks written in natural language. Custom instructions are limited to 1000 characters.
- Each check has a mode: `off`, `warning` (default), or `error`. `error` blocks merge only "When paired with Request Changes Workflow".
- The results are Error, Warning, Passed, and "❓ Inconclusive: incomplete instructions, insufficient information, or analysis that could not be completed; does not block the pull request".
- Docs advice: "Start new checks in `warning` mode to gather feedback, then move to `error` mode".
- Override is the "Ignore failed checks" checkbox or `@coderabbitai ignore pre-merge checks`. It is per PR, and rows get tagged `[IGNORED]`. With `override_requested_reviewers_only: true`, "The pull request author cannot override the checks, and CodeRabbit records who performed the override".

```yaml
reviews:
  pre_merge_checks:
    docstrings:
      mode: "error"
      threshold: 85
    custom_checks:
      - name: "Undocumented Breaking Changes"
        mode: "warning"
        instructions: "Pass/fail criteria: ..."
```

`request_changes_workflow` means: "Automatically approve when CodeRabbit's comments are resolved, the latest commit has been reviewed, and no pre-merge checks are failing" ([configuration reference](https://docs.coderabbit.ai/reference/configuration)).

Triage rules (Team plan, GitHub Cloud) are a second condition system for automated merge, close, and auto-fix ([triage rules](https://docs.coderabbit.ai/triage/rules)).

- "Any trigger can match; conditions joined by **and** within a trigger must all match". That makes the whole rule an OR of ANDs.
- Properties include risk, blast radius, review effort, security risk, review state, and hours since last human activity.
- The merge action "merges only after the required checks and reviews pass and GitHub permits the merge. Low risk and low effort do not override branch protection". With a GitHub merge queue it enqueues instead.
- "Conditions use current evidence about the pull request. Missing, incomplete, or stale evidence stays unknown; it cannot authorize an action. For example, pushing a new commit can make a risk condition wait for an updated review."
- "Editing creates a new version and cancels pending actions from the previous version."

The Change Stack view separates "Merge readiness" (an LLM score band with high, medium, or low confidence) from mergeability. "A pull request can be mergeable and not ready, or ready and not mergeable" ([findings](https://docs.coderabbit.ai/change-stack/findings)).

### Fix loop

- **Autofix.** `@coderabbitai autofix` pushes to the branch. `@coderabbitai autofix stacked pr` opens a stacked PR. It "only processes unresolved CodeRabbit review threads with valid fix instructions", which come from each thread's "Prompt for AI Agents" block, and it no-ops if there are none. "When the pull request has merge conflicts, Autofix exits without making changes." It runs a build verification step, but "Even if verification fails, the generated changes are still delivered" ([autofix](https://docs.coderabbit.ai/finishing-touches/autofix)).
- **Fix CI.** `@coderabbitai fix-ci` or `fix-ci commit`. The agent "focuses on the code under test: infrastructure and configuration files (CI and build config, lock files, and other dependency manifests) are left out of the change and surfaced separately", and "CodeRabbit cannot commit changes under `.github/workflows/`". "Your project's real CI runs against the resulting commit ... That run is the source of truth." A retry on a CodeRabbit stacked PR commits to that PR "instead of creating an endlessly growing stack" ([fix CI](https://docs.coderabbit.ai/finishing-touches/fix-ci)).
- **Auto-fix PRs Triage rule ("Bring PR to ready").** The settings table gives "Maximum repair rounds" a default of 3 and a range of 1 to 10. It waits until the PR has been inactive for N hours (default 4), and bot activity resets that clock too. "It does not merge the pull request or replace required human approval." "The agent replies to human feedback and leaves human threads open for review." "A run can finish ready, awaiting human review, with nothing actionable, or blocked" ([triage rules](https://docs.coderabbit.ai/triage/rules)).
- **Review re-runs.** `auto_incremental_review` re-reviews on each push. `auto_pause_after_reviewed_commits` defaults to 5 ([configuration reference](https://docs.coderabbit.ai/reference/configuration)).

### Noise filtering

- `profile` is `quiet`, `chill` (default), or `assertive` ("more feedback (which may feel nitpicky)"). `path_filters` takes globs with `!` exclusions. `path_instructions` pairs a glob with instructions. There are also `ignore_title_keywords`, label filters, and `drafts: false` ([configuration reference](https://docs.coderabbit.ai/reference/configuration)).
- Learnings are created from chat replies. `learnings.approval_delay` is 0 to 30 days, default 0, meaning "apply learnings immediately without approval" ([configuration reference](https://docs.coderabbit.ai/reference/configuration), [learnings](https://docs.coderabbit.ai/knowledge-base/learnings)). The learnings page also advises: "For one-time exceptions ... resolve the comment without creating a learning."
- Findings carry four independent axes (type, severity, effort, and more): "They are four separate axes, not one severity ladder" ([findings](https://docs.coderabbit.ai/change-stack/findings)).

## Graphite

### Merge conditions

Graphite has no condition language of its own for merging. "Merge when ready" merges "after all branch protection rules have been met" ([merge when ready](https://graphite.com/docs/merge-when-ready)). The merge queue sits on GitHub branch protection or rulesets, with `graphite-app` on the bypass list ([set up merge queue](https://graphite.com/docs/set-up-merge-queue)). Automations are filter plus action (reviewers, labels, comments, Slack), not merge gates. "Rules match once per PR ... After it's matched once, it won't trigger again on that PR", and edits do not re-apply to PRs that already matched ([automations](https://graphite.com/docs/automations)).

### Fix loop

- **Merge queue.** A per-queue timeout caps "the amount of time a PR can stay at the head of the queue" ([set up merge queue](https://graphite.com/docs/set-up-merge-queue)). With Parallel CI, a failure evicts the PR and cancels the speculative runs stacked on it. The docs warn that "because parallel CI assumes that your CI tests in the merge queue will pass, be careful with flaky tests". Batch failure isolation uses full parallel isolation (default) or bisection ([merge queue optimizations](https://graphite.com/docs/merge-queue-optimizations)). I found no retry-on-flake setting.
- **Agents.** Cursor Cloud Agents. On a PR, "Any changes made by the agent are committed directly to the branch". This is available to PR authors only ([agents](https://graphite.com/docs/agents)). No loop or cap is documented.

### Noise filtering

The AI reviewer is now called "Graphite Agent". I found no current docs page for "Diamond".

- **Exclusions** are natural-language "situations where Graphite Agent should **not** leave comments". The docs warn that "If an exclusion is written too broadly, then Graphite Agent may not leave valid comments" ([AI review customization](https://graphite.com/docs/ai-review-customization)).
- **Custom rules.** Graphite advises against "Non-prescriptive verbs ('comment on' or 'flag')" and against mixing exclusions into rules.
- **`linguist-generated` files** are skipped.
- **PR-level filters** cover author, paths, labels, title and description, and parent branch.
- **Metrics.** Per rule you get issues found, accepted issues, acceptance rate, and upvote and downvote rates. Per exclusion you get "Issues caught" and "Percentage caught". The docs suggest you "Refine or remove rules with low acceptance rates" (same page).
- **Size limit.** PRs over 200,000 characters are "Not running" ([AI review comments](https://graphite.com/docs/ai-review-comments)).
- **Confidence.** I found no documented confidence threshold.

## Aviator (MergeQueue, Verify, Runbooks)

### Merge conditions

MergeQueue config lives in `.aviator/config.yml`, "only read once it is merged into the repository's default branch" ([merge rules](https://docs.aviator.co/mergequeue/configuration-file)). Required checks support per-check acceptable statuses ([customize required checks](https://docs.aviator.co/mergequeue/how-to-guides/customize-required-checks)):

```yaml
merge_rules:
  labels:
    trigger: mq
  preconditions:
    use_github_mergeability: false
    required_checks:
      - unit-test
      - "golang-*"
      - name: conditional_build
        acceptable_statuses:
          - success
          - missing
```

"a check is `pending` when it has been reported but has not finished yet, whereas a check is `missing` when it has not been reported at all". A wildcard "requires at least one matching check to be present". `require_all_checks_pass` "Requires at least one check to be present". Pre-queue `validations` apply regexes to the title and body ([pre-queue conditions](https://docs.aviator.co/mergequeue/how-to-guides/set-up-pre-queue-conditions)).

Verify is the closest match to slopwatch's Jev judgements. It checks intent plus acceptance criteria against the diff (code-scan) or a preview (runtime). Team-wide "invariants" are picked per change by "an LLM **selector**", gated by optional conditions such as `file_path_glob` ([how Verify works](https://docs.aviator.co/verify/how-it-works)). "All AI drafts wait in pending status. An admin promotes drafts to active before they start producing verdicts." Results ([understanding verification results](https://docs.aviator.co/verify/reference/understanding-verification-results)):

- The run status is `pending`, `in_progress`, `passed` ("Every criterion passed (or was waived)"), `failed`, `error` ("pipeline issue, not a code verdict"), or `deferred`.
- The criterion status is `pass`, `fail` ("or the verifier couldn't confirm it should pass"), `warn` (non-blocking), or `error` ("Treat as needing human review").
- The run records `commit_sha` and `criteria_skipped` / `criteria_waived` counts.
- One GitHub check, `aviator/verify`, mirrors the run and maps `error` to `failure`. "Waiving a verdict, or removing an acceptance criterion, recomputes the run's counts ... so the gate can flip to `success` without a new run."
- The waiver categories are `false_positive`, `doesnt_apply`, `accepted_risk`, and `fix_in_followup`. "Every waiver is recorded with the reviewer, the category, and a free-text reason."

### Fix loop

- **Automatic requeue.** `max_requeue_attempts: 3` makes PRs "automatically requeue before giving up. Only available in parallel mode" ([merge rules](https://docs.aviator.co/mergequeue/configuration-file)).
- **Flaky test management** (beta, parallel mode, off by default) runs a decision ladder ([flaky test management](https://docs.aviator.co/mergequeue/concepts/flaky-test-management)):
  1. Only opted-in `retriable_checks` are considered.
  2. "No log means no verdict".
  3. If the failure output "names a file the batch modifies", the failure counts as real. The docs note this "resolves a large share of real breakages without any model involved".
  4. A stable failure identity lets past verdicts be reused. The same failure across unrelated batches is treated as a repo-wide incident.
  5. The model runs last, with one question: "Would this check pass if it were run again on the same code?" and deliberately not "did this change cause it".
  6. "Each failure is retried at most once." "It never marks a check as passing." "Uncertainty changes nothing."
  7. Users can add per-check `context` or `context_file` describing flaky and genuine failure modes. Those files live in `.aviator/mergequeue/flake/` ([configure flaky test management](https://docs.aviator.co/mergequeue/how-to-guides/configure-flaky-test-management)).
- **Runbooks CI auto-rework.** It defaults to 2 attempts and posts a summary comment per attempt. "If latest head commit SHA is added manually (not by Runbooks), CI is not monitored." "If a new commit gets added in middle of the CI validation workflow, the CI rework workflow is terminated." It works "through only the first failed status check" ([handling CI failure](https://docs.aviator.co/runbooks/how-to-guides/handling-ci-failure)).
- **Verify runtime runner termination reasons.** These are `caps_exceeded` (tool calls, wall time, or cost), `loop_detected`, `stuck`, `give_up`, and `unhandled_error`. "If you're waiving the same invariant repeatedly across changes, the rule is wrong" ([fixing verification failures](https://docs.aviator.co/verify/how-to-guides/fixing-verification-failures)).

## Close analogues

- **Kodiak.** Merges only PRs "passing your GitHub branch protection rules". It is gated by `merge.automerge_label` (default `"automerge"`) and `merge.blocking_labels`. `merge.dont_wait_on_status_checks` skips checks "that run indefinitely, like deploy jobs" ([config reference](https://github.com/chdsbd/kodiak/blob/master/docs/docs/config-reference.md)).
- **Bors-ng.** Archived. Its README reads "If you want to implement a workflow like this, use GitHub's built-in merge queue" ([bors-ng](https://github.com/bors-ng/bors-ng)).
- **Trunk Merge Queue.** On failure a group enters "Pending Failure". It always waits for predecessors, and with `pending_failure_depth` > 0 it also waits for successors. With optimistic merging, a passing successor that includes the failed changes "is proof that those changes work" ([pending failure depth](https://docs.trunk.io/merge-queue/optimizations/pending-failure-depth)). Quarantine overrides the exit code for known-flaky tests, but "Broken tests are not quarantine candidates" ([quarantining](https://docs.trunk.io/flaky-tests/quarantining/index)).
- **Qodo PR-Agent.** Self-reflection is a second model call that scores each suggestion 0 to 10 and drops score-0 ones ([self-reflection](https://qodo-merge-docs.qodo.ai/core-abilities/self_reflection/)). `suggestions_score_threshold` defaults to 0 with the note "recommend not to set this value above 8, since above it may clip highly relevant suggestions". `commitable_code_suggestions` defaults to false (a table, not commit buttons). `focus_only_on_problems` defaults to true. `max_number_of_calls` is 3 ([configuration.toml](https://github.com/qodo-ai/pr-agent/blob/main/pr_agent/settings/configuration.toml)). The docs add that "a user should not accept all of them automatically" ([improve tool](https://qodo-merge-docs.qodo.ai/tools/improve/)).
- **Renovate.** "Currently Renovate's default behavior is to only automerge if every status check has succeeded." `ignoreTests: true` "means that Renovate will ignore _all_ status checks" ([configuration options](https://docs.renovatebot.com/configuration-options/)). With `automergeType: "pr"`, a PR that falls behind is rebased and waits for green again, so "an automerge like this will keep getting deferred with every rebase" if main moves faster than Renovate runs (same page).
- **Sourcery.** It posts a `Sourcery review` check: In progress, Success, "Failure, when blocking security findings require changes", or "Skipped, when a rate limit or the re-review cap applies". Automatic re-reviews are "capped at five per pull request". The same page also says "Requiring it never blocks a merge", which contradicts the Failure state. I could not resolve that ([anatomy of a review](https://docs.sourcery.ai/reviews/anatomy-of-a-review/)).
- **Sweep.** Its README now says the team is "building an AI coding assistant for JetBrains". I found no current docs for its PR fix loop, so nothing here comes from Sweep ([sweepai/sweep](https://github.com/sweepai/sweep)).

## Borrow

### Gate expressions

1. **Use Mergify's shape.** A list means AND, with explicit `or:` / `and:` / `not:` blocks, `-` for negation, and `#` for counts. It is YAML-native and already familiar to users. Keep Step references as bare Step IDs from the Pipeline, not free-text check names.
2. **Scope rules with `if` / `success_conditions`.** An inactive rule is ignored, not failed (Mergify Merge Protections). This covers the common case "if files touch `auth/`, require the security Jev Step" without an `or` gymnastics.
3. **Give Step outcomes an explicit multi-valued state and require the Gate to say how each value is treated.** Aviator's `pending` vs `missing`, its `acceptable_statuses`, and CodeRabbit's Inconclusive all show that pass/fail is not enough. The default should follow CodeRabbit Triage and Aviator: unknown, stale, or errored never satisfies the Gate.
4. **Key every outcome to the head SHA and treat outcomes from older SHAs as stale.** GitHub's dismiss-stale-approvals, Copilot's dismissed-on-push approval, and Aviator's `commit_sha` all do this.
5. **Record waivers as first-class Gate inputs.** Use Aviator's four categories (`false_positive`, `doesnt_apply`, `accepted_risk`, `fix_in_followup`) and record who waived, when, and why. A waiver flips the Gate without a new Run, as `aviator/verify` does. CodeRabbit's rule that the author cannot override is worth copying where a second person exists.
6. **Give each Jev judgement a per-Step `mode: off | warning | error`, with new judgements starting in `warning`** (CodeRabbit pre-merge checks).
7. **Fail a Gate that matches zero Steps or zero checks.** Aviator does this for wildcards and `require_all_checks_pass`.
8. **Qualify CI check references by the publishing app when they come from GitHub.** Mergify `@app/check` and ruleset source-app pinning exist because same-named checks collide.

### Fix loop

1. **Set a hard, small, per-Run cap on Fix iterations and make it visible.** Borrow CodeRabbit's default of 3 (range 1 to 10) and Mergify's "attempt 2/3" display.
2. **Enumerate terminal Run outcomes.** Start from CodeRabbit's "ready, awaiting human review, nothing actionable, blocked" and add Aviator's `caps_exceeded`, `loop_detected`, `stuck`, and `give_up`. A Run should always end in one of these, never just stop.
3. **Detect repeated failures by identity and stop on `loop_detected`.** Aviator hashes the test name, error kind, and source frames, so a Fix that produces the same failure again is recognizable.
4. **Classify before fixing.** A CI failure should pass through Aviator's ladder (log readable? names a changed file? seen before? same on unrelated PRs? would a rerun pass?) before a Fix Step gets it. Rerun at most once. Never let a Fix agent "fix" a flake.
5. **Block Fix Steps from writing CI config, workflow files, lockfiles, and slopwatch's own Pipeline config.** Surface those diffs instead (CodeRabbit Fix CI). Otherwise the fixer can pass the Gate by weakening it.
6. **Fix only from structured instructions.** CodeRabbit Autofix acts only on threads with a "Prompt for AI Agents" block and no-ops otherwise. A Fix Step should consume failing Step outcomes that carry fix instructions, and should report "nothing actionable" when none exist.
7. **Exit the Fix Step on merge conflicts** instead of generating code (CodeRabbit Autofix).
8. **Let real CI judge the Fix commit.** "That run is the source of truth" (CodeRabbit), and "It never marks a check as passing" (Aviator).
9. **Keep Aviator Runbooks' foreign-push rule.** It confirms slopwatch's Run model. A push the Run did not make ends the Run's monitoring, and a push mid-validation terminates the rework.

### Noise filtering

1. **Use path-scoped instructions per Jev Step** (CodeRabbit `path_instructions`, Copilot `applyTo`), plus path filters that skip generated files (`linguist-generated`, as Graphite does).
2. **Track per-judgement acceptance and waiver rates, and prune judgements that get waived often.** Graphite exposes these metrics per rule. Aviator says repeated waivers mean "the rule is wrong".
3. **Keep judgement-shaping context in versioned repo files** (Aviator `.aviator/mergequeue/flake/*.md`, `.aviator/config.yml` read from the default branch). Changes to what the Gate enforces then go through review.
4. **If a Jev Step returns confidence, map low confidence to inconclusive, not pass.** Copilot's automation levels and CodeRabbit's readiness confidence both treat low confidence as "hold for a human".

## Avoid

- **Overloaded failure semantics.** Mergify's `check-failure` also matches cancelled checks for backward compatibility. GitHub reports a conditionally skipped job as Success but leaves a path-filtered workflow Pending forever. Pick one meaning per state.
- **"Any element matches" as the default on lists.** In Mergify it is correct but easy to misread (`label != bug` means no label equals bug). Gate authors will write `step = review` and expect "the review Step passed", so make that the only reading.
- **Delivering a Fix that failed its own verification.** CodeRabbit Autofix does this on purpose, for a human to iterate on. In an unattended Run it pushes a known-broken commit and starts another CI cycle. Treat it as a failed Fix Step instead.
- **Letting an agent's approval count as a human approval.** Copilot approvals can satisfy required-approval rules. GitHub itself added an extra-approval rule for unattributed Copilot PRs. slopwatch's Human Step should never be satisfiable by a Step output.
- **Auto-applied inferred rules.** CodeRabbit learnings default to `approval_delay: 0`, so a chat reply can silently change future reviews. Anything that changes Gate behavior should need an explicit edit.
- **Rules that fire once per PR and never re-evaluate** (Graphite automations). That fits side effects. It does not fit a Gate, which has to re-evaluate on every SHA.
- **All-or-nothing check bypasses** like Renovate's `ignoreTests` (ignores all checks) and a merge queue timeout with no retry. Prefer per-Step modes.
- **Unbounded speculative work on flaky CI.** Graphite's docs say Parallel CI assumes CI passes and multiplies runs on flakes.
- **Contradictory check docs.** Sourcery says both that its check can fail on blocking findings and that "Requiring it never blocks a merge". slopwatch should state plainly whether its Gate result blocks.

## Open questions

1. **Should slopwatch publish the Gate as a GitHub check** (like `aviator/verify`, `Mergify Merge Protections`, `Sourcery review`) so branch protection enforces it, or only merge itself? If the repo has a merge queue, the Merge Step becomes "enqueue" (as CodeRabbit Triage does). Then is a later queue ejection part of the same Run or a new one?
2. **Push identity.** Which identity pushes Fix commits, and how does it interact with GitHub's "approval from someone other than the last person to push" and "dismiss stale approvals"? If Fix commits are pushed as the user, the user can never satisfy last-push approval on their own PR. If they are pushed as a bot, the extra-approval rule for unattributed agent PRs is a precedent GitHub may extend.
3. **Truth tables for pending states.** Does `or: [A, B]` pass as soon as A passes while B is pending? Mergify waits on pending checks. Kleene logic would short-circuit. Both are defensible, but the Gate must pick one and show it.
4. **Re-running judgements after a Fix push.** Does every Jev judgement re-run on the new SHA, or only the ones whose inputs changed? Every vendor treats a new commit as invalidating everything, and CodeRabbit auto-pauses after 5 reviewed commits for cost.
5. **Who can waive in a single-user desktop app** where the user is both author and approver? The CodeRabbit and Aviator "author cannot override" rule assumes a team.
6. **Where the Fix cap lives.** Per Run, per Step, or per failure identity? Aviator's "retry once per failure" suggests capping per failure identity, with an outer per-Run cap.
7. **Agent-CLI review Steps and Fix pushes.** Should review Steps re-run on every Fix push (Copilot "Review new pushes", off by default) or only once per Run?

## Could not verify

- A default for Mergify `batch_max_failure_resolution_attempts`.
- Any iteration cap for Copilot agentic autofix or cloud agent beyond the 59-minute session.
- Any Graphite "Diamond" docs, or any Graphite confidence threshold or false-positive setting beyond exclusions and downvotes.
- Sweep's PR fix loop, because its docs are gone.
- Sourcery's contradictory blocking statement.
