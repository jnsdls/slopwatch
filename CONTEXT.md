# slopwatch

A desktop app that takes a pull request from "opened" to "shippable" by running it through a developer-defined series of automated checks and fixes. It manages PRs after they exist; it is neither a code-review tool nor a coding harness.

## Language

**Step**:
A configured, reusable unit of work with a typed outcome, such as a Jev judgement, waiting for CI, an agent review, or a fix. It runs a Plugin with its own settings.
_Avoid_: check, task, job, node

**Plugin**:
The executable a Step runs, together with the manifest that describes it. Built-in Plugins, such as Jev, CI and Merge, ship with the app. A Plugin runs only once the developer has approved it, which covers what its manifest asks for: a workspace, Effects and Secrets. Built-in Plugins come approved.
_Avoid_: step type, runner, action

**Approval**:
The developer's consent that a Plugin may run with what its manifest asks for. It belongs to the Plugin's name, not its code, so a new build keeps it, but a manifest that asks for more needs a new Approval.
_Avoid_: trust, permission, allowlist

**Secret**:
A named credential, such as an API key, that the daemon keeps and passes to a Step only when the Step's Plugin has an Approval that covers it. A Secret is never shown back once set.
_Avoid_: credential, token, key

**Library**:
The developer's own collection of configured Steps, shared across their repos. Presets ship as Library Steps.
_Avoid_: catalog, registry, templates

**Starter**:
A ready-made Pipeline, built from preset Library Steps, offered when the developer adds a repo. Picking one copies its Steps into the repo's draft Pipeline, and the draft keeps no link to the Starter after that.
_Avoid_: template, preset, recipe

