# Running agent CLIs headless

Question (issue #4): how do Claude Code, Codex and similar agent CLIs run non-interactively from a daemon? This covers structured and schema-constrained output, permission and sandbox modes, auth, cost and token reporting, session resume, timeouts, and whether they can commit and push inside a worktree the daemon owns.

Researched 2026-09-30. Local versions: Claude Code 2.1.285, codex-cli 0.159.0, opencode 1.18.26, cursor-agent 2026.04.17-787b533. Gemini CLI was not installed, so it comes from docs and source only (v0.62.0). No paid prompts were run. Anything marked **unverified** could not be confirmed against a primary source or a local run. Some Codex items came only from a WebFetch page summary and are marked "(summary)".

## Comparison

| | Claude Code `claude -p` | Codex `codex exec` | Gemini CLI `gemini -p` | opencode `opencode run` | Cursor `cursor-agent -p` |
|---|---|---|---|---|---|
| Machine output | `--output-format json` (one result object) or `stream-json` (NDJSON, last line `type:"result"`) | `--json` JSONL events; `-o` writes the last message to a file | `-o json` or `stream-json` | `--format json` JSONL | `--output-format json` or `stream-json` |
| Schema-constrained output | Yes. `--json-schema`, result in `.structured_output`, client-side validation with up to 5 attempts | Yes. `--output-schema <file>`, enforced server-side by the Responses API, result is the final `agent_message.text` as a JSON string | No. Feature request closed "not planned" | Only through `opencode serve` and the SDK (`format: {type:"json_schema"}`), not the `run` CLI | No |
| Cost reporting | USD estimate at list price: `total_cost_usd`, per-model `modelUsage[].costUSD`, plus tokens | Tokens only, in `turn.completed.usage`. No USD | Tokens only | USD `cost` and tokens on each `step_finish` | None documented |
| Budget cap | `--max-budget-usd`, `--max-turns` | None found | `model.maxSessionTurns` (exit 53) | None | None |
| Wall-clock timeout | None documented (**unverified that none exists**) | None found | None | None | None |
| Headless approval default | Mode `default`. Anything that would prompt is denied under `--permission-prompts none` or `dontAsk`. Needs allow rules or `bypassPermissions` for git | `approval_policy` is hardcoded to `never`, and the sandbox defaults to `read-only` | Anything that would ask counts as deny, so shell and git are blocked under `default` | "ask" is auto-rejected unless `--auto`, but `bash` defaults to allow | Changes are only proposed unless `--force` |
| OS sandbox | Opt-in. Seatbelt on macOS, bubblewrap on Linux, Bash only | Always on unless bypassed. Seatbelt on macOS, bubblewrap plus seccomp on Linux | Opt-in (`-s`): Seatbelt, Docker, Podman and others | None | `--sandbox enabled` |
| Can commit in a daemon-owned linked worktree | Yes, with allow rules for git. Docs say the sandbox allows writes to the shared `.git` of a linked worktree. HTTPS push needs `github.com` in `allowedDomains` | **No** under `workspace-write`: `.git` and the worktree's gitdir target are read-only, and network is off. Needs `danger-full-access`, or the daemon commits | Yes without the sandbox, via `--approval-mode yolo` or a policy rule. With the sandbox, probably blocked (**unverified**) | Yes, `bash` is allowed by default | Yes with `--force` or a `Shell(git)` rule. Changelog says sandboxed git writes are allowed. Not checked against a worktree layout |
| Resume | `--resume <id>`, `--session-id <uuid>` to pick the ID up front | `codex exec resume <thread_id>` | `-r <uuid>` | `-s <id>` | `--resume <chatId>` |
| Unattended auth | `ANTHROPIC_API_KEY`, `apiKeyHelper`, or `CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token` | `CODEX_API_KEY` per run, or a stored `auth.json` | `GEMINI_API_KEY` or Vertex with ADC | Provider env vars | `CURSOR_API_KEY` |

What this means for slopwatch:

- Every CLI prints a session ID in its structured output, and every one needs the daemon to enforce its own deadline and kill the process.
- Only Claude Code and Codex give schema-constrained output from the CLI. A Step that needs typed findings from Gemini or Cursor has to prompt for JSON and validate it itself.
- Cost in USD comes from Claude Code (list-price estimate) and opencode. For Codex and Gemini the daemon has to turn token counts into money with its own price table.
- Codex cannot commit inside its default sandbox. Having the daemon commit and push after the agent finishes works for every CLI, and it keeps the push inside slopwatch's control, which also makes "a push made by the Run's own Steps" easy to recognize.
- There is no Rust SDK for any of them. The daemon spawns the CLI and parses JSONL, or talks JSON-RPC to `codex app-server` or HTTP to `opencode serve`.

## Claude Code

Doc base: https://code.claude.com/docs/en/ (shortened to `…/` below). Most facts come from these docs plus local `claude --help` on 2.1.285. The researcher lost shell access partway through, so it did not run `claude <subcommand> --help` or read the CHANGELOG.

### Headless invocation

- `-p/--print`. Stdin is read when piped, capped at 10MB (non-zero exit if exceeded). Exit 0 on success, non-zero on failure. Invalid flags go to stderr. Failures inside the run, such as missing auth, are printed as the result on stdout. `…/headless`
- `--output-format text|json|stream-json`. `json` is one result object. `stream-json` is NDJSON whose last line has `type:"result"`. `--input-format text|stream-json` works only with `-p`. `…/cli-reference`
- `--include-partial-messages` needs `-p` and stream-json, and emits `type:"stream_event"` with `.event.delta.type=="text_delta"`. `--replay-user-messages` needs stream-json on both sides. `--forward-subagent-text` includes subagent text. `--include-hook-events` also exists. `…/cli-reference`, `…/headless`
- `--verbose` with stream-json: every doc example passes it. The hard error "When using --print, --output-format=stream-json requires --verbose" (exit 1) is reported only by third parties, for example https://github.com/bsenel/karakuri/pull/137. **Unverified locally.** Always pass it.
- Stream events:
  - `system/init` carries model, tools, `mcp_servers`, `mcp_server_errors`, `plugins`, `plugin_errors`, `capabilities[]`.
  - `system/api_retry` carries `attempt`, `max_retries`, `retry_delay_ms`, `error_status`, and `error` (for example `authentication_failed`, `rate_limit`, `overloaded`, `billing_error`).
  - `permission_denied` system messages.
  - Subagent messages carry `parent_tool_use_id`. `…/headless`
- Stdin user message for `--input-format stream-json`: `{"type":"user","message":{"role":"user","content":...},"parent_tool_use_id":null}`. `…/agent-sdk/streaming-vs-single-mode`
- Result object (`SDKResultMessage`), `…/agent-sdk/typescript#sdkresultmessage`:
  - Success: `type:"result"`, `subtype:"success"`, `uuid`, `session_id`, `duration_ms`, `duration_api_ms`, `is_error`, `api_error_status?`, `num_turns`, `result` (string), `stop_reason`, `ttft_ms?`, `total_cost_usd`, `usage` (Anthropic `Usage`: `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, `service_tier`, and more), `modelUsage` (`{[model]: {inputTokens, outputTokens, thinkingTokens?, cacheReadInputTokens, cacheCreationInputTokens, webSearchRequests, costUSD, contextWindow, maxOutputTokens, canonicalModel?, provider?, costBasis?}}`), `permission_denials: [{tool_name, tool_use_id, tool_input}]`, `structured_output?`, `deferred_tool_use?`, `terminal_reason?`, `result_index?`, `origin?`.
  - Error: `subtype` is `error_max_turns`, `error_during_execution`, `error_max_budget_usd` or `error_max_structured_output_retries`. Same metadata, no `result`, plus `errors: string[]` and `startup_failure_reason?` (for example `worktree_unverified`, `worktree_resume_refused`).
  - `terminal_reason` values include `completed`, `max_turns`, `budget_exhausted`, `structured_output_retry_exhausted`, `api_error`, `prompt_too_long`, `hook_stopped`.
  - The Python `ResultMessage` has the same fields in snake_case (`model_usage`). `…/agent-sdk/python`
- `usage` covers only the main loop. `modelUsage` and `total_cost_usd` include subagents, so prefer `modelUsage`. `…/agent-sdk/typescript`

### Schema-constrained output

- `--json-schema '<schema>'` works only with `-p` and pairs with `--output-format json`. The result lands in `.structured_output`. `…/headless`
- An invalid schema exits at startup with "Error: --json-schema is not a valid JSON Schema" (v2.1.205+; earlier versions ignored it). `format` is an annotation and is not enforced. Validation uses draft-07. `…/headless`, `…/agent-sdk/structured-outputs`
- Claude Code validates the output client-side and re-prompts on mismatch. `MAX_STRUCTURED_OUTPUT_RETRIES` defaults to 5 (first attempt plus 4 retries). After that the result is `error_max_structured_output_retries`. `…/env-vars`, `…/agent-sdk/structured-outputs`
- A result can be `subtype:"success"` with no `structured_output`. Treat that as a failure. `…/agent-sdk/troubleshooting`
- Whether the mechanism is a synthetic tool or something else is **not documented, unverified**.

### Permissions and sandbox

- `--permission-mode` accepts `default` (alias `manual`), `acceptEdits`, `plan`, `auto`, `dontAsk`, `bypassPermissions`. Under `-p` and the SDK the starting mode is `default`. `…/permission-modes`
  - `dontAsk` denies anything that would prompt. Allow rules, read-only commands and reads inside the working directory still run. The docs recommend it for CI.
  - `auto` uses a classifier and needs Opus 4.6+, Sonnet 4.6+ or Fable on the first-party API. Under `-p`, repeated blocks deny and the run continues.
  - `bypassPermissions` equals `--dangerously-skip-permissions`. It refuses to run as root or under sudo on Linux and macOS unless inside a recognized sandbox. It still honours deny rules, ask rules and the critical-path `rm` check.
  - `plan` keeps its blocks under `-p`.
- `--permission-prompts none` (v2.1.259+) auto-denies anything that would prompt and removes `AskUserQuestion`. Denials appear in `permission_denials`. `…/headless`
- `--permission-prompt-tool <mcp_tool>` lets an MCP tool answer prompts. The run waits up to `MCP_TIMEOUT` for it to connect. Its input and output JSON shape is **not documented on the pages read, unverified**. The SDK equivalent returns `{behavior:"allow", updatedInput?}` or `{behavior:"deny", message, interrupt?}`. `…/cli-reference`, `…/agent-sdk/typescript`
- Rule syntax, `…/permissions`:
  - `Bash(git commit *)`. The space before `*` matters. The legacy `Bash(git commit:*)` is equivalent but only valid at the end of a pattern.
  - Compound commands (`&&`, `||`, `;`, `|`, `|&`, `&`, newline) must match per subcommand.
  - Wrappers `timeout`, `time`, `nice`, `nohup`, `stdbuf`, `command`, `builtin`, `noglob` and bare `xargs` are stripped before matching.
  - Order is deny, then ask, then allow. First match wins.
  - A deny such as `Bash(git push *)` does not catch `git -C . push`. The docs say these rules are not a security boundary.
  - Read-only built-ins (ls, cat, grep, find, read-only git) never prompt.
- `--allowedTools` and `--disallowedTools` take comma- or space-separated lists. A bare-name deny removes the tool. `--disallowedTools "mcp__*"` blocks all MCP. `--tools "Bash,Edit,Read"` restricts the built-in set. `…/cli-reference`
- settings.json `permissions` keys: `allow`, `ask`, `deny`, `additionalDirectories`, `defaultMode`, `disableBypassPermissionsMode`, `disableAutoMode`, `blockReadsOutsideWorkingDirectories`. `…/settings-reference`
- Edit and Write never auto-approve protected paths, except in bypass mode: `.git`, `.claude` (except `.claude/worktrees`), `.gitconfig`, `.husky`, `.mcp.json` and similar. `…/permission-modes#protected-paths`
- Sandbox, `…/sandboxing`:
  - Settings: `sandbox.enabled`, `autoAllowBashIfSandboxed` (default true), `excludedCommands`, `allowUnsandboxedCommands` (false means strict), `failIfUnavailable` (without it a missing sandbox only warns and runs unsandboxed), `filesystem.{allowWrite,denyWrite,denyRead,allowRead,disabled}`, `network.{allowedDomains,deniedDomains,strictAllowlist}`, `credentials.{files,envVars}` with deny or mask.
  - macOS uses Seatbelt. Linux and WSL2 use bubblewrap plus socat, with optional seccomp via `npm i -g @anthropic-ai/sandbox-runtime`. Ubuntu 24.04+ needs an AppArmor profile for bwrap.
  - Default writes: cwd, `--add-dir` directories and a per-user TMPDIR. Default reads: the whole machine.
  - Network goes through a proxy. No domains are pre-allowed.
  - It covers Bash, PowerShell and Monitor only.
  - One-shot: `--settings '{"sandbox":{"enabled":true,"allowUnsandboxedCommands":false}}'`.
- `--add-dir` grants file access but not config discovery. `--settings <file|json>` overrides keys for the session. `--setting-sources user,project,local` picks which files load; dropping a source also drops its sandbox filesystem entries. `--mcp-config` with `--strict-mcp-config` makes MCP config exclusive. `…/cli-reference`
- `--bare` skips hooks, CLAUDE.md, plugins, MCP autodiscovery and the keychain. The docs recommend it for scripts and say it "will become the default for `-p`". `--restricted` removes command-running tools and refuses bypass. `…/cli-reference`, `…/headless`
- Without `--bare`, `-p` runs the repo's `.claude/settings.json` hooks and `.mcp.json` servers with no trust dialog. For slopwatch that means a PR branch can run code on the daemon's machine through repo config. `…/headless`

### Auth

- Precedence, `…/authentication#authentication-precedence`:
  1. `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_USE_FOUNDRY`
  2. `ANTHROPIC_AUTH_TOKEN` (Bearer)
  3. `ANTHROPIC_API_KEY` (always used under `-p` when present)
  4. `apiKeyHelper` (`sh -c`, first stdout line; refresh via `CLAUDE_CODE_API_KEY_HELPER_TTL_MS`)
  5. `CLAUDE_CODE_OAUTH_TOKEN`
  6. Anthropic profile or WIF
  7. `/login` subscription OAuth
- `claude setup-token` prints a one-year OAuth token and needs Pro, Max, Team or Enterprise. It is not stored; set it as `CLAUDE_CODE_OAUTH_TOKEN`. `--bare` does not read it and needs an API key or `apiKeyHelper`. `…/authentication#generate-a-long-lived-token`
- Bedrock: `CLAUDE_CODE_USE_BEDROCK=1` and `AWS_REGION`. Vertex: `CLAUDE_CODE_USE_VERTEX=1`, `ANTHROPIC_VERTEX_PROJECT_ID`, `CLOUD_ML_REGION` (default us-central1). `…/env-vars`
- Credentials live in the macOS Keychain, or `~/.claude/.credentials.json` on Linux. `CLAUDE_CONFIG_DIR` isolates accounts. `…/authentication`
- Policy, `…/legal-and-compliance`: OAuth "is intended exclusively for purchasers of Claude Free, Pro, Max, Team, and Enterprise subscription plans and is designed to support ordinary use". Developers building products or services, including with the Agent SDK, "should use API key authentication". Third parties may not route requests through Free, Pro or Max credentials on behalf of their users. Pro and Max limits "assume ordinary, individual usage of Claude Code and the Agent SDK". An end user may still sign in to the unmodified binary with their own subscription. The Agent SDK overview says the same.
  - Reading: a personal daemon on your own machine with your own subscription is a grey zone. A product for other users needs API keys or a cloud provider.

### Cost and tokens

- `total_cost_usd` and `modelUsage[].costUSD` are client-side list-price estimates, not billing. On `--resume` or `--continue` they include the session's earlier spend (v2.1.277+), so read the latest value and don't sum. `…/agent-sdk/cost-tracking`, `…/headless`
- `--max-budget-usd N` works only with `-p`. It counts subagents but not restored totals, and ends in `error_max_budget_usd`. In that result `usage` omits the response that crossed the limit but `total_cost_usd` includes it. `--max-turns N` works only with `-p`, has no default limit, and ends in `error_max_turns`. `…/cli-reference`, `…/agent-sdk/cost-tracking`
- For subscribers, `/usage` (formerly `/cost`) says the session cost figure "isn't relevant for billing purposes". The docs don't say whether `total_cost_usd` is filled in under OAuth. It is probably still computed at list price. **Unverified.** `…/costs`

### Sessions

- `--resume <id|name|/abs/path.jsonl>`, `--continue` (under `-p` it includes `-p` and SDK sessions), `--session-id <uuid>`, `--fork-session` (with resume or continue), `--no-session-persistence` (`-p` only; or `CLAUDE_CODE_SKIP_PROMPT_HISTORY`). `…/cli-reference`
- Storage: `~/.claude/projects/<cwd with non-alphanumerics replaced by '-'>/<session-id>.jsonl`, truncated to 200 characters plus a hash. `CLAUDE_CONFIG_DIR` and `CLAUDE_CODE_PROJECT_DIR_NAME` override it (v2.1.234+). Default retention is `cleanupPeriodDays: 30`. The JSONL format is internal. `…/sessions`
- Since v2.1.223, `--resume <id>` searches the current project, its worktrees, then all projects. Before that the cwd had to match. `--continue` is per directory. `…/sessions`
- Resume does not restore `--mcp-config`, `--settings`, `--add-dir` or `--fallback-model`. Pass them again. A `-p` resume starts in the mode a new `-p` run would. `…/sessions`

### Timeouts and signals

- The docs read show no overall wall-clock limit on a `-p` run. **Unverified that none exists.** Supervise it from outside.
- `BASH_DEFAULT_TIMEOUT_MS` 120000, `BASH_MAX_TIMEOUT_MS` 600000, `API_TIMEOUT_MS` 600000, `CLAUDE_CODE_MAX_RETRIES` 10, `MCP_TIMEOUT` and `MCP_TOOL_TIMEOUT` 600000 (the `-p` startup wait for MCP is 30s), `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS` 600000 (0 means no cap). `…/env-vars`
- Background Bash shells are killed about 5s after the final result. Background subagents keep the process alive up to that ceiling. `…/headless`
- SIGTERM exits with 143, kills the Bash process tree, runs SessionEnd hooks, and records no result for the in-flight turn. SIGINT, or SDK `interrupt()`, ends the turn cleanly. `CLAUDE_CODE_RESUME_INTERRUPTED_TURN=1` continues the turn on resume. `…/headless#stop-a-run-with-sigterm`

### Git inside a worktree

- The Bash tool runs git and gh. Under `-p` it needs allow rules such as `--allowedTools "Bash(git add *),Bash(git commit *),Bash(git push *),Bash(gh pr create *)"`, or `auto` or `bypassPermissions` mode. `…/headless#create-a-commit`
- Auto mode allows pushing to any branch of the current repo and creating PRs. It blocks force push, `reset --hard`, amending commits not created in this session, and changing remotes. `…/permission-modes`
- Sandbox with a linked worktree: writes to the main repo's shared `.git` are allowed, so `git commit` works. `.git/hooks` and `config` stay denied. `git push` over HTTPS needs `github.com` in `allowedDomains`. SSH push through the proxy is **unverified**. `…/sandboxing`, `…/worktrees`
- Attribution settings: `attribution.commit` defaults to `"Co-Authored-By: Claude <claude@anthropic.com>"` and `attribution.pr` to `"Assisted by Claude"`. Set either to `false` to drop it. `includeCoAuthoredBy` is deprecated. `--bare` skips attribution. `…/settings-reference`
  - The trailer observed in this environment names the model and uses `noreply@anthropic.com`, which differs from the documented default. The doc default may be stale. **Unverified.**
- `-w/--worktree [name|#PR|PR-URL]` creates `<repo>/.claude/worktrees/<name>` on branch `worktree-<name>` and skips the trust check under `-p`. It does not clean up after `-p` and leaves a git worktree lock. A daemon that owns its worktrees should skip `-w` and set the cwd. `…/worktrees`
- "Don't ask again" approvals made in a worktree save to the main checkout's `.claude/settings.local.json`. `…/worktrees`

### Agent SDK

- TypeScript: `npm install @anthropic-ai/claude-agent-sdk`. It bundles the native Claude Code binary as an optional platform dependency (override with `pathToClaudeCodeExecutable`). `…/agent-sdk/typescript`
- Python: `pip install claude-agent-sdk`, imported as `claude_agent_sdk`. It needs the CLI (`cli_path`). `…/agent-sdk/python`
- Both run the CLI as a subprocess. `…/agent-sdk/overview`
- Options: `permissionMode` (default `'default'`), `canUseTool(toolName, input, {signal, toolUseID, requestId, …})` which only fires when evaluation falls through to a prompt, `outputFormat: {type:'json_schema', schema}`, `maxBudgetUsd`, `maxTurns`, `resume`, `forkSession`, `sessionId`, `continue`, `cwd` (default `process.cwd()`), `settingSources` (default all sources; `[]` disables them), `persistSession`, `permissionPromptToolName`, `sandbox`. `env` replaces the environment instead of merging. `…/agent-sdk/typescript`
- Permission evaluation order: hooks, deny, ask, mode, allow, canUseTool. `…/agent-sdk/permissions`
- No Rust SDK. The overview says: "To drive the same agent loop from a language other than Python or TypeScript, run the CLI as a subprocess with the `-p` flag and `--output-format json`." Only community Rust ports exist, for example https://github.com/mcfearsome/claude-agent-sdk-rust.
- The bidirectional stdio control protocol (`control_request` and `control_response` for can_use_tool and interrupt) is mentioned, but its wire format is **not publicly documented on the pages read, unverified**. From Rust, the simple route is `-p` with stream-json plus `--permission-prompts none`, or `dontAsk` with allow rules.

### Model and system prompt

- `--model <alias|full id>` (aliases `fable`, `opus`, `sonnet`, `haiku`) overrides the `model` setting and `ANTHROPIC_MODEL`. `--fallback-model a,b` is a chain that retries the primary each turn. `--effort low|medium|high|xhigh|max`. `…/cli-reference`
- `--system-prompt` and `--system-prompt-file` replace the default prompt and exclude each other. `--append-system-prompt` and `--append-system-prompt-file` append and combine with either. `--append-subagent-system-prompt` is `-p` only. `…/cli-reference#system-prompt-flags`
- `--system-prompt-snapshot on` is the default: the prompt recorded on the first request is reused on resume until compaction. It is off in `--bare` unless you pass `on`. `…/cli-reference`

### Notes for a Rust daemon

- Use `--bare`, or `--setting-sources` plus `--strict-mcp-config`, for reproducible runs.
- Pass `--session-id <uuid>` so the ID is known before the run starts.
- Parse NDJSON until `type=="result"`.
- Stop with SIGINT first, then SIGTERM.
- Pass `--mcp-config`, `--settings` and `--add-dir` again on every resume.
- Clean up worktrees yourself.

## Codex

Verified against local 0.159.0 `--help` and openai/codex main at 60947e2 (2026-09-30). The researcher lost shell access partway through; items sourced only from a WebFetch page summary are marked "(summary)".

Sources:
- developers.openai.com/codex/* now 308-redirects to learn.chatgpt.com/docs/*: [non-interactive](https://learn.chatgpt.com/docs/non-interactive-mode), [auth](https://learn.chatgpt.com/docs/auth), [sandboxing](https://learn.chatgpt.com/docs/sandboxing), [permissions](https://learn.chatgpt.com/docs/permissions), [config reference](https://learn.chatgpt.com/docs/config-file/config-reference), [agent approvals and security](https://developers.openai.com/codex/agent-approvals-security).
- The repo's docs/exec.md, docs/sandbox.md and docs/authentication.md are stubs pointing at those pages.
- Source: [exec_events.rs](https://github.com/openai/codex/blob/main/codex-rs/exec/src/exec_events.rs), [lib.rs](https://github.com/openai/codex/blob/main/codex-rs/exec/src/lib.rs), [cli.rs](https://github.com/openai/codex/blob/main/codex-rs/exec/src/cli.rs), [TS SDK README](https://github.com/openai/codex/blob/main/sdk/typescript/README.md), [Python SDK README](https://github.com/openai/codex/blob/main/sdk/python/README.md), [app-server README](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md).
- `codex exec` now runs an in-process app-server client (`InProcessAppServerClient` in lib.rs). It is a wrapper over app-server, not a separate code path.

### Invocation and JSONL

- `codex exec [OPTIONS] [PROMPT]`, alias `codex e`. With no prompt or `-`, the prompt is read from stdin. With a prompt and piped stdin, stdin is appended as a `<stdin>` block.
- `--json` (alias `--experimental-json`) prints JSONL events to stdout. Without it, progress goes to stderr and the final message to stdout.
- `-o/--output-last-message <FILE>` writes the final agent message to a file.
- Top-level events, tagged by `type` (`ThreadEvent` in exec_events.rs):
  - `thread.started {thread_id}`
  - `turn.started {}`
  - `turn.completed {usage}`
  - `turn.failed {error:{message}}`
  - `item.started`, `item.updated`, `item.completed {item}`
  - `error {message}`
- `item` is `{id, type, ...}`. Item types:
  - `agent_message {text}`. The text is a JSON string when a schema is set.
  - `reasoning {text}`, a reasoning summary.
  - `command_execution {command, aggregated_output, exit_code?, status: in_progress|completed|failed|declined}`
  - `file_change {changes:[{path, kind: add|delete|update}], status}`, emitted only as a completed item.
  - `mcp_tool_call {server, tool, arguments, result?{content[], structured_content, _meta?}, error?{message}, status}`
  - `collab_tool_call {tool: spawn_agent|send_input|wait|close_agent, sender_thread_id, receiver_thread_ids, prompt?, agents_states, status}`, new, for sub-agents.
  - `web_search {id, query, action, results?}`
  - `todo_list {items:[{text, completed}]}`
  - `error {message}`, non-fatal.
- Exit codes (lib.rs): 1 on startup or config errors, a bad schema file, or a failed git check ("Not inside a trusted directory and --skip-git-repo-check was not specified."). 1 at the end if `error_seen` was set; which events set it is **unverified**, probably `turn.failed` and `error`. 0 otherwise. No other codes seen.
- Per the docs, if an MCP server marked `required = true` fails, exec exits with an error.

### `--output-schema <FILE>`

- The CLI only checks that the file reads and parses as JSON (exit 1 otherwise). It passes the schema as `output_schema` on the turn, and the Responses API enforces it server-side as structured output.
- Output is the final `agent_message.text` as a JSON string, so it also appears on stdout and in the `-o` file.
- Strict-mode rules (`additionalProperties:false`, every property in `required`) are **unverified** in the docs. Assume OpenAI's strict structured-output rules apply. The TS SDK README says to use zod-to-json-schema with `target:"openAi"`, which points to strict mode.
- `exec resume` also accepts `--output-schema`.

### Sandbox and approvals

- `-s/--sandbox read-only|workspace-write|danger-full-access`. exec defaults to `read-only`.
- exec hardcodes `approval_policy = Never` (lib.rs:574, "Default to never ask for approvals in headless mode"). A blocked action goes straight back to the model as a failure.
- `--full-auto` is gone. It is absent from 0.159 help, and the docs say "Deprecated; use `--sandbox workspace-write`".
- `--approve-for-me` routes approvals to an automatic reviewer (`approvals_reviewer = auto_review`).
- `--dangerously-bypass-approvals-and-sandbox` forces full access and skips the git repo check. A lib.rs comment calls it "--yolo", but the `--yolo` alias is not in help output. **Unverified.**
- `-C/--cd <DIR>` sets the working root. `--add-dir <DIR>` adds writable dirs. `--skip-git-repo-check` allows running outside a repo. `--worktree` runs the session in a new managed git worktree and can't be combined with `--ephemeral`.
- Config keys: `sandbox_workspace_write.network_access` (default off), `sandbox_workspace_write.writable_roots`, `sandbox_workspace_write.exclude_tmpdir_env_var`, `sandbox_workspace_write.exclude_slash_tmp`. Example: `-c sandbox_workspace_write.network_access=true`.
- Newer permission profiles (learn.chatgpt.com/docs/permissions): built-ins `:read-only`, `:workspace`, `:danger-full-access`; `default_permissions = "..."` with `[permissions.<name>]` tables for read, write and deny rules and network allowlists. Network allowlists need `features.network_proxy = true`.
- Enforcement: Seatbelt on macOS. On Linux and WSL, bubblewrap plus seccomp, falling back to a bundled helper that needs unprivileged user namespaces (summary). Landlock isn't mentioned in the current docs. Native sandbox on Windows.
- The sandbox applies to spawned commands such as git and package managers, not only built-in edits.

### Auth

- `codex login` uses the ChatGPT browser flow by default. `--device-auth` uses a device code, for headless machines. `--with-api-key` reads the key from stdin, for example `printenv OPENAI_API_KEY | codex login --with-api-key`. `--with-access-token` reads `CODEX_ACCESS_TOKEN` from stdin. `codex login status` checks state.
- For single exec runs the docs recommend `CODEX_API_KEY=<key> codex exec --json ...`. They warn against setting `OPENAI_API_KEY` or `CODEX_API_KEY` job-wide when running repo-controlled code.
- Stored credentials: `$CODEX_HOME/auth.json` (default `~/.codex`). `cli_auth_credentials_store = file|keyring|auto|ephemeral`. `forced_login_method = chatgpt|api`. On a headless machine you can copy `auth.json` over. `--ignore-user-config` skips config.toml but still uses `CODEX_HOME` for auth.

### Cost and tokens

- `turn.completed.usage` is `{input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens, reasoning_output_tokens}`, all i64. `cache_write_input_tokens` is new and defaults to 0.
- No USD cost field anywhere. The caller computes cost.
- app-server also emits `thread/tokenUsage/updated`.

### Resume

- `codex exec resume [SESSION_ID|thread name] [PROMPT|-]` or `--last`. `--all` disables filtering by cwd, so `--last` presumably only sees sessions from the same cwd. Also `codex exec fork` and `codex exec review`.
- Use `thread.started.thread_id` as the ID.
- Sessions are stored in `~/.codex/sessions` (per the SDK README). `--ephemeral` persists nothing. A `codex migrate-rollouts` command suggests the layout is moving to "paginated thread history"; the exact path is **unverified**.
- `resume` has no `-s/--sandbox` or `-C`, only `-c` overrides and the bypass flag. A daemon should pass `-c sandbox_mode=...` and set the process cwd itself.

### Timeouts and model

- No turn or wall-clock limit in the docs or config reference. The daemon has to enforce its own.
- `model_providers.<id>.stream_idle_timeout_ms` (default 300000), `stream_max_retries` (5), `request_max_retries` (4).
- MCP: `startup_timeout_sec` (default 10), `tool_timeout_sec` (default 60).
- A per-shell-command timeout key was **not found, unverified**.
- `-m/--model`, `-c model_reasoning_effort=low|medium|high|...`. `-p/--profile <name>` layers `$CODEX_HOME/<name>.config.toml` over the base config, which differs from the older `[profiles.x]` tables.

### Git under workspace-write

- Docs: "`<writable_root>/.git` is protected as read-only whether it appears as a directory or file". The sandboxing page (summary) also lists the gitdir target of a worktree's `.git` file, `.codex` and `.agents` as protected.
- So under `workspace-write` the agent cannot commit, since a commit writes objects, the index and refs. Network is off by default, so push fails too.
- In a linked worktree, the gitdir (`<main>/.git/worktrees/<name>`) and the shared object store sit outside the workspace, and they are protected or unwritable either way.
- Options for a daemon: (a) the daemon commits and pushes after the run, (b) `danger-full-access` or the bypass flag inside external isolation, (c) a custom permission profile granting write on the gitdir. Whether (c) can override the built-in protection is **unverified**.
- Worth a live test: `codex sandbox` runs a command inside the Codex sandbox, so `git commit` can be tried in a worktree without a model call.

### SDKs and other interfaces

- TypeScript `@openai/codex-sdk` (sdk/typescript) spawns the `codex` CLI and exchanges JSONL over stdio. `new Codex({env, config, configOverrides, baseUrl})`, `startThread({workingDirectory, skipGitRepoCheck, ...})`, `thread.run(input, {outputSchema})` returning `{finalResponse, items}`, `thread.runStreamed()` yielding the same events as `--json`, `resumeThread(id)`. AbortSignal support is not documented.
- Python: `pip install openai-codex` (sdk/python plus sdk/python-runtime), experimental. `Codex` context manager, `thread_start()`, `run()` returning a `TurnResult`. The transport is not stated; likely app-server (**unverified**).
- `codex app-server` (experimental): JSON-RPC 2.0 over stdio JSONL or websocket, with an `initialize` handshake. Methods `thread/start|resume|fork|archive`, `turn/start|interrupt|steer`. Notifications `item/*`, `turn/completed`, `thread/tokenUsage/updated`. Server-to-client approval requests. Schema codegen via `generate-ts` and `generate-json-schema`. This gives a daemon interrupt, steer and approvals, and exec already wraps it.
- 0.159 top-level help has no `mcp-server` or `proto` subcommand. `codex mcp` only manages external MCP servers. It does list `exec-server` (experimental) and `remote-control`.

## Gemini CLI

v0.62.0 (2026-09-29), Apache-2.0, https://github.com/google-gemini/gemini-cli. Not installed locally, so everything here comes from docs and source.

- Headless: `gemini -p "<prompt>"`. Headless mode also turns on without a TTY. Stdin is prepended to `-p`. [headless.md](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/headless.md)
- Exit codes: 0 ok, 1 error or API failure, 42 bad input, 53 turn limit exceeded.
- Output: `-o/--output-format text|json|stream-json`. Types in [types.ts](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/output/types.ts).
  - `json` is one object: `{session_id?, response?, stats?: SessionMetrics, error?: {type,message,code?}, warnings?[]}`.
  - `SessionMetrics` ([uiTelemetry.ts](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/telemetry/uiTelemetry.ts)): `models[name].api{totalRequests,totalErrors,totalLatencyMs}`, `models[name].tokens{input,prompt,candidates,total,cached,thoughts,tool}`, `tools{totalCalls,totalSuccess,totalFail,totalDurationMs,totalDecisions,byName}`, `files{totalLinesAdded,totalLinesRemoved}`.
  - `stream-json` is JSONL. Every event has `type` and `timestamp`: `init{session_id,model}`, `message{role,content,delta?}`, `tool_use{tool_name,tool_id,parameters}`, `tool_result{tool_id,status,output?,error?}`, `error{severity,message}`, `result{status,error?,stats{total_tokens,input_tokens,output_tokens,cached,input,duration_ms,tool_calls,models{}}}`.
- Cost: no USD field, tokens only.
- Schema-constrained output: none. [Issue #13388](https://github.com/google-gemini/gemini-cli/issues/13388) (`--schema-file`) was closed "not planned". The only route is prompting for JSON and parsing `.response`.
- Approvals: `--approval-mode default|auto_edit|yolo|plan`. `-y/--yolo` is deprecated. In non-interactive mode "ask_user" counts as deny, so under `default` the shell tool, and with it git, is effectively denied. Finer rules go in TOML under `~/.gemini/policies/*.toml`, for example `toolName="run_shell_command"`, `commandPrefix="git"`, `decision="allow"`. `--allowed-tools` is deprecated. [policy-engine.md](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/policy-engine.md), [cli-reference.md](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/cli-reference.md)
- Folder trust: headless mode in an untrusted folder raises `FatalUntrustedWorkspaceError`. Pass `--skip-trust` or set `GEMINI_CLI_TRUST_WORKSPACE=true`. Every fresh worktree hits this. [trusted-folders.md](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/trusted-folders.md)
- Sandbox: `-s/--sandbox`, `GEMINI_SANDBOX=true|docker|podman|sandbox-exec|runsc|lxc`, or `tools.sandbox` in settings. Seatbelt profiles via `SEATBELT_PROFILE`: `permissive-open` (default), `permissive-proxied`, `restrictive-open`, `restrictive-proxied`, `strict-open`, `strict-proxied`. The default profile limits writes to the project dir. [sandbox.md](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/sandbox.md)
  - **Unverified inference:** in a linked worktree, `git commit` writes to `<main>/.git/worktrees/<name>`, outside the project dir, so the sandbox would probably block it.
- Auth: `GEMINI_API_KEY`, or Vertex via `GOOGLE_CLOUD_PROJECT` plus `GOOGLE_CLOUD_LOCATION` with ADC, `GOOGLE_APPLICATION_CREDENTIALS` or `GOOGLE_API_KEY`. A cached Google-login credential is reused headless, but logging in needs a browser. https://geminicli.com/docs/get-started/authentication/
- Resume: `-r/--resume latest|<index>|<uuid>`. Sessions live in `~/.gemini/tmp/<project_hash>/chats/`, scoped to the cwd project, 30-day retention. The cheatsheet shows `gemini -r "<id>" "query"`; combining it with `-p` isn't documented (**unverified**). `session_id` is in the `json` output and the `init` event. [session-management.md](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/session-management.md)
- Timeouts: no wall-clock flag. `model.maxSessionTurns` (default -1, exit 53 when hit), `tools.shell.inactivityTimeout` (300s), per-MCP-server `timeout`. [configuration.md](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/configuration.md)
- Git: possible through `run_shell_command` with `--approval-mode yolo` or a policy allow rule, subject to the sandbox caveat above.

## opencode

1.18.26 installed, latest v1.18.33 (2026-09-28), MIT. The repo moved to https://github.com/anomalyco/opencode (sst/opencode points there).

- Headless: `opencode run [message..]` (local `--help`). Flags: `-m provider/model`, `--agent`, `--variant`, `--dir <path>`, `-f` (attach file), `--title`, `--attach http://host:4096`, `-p`/`-u` (basic auth), `--port`, `--pure`, `--auto`. `--auto` approves anything not explicitly denied.
- Output: `--format json` gives JSONL, one `{type, timestamp, sessionID, ...}` per line. Types `step_start`, `text`, `reasoning`, `tool_use`, `step_finish` (each carries `part`) and `error` (carries `error`). Exit 1 on error. [run.ts](https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/cli/cmd/run.ts)
- Cost and tokens: `step_finish.part` is `{type:"step-finish", reason, cost, tokens{input,output,reasoning,cache{read,write}}}`. The AssistantMessage has the same `cost` and `tokens`. Also `opencode stats` and `opencode export [sessionID]`. [types.gen.ts](https://github.com/anomalyco/opencode/blob/dev/packages/sdk/js/src/v2/gen/types.gen.ts)
- Schema-constrained output: through the server and SDK only, not the `run` CLI. Prompt body `format: {type:"json_schema", schema, retryCount?}`. The result lands in `AssistantMessage.structured`. Failure gives `StructuredOutputError{message, retries}`. https://opencode.ai/docs/sdk/
- Server: `opencode serve --port --hostname`, default 127.0.0.1:4096 (the CLI default port is 0, random). Basic auth via `OPENCODE_SERVER_PASSWORD` and `OPENCODE_SERVER_USERNAME`. OpenAPI spec at `/doc`. Endpoints: `POST /session`, `POST /session/:id/message` (sync), `POST /session/:id/prompt_async`, `POST /session/:id/abort`, `POST /session/:id/permissions/:permissionID` (`once|always|reject`), SSE on `GET /event` and `GET /global/event`. Per-request cwd via the `x-opencode-directory` header or `?directory=`, so one server can drive many worktrees. SDK is `@opencode-ai/sdk`, TypeScript only; a Rust daemon would call HTTP or generate a client from the OpenAPI spec. https://opencode.ai/docs/server/
- Permissions: keys `read`, `edit`, `bash`, `external_directory`, `webfetch`, `websearch`, `task`, `skill`, `doom_loop`; values `allow|ask|deny`; `*` and `?` globs such as `"git push *"`; per-agent overrides. Most keys default to allow. `doom_loop` and `external_directory` default to ask. `.env` reads are denied. Inject config with `OPENCODE_PERMISSION`, `OPENCODE_CONFIG` or `OPENCODE_CONFIG_CONTENT`. In `run`, any "ask" is auto-rejected unless `--auto` is set (run.ts). https://opencode.ai/docs/permissions/, https://opencode.ai/docs/cli/
- Sandbox: none documented. Only permission rules.
- Auth: provider env vars (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `AWS_PROFILE`, ...), `{env:VAR}` in config, and `opencode auth login` storing to `~/.local/share/opencode/auth.json`. The docs list Claude Pro/Max, ChatGPT and Copilot OAuth. Whether Claude Pro/Max use here is allowed under Anthropic's current terms is **unverified**. https://opencode.ai/docs/providers/
- Resume: `-s/--session <id>`, `-c/--continue`, `--fork` (local help).
- Timeouts: provider `options.timeout` (default 300000 ms), `headerTimeout`, `chunkTimeout`. No wall-clock flag on `run`; abort through the API. https://opencode.ai/docs/config/
- Git: yes, through the `bash` tool, which is allowed by default. Deny `git push *` in `OPENCODE_PERMISSION` if the daemon should own pushing.
- Also set `OPENCODE_DISABLE_AUTOUPDATE` and `share: "disabled"`. Snapshots are on by default.

## Cursor CLI

`cursor-agent` (also `agent`), 2026.04.17-787b533 installed. Proprietary, no public repo. The current docs list commands the local build lacks (`agent sandbox run`, `agent worker start`, `--idle-release-timeout`), so the local build is behind. https://cursor.com/docs/cli/reference/parameters

- Headless: `cursor-agent -p [--output-format text|json|stream-json] [--stream-partial-output] --trust --workspace <path> --model <m> "<prompt>"`. `--trust` skips the workspace-trust prompt and only works headless. Without `--force` (or `--yolo`), "changes are only proposed, not applied". Also `--mode plan|ask` (read-only), `--approve-mcps`, and `-w/--worktree`, which creates its own worktree under `~/.cursor/worktrees`; skip it since the daemon owns worktrees. https://cursor.com/docs/cli/headless
- Output (https://cursor.com/docs/cli/reference/output-format):
  - `json`: `{type:"result", subtype:"success", is_error, duration_ms, duration_api_ms, result, session_id, request_id?}`.
  - `stream-json`: `system/init{apiKeySource, cwd, session_id, model, permissionMode}`, `user{message}`, `assistant{message.content[]}`, `tool_call/started`, `tool_call/completed{call_id, tool_call{<x>ToolCall{args,result}}}`, `result`.
- Cost and tokens: none documented.
- Schema-constrained output: none.
- Permissions: `~/.cursor/cli-config.json` or `<project>/.cursor/cli.json`, with `allow` and `deny` lists using `Shell(git)`, `Read(glob)`, `Write(glob)`, `WebFetch(domain)`, `Mcp(server:tool)`. Deny wins. https://cursor.com/docs/cli/reference/permissions
- Sandbox: `--sandbox enabled|disabled` (local help). Per the [2.5 changelog](https://cursor.com/changelog/2-5): writes limited to workspace and temp, Keychain blocked, network through an allowlist proxy, and "git writes are now allowed". Not checked against a worktree layout. A published sandbox escape exists: https://accomplish.ai/blog/beltdown2-escaping-the-cursor-cli-sandbox/
- Auth: `CURSOR_API_KEY` or `--api-key`. `init` reports it as `apiKeySource: env|flag|login`. Login otherwise needs a browser.
- Resume: `--resume [chatId]`, `--continue` (same as `--resume=-1`), and `create-chat` to pre-allocate an ID. `session_id` is in every event.
- Timeouts: none documented.
- Git: yes. Local help says `-p` "has access to all tools, including write and shell". Needs `--force` or a `Shell(git)` allow rule.

Other sources: [Cursor CLI overview](https://cursor.com/docs/cli/overview), [Cursor sandbox forum post](https://forum.cursor.com/t/agent-sandboxing-available-in-cursor-2-0/139449), [gemini-cli #5021](https://github.com/google-gemini/gemini-cli/issues/5021).
