# slopwatch

A desktop app that takes a pull request from "opened" to "shippable" by running it through a developer-defined series of automated checks and fixes. It manages PRs after they exist; it is neither a code-review tool nor a coding harness.

## Language

**Step**:
A reusable unit of work with a typed outcome, such as a Jev judgement, waiting for CI, an agent review, or a fix.
_Avoid_: check, task, job, node

**Pipeline**:
A graph of Steps bound to one repo, with one input (a Watched PR) and one output (the Gate's verdict).
_Avoid_: workflow, flow

**Gate**:
The condition over Step outcomes that declares a PR shippable.
_Avoid_: shippable check, done condition

**Run**:
One execution of a Pipeline against one head SHA of a Watched PR. Any push, including one made by a Fix Step, ends the Run and starts a new one.
_Avoid_: execution, build, job

**End reason**:
Why a Run stopped: shippable, not shippable, fix pushed, superseded (by a push from outside the Run), or cancelled (by the developer).
_Avoid_: status, result

**Watched PR**:
A pull request the app has picked up and runs its repo's Pipeline on.
_Avoid_: tracked PR, managed PR

**Run history**:
Every Run of a Watched PR, newest first, including superseded and cancelled ones. It belongs to the PR, so it survives unwatching and re-watching.
_Avoid_: run group, timeline

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
