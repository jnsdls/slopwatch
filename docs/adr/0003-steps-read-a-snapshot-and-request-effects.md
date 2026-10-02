# Steps read a PR snapshot and request Effects; they never hold a GitHub token

A Step never talks to GitHub. At the start of a Step, the daemon hands it a snapshot of the Watched PR pinned to the Run's head SHA: title, body, linked issue, diff, files, checks, reviews and comments. It pushes updates over the Step's stdio session as its own poll sees them. A Step that wants to change something on GitHub (comment, label, merge) sends an Effect request. The daemon performs it with the user's token, journals it, and drops it if the Run is no longer current. Committing stays with the daemon, as in ADR 0002.

## Considered options

- Give Steps the user's `gh` token. It's simpler for plugin authors, but every Step process then holds a credential that can push and merge, each Step spends the user's 5,000 points/hour on its own polling, and a Step from a superseded Run can still comment or merge on the new head.
- No GitHub side effects at all in v1. That would make Merge a special case outside the Step contract.

## Consequences

- A Step that needs GitHub data the snapshot lacks can't fetch it. The fix is a wider snapshot, which means a protocol feature string.
- Merge is an ordinary Step that requests the `merge` Effect. The daemon's head-SHA check keeps it from merging a SHA the Gate never saw.
- The Effect list is closed and small in v1: `comment`, `label`, `rebase`, `merge` (`rebase` added by ADR 0004). New Effects are protocol additions, not plugin code.
