# Clients speak WebSocket JSON to the daemon, over a Unix socket in v1

The daemon listens on a Unix socket in `~/Library/Application Support/slopwatch/` and speaks WebSocket with JSON frames on it. A hosted daemon later adds a TCP listener with TLS and keeps the same frames, so the GUI, the future MCP server and any CLI never learn a second framing. The first frame is `hello`. It carries a dialect number that must match exactly, feature strings either side may ignore, the build id, and an `auth` field. v1 accepts only the `local` scheme: Unix socket only, with the peer's uid checked by `getpeereid` against the daemon's own. A network listener adds a `token` scheme without changing any frame. The versioning follows tty7.

## Considered options

- Plain JSONL over the Unix socket now, WebSocket once there's a network. Two framings would have to stay in sync, and the remote one would get no testing until hosting.
- WebSocket on loopback TCP from day one. Any local process could connect, so token auth would be needed in v1 for no gain.
- No auth field until hosting, as zeron's local WebSocket does. Adding it later would change the first frame every client sends.

## Consequences

- Messages are requests and responses with ids, plus subscriptions. A client subscribes to a topic (`inbox`, `watched_prs`, `run/<id>`, `pipeline/<repo>`), gets a snapshot, then ordered deltas with sequence numbers. On reconnect it resubscribes with its last sequence number. Run topics replay from the Run's event journal, and other topics resend a snapshot.
- Several clients can connect at once. Commands carry an actor field, so answering the Inbox looks the same from the GUI or anywhere else.
- A dialect mismatch refuses the connection and tells the client to restart the daemon (ADR 0009). Additive changes, such as a wider PR snapshot (ADR 0003), are feature strings.
- The daemon never assumes a client shares its filesystem. Logs, diffs and evidence travel over the protocol, not as paths.
