# A Stack lands one PR at a time from the bottom, and the daemon retargets what's left

Every PR in a Stack runs its own Pipeline as soon as it is watched, judged against its parent's head, so the whole Stack gets feedback early. Only the bottom PR can merge. A Merge Step on a PR whose base is another open PR's head ends `inconclusive` with a "stacked on <parent>" Finding and no Escalation. Once the bottom PR lands, the next PR gets retargeted, which ends its Run and starts a new one, and that Run's Merge lands it. Each PR's own Gate judges it, and its own Merge Step lands it. slopwatch never merges a child into its parent's branch.

Merge requests go through GitHub's async merge API (`PUT /pulls/{n}/merge-async`) for every PR, stacked or not, with `sha` set to the judged head and `merge_action: default`. That one call merges directly or enqueues, so it replaces both `mergePullRequest` and `enqueuePullRequest` from [Merge ownership](https://github.com/jnsdls/slopwatch/issues/16). `mergePullRequest` doesn't support native stacks. The daemon polls the returned request until it reports `merged`, `enqueued` or `failed`. `enqueued` is final, so after it the daemon watches the PR's merged state as it already does for queues.

When a parent merges outside a native stack, the daemon retargets each child to the parent's base with `updatePullRequest(baseRefName)` right away. GitHub only retargets a child when the parent's head branch is deleted, it's unclear whether its auto-delete fires for API and queue merges, and deleting the branch without a retarget closes the child for good. Then, before the next Run starts, the daemon updates the child's branch with `updatePullRequestBranch`. It uses `MERGE` if the parent was squash-merged or rebase-merged, because a rebase would replay the parent's original commits onto a base that already has copies of them, and `REBASE` only after a merge commit, which put the originals on the base. The daemon tells them apart by the parent's merge commit: two parents means a merge commit. ADR 0004's signed-commit rule still forces `MERGE` where it applies. Native stacks rebase and retarget their upper layers themselves, so the daemon only does this for non-native Stacks.

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
- The Watched PR list shows a Stack as one summary row, which expands to a tree with children indented in stack order: `stackEntry.position` for native stacks, the base chain otherwise. An unwatched parent shows as a dim row you can't select. [#115](https://github.com/jnsdls/slopwatch/issues/115) added the summary row.
- The `MERGE` choice after a squash relies on git's 3-way merge seeing identical changes on both sides. [#76](https://github.com/jnsdls/slopwatch/issues/76) tested it against real squash-merged stacks (see "What the build showed"). It comes out clean unless the child changed what the parent changed, and `REBASE` fails even then.

## What the build showed

[#76](https://github.com/jnsdls/slopwatch/issues/76) built this and checked it in `jnsdls/slopwatch-sandbox`, a personal private repo, with two-PR stacks whose parent added a file in two commits.

- After a squash merge and a retarget, `MERGE` comes out clean when the child doesn't touch what the parent changed. GitHub signs the merge commit, and the child's diff against the new base is only its own change.
- A child that edits a line its parent added conflicts. GitHub marks it `CONFLICTING` as soon as it is retargeted, before any update, and refuses `MERGE` with "merge conflict between base and head". The same happens after a rebase merge. The squash or rebase made new copies of the parent's commits, so the merge base predates both sides' copy of the file. The daemon raises a PR entry and waits for the developer, since no Step can resolve a conflict (ADR 0004).
- `REBASE` after a squash fails even for the clean child, with "rebase conflict between base and head", so `MERGE` is the only method that works there. After a merge commit, `REBASE` is clean even for a child that edited the parent's line, and the child ends up with only its own commit.
- The whole daemon landed a three-PR Stack bottom-up: it squash-merged the bottom, retargeted and updated the middle with `MERGE`, ran it again and merged it, then did the same for the top. Each upper PR's first Run ended shippable with Merge `inconclusive`.
- A personal repo has no native stacks. `stackEntry` reads `null` there, so the native path is tested only against the fake GitHub.

The build made these calls:

- The retarget and the update share one intent row, recorded before the first call and moved on after each. The child's Run ends as pushed when the daemon takes the row on. While the row is open, the child gets no Run, so nothing judges the retargeted but not yet updated diff. The row closes once a poll shows the head GitHub pushed. After a crash, the first sync makes the calls again from the stage the row reached. A retarget onto the base the child already has succeeds.
- The daemon only retargets Watched PRs. It never changes a PR the developer hasn't opted in.
- A refused retarget or update raises a PR entry and holds the child. A push to it makes the daemon try again on the new head, so the update still comes before any Run. Moving the child to another base by hand ends the daemon's part. If the base is already up to date with the child, the daemon skips the update, and the base change starts a same-SHA Run. The Run the retarget ended still reads pushed then.
- A child that GitHub retargeted itself, after the parent's branch was deleted, still gets its update. The daemon looks for the merged parent whether the child sits on the parent's branch or on the parent's base. A child the developer moved onto the parent's base while the parent was still open is theirs, and the daemon leaves it alone.
- Until the daemon knows what became of a parent that left the poll, the child still counts as stacked on it, so its Merge Step doesn't land it. That covers a parent that closed unmerged, too, after the developer pushes to the child.
- A child whose parent closed unmerged starts no same-SHA Run, such as for a Pipeline change, until the developer moves it. A push still starts a Run, and that Run closes the entry.
- The poll reads each PR's parent and the parent's base. The root base walks down through the developer's own PRs. A parent that isn't the developer's ends the walk at that parent's base, so a Stack with two other people's PRs in a row below a Watched PR reads the Pipeline from the higher one's base.
- The Run that a retarget ends has no `rebase` Effect, so it ends ADR 0004's rebase streak rather than counting toward it.
