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
