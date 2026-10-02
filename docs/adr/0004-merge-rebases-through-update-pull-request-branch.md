# Merge rebases a behind PR through `updatePullRequestBranch`, then lets a new Run judge it

Before merging, the Merge Step checks how far the PR is behind its base (`baseRef.compare(headRef).behindBy`, which works without branch protection). If it is behind and the base branch has no merge queue, Merge requests the `rebase` Effect instead of `merge`. The daemon runs GraphQL `updatePullRequestBranch` with `updateMethod: REBASE` and `expectedHeadOid`, and the resulting push ends the Run with end reason pushed. The next Run judges the rebased SHA, and its Merge Step lands it. That way the Gate always judged the tree that lands. `createCommitOnBranch` can only append a commit, so it can't rebase, and ADR 0002 rules out `git push`. This and the update after a retarget (ADR 0011) are the only places the daemon rewrites a branch.

## Considered options

- Rebase only when GitHub reports `BEHIND`. That status seems to appear only under "require branches to be up to date", so on other repos the Gate would pass a SHA whose merge with the current base nobody tested.
- Rebase every Watched PR whenever its base moves. Each rebase reruns the whole Pipeline, LLM Steps included, so a busy base would cost a full Run per Watched PR per base commit.
- Build the rebased commits with the Git Data API and force-update the ref. Those commits are just as unsigned, and the daemon would be rewriting history itself.
- Leave the rebase to the developer, with an Escalation. The developer said rebasing has to be automatic.

## Consequences

- GitHub can't sign rebased commits, so a branch that requires signed commits rejects the rebase. There the daemon uses `updateMethod: MERGE`, which is believed to produce web-flow-signed merge commits (not yet verified). Merge Step config can force either method.
- The mutation is async and doesn't return the new head. The daemon marks the PR as rebasing and attributes the next head change to the rebase. A push from someone else in that window gets the wrong end reason, and that is the only harm.
- A conflicting rebase ends Merge with `fail`, a "conflicts with base" Finding and an Escalation. Fix can't resolve the conflict either, because `createCommitOnBranch` only appends a single-parent commit (ADR 0008).
- A base that keeps moving could rebase forever. After 3 consecutive rebase-started Runs without a merge, the daemon raises an Escalation suggesting a merge queue. Rebase-started Runs neither count toward nor reset the Fix round cap.
- A rebase dismisses stale approvals and triggers CI like any push.
