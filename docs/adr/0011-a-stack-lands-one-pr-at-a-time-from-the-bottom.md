# A Stack lands one PR at a time from the bottom, and the daemon retargets what's left

Every PR in a Stack runs its own Pipeline as soon as it is watched, judged against its parent's head, so the whole Stack gets feedback early. Only the bottom PR can merge. A Merge Step on a PR whose base is another open PR's head ends `inconclusive` with a "stacked on <parent>" Finding and no Escalation. Once the bottom PR lands, the next PR gets retargeted, which ends its Run and starts a new one, and that Run's Merge lands it. Each PR's own Gate judges it, and its own Merge Step lands it. slopwatch never merges a child into its parent's branch.

Merge requests go through GitHub's async merge API (`PUT /pulls/{n}/merge-async`) for every PR, stacked or not, with `sha` set to the judged head and `merge_action: default`. That one call merges directly or enqueues, so it replaces both `mergePullRequest` and `enqueuePullRequest` from [Merge ownership](https://github.com/jnsdls/slopwatch/issues/16). `mergePullRequest` doesn't support native stacks. The daemon polls the returned request until it reports `merged`, `enqueued` or `failed`. `enqueued` is final, so after it the daemon watches the PR's merged state as it already does for queues.

When a parent merges outside a native stack, the daemon retargets each child to the parent's base with `updatePullRequest(baseRefName)` right away. GitHub only retargets a child when the parent's head branch is deleted, it's unclear whether its auto-delete fires for API and queue merges, and deleting the branch without a retarget closes the child for good. Then, before the next Run starts, the daemon updates the child's branch with `updatePullRequestBranch`. It uses `MERGE` if the parent was squash-merged, because a rebase would replay the parent's original commits onto a base that already has their squashed copy, and `REBASE` otherwise. ADR 0004's signed-commit rule still forces `MERGE` where it applies. Native stacks rebase and retarget their upper layers themselves, so the daemon only does this for non-native Stacks.

## Considered options

- Let a child's Merge land the whole native stack below it, which is what the async API does when asked to merge layer N. That merges parents whose own Gate may not have passed. Requesting only the bottom layer merges just that layer.
- Hold a child's Run until its parent's Gate passes, or until the parent merges. The developer would get the child's verdicts late, and the child's diff against its parent's head is already the right one to judge.
- Restack a child on every push to its parent. That's ADR 0004's rejected rebase-on-every-base-move, costing a full Run each time.
- Leave a retargeted child alone until Merge's ADR 0004 behind-check rebases it. A whole Run would judge a diff that still contains the parent's changes.

## Consequences

- A base change from outside the Run, without a push, ends the Run as superseded and starts a new one on the same SHA, because the root base and the diff changed. The daemon's retarget plus update ends it as pushed.
- The update after a retarget counts as a rebase made by slopwatch. It doesn't touch the Fix round cap, doesn't reset the Watched PR's Budget, and doesn't count toward ADR 0004's 3-rebase Escalation.
- A parent closed without merging leaves its child on a dead branch. The daemon raises an Escalation on the child and doesn't retarget it, since the developer may want to drop it.
- Opting in stays per PR. Labelling the bottom PR doesn't watch the rest of the Stack. A parent that isn't watched, or isn't the developer's, still makes the child stacked.
- The Watched PR list shows a Stack as a tree, with children indented in stack order: `stackEntry.position` for native stacks, the base chain otherwise. An unwatched parent shows as a dim row you can't select.
- The `MERGE` choice after a squash relies on git's 3-way merge seeing identical changes on both sides. GitHub doesn't document what server-side `REBASE` does with already-applied patches, so this stays unverified until a test against a real squash-merged stack.
