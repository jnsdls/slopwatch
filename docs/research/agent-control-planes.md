# Agent control planes: zeronsh/zeron and l0ng-ai/tty7

Research for [#19](https://github.com/jnsdls/slopwatch/issues/19), checked on 2026-09-30 against zeron `main` at [`ed3b1aa`](https://github.com/zeronsh/zeron/tree/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5) and tty7 `main` at [`4e4789a`](https://github.com/l0ng-ai/tty7/tree/4e4789a045f03176885388555057ffebb85352b9). Links below point at those commits.

## Answer

Neither overlaps slopwatch much. Both are places where a human drives agents interactively, before a PR exists. Zeron is a chat client for many agents with multi-device sync. tty7 is a terminal whose shells live in a background server, with agent status bolted on through hooks. Neither runs anything against a PR once it is open. Zeron shows a PR badge per checkout. tty7 has a read-only GitHub panel that folds checks, reviews and merge state into one "readiness" value. That fold is the closest thing to a Gate in either codebase, and it is display only.

The architecture is where they are useful. Both are Rust daemon plus GPUI client, which is the shape #1 already settled on, and both hit problems slopwatch will hit:

- **Zeron** runs one binary, headed or headless. The UI talks typed RPC to the engine over a loopback WebSocket, or over an in-memory channel carrying the same frames when the engine runs in-process. Its `Harness` trait turns eight agent CLIs into one stream of `AgentEvent`s. That is the closest prior art for slopwatch's agent Steps.
- **tty7** splits a framework-free `tty7-core` crate (protocol, daemon, domain model) from the GUI. The same server binary runs on remote machines over its own SSH client. Its protocol code is full of hard-won lessons about versioning a daemon wire format that peers of different ages must speak.

GPUI sourcing: zeron uses its own extracted GPUI fork (`zeronsh/zui`) at a pinned rev. tty7 pins a Zed git rev and then redirects it with `[patch]` to its own Zed fork branch (`l0ng-ai/zed`, branch `tty7`). Both also fork a component library. Neither uses crates.io GPUI or `gpui-pre`.

Licenses: zeron is MIT, tty7 is Apache-2.0. Both are compatible with borrowing ideas and code, with attribution.

## Zeron

**What it is.** "A native control plane for Claude Code, Codex, Cursor, Devin and other coding agents" ([README](https://github.com/zeronsh/zeron/blob/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5/README.md)). You start a chat with an agent in a workspace, watch the transcript stream, steer or interrupt, and optionally follow the same session from another device. MIT, 2.5k stars, created 2026-07-20, active daily. [ARCHITECTURE.md](https://github.com/zeronsh/zeron/blob/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5/ARCHITECTURE.md) says it is a Rust rewrite of an earlier Electron app of the same name.

**Overlap with slopwatch.** Low. The unit of work is a chat, and a human starts every one. There is no pipeline, no gate, no CI handling, no automated fix loop. PR awareness is limited to a sidebar badge. The engine resolves "is there a PR for this checkout's branch" by shelling out to the user's `gh` ([source_control.rs#L183](https://github.com/zeronsh/zeron/blob/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5/crates/engine/src/source_control.rs#L183)). It caches the answer for 2 minutes when a PR exists and 45 seconds when none does, with backoff from 20 seconds to 15 minutes on failure. Polling runs only while a UI subscription is live ([change_requests.rs#L23](https://github.com/zeronsh/zeron/blob/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5/crates/engine/src/change_requests.rs#L23)). The PR model is state plus refs, with nothing about checks or reviews ([entities.rs](https://github.com/zeronsh/zeron/blob/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5/crates/proto/src/entities.rs#L793-L829)). A search of issues for "pull request" and "CI" turned up nothing about PR automation.

**Daemon/client split.** One binary, `zeron` ([ARCHITECTURE.md §1](https://github.com/zeronsh/zeron/blob/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5/ARCHITECTURE.md)):

- `zeron` (headed) connects to a daemon if one is listening on the IPC port. Otherwise it runs the engine in-process and also serves it on the IPC port, so other viewports can attach.
- `zeron headless` runs only the engine. A VPS can run this while a laptop's UI drives it.
- `zeron daemon install` sets up launchd on macOS. The Linux installer keeps it running across reboots.

The in-process mode uses "RPC over an in-memory duplex, same protocol, zero serialization shortcuts, so the boundary stays honest". That is the trick worth copying: slopwatch can ship one app that embeds the daemon for v1 and still keep the network protocol honest.

**IPC.** `zeron-rpc` ([lib.rs](https://github.com/zeronsh/zeron/blob/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5/crates/rpc/src/lib.rs)) is hand-rolled request/response plus streams, as ndjson envelopes over a WebSocket on `127.0.0.1` (default port 27654, [mcp/lib.rs](https://github.com/zeronsh/zeron/blob/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5/crates/mcp/src/lib.rs)):

- client sends `{id, method, params}` to call, `{id, cancel: true}` to stop a stream
- server replies `{id, ok}` or `{id, err}`, or streams `{id, item}`* then `{id, done: true}`

Method names live in one `methods` module shared by both ends. Local auth is thin. The server rejects any handshake carrying an `Origin` header so a browser page can't dial it, and that is all ([server.rs#L156](https://github.com/zeronsh/zeron/blob/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5/crates/rpc/src/server.rs#L156)). Cross-device traffic goes through a Cloudflare Durable Object relay, with Loro CRDT docs for transcripts and the workspace registry. Slopwatch doesn't need that for v1.

`zeron mcp` exposes the engine as an MCP server over stdio, proxying into the same local WebSocket. The engine injects it into agents it launches, so one agent can message another chat. This is a cheap way to let agents query or drive the daemon.

**Agent abstraction.** The [`Harness` trait](https://github.com/zeronsh/zeron/blob/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5/crates/harness/src/lib.rs#L83) has one core method:

```rust
async fn run(&self, request: RunRequest, controls: RunControls)
    -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError>;
```

The stream ends with `AgentEvent::Done`. Around it sit capability flags (`supports_steering`, `deterministic_turn_end`, `authoritative_prompt_end`), `installed()` and `executable_path()`, and model, command and skill catalogs. [`AgentEvent`](https://github.com/zeronsh/zeron/blob/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5/crates/proto/src/agent.rs#L484) covers session start, text and reasoning deltas, tool calls and results with optional diffs, and context usage. Drivers use whatever wire each agent speaks. Claude Code uses `stream-json` over a subprocess. Codex uses its app-server JSON-RPC. opencode uses `opencode serve` over HTTP/SSE. Pi uses its own JSONL RPC. Devin, Grok, Hermes and Antigravity use ACP ([agent.rs#L7](https://github.com/zeronsh/zeron/blob/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5/crates/proto/src/agent.rs#L7)).

Lessons for slopwatch Steps:

- The flag `deterministic_turn_end` exists because ACP-mediated agents don't reliably say when they're done, so the engine keeps a quiesce watchdog for them. Agent Steps need the same: a Step outcome can't depend on the agent announcing completion.
- A 10-minute stall watchdog and a run journal with resumable `seq` replay give crash recovery. On restart the engine stamps interrupted runs `aborted` ([ARCHITECTURE.md §5](https://github.com/zeronsh/zeron/blob/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5/ARCHITECTURE.md)). A Run log needs the same, since the daemon will die mid-Step.
- `compose_child_path` merges the login shell's `PATH` into child processes. A daemon started by launchd doesn't inherit the user's shell `PATH`, so `claude`, `gh`, `node` and npm shims go missing. Slopwatch will hit this on day one.
- Zeron's Harness is a Rust trait, compiled in. Slopwatch's Step is an external process with a JSON protocol, which is more open. Zeron's `AgentEvent` enum is still a good starting point for the event half of that protocol.

**Worktrees.** The engine owns worktrees under `~/.zeron/worktrees`, drives git through a subprocess rather than libgit2, and caps diff capture at 3 MiB. That matches the "daemon owns its own clones and worktrees" decision.

## tty7

**What it is.** "The terminal that outlives its window." A GPU terminal whose shells belong to a background server, so quitting the app kills nothing ([README](https://github.com/l0ng-ai/tty7/blob/4e4789a045f03176885388555057ffebb85352b9/README.md)). It detects 26 coding CLIs, shows working, waiting or done per pane, and has a CLI (`tty7 split`, `send`, `wait`, `capture`) so one agent can drive another. Apache-2.0, 1.2k stars, created 2026-07-06, active daily.

**Overlap with slopwatch.** Low, with one near miss. tty7 doesn't wrap agents at all: "The agent you start is the real one, running in an ordinary PTY." It has no pipeline, no automation against PRs, and no writes to GitHub. It does have a read-only GitHub tab in the side panel that lists PRs with checks, reviewers and files ([core/github](https://github.com/l0ng-ai/tty7/tree/4e4789a045f03176885388555057ffebb85352b9/crates/tty7-core/src/core/github)). Its [`readiness()`](https://github.com/l0ng-ai/tty7/blob/4e4789a045f03176885388555057ffebb85352b9/crates/tty7-core/src/core/github/model.rs#L215-L261) folds `mergeStateStatus`, check counts and review states into one of `Ready`, `Conflicts`, `ChecksFailing(n)`, `WaitingOnChecks(n)`, `ChangesRequested`, `ReviewRequired`, `Behind`, `Blocked`, "the way GitHub's merge box leads with its most pressing reason". The headless server never talks to GitHub. Only the GUI does.

**Daemon/client split.** `tty7-core` is framework-free and shared by the GUI binary and a lean static `tty7-server` that the GUI pushes to remote machines over SSH ([Cargo.toml](https://github.com/l0ng-ai/tty7/blob/4e4789a045f03176885388555057ffebb85352b9/Cargo.toml)). The Cargo comments explain feature gating in detail. For example, `keyring` sits in the GUI crate so the static server doesn't link `zbus` and 30-odd crates it can never use. Remote is the local case with the server on another machine. The window only shows state, and no files sync ([concepts](https://github.com/l0ng-ai/tty7/blob/4e4789a045f03176885388555057ffebb85352b9/docs/getting-started/concepts.mdx)).

**IPC.** A Unix socket at `<config_dir>/daemon.sock` (with a fallback path when that is too long), or a port file on Windows ([transport.rs](https://github.com/l0ng-ai/tty7/blob/4e4789a045f03176885388555057ffebb85352b9/crates/tty7-core/src/daemon/transport.rs)). Frames are a little-endian `u32` length, a one-byte kind, then a JSON payload ([protocol.rs](https://github.com/l0ng-ai/tty7/blob/4e4789a045f03176885388555057ffebb85352b9/crates/tty7-core/src/daemon/protocol.rs)). A router multiplexes channels over one link to remote servers. Versioning is two-part:

- a dialect number (`PROTOCOL_VERSION = 6` for panes, `CONTROL_VERSION = 12` for control) that must match exactly, or the peer is refused
- feature strings (`resize-echo`, `handoff`, `size-lease`) for additions a peer can safely ignore

The comment above [`CONTROL_VERSION`](https://github.com/l0ng-ai/tty7/blob/4e4789a045f03176885388555057ffebb85352b9/crates/tty7-core/src/daemon/control.rs#L12-L64) records what went wrong. New request variants shipped without a bump, older servers answered the hello, then dropped the link on the first unknown call. That left remote workspaces with no tabs. The number is also baked into the remote server's filename (`tty7-server-c{control}p{protocol}`), so a mismatch makes the client install a matching server. Slopwatch's daemon protocol must work over a network with a hosted daemon later, so it needs this from the first version.

Other daemon details worth noting: a singleton seat that decides who may clear a stale socket, avoiding a restart race ([transport.rs](https://github.com/l0ng-ai/tty7/blob/4e4789a045f03176885388555057ffebb85352b9/crates/tty7-core/src/daemon/transport.rs)), and in-place daemon upgrade via `execve` handoff on Unix that keeps PTYs alive.

**Agent abstraction.** None in the Step sense. tty7 detects agents by process and learns their state from hooks it installs into each agent's own config. The hook command writes an escape sequence to the pane's controlling TTY, and the terminal parses status out of the byte stream ([agent_hooks.rs](https://github.com/l0ng-ai/tty7/blob/4e4789a045f03176885388555057ffebb85352b9/crates/tty7-core/src/core/agent_hooks.rs)). Status is `working`, `waiting` (needs input), `done`, or `idle`. The [status docs](https://github.com/l0ng-ai/tty7/blob/4e4789a045f03176885388555057ffebb85352b9/docs/agents/status.mdx) list per-agent gaps. `cursor-agent -p` fires no turn hooks, Crush has no turn boundary, and Antigravity has no permission event. That confirms that headless agent runs need their own completion signal. Hooks aren't enough.

## GPUI sourcing

| | zeron | tty7 |
| --- | --- | --- |
| `gpui`, `gpui_platform` | `zeronsh/zui` at rev `18a89af` ([Cargo.toml#L79](https://github.com/zeronsh/zeron/blob/ed3b1aae4a5189eef67143db7b8c5c3ee7a933c5/Cargo.toml#L79)) | `zed-industries/zed` rev `1d217ee`, redirected by `[patch]` to `l0ng-ai/zed` branch `tty7` ([Cargo.toml#L453](https://github.com/l0ng-ai/tty7/blob/4e4789a045f03176885388555057ffebb85352b9/Cargo.toml#L453)) |
| Tokio bridge | `gpui_tokio` from zui | none, uses `smol` |
| Component library | `zeronsh/gpui-component` (`gpui-base` only, for the text input `EditorState`) | `l0ng-ai/gpui-component` branch `tty7`, a fork of `longbridge/gpui-kit` |
| Zed GPL crates | explicitly avoided (`markdown`, `ui`, `theme`, `editor`) | not used |

[zui](https://github.com/zeronsh/zui) is `gpui`, `gpui_platform`, `gpui_tokio` and their in-tree dependencies extracted from a Zed fork into a standalone repo with no shared git history. Upstream commits are ported with `git format-patch | git apply --3way`. Its patches add backdrop blur, edge fades, GPU memory bounds, an image eviction fix, and a macOS 26 blur fix. It removes GPL tracing crates, but its `path` crate stays GPL-3.0. Zeron's lockfile doesn't pull `path`, so zeron ships clean.

tty7's Cargo comment lists seven GPUI patches: text layout under truncation, macOS menu key equivalents, paint order for aligned text, and others. It explains why it uses `[patch]` instead of swapping the pin. `gpui-component` declares its own upstream `gpui`, so a plain pin swap would put two incompatible copies of GPUI in the tree. `[patch]` rewrites the source for every dependent at once. tty7 also forks `alacritty_terminal` and `russh`, pinned by exact rev.

Both confirm what [#2](https://github.com/jnsdls/slopwatch/issues/2) found. Serious GPUI apps end up carrying a fork. If slopwatch starts on a plain Zed rev or `gpui-kit`, it should use tty7's `[patch]` layout so a fork can slot in later without touching every dependent.

## What to borrow

1. **Headed or headless, one binary, same protocol in-process.** Zeron's in-memory duplex that runs the real frames lets v1 ship a single app while the daemon boundary stays network-ready.
2. **Protocol versioning from day one.** tty7's dialect number plus feature strings, and its rule that any new request variant bumps the dialect.
3. **Framework-free core crate.** tty7's `tty7-core` rule: nothing that needs GPUI goes in the daemon's crate, and optional heavy deps are feature-gated out of the daemon.
4. **Agent events.** Zeron's `AgentEvent` enum and its capability flags as a starting point for the agent-Step side of the Step protocol, plus a stall watchdog for agents without a reliable end-of-turn signal.
5. **Run journal with replay.** Zeron's resumable `seq` journal and startup recovery that marks interrupted runs `aborted`.
6. **Login-shell `PATH` for children.** Zeron's `compose_child_path`, since launchd-started daemons can't find user CLIs otherwise.
7. **PR readiness fold.** tty7's `readiness()` is a tidy display summary of GitHub's merge box. Slopwatch's Gate decides, not just displays, but the same inputs (`mergeStateStatus`, checks, reviews) are Gate conditions.
8. **Agent-facing MCP.** Zeron's `zeron mcp` stdio server proxying into the local RPC. The same approach would let a Claude Code session ask slopwatch about a Watched PR's Run.

## What not to copy

- Zeron's CRDT sync, Durable Object relay and WorkOS auth. That's multi-device chat, out of scope for v1.
- Zeron's Origin-only guard on the local WebSocket. It's fine for loopback. A daemon that may later be hosted needs real auth in the protocol from the start, even if v1 only uses a local token.
- tty7's hook-and-escape-sequence status channel. It fits interactive PTYs. Slopwatch runs agents headless with JSON output, so it gets status from the agent's own stream.
