# A Run reads the Pipeline from the base branch, and GUI edits land as a PR

The Pipeline file (`.slopwatch/pipeline.yml`) that judges a PR is the one at the head of the PR's base branch when the Run starts. The Run records that base SHA. A PR that edits the Pipeline file gets judged by the old Pipeline, and its edit takes effect once it merges. The GUI is the main editor, but the daemon never touches the developer's checkouts. GUI edits therefore collect in a draft the daemon keeps. Publishing replays the draft's edits onto the current file and commits the result with `createCommitOnBranch` to one long-lived `slopwatch/pipeline` branch per repo, then opens or updates a PR from it. The developer merges that PR like any other.

## Considered options

- Read the Pipeline from the PR's head, as GitHub Actions does for `pull_request`. A Pipeline edit would test itself, but any PR could delete the Gate terms it fails and pass. That's harmless for one developer and wrong once a team shares the repo.
- Read it from the default branch whatever the PR's base. That breaks a release branch that needs a different Pipeline, and Conditions on `base` already cover the variation.
- Have the GUI commit straight to the default branch. It's the fastest loop, but branch protection blocks it on many repos, and an edit nobody reviewed starts judging every Watched PR right away.
- Have the GUI only export the file and leave the commit to the developer. Then the GUI isn't the main editor.

## Consequences

- When the Pipeline changes on a base branch, every Watched PR on that base whose latest Run has ended gets a new Run on the same SHA. Outcome reuse keys on the Step's resolved settings and Plugin version, so only new or changed Steps actually run.
- A draft records the blob SHA it started from. Publishing applies its edits (add a Step, set a key, rewire `needs`) to the file as it is now. If the same node was edited on both sides, publishing stops and shows both versions.
- Editing the file means a lossless tree (`yaml-edit`) that only touches changed nodes, so hand-written comments and ordering survive GUI edits. The daemon loads the file with a separate typed parser, and a round-trip test suite keeps the two in agreement.
