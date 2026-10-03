# A Run reads the Pipeline from the base branch, and GUI edits land as a PR

The Pipeline file (`.slopwatch/pipeline.yml`) that judges a PR is the one at the head of the PR's root base when the Run starts. That is the PR's base branch, or for a PR in a Stack, the base of the Stack's bottom PR. The Run records that base SHA. A PR that edits the Pipeline file gets judged by the old Pipeline, and its edit takes effect once it merges. The GUI is the main editor, but the daemon never touches the developer's checkouts. GUI edits therefore collect in a draft the daemon keeps. Publishing replays the draft's edits onto the current file and commits the result with `createCommitOnBranch` to one long-lived `slopwatch/pipeline` branch per repo, then opens or updates a PR from it. The developer merges that PR like any other.

## Considered options

- Read the Pipeline from the PR's head, as GitHub Actions does for `pull_request`. A Pipeline edit would test itself, but any PR could delete the Gate terms it fails and pass. That's harmless for one developer and wrong once a team shares the repo.
- Read a stacked PR's Pipeline from its own base branch, which is its parent's head. An unmerged Pipeline edit in the parent would then judge the child, which is the same hole as reading from the head.
- Read it from the default branch whatever the PR's base. That breaks a release branch that needs a different Pipeline, and Conditions on `base` already cover the variation.
- Have the GUI commit straight to the default branch. It's the fastest loop, but branch protection blocks it on many repos, and an edit nobody reviewed starts judging every Watched PR right away.
- Have the GUI only export the file and leave the commit to the developer. Then the GUI isn't the main editor.

## Consequences

- When the Pipeline changes on a root base, every Watched PR on that root base, stacked or not, whose latest Run has ended gets a new Run on the same SHA. Outcome reuse keys on the Step's resolved settings and Plugin version, so only new or changed Steps actually run.
- A draft records the blob SHA it started from. Publishing applies its edits (add a Step, set a key, rewire `needs`) to the file as it is now. If the same node was edited on both sides, publishing stops and shows both versions.
- Editing the file means a lossless tree (`yaml-edit`) that only touches changed nodes, so hand-written comments and ordering survive GUI edits. The daemon loads the file with a separate typed parser, and a round-trip test suite keeps the two in agreement. `yaml-edit` 0.3.2 passed the suite as the tree, not as the editor: its own mutations damage blank lines and comments next to an edit, so core finds nodes in its tree and splices the text at their byte ranges ([#58](https://github.com/jnsdls/slopwatch/issues/58)). YAML stays, and the KDL fallback isn't needed.

## What the build settled

[#81](https://github.com/jnsdls/slopwatch/issues/81) built publishing.

- A node is a Step, the Gate or `fix_rounds`. Each edit touches one. A node counts as changed on the branch when the loader reads it differently, so a comment doesn't count. A node both sides changed to the same thing isn't a conflict, and the draft's edits on it are dropped. That's also how a draft notices its PR merged: once the branch has every edit, the draft starts over from it.
- The daemon refuses to publish a Pipeline that wouldn't load, except for Plugins or Library Steps this machine lacks.
- Besides `createCommitOnBranch`, publishing moves the `slopwatch/pipeline` ref through the REST refs API, but only when no PR from it is open: it creates the branch, or resets one left by a merged or closed PR, at the base commit the draft was replayed onto. While a PR is open, the branch only gains commits. If the Pipeline file on the base changed since the branch forked, the daemon first merges the base in with `updatePullRequestBranch` (ADR 0004), so the PR's diff stays the draft's edits. Both commits go in the push journal.
