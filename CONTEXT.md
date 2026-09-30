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
One execution of a Pipeline against a Watched PR. A push made by the Run's own Steps continues it; a push from anyone else ends it and starts a new Run.
_Avoid_: execution, build, job

**Watched PR**:
A pull request the app has picked up and runs its repo's Pipeline on.
_Avoid_: tracked PR, managed PR
