# Fix commits land through the GitHub API, never `git push`

Amended by ADR 0004: the daemon may also rebase a PR branch, through `updatePullRequestBranch`.

The daemon commits a Fix Step's worktree changes with GraphQL `createCommitOnBranch`, authenticated with the user's `gh` token. It never runs `git push`. GitHub signs API commits, so they come out Verified (tested with a `gh` OAuth token) and pass "require signed commits" without the daemon touching the user's signing key. `expectedHeadOid` makes the write a compare-and-swap. A branch that moved while Fix ran fails with `STALE_DATA`, and nothing gets written. The mutation also returns the new SHA, which the daemon journals as its own push before any poll sees it.

## Considered options

- `git push` from the daemon's clone. The commits are unsigned unless the daemon borrows the user's signing key, so repos that require signed commits reject them.
- A slopwatch GitHub App as the pusher. It gets its own rate budget and a bot identity, but each repo or org has to install it. It doesn't help with the last-push approval rule either, because a PR author can never approve their own PR. It stays the path for teams.

## Consequences

- The mutation can't express file modes, symlinks or submodules. A Fix diff that touches them fails the Step and raises an Escalation.
- GitHub sets the commit author to the token owner. slopwatch shows up only through `Slopwatch-Run` and `Co-authored-by` trailers.
- The daemon trusts only its SHA journal to recognize its own pushes, never the trailer, because rebase and cherry-pick copy trailers. An amended or rebased Fix commit counts as an outside push.
- Large diffs risk GraphQL's 10-second timeout. A Fix that big should escalate anyway.
