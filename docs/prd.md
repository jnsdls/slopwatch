# slopwatch v1 PRD

This is the index for the v1 spec. Each decision lives in one place, a closed ticket on the [v1 spec map](https://github.com/jnsdls/slopwatch/issues/1) or an ADR in [`docs/adr/`](adr/), and this document links to it instead of restating it. Terms in capitals are defined in [`CONTEXT.md`](../CONTEXT.md).

## Problem

Coding agents now open PRs faster than one developer can shepherd them. Each PR still needs the same loop after it exists: wait for CI, check that the description matches the diff and the linked issue, get an agent review, fix what the review found, rerun what flaked, rebase, merge. Every step is mechanical, and I still do all of them by hand, PR by PR. The agents stop at the PR. Nothing picks it up from there.

## Solution

slopwatch is a macOS app with a background daemon that runs every Watched PR through a Pipeline until its Gate says shippable. I define the Pipeline once per repo, as a graph of Steps in `.slopwatch/pipeline.yml`, and edit it mostly in the app's graph editor. Steps judge the PR with Jev, wait for CI, run Claude Code or Codex headless as reviewers, and fix Findings in a daemon-owned worktree. Some Steps ask me. The daemon commits fixes through the GitHub API, starts a new Run on every push, stops the Fix loop at a small cap, and merges when the Gate passes if the Pipeline has a Merge Step. When it can't go further, it puts one entry in the Inbox and posts a notification.

It is not a code-review tool. The app never shows a diff or a file tree. It doesn't start feature work either, and it runs next to Claude Code, Codex or T3 Code, not instead of them.

## Who v1 is for

One developer, me, on one Mac running macOS 13 or later, watching my own open PRs in repos I can push to. Teams come later. These v1 choices keep that door open, and the build must not close it:

- GitHub auth sits behind one seam in the daemon, so a GitHub App can replace the user's `gh` token ([GitHub identity and push ownership](https://github.com/jnsdls/slopwatch/issues/15)).
- Every command carries an actor, and `hello` carries an `auth` field ([ADR 0010](adr/0010-clients-speak-websocket-json-over-a-unix-socket.md)).
- The daemon never assumes a client shares its filesystem. Logs, diffs and evidence travel over the protocol.
- A Run reads the Pipeline from the root base, never the PR head, so a PR can't weaken its own Gate ([ADR 0007](adr/0007-a-run-reads-the-pipeline-from-the-base-branch.md)).
- The daily Budget lives in daemon settings, never the repo ([Cost budgets for LLM Steps](https://github.com/jnsdls/slopwatch/issues/31)).

## User stories

### Setting up a repo

1. As the developer, I add a repo from the list `gh` gives me and land on its empty Pipeline canvas with a coach-mark tour, so I learn the editor I'll keep using.
2. As the developer, I pick a Starter that fills the canvas with a working Pipeline, then add or remove Library Steps.
3. As the developer, I paste a missing Secret from the red badge on the Step that needs it, and I can publish before every Secret is set.
4. As the developer, I publish the Pipeline as a PR from the `slopwatch/pipeline` branch and merge it with "Merge it now".
5. As the developer, I watch PRs before the Pipeline lands, and they wait, then start once it reaches the base branch.
6. As the developer, I open a repo that already has `.slopwatch/pipeline.yml` and go straight to watching.

### Editing a Pipeline

7. As the developer, I see the Pipeline as a left-to-right graph, with the Gate as a node, Merge and Fix after it, and advisory Steps dashed.
8. As the developer, I add a Step by dragging it from the Library palette, and I wire `needs` and Gate terms by dragging ports.
9. As the developer, I get an immediate refusal when a gesture would make an invalid Pipeline, such as a cycle or the Gate reading a write Step.
10. As the developer, I edit a Step's `uses`, `with`, `needs`, Condition and Gate role in the inspector.
11. As the developer, I drag nodes where I want them, keep those positions out of git, and "Tidy" back to auto-layout.
12. As the developer, I publish GUI edits as a PR that keeps my hand-written YAML comments and ordering, and I see both versions when a node changed on both sides.
13. As the developer, I keep reusable Steps in my Library at `~/.config/slopwatch/steps/` and override their keys per repo with `with:`.

### Watching and Runs

14. As the developer, I watch or unwatch a PR from the app or by adding or removing the `slopwatch` label on GitHub.
15. As the developer, I get a new Run on every push to a Watched PR, whoever made it.
16. As the developer, I get same-SHA Runs on every ended Watched PR when the Pipeline changes on its root base, and only new or changed Steps actually run.
17. As the developer, I see Steps run in parallel once their upstream Steps settle, and a Step skipped when its Condition is false, with the reason.
18. As the developer, I see the Gate turn pass, fail or pending as early as the logic allows.
19. As the developer, I cancel any Run.
20. As the developer, I retry an errored Step in the same Run.

### The Gate and Waivers

21. As the developer, I waive one Step's settled, non-pass Verdict for this head SHA with a category and a reason.
22. As the developer, I override the Gate in one action that waives every failing term, and the Run reads "shippable (waived)".
23. As the developer, I waive from a not-shippable PR entry, and a new same-SHA Run reuses the settled Outcomes.

### Fixing

24. As the developer, I have a Fix Step act on the Findings behind failing, unwaived Gate terms, plus CI log tails for failed Actions jobs.
25. As the developer, I trust that a fix lands only when the write Step reports pass with a non-empty diff that touches no Guarded path.
26. As the developer, I see a flaky Actions job rerun once before CI reports fail, so a flake never reaches Fix.
27. As the developer, I see the Fix loop stop on the round cap, an empty diff or a repeated tree, with one PR entry naming why.

### Merging

28. As the developer, I let the Merge Step land a PR once its Gate passes, directly or through the merge queue, and only at the head SHA the Gate judged.
29. As the developer, I let Merge rebase a PR that is behind its base, and the next Run judges and lands the rebased SHA.
30. As the developer, I get one PR entry when a rebase conflicts, the queue ejects the PR, or GitHub blocks the merge past the Merge Step's timeout.
31. As the developer, I see drafts run the Pipeline but never merge.

### Stacks

32. As the developer, I watch each PR of a Stack on its own, and every one runs right away against its parent's head.
33. As the developer, I see a Stack as a tree in the PR list, with an unwatched parent as a dim row.
34. As the developer, I let a Stack land bottom-up, each PR through its own Gate and Merge, with the daemon retargeting and updating the next PR of a non-native Stack.
35. As the developer, I get a PR entry on a child whose parent closed unmerged, and the daemon leaves the child alone.

### Attention

36. As the developer, I answer a Human Step with approve or reject and an optional note that later Steps read.
37. As the developer, I see every open Human Step and Escalation across repos in one Inbox, oldest first, with the count on the Dock badge.
38. As the developer, I get a macOS notification for each new Inbox entry and each shippable PR, and a click opens that PR.
39. As the developer, I get one shared entry for a cause that holds back many PRs, such as a missing Secret or the day's Budget, and every held PR starts again once the cause clears.
40. As the developer, I dismiss a PR entry I don't want to act on.
41. As the developer, I see in a PR pane what failed and why, as a Step list with Verdicts, Findings, notes, cost and a log tail, without ever seeing the diff.
42. As the developer, I switch a Run to the graph view to see what ran in parallel and what read what.

### Plugins, Secrets and Budgets

43. As the developer, I drop an executable or symlink into `~/.config/slopwatch/plugins/` and approve what its manifest asks for before it runs.
44. As the developer, I keep running a Plugin I rebuild without approving it again, unless its manifest asks for more.
45. As the developer, I set and rotate Secrets in the app without slopwatch ever showing a value back.
46. As the developer, I run Claude and Codex Steps on my subscription login by default, and switch a Step to an API key Secret.
47. As the developer, I cap list-price spend per Step, per Watched PR and per day, and a spent PR or daily Budget ends the Run as over budget with one entry offering to raise it or run anyway once.
48. As the developer, I see cost per Run and per Step, with unknown cost shown as `+?` instead of zero.

### History and logs

49. As the developer, I keep every Run's record forever, including after unwatching, with End reason, Verdicts, Findings, Waivers, cost and Inbox history.
50. As the developer, I follow a running Step's log live, page through it, search it, filter it and interleave the Run's events.
51. As the developer, I trust Step logs never contain the values of Secrets the Step received.

### Running the app

52. As the developer, I install and update slopwatch by running one script that builds, signs, swaps the bundle and relaunches.
53. As the developer, I quit the GUI without stopping slopwatch, and the next notification brings it back without a window.
54. As the developer, I see a clear "daemon not running" state with a way to re-register or open Login Items when the daemon is down.
55. As the developer, I rely on Runs resuming in place after a crash, a reboot or an update.

## Decisions

One line per area, then the tickets and ADRs that hold the detail.

### Runs, Pipelines and the Gate

- Every push ends the Run and starts a new one. The Pipeline is a DAG and loops run across Runs. [ADR 0001](adr/0001-every-push-starts-a-new-run.md), [Human Step and attention model](https://github.com/jnsdls/slopwatch/issues/12).
- Untyped `needs` edges, per-Step Conditions with the default "every upstream Step passed", and a Mergify-shaped Gate over Verdicts only, evaluated three-valued, sitting in the graph. Waivers per Step per SHA. Same-SHA Runs reuse settled Outcomes. Load-time validation rules. [ADR 0006](adr/0006-the-gate-is-a-three-valued-node-in-the-pipeline.md), [Pipeline graph semantics and Gate expressions](https://github.com/jnsdls/slopwatch/issues/8).
- YAML at `.slopwatch/pipeline.yml`, read from the root base at Run start. `lib/` Library Steps resolve live with per-key `with:` overrides. The reuse key is head SHA, Pipeline id, resolved config hash and Plugin version. GUI edits replay onto the file and publish as a PR. [ADR 0007](adr/0007-a-run-reads-the-pipeline-from-the-base-branch.md), [Pipeline file format and Step reuse](https://github.com/jnsdls/slopwatch/issues/9).

### Steps and Plugins

- One process per Step per Run, JSONL over stdio, `describe` manifest, a head-SHA PR snapshot in, a Verdict plus outputs and Effect requests out, no GitHub token, per-Step worktrees, daemon-enforced timeout, stall, cancel and budget, no auto-retry. [ADR 0003](adr/0003-steps-read-a-snapshot-and-request-effects.md), [Step contract](https://github.com/jnsdls/slopwatch/issues/7).
- Third-party Plugins are drop-in executables. The daemon runs `describe` locked down, and one Approval per Plugin name covers workspace, Effects and Secrets. Reuse version is manifest version plus executable hash. [ADR 0012](adr/0012-plugin-approval-covers-capabilities-not-code.md), [Third-party Plugin install and trust](https://github.com/jnsdls/slopwatch/issues/36).
- Secrets are daemon-created Keychain items, write-only over the protocol, one namespace by env var name, handed out only under an Approval. Claude and Codex default to `auth: subscription`. [Secrets for Steps](https://github.com/jnsdls/slopwatch/issues/30), [Research: subscription login for headless agent CLIs](https://github.com/jnsdls/slopwatch/issues/29).
- Budgets in list-price USD per Step, per Watched PR and per day, with defaults and the over budget End reason. [Cost budgets for LLM Steps](https://github.com/jnsdls/slopwatch/issues/31).

### Fixing and committing

- The daemon's commit rules key on `workspace: write`, not on Fix. Commit only on pass with a non-empty diff, refuse the whole commit on a Guarded path, `fix_rounds` default 3 and ceiling 10, stop on cap, empty diff or repeated tree, CI reruns a failed Actions job once. [ADR 0008](adr/0008-the-daemon-decides-what-a-write-step-commits.md), [Fix loop](https://github.com/jnsdls/slopwatch/issues/10).
- Only the daemon commits, as the user, through `createCommitOnBranch` with `expectedHeadOid`, never `git push`. Its own pushes are recognized by a SHA journal. [ADR 0002](adr/0002-fix-commits-land-through-the-github-api.md), [GitHub identity and push ownership](https://github.com/jnsdls/slopwatch/issues/15).

### Merging and Stacks

- slopwatch merges through the Merge Step, never auto-merge, and doesn't publish the Gate to GitHub in v1. A behind PR rebases through `updatePullRequestBranch` and a new Run judges it. [ADR 0004](adr/0004-merge-rebases-through-update-pull-request-branch.md), [Merge ownership](https://github.com/jnsdls/slopwatch/issues/16).
- Stacks run every PR at once, read the Pipeline from the root base and land bottom-up through the async merge API with `sha`. The daemon retargets and updates children of a non-native Stack. [ADR 0011](adr/0011-a-stack-lands-one-pr-at-a-time-from-the-bottom.md), [Stacked PRs](https://github.com/jnsdls/slopwatch/issues/35).

### The daemon

- A crash-only launchd agent registered with `SMAppService.agent`. The GUI is only a client. Restart resumes Runs in place, and an Effect intent journal reconciles with GitHub. [ADR 0009](adr/0009-the-daemon-is-a-separate-crash-only-process.md), [Daemon architecture](https://github.com/jnsdls/slopwatch/issues/11).
- WebSocket JSON frames on a Unix socket, `hello` with dialect, features, build id and `auth`, topic subscriptions with sequence numbers. [ADR 0010](adr/0010-clients-speak-websocket-json-over-a-unix-socket.md).
- SQLite in WAL mode plus a per-Run event journal, a global Step cap of 8 and per-Plugin caps, one blobless bare clone per repo, adaptive batched GraphQL polling at 30 s or 2 min within about 25% of the hourly budget, and the login-shell env for Steps. [Daemon architecture](https://github.com/jnsdls/slopwatch/issues/11), [Research: watching GitHub PRs without webhooks](https://github.com/jnsdls/slopwatch/issues/5).
- Two tiers of storage. The record is kept forever. Detail is pruned 14 days after a Run ends under a soft 2 GB cap, Step logs are capped at 16 MB per attempt, and Secret values are masked. [Run history and log retention](https://github.com/jnsdls/slopwatch/issues/34).

### Attention

- The Human Step is approve or reject plus a note. Escalations are the unplanned asks, and both go in one Inbox. [Human Step and attention model](https://github.com/jnsdls/slopwatch/issues/12).
- An Escalation belongs to a Run, a Watched PR or a shared cause, and each scope has its own closing rule. Only a PR entry can be dismissed, and a cleared cause restarts every PR it held back. [Inbox entry lifetime and scope](https://github.com/jnsdls/slopwatch/issues/46).
- The GUI posts every notification. The daemon keeps acked records and launches the GUI windowless with `open -g -j` when none is connected. [ADR 0013](adr/0013-the-gui-posts-every-notification.md), [Notifications and the daemon's executables](https://github.com/jnsdls/slopwatch/issues/50), [Research: notifications and executables in an SMAppService agent](https://github.com/jnsdls/slopwatch/issues/47).

### The GUI

- Three panes: sources with the Inbox, the Watched PR list, and the PR pane with the open entry, Run history chips and the Run as a Step list with a Graph toggle. [Prototype: main window](https://github.com/jnsdls/slopwatch/issues/14).
- The Pipeline editor is a node canvas with flow-chart editing and positions in a daemon-side sidecar. [Prototype: Pipeline graph view and editor](https://github.com/jnsdls/slopwatch/issues/13).
- Onboarding is the editor with a coach-mark tour, Starters and "Merge it now". [Prototype: onboarding a repo](https://github.com/jnsdls/slopwatch/issues/37).
- GPUI comes from `gpui-kit`'s exact `gpui-pre` pin, imported only by the client crate. [ADR 0005](adr/0005-gpui-comes-from-gpui-kit-snapshots.md), [GPUI sourcing](https://github.com/jnsdls/slopwatch/issues/17), [Research: GPUI as a standalone app framework](https://github.com/jnsdls/slopwatch/issues/2).

### Distribution

- Local builds only, ad-hoc signed (`codesign -s -`) inside-out, with no Apple identity. A real identity comes with notarized releases. The install script is the updater. On a build-id mismatch the GUI sends `restart`, then `unregister` and `register`. Bundle IDs `com.jnsdls.slopwatch` and `.dev`. Build id is the git SHA plus a dirty hash. [macOS distribution and updates](https://github.com/jnsdls/slopwatch/issues/33), [Research: shipping and updating a Rust/GPUI macOS app](https://github.com/jnsdls/slopwatch/issues/32).

### Facts the built-in Plugins rely on

- Jev's `/v1/evaluate`: question types, the 32k-token state cap, price, unstable rate limits, and which PR evidence fits. [Research: Jev evaluate API for PR judging](https://github.com/jnsdls/slopwatch/issues/3).
- Jev always sends `zeroDataRetention: true` and `only: ["typesafe-ai"]`, and fails rather than retrying without them. [Task: confirm Jev zero data retention](https://github.com/jnsdls/slopwatch/issues/18).
- Flags, schema output, cost reporting and commit limits of the five agent CLIs run headless. [Research: running agent CLIs headless](https://github.com/jnsdls/slopwatch/issues/4).
- What to borrow from Mergify, Aviator and CodeRabbit. [Research: prior art in PR automation](https://github.com/jnsdls/slopwatch/issues/6).
- What to borrow from zeron and tty7: the in-process transport carrying real frames, the stall watchdog, the run journal and the login-shell `PATH` merge. [Research: zeronsh/zeron and similar agent control planes](https://github.com/jnsdls/slopwatch/issues/19).

## What ships

### Executables

The `.app` holds two executables in `Contents/MacOS`: the GUI and the bare daemon, plus the agent plist in `Contents/Library/LaunchAgents` ([ADR 0013](adr/0013-the-gui-posts-every-notification.md)).

### Crates

- **core.** Framework-free. The Pipeline model, Gate and Condition evaluation, validation and the reuse key.
- **protocol.** Client frames ([ADR 0010](adr/0010-clients-speak-websocket-json-over-a-unix-socket.md)) and the Step JSONL messages and manifest ([Step contract](https://github.com/jnsdls/slopwatch/issues/7)).
- **daemon.** Scheduling, polling, Effects, the journals, storage and Step processes.
- **client.** The GUI. The only crate that may depend on GPUI or `gpui-kit`, checked in CI with `cargo tree` ([ADR 0005](adr/0005-gpui-comes-from-gpui-kit-snapshots.md)).

### Built-in Plugins

`jev`, `ci`, `claude`, `codex`, `fix`, `human` and `merge`. They come approved, and their names are reserved ([ADR 0012](adr/0012-plugin-approval-covers-capabilities-not-code.md)). `fix` is the write Plugin and picks its agent with `agent: claude | codex` ([Fix loop](https://github.com/jnsdls/slopwatch/issues/10)).

### Library presets and Starters

Five presets ship as Library Steps: `desc-matches-diff`, `resolves-issue`, `claude-review`, `codex-review` and `claude-fix`. Four Starters ship: Review and fix, Ask me, then merge, Hands off, and Just CI. [Prototype: onboarding a repo](https://github.com/jnsdls/slopwatch/issues/37) has the table of what each contains.

That ticket lists `claude-fix` under the `claude` Plugin. A manifest declares one `workspace`, and `claude-review` needs `read`, so `claude-fix` is the `fix` Plugin with `agent: claude`.

## Testing

What the decisions already require:

- A round-trip suite that keeps the `yaml-edit` editor and the `serde-saphyr` loader in agreement: inserting Steps, nested `or:`, flow sequences, comments kept. If `yaml-edit` fails it, the fallback is KDL ([Pipeline file format and Step reuse](https://github.com/jnsdls/slopwatch/issues/9)).
- The `cargo tree` check that only the client crate pulls in GPUI.
- zeron's in-process transport as a test fixture that carries real protocol frames ([Daemon architecture](https://github.com/jnsdls/slopwatch/issues/11)).
- Every dev rebuild goes through the update handoff, so the restart path gets exercised every day ([macOS distribution and updates](https://github.com/jnsdls/slopwatch/issues/33)).
- Core is framework-free, so Gate evaluation, Conditions, skip cascades, validation and Outcome reuse are unit-testable without a daemon.

## Open details for the build

Each ticket left these for the build. None of them changes a decision on the map.

### Checks that need a live system

- Signed-build notification checks: the agent really can't post, a background launch with GPUI shows no window and takes no focus, and a click reaches the GUI while the daemon shares its bundle ID. If the background launch fails, the nested helper `.app` replaces it ([ADR 0013](adr/0013-the-gui-posts-every-notification.md)).
- Whether gpui-kit's pinned `gpui-pre` includes `show_system_notification`, zed #61189, with a usable identifier and removal, or the client calls `UNUserNotificationCenter` through `objc2` like the Dock badge ([Notifications and the daemon's executables](https://github.com/jnsdls/slopwatch/issues/50)).
- An N to N+1 update test: whether re-registering the agent brings back "Background Items Added", and whether a plain kill would have been enough ([ADR 0009](adr/0009-the-daemon-is-a-separate-crash-only-process.md)). Answered by [#56](https://github.com/jnsdls/slopwatch/issues/56) in ADR 0009's "What the first build showed".
- Whether the daemon needs its own `codesign -i` identifier, and whether a build with no entitlements runs ([macOS distribution and updates](https://github.com/jnsdls/slopwatch/issues/33)). Answered by [#56](https://github.com/jnsdls/slopwatch/issues/56) in ADR 0009's "What the first build showed".
- What ad-hoc signing costs. No Apple source says whether `SMAppService.register()` accepts an ad-hoc build, and DTS says ad-hoc builds lose approval across rebuilds ([Research: shipping and updating a Rust/GPUI macOS app](https://github.com/jnsdls/slopwatch/issues/32)). An ad-hoc designated requirement is the cdhash, so every rebuild is a new code identity. The first build checks whether the agent registers, and what a rebuild does to Login Items approval, notification permission and the daemon's access to its own Keychain items. If those break on every rebuild, v1 moves to a self-signed certificate, which keeps the designated requirement stable without an Apple account. Answered, except for notification permission, by [#56](https://github.com/jnsdls/slopwatch/issues/56) in ADR 0009's "What the first build showed".
- Whether `claude -p` spawned by the launchd agent reads Claude's Keychain login without a prompt ([Secrets for Steps](https://github.com/jnsdls/slopwatch/issues/30)).
- Whether `claude -p` stream-json emits `rate_limit_event` and fills `total_cost_usd` under a subscription login ([Research: subscription login for headless agent CLIs](https://github.com/jnsdls/slopwatch/issues/29)).
- Whether `updatePullRequestBranch` with `MERGE` produces signed merge commits ([ADR 0004](adr/0004-merge-rebases-through-update-pull-request-branch.md)).
- Whether `MERGE` comes out clean on a child after its parent was squash-merged ([ADR 0011](adr/0011-a-stack-lands-one-pr-at-a-time-from-the-bottom.md)).
- Whether `yaml-edit` passes the round-trip suite. It is 0.x, so pin it.

### Values with no default yet

- The Merge Step's timeout, which bounds its wait on a `BLOCKED` PR ([Task: consistency sweep before the PRD](https://github.com/jnsdls/slopwatch/issues/45)). The Step contract sets `timeout` and `stall_after` defaults only for agent, CI, Jev and Human Steps.
- The `jev` Plugin's per-Plugin cap. Parallax runs 4 in flight with 1 to 30 s backoff ([Research: Jev evaluate API for PR judging](https://github.com/jnsdls/slopwatch/issues/3)).
- The co-author email in the `Co-authored-by: slopwatch` trailer, open until a slopwatch domain or App bot exists ([GitHub identity and push ownership](https://github.com/jnsdls/slopwatch/issues/15)).

### Mechanics to work out in code

- Journal the daemon's Stack retarget and branch update like Effects. They aren't Step-requested, so the intent journal doesn't obviously cover them, and a crash between the two could start a Run on the un-updated diff ([Task: consistency sweep before the PRD](https://github.com/jnsdls/slopwatch/issues/45)).
- Load-time validation also rejects a Merge that isn't downstream of the Gate. [ADR 0006](adr/0006-the-gate-is-a-three-valued-node-in-the-pipeline.md) doesn't list that rule, [Pipeline graph semantics and Gate expressions](https://github.com/jnsdls/slopwatch/issues/8) does.
- How the `jev` Plugin keeps the state under 32k tokens: it measures before sending, and large diffs go per file or filtered. p90 PRs don't fit in one call ([Research: Jev evaluate API for PR judging](https://github.com/jnsdls/slopwatch/issues/3)).
- The price table for Steps that report tokens but no USD, such as Codex ([Step contract](https://github.com/jnsdls/slopwatch/issues/7)).
- Which binary built-in Plugins run from. Their reuse version hashes the app binary ([Third-party Plugin install and trust](https://github.com/jnsdls/slopwatch/issues/36)).
- Built-in agent Steps ignore repo hooks and MCP servers by default, through `--bare` for Claude and each CLI's equivalent ([Step contract](https://github.com/jnsdls/slopwatch/issues/7)). Only Claude's flag is recorded, so each other CLI's equivalent needs finding.

### UI with no prototype

- In Graph mode the canvas doesn't fit the PR pane at 1440 px, so Graph mode should give it more room, for example by collapsing the sources column ([Prototype: main window](https://github.com/jnsdls/slopwatch/issues/14)).
- On a narrow window the coach card can cover the Steps after the Gate. Placement should prefer the side of the anchor that has room ([Prototype: onboarding a repo](https://github.com/jnsdls/slopwatch/issues/37)).
- The screens no prototype covered: the Secrets list, the Plugins list with its Approval prompt, the Library editor, and daemon settings for the daily Budget, retention, Step caps and per-Plugin paths. Their contents are decided in their tickets. The layout is the build's call.

## Out of scope

The map's [Out of scope](https://github.com/jnsdls/slopwatch/issues/1) section has the reasons. In short, v1 has no hosted or multi-user daemon, no UI for reading code, no feature work, no channels beyond macOS notifications, no agent resolution of rebase conflicts, no Linux or Windows, no MCP server, and no notarized releases or in-app updater.
