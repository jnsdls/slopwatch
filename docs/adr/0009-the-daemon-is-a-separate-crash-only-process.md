# The daemon is a separate, crash-only process

The daemon always runs as the launchd login agent registered from the `.app` (`SMAppService.agent`), and the GUI is only ever a client of it. It has no graceful shutdown path. Whether it crashed, the Mac rebooted or an update replaced the binary, it comes back the same way. It does one poll first. If a Run's head moved while it was down, that Run ends (pushed if the new SHA is in the push journal, superseded otherwise). Otherwise it keeps the settled Outcomes and respawns the interrupted Steps from scratch in the same Run. An update is just a restart. When the GUI sees a different build id in `hello`, it sends `restart`, and the daemon kills its Step process groups and exits. The GUI then calls `unregister` on the agent, waits for the completion handler, and calls `register`, so launchd starts the new binary. One recovery path that runs on every update stays tested. A second path that only runs after a crash would rot.

## Considered options

- Run the engine inside the GUI when no agent is running, as zeron's headed mode does. Two engines could then race over one state store, and notifications stop when the GUI quits.
- Drain before exiting: start no new Steps and wait for running ones to settle. Agent Steps can run for 20 minutes or more, so updates would lag, and the drain code would be one more path to keep correct.
- End an interrupted Run as cancelled and start a new Run on the same SHA. It does the same work, but every reboot leaves a cancelled Run in Run history, and cancelled means the developer cancelled it.

## Consequences

- Respawning an interrupted Step isn't an auto-retry, because the Step never reported. A Step interrupted by two restarts in a row gets `error(daemon_restart)` and an Escalation, so a Step that crashes the daemon can't loop it.
- Every Effect gets an intent row (Effect, expected head OID, and the intended tree for a commit) before the daemon calls GitHub, and is marked done after. On restart the daemon reconciles open intents against GitHub. A commit whose parent is the expected OID and whose tree matches goes into the push journal. Otherwise a lost SHA would read as an outside push, reset the Fix round count and give the Run the wrong end reason. Comments carry a hidden `<!-- slopwatch:effect=<id> -->` marker so the daemon can find them before reposting. Merge reads PR state, label and rerun are redone, and rebase puts the PR back in its rebasing state.
- Each Step runs in its own process group, and the daemon records the pgid and process start time. On restart it kills leftover groups (the start time guards against PID reuse) and deletes the interrupted Steps' worktrees.
- An update throws away the work of any Step in flight. Updates are rare, so that costs little.
- The daemon holds an exclusive `flock` on its data dir. Dev builds use their own data dir, socket, launchd label and bundle ID (`com.jnsdls.slopwatch.dev`), because Background Task Management keys its records on bundle ID, Team ID and label.
- Re-registering on every update follows `SMAppService.h`, which says an app "must" re-register an agent after its executable changes "or it may not launch", and recommends `unregister` first. Apple DTS has said in one forum thread that killing the old process is enough. The header wins until an N to N+1 update test shows otherwise. That test should also show whether re-registering brings back the "Background Items Added" notification. Retrying only when no new `hello` arrives would be a second path that rarely runs, and that is what this ADR avoids.
