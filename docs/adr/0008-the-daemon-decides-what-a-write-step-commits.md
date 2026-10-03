# The daemon decides what a write Step commits

The daemon doesn't know about Fix. It knows Steps that declare `workspace: write`, and every commit rule is keyed on that, so a third-party fixer or a formatter Step follows the same rules as the built-in Fix Plugin. The daemon commits a write Step's diff only when the Step reports `pass` and the diff isn't empty. It refuses the whole commit if the diff touches a Guarded path: `.github/**`, other CI configs, lockfiles and `.slopwatch/**`. A Pipeline can unguard lockfiles, but never workflows or the Pipeline file. The Step then ends `error(guarded_path)` with an Escalation listing the files. A fixer that can edit what judges it can pass the Gate by weakening the Gate, and the Plugin can't be trusted to guard itself.

## Considered options

- Put the rules in the Fix Plugin. Any other write Step would then bypass them, and so would a Fix Plugin with a bug.
- Drop the guarded files and commit the rest. That lands a diff neither the agent nor the developer saw as a whole.
- Commit whatever a write Step leaves behind, then let the next Run judge it. Prior art (CodeRabbit Autofix) shows this pushes fixes that already failed their own checks.

## Consequences

- The Fix round cap is one Pipeline setting (`fix_rounds`, default 3, ceiling 10) that counts commits from any write Step. Reaching it skips write Steps with reason "round cap" and raises an Escalation.
- The loop also stops when a write Step's diff is empty ("nothing actionable") or would recreate a tree from earlier in the same streak ("loop detected"). Nothing detects a Fix that made things worse. The cap bounds that, and nothing gets reverted automatically.
- A Pipeline can have several write Steps. The first commit ends the Run, and the others are cancelled with their diffs thrown away.
- The CI Step reruns a failed GitHub Actions job once per SHA through a new `rerun` Effect before it reports `fail`, so a flake never reaches a write Step. Checks outside Actions can't be rerun with the user's token.
- Fix can't resolve rebase conflicts. `createCommitOnBranch` (ADR 0002) only appends a single-parent commit, which leaves the conflict in place on GitHub.

## What the build settled

[#74](https://github.com/jnsdls/slopwatch/issues/74) built the commit rules and the `fix` Plugin.

- A write Step that reports `pass` keeps its Verdict back until its process exits. The daemon then reads the worktree into a tree in the repo's clone, with a scratch index built from the head, so whatever the Step did to the worktree's own index or `HEAD` doesn't count. Files `.gitignore` names stay out of the commit.
- Guarded paths are `.github/`, `.slopwatch/`, `.circleci/`, `.buildkite/`, `.woodpecker/` and `.gitlab/` directories, the root CI files of GitLab, Travis, Drone, Woodpecker, AppVeyor, Azure Pipelines, Bitbucket, Jenkins and Cloud Build, and lockfiles by name anywhere in the tree. Case is ignored. `guard_lockfiles: false` at the top of the Pipeline unguards lockfiles and nothing else.
- `createCommitOnBranch` keeps the mode of a file it rewrites. Checked live: an edit to a `100755` file stayed executable, and GitHub made the same tree the clone did. A new executable file, a mode change, a symlink, a submodule or a type change fails the Step as `error(unsupported_change)`.
- The commit message heads with the first line of the Step's note and ends with `Slopwatch-Run: <run id>` and `Co-authored-by: slopwatch <noreply@slopwatch.invalid>`. The `.invalid` domain is reserved, so no GitHub account can own the address and pick up the attribution. It stays until a slopwatch domain or App bot exists.
- Commits are journaled in their own `commits` table, not the Effect intent table, because no Step asks for them. The row holds the expected head and the tree, and finishing it writes the push journal and the Run's `committed` event in one transaction. A row a crash left open is settled on the next sync, before a moved head is read. The PR's head counts as the daemon's commit when its only parent is the expected head and its tree is the expected tree.
- An empty diff and a repeated tree keep the Step's `pass` with the reason "nothing actionable" or "loop detected". The "Fix stopped" PR entry names that reason, or the round cap, then lists the failing Gate terms.
- A write Step never reuses an earlier Outcome. Its work is the changes it leaves, which an Outcome doesn't hold.
- Rebases and branch updates slopwatch made neither count toward the round cap nor reset it, as [#75](https://github.com/jnsdls/slopwatch/issues/75) asked. Same-SHA Runs continue the streak.
