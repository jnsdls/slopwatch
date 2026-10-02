# slopwatch

A desktop app that takes a pull request from "opened" to "shippable" by running it through a developer-defined series of automated checks and fixes. It manages PRs after they exist; it is neither a code-review tool nor a coding harness.

## Language

**Step**:
A configured, reusable unit of work with a typed outcome, such as a Jev judgement, waiting for CI, an agent review, or a fix. It runs a Plugin with its own settings.
_Avoid_: check, task, job, node

**Plugin**:
The executable a Step runs, together with the manifest that describes it. Built-in Steps such as Jev, CI and Merge are Plugins that ship with the app.
_Avoid_: step type, runner, action

**Secret**:
A named credential, such as an API key, that the daemon keeps and passes to a Step only when the developer has granted it to the Step's Plugin. Built-in Plugins come granted the Secrets their manifests name. A Secret is never shown back once set.
_Avoid_: credential, token, key

**Library**:
The developer's own collection of configured Steps, shared across their repos. Presets ship as Library Steps.
_Avoid_: catalog, registry, templates

**Pipeline**:
A graph of Steps bound to one repo, with one input (a Watched PR) and one output (the Gate's verdict).
_Avoid_: workflow, flow

**Gate**:
The condition over Step Verdicts that declares a PR shippable. It is pass, fail or pending, and it sits in the Pipeline as a node that later Steps such as Merge and Fix run after.
_Avoid_: shippable check, done condition

**Condition**:
The rule, over upstream Verdicts, the Gate and facts about the PR, that decides whether a Step runs. A Step whose Condition is false is skipped.
_Avoid_: filter, trigger, when-clause

**Run**:
One execution of a Pipeline against one head SHA of a Watched PR, using the Pipeline as it stands on the PR's base branch when the Run starts. Any push, including a Fix commit or a rebase made by slopwatch, ends the Run and starts a new one. A change to the Pipeline on the base branch starts a new Run on the same SHA for a PR whose latest Run has ended.
_Avoid_: execution, build, job

**End reason**:
Why a Run stopped: merged, shippable (the Gate passed and the Pipeline has no Merge Step), not shippable, pushed (by one of the Run's own Steps, such as a Fix commit or a Merge rebase), superseded (by a push from outside the Run), closed (merged or closed on GitHub by someone else), over budget (the Watched PR's or the day's Budget ran out), or cancelled (by the developer).
_Avoid_: status, result

**Watched PR**:
A pull request the app has picked up and runs its repo's Pipeline on.
_Avoid_: tracked PR, managed PR

**Run history**:
Every Run of a Watched PR, newest first, including superseded and cancelled ones. It belongs to the PR, so it survives unwatching and re-watching.
_Avoid_: run group, timeline

### Outcomes

**Outcome**:
What one Step reports for one Run: a Verdict plus named outputs such as Findings, a probability or a note. It belongs to the Run's head SHA and the Step's settings, so a later Run on the same SHA reuses it unless its Verdict was error, cancelled or missing, or the Step's settings or Plugin version have changed.
_Avoid_: result, status

**Verdict**:
The one-word part of an Outcome. The Step reports pass, fail or inconclusive. The daemon assigns error, cancelled, skipped or missing when the Step didn't report. A Gate term is satisfied only by pass, unless the term also accepts skipped.
_Avoid_: status, conclusion

**Waiver**:
The developer's ruling that one Step's settled, non-pass Verdict counts as pass for one head SHA. It carries a category (false positive, doesn't apply, accepted risk, fix in follow-up) and a reason.
_Avoid_: override, exception, skip

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
A cap, in list-price US dollars, on what Steps may spend: per Step in one Run, per Watched PR since its last outside push, and per day across all repos. Usage is priced at list price whatever the developer is actually billed, so Steps on a subscription login spend against it too. A Step that reports no cost isn't budgeted.
_Avoid_: quota, limit, spend cap

**Guarded path**:
A file no Step may change through slopwatch, such as CI workflows or the Pipeline file, so that no Step can pass the Gate by weakening it.
_Avoid_: protected file, denylist

### Attention

**Human Step**:
A Step whose outcome only the developer can supply: approve or reject, with an optional note later Steps can read. Waiting on anyone else, such as a GitHub reviewer, is a different Step.
_Avoid_: approval step, manual step

**Escalation**:
The daemon's report that a Run can't go further without the developer, such as a Step error, a Fix round cap reached, a stall, or a not-shippable end. It isn't a node in the Pipeline.
_Avoid_: alert, failure, incident

**Inbox**:
The one list, across repos, of open Human Steps and Escalations waiting on the developer, oldest first.
_Avoid_: queue, notifications, attention list