**Pipeline**:
A graph of Steps bound to one repo, with one input (a Watched PR) and one output (the Gate's verdict).
_Avoid_: workflow, flow

**Gate**:
The condition over Step Verdicts that declares a PR shippable. It is pass, fail or pending, and it sits in the Pipeline as a node that later Steps such as Merge and Fix run after. A Step the Gate doesn't reference is advisory: it still runs and its Findings still reach later Steps, but it can't hold the PR back.
_Avoid_: shippable check, done condition

**Condition**:
The rule, over upstream Verdicts, the Gate and facts about the PR, that decides whether a Step runs. A Step whose Condition is false is skipped.
_Avoid_: filter, trigger, when-clause

**Run**:
One execution of a Pipeline against one head SHA of a Watched PR, using the Pipeline as it stands on the PR's root base when the Run starts. The root base is the PR's base branch, or for a PR in a Stack, the base of the Stack's bottom PR. Any push, including a Fix commit or a rebase made by slopwatch, ends the Run and starts a new one. A change to the Pipeline on the root base starts a new Run on the same SHA for a PR whose latest Run has ended.
_Avoid_: execution, build, job

**End reason**:
Why a Run stopped: merged, shippable (the Gate passed and the Pipeline has no Merge Step), not shippable, pushed (by slopwatch, such as a Fix commit, a Merge rebase or the update after a retarget), superseded (by a push or a base change from outside the Run), closed (merged or closed on GitHub by someone else), over budget (the Watched PR's or the day's Budget ran out), or cancelled (by the developer).
_Avoid_: status, result

**Watched PR**:
One of the developer's open pull requests, in a repo slopwatch has added, that carries the `slopwatch` label. The daemon creates the label the first time the developer watches a PR, and watching or unwatching from the app adds or removes it. A Watched PR in a repo whose Pipeline hasn't reached the base branch yet waits, and its first Run starts once the Pipeline lands.
_Avoid_: tracked PR, managed PR

**Stack**:
A chain of open PRs in one repo where each PR's base branch is the head branch of the PR below it, and the bottom PR's base is the root base. GitHub's native stacked PRs are one kind; branches chained by hand or by another tool are the other.
_Avoid_: chain, train, dependent PRs

**Run history**:
Every Run of a Watched PR, newest first, including superseded and cancelled ones. It belongs to the PR, so it survives unwatching and re-watching.
_Avoid_: run group, timeline

### Outcomes

**Outcome**:
What one Step reports for one Run: a Verdict plus named outputs such as Findings, a probability or a note. It belongs to the Run's head SHA and the Step's settings, so a later Run on the same SHA reuses it unless its Verdict was error, cancelled, missing or skipped, or the Step's settings or Plugin version have changed. A skip is decided again because it depends on the rest of the Run.
_Avoid_: result, status

**Verdict**:
The one-word part of an Outcome. The Step reports pass, fail or inconclusive. The daemon assigns error, cancelled, skipped or missing when the Step didn't report. A Gate term is satisfied only by pass, unless the term also accepts skipped.
_Avoid_: status, conclusion

**Waiver**:
The developer's ruling that one Step's settled, non-pass Verdict counts as pass for one head SHA. It carries a category (false positive, doesn't apply, accepted risk, fix in follow-up) and a reason. Overriding the Gate is one action that waives every Step behind a failing Gate term, and a Run whose Gate passes only through Waivers reads "shippable (waived)".
_Avoid_: override (for a single Step), exception, skip

**Finding**:
One specific problem a Step reports about the PR, with a severity and optionally a file and line. Later Steps, such as Fix, read the Findings of the Steps upstream of them.
_Avoid_: issue, comment, violation

**Step log**:
What a Step wrote while it ran, its stderr and log messages, one per attempt. It is kept for a while and then pruned, unlike the Outcome, which stays in Run history.
_Avoid_: output, transcript

**Effect**:
A change on GitHub, such as a comment, a label, a check rerun, a rebase or a merge, that a Step asks for and the daemon carries out only while the Step's Run is still current.
_Avoid_: action, side effect

### Fixing

**Fix round**:
A Run started by a commit from one of slopwatch's own Steps, such as Fix. The Pipeline caps consecutive Fix rounds on a Watched PR. An outside push resets the count, and a rebase made by slopwatch leaves it alone.
_Avoid_: retry, iteration, attempt

**Budget**:
A cap, in list-price US dollars, on what Steps may spend: per Step in one Run, afresh each time it is retried, per Watched PR since its last outside push, and per day across all repos. Usage is priced at list price whatever the developer is actually billed, so Steps on a subscription login spend against it too. A Step that reports no cost isn't budgeted.
_Avoid_: quota, limit, spend cap

**Guarded path**:
A file no Step may change through slopwatch, such as CI workflows or the Pipeline file, so that no Step can pass the Gate by weakening it.
_Avoid_: protected file, denylist

### Attention

**Human Step**:
A Step whose outcome only the developer can supply: approve or reject, with an optional note later Steps can read. Waiting on anyone else, such as a GitHub reviewer, is a different Step.
_Avoid_: approval step, manual step

**Escalation**:
The daemon's report that something can't go further without the developer. It belongs to one of three things. A Run, for a problem mid-Run such as a Step error or a stall, and it closes when answered or when the Run ends. A Watched PR, for a Run that ended needing the developer, such as not shippable or over budget, or for a problem between Runs such as a Stack parent closed unmerged. A PR has at most one open, and it closes when the PR's next Run starts or the PR stops being watched. Or a cause shared across PRs, such as a missing Secret, an unapproved Plugin, an invalid Pipeline or the day's Budget, and it closes when the daemon sees the cause cleared, which starts a same-SHA Run for every PR it held back. A Run the developer ended by rejecting or cancelling raises none. It isn't a node in the Pipeline.
_Avoid_: alert, failure, incident

**Inbox**:
The one list, across repos, of open Human Steps and Escalations waiting on the developer, oldest first. An Escalation shared across PRs is one entry, however many PRs it holds back. Each Run's record keeps the entries that touched it and how each one closed.
_Avoid_: queue, notifications, attention list

**Notification**:
A macOS banner for a new Inbox entry or a Run that ended shippable. The daemon decides on it and keeps it until the GUI acks it, and the GUI posts it. A click opens its PR. When its Inbox entry closes, the banner goes too. It points at the Inbox and doesn't replace it.
_Avoid_: alert, Inbox (for the banner)
