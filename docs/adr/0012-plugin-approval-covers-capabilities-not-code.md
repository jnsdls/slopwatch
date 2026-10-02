# Plugin Approval covers what the manifest asks for, not the code

A third-party Plugin is an executable, or a symlink to one, that the developer puts in `~/.config/slopwatch/plugins/<name>` on the daemon's machine. The daemon doesn't fetch Plugins in v1. On finding one, the daemon runs `describe` with no Secrets, a scrubbed env, an empty temp dir and a 5 s timeout, then checks the manifest. A Plugin runs only after an Approval that covers its `workspace`, its Effects and the Secrets it names. Approval is the Secret grant. The Approval belongs to the Plugin's name, so a new build of the same Plugin keeps running. A manifest that asks for more, such as `write`, `merge` or a new Secret, pauses the Plugin until it's approved again. Built-in Plugins ship with an Approval through the same record, so the rules don't special-case them.

## Considered options

- Pin the Approval to a content hash. Any rebuild would need approving again, which nags the person developing a Plugin. It also protects little, because without an OS sandbox (ADR 0003) an approved Plugin can already do anything the user can do.
- Approve each capability on its own, and fail a Step that uses one it lacks. That's finer control for one developer than they need, and it needs more UI.
- A static manifest file next to the executable, so nothing runs before Approval. Putting the file in place already counts as consent to execute it, and `describe` is already the manifest's single source (Step contract).

## Consequences

- Approval limits only what goes through the daemon: Secrets, the worktree and Effects. Network access isn't declared, because nothing could enforce it.
- The version that Outcome reuse keys on is the manifest version plus a hash of the resolved executable, re-hashed at each spawn. A rebuild without a version bump still reruns, but a script's imports outside the executable go unnoticed.
- A Step whose Plugin lacks an Approval gets `error(plugin unapproved)` without spawning, and downstream Steps follow their Conditions. The daemon raises one shared Escalation per Plugin, and approving from that Inbox entry offers a same-SHA rerun.
- Built-in names are reserved. A third-party Plugin with one fails to load.
