# Every push starts a new Run, and Fix is a terminal Step

A Run executes the Pipeline against one head SHA. Any push ends it, including a push made by slopwatch's own Fix Step, and the next Run starts from the new SHA. We first had Fix loop inside a Run, with the daemon's own pushes continuing it. We dropped that because the Pipeline would need cycles, and outcomes would need SHA-keying rules within a single Run. With this design the Pipeline is a DAG and every outcome in a Run belongs to the same SHA.

## Consequences

- Each Fix push reruns the whole Pipeline, including Jev and agent Steps that passed on the previous SHA. Cost budgets, not Step skipping, are how we'll limit that, because skipping unchanged Steps would bring back state shared across SHAs.
- The Fix round cap counts consecutive Fix-started Runs on the Watched PR, and an outside push resets it. The daemon still has to recognize its own pushes, now to keep that count.
- A Human Step approval covers only its Run's SHA. If Fix pushes after you approve, you approve again on the next Run.
