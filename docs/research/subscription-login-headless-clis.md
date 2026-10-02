# Subscription login for headless agent CLIs

Research for [#29](https://github.com/jnsdls/slopwatch/issues/29), building on [#4](https://github.com/jnsdls/slopwatch/issues/4) ([findings](https://github.com/jnsdls/slopwatch/blob/research/headless-agent-clis/docs/research/headless-agent-clis.md)).

Question: can an agent Step run Claude Code, Codex, Gemini CLI, opencode or Cursor CLI headless under the user's subscription login, or do the providers require an API key for unattended use? How does each CLI pick up each credential when spawned non-interactively, and does subscription use report cost the way API use does?

All sources were read on 2026-10-02. Local versions: Claude Code 2.1.286, codex-cli 0.159.0, opencode 1.18.26, cursor-agent 2026.04.17-787b533. No prompts were run, so nothing here was observed on a live subscription. Claims I couldn't confirm against a primary source are marked **unverified**.

## Short answer

| | Subscription login headless? | What the provider says | Credential pickup under `-p`/`exec` | Cost reported on subscription |
| - | - | - | - | - |
| Claude Code | Yes, for the user's own unmodified binary and own login | API key for products; subscription allowed for "ordinary, individual usage"; `claude -p` draws on plan limits today | `ANTHROPIC_API_KEY` wins if set; else `CLAUDE_CODE_OAUTH_TOKEN`; else the `/login` credential in the Keychain. `--bare` ignores OAuth | `total_cost_usd` still emitted, but it is a list-price estimate, not a charge. Plan usage only as `rate_limit_event` utilization |
| Codex | Yes, documented, but "API keys are the recommended default" | ChatGPT auth in CI is an "advanced" path for trusted private runners | `CODEX_API_KEY` per run, else saved `auth.json` or keyring | Tokens only. No USD, no plan-usage figure in `exec --json` |
| Gemini CLI | Yes, cached Google login is reused headless | Only through Gemini CLI itself. Using its OAuth from other software is a ToS violation | Cached Google login, else `GEMINI_API_KEY` or Vertex env | Tokens only |
| opencode | ChatGPT, Copilot: yes. Claude Pro/Max: no | opencode's docs say "Anthropic explicitly prohibits this" and dropped the plugins in 1.3.0 | `~/.local/share/opencode/auth.json` or provider env vars | USD per step, computed from list prices |
| Cursor CLI | Yes, there is no separate API product | `CURSOR_API_KEY` is a user key on the same Cursor account, recommended for scripts | `CURSOR_API_KEY` or `--api-key`, else stored browser login | Nothing in the JSON output |

For slopwatch: a v1 daemon that spawns the user's own installed `claude` or `codex`, and lets each CLI read its own stored login, is within the published terms for a single developer on their own machine. slopwatch must not mint, store or pass the user's Claude subscription token itself. API keys stay the safe default for anything that looks like a product or a shared service, and are the only option the docs fully bless for automation. Anthropic has already announced, then paused, a change that would move `claude -p` off plan limits onto a separate metered credit, so this could shift again.

## Claude Code

### What the terms say

[Consumer Terms of Service](https://www.anthropic.com/legal/consumer-terms) (effective October 8, 2025), which govern Free, Pro and Max users of Claude Code per the [legal page](https://code.claude.com/docs/en/legal-and-compliance#license). Under the list of things you may not do:

> Except when you are accessing our Services via an Anthropic API Key or where we otherwise explicitly permit it, to access the Services through automated or non-human means, whether through a bot, script, or otherwise.

And on accounts:

> You may not share your Account login information, Anthropic API key, or Account credentials with anyone else.

So the fog note on the map is right as a default. Scripted use on a consumer plan needs either an API key or an explicit permission. The Claude Code docs give that permission for Claude Code itself.

[Authentication: Generate a long-lived token](https://code.claude.com/docs/en/authentication#generate-a-long-lived-token):

> For CI pipelines, scripts, or other environments where interactive browser login isn't available, generate a one-year OAuth token with `claude setup-token` [...] This token authenticates with your Claude subscription and requires a Pro, Max, Team, or Enterprise plan.

[Legal and compliance: Authentication and credential use](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use):

> **OAuth authentication** is intended exclusively for purchasers of Claude Free, Pro, Max, Team, and Enterprise subscription plans and is designed to support ordinary use of Claude Code and other native Anthropic applications.

> **Developers** building products or services that interact with Claude's capabilities, including those using the Agent SDK, should use API key authentication through Claude Console or a supported cloud provider. Anthropic does not permit third-party developers to offer Claude.ai login into their own applications, or to route requests through Free, Pro, or Max plan credentials on behalf of their users. Moreover, developers may not collect, store, or intermediate Claude.ai credentials or session tokens [...]

Sign-in has to go through Anthropic's own flow, the same paragraph adds.

> [...] Nor does it prevent an end user from signing in to the unmodified Claude Code binary with their own Claude subscription, including where a platform hosts Claude Code as described under *Can customers offer Claude Code in their products?* above.

The same page, on [running Claude Code in your products](https://code.claude.com/docs/en/legal-and-compliance#can-customers-offer-claude-code-in-their-products):

> **The Claude Code binary must not be modified.** [...] customers may not remove, disable, or restrict any authentication method built into it (including methods that permit signing in with a Claude account or the user's own API key).

> **Customers may not pay for, resell, or intermediate Claude usage on their end users' behalf.** Each end user must authenticate with their own Anthropic API key, Claude subscription plan credentials, or 3P inference provider credential [...]

And under [Acceptable use](https://code.claude.com/docs/en/legal-and-compliance#acceptable-use):

> Advertised usage limits for Pro and Max plans assume ordinary, individual usage of Claude Code and the Agent SDK.

The [Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview) adds:

> Unless previously approved, Anthropic does not allow third party developers to offer claude.ai login or rate limits for their products, including agents built on the Agent SDK. Use the API key authentication methods described in the Quickstart instead.

The Help Center article [Use the Claude Agent SDK with your Claude plan](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan) confirms where `claude -p` usage lands today:

> Update June 15: We're pausing the changes to Claude Agent SDK usage described below. For now, nothing has changed: Claude Agent SDK, `claude -p`, and third-party app usage still draw from your subscription's usage limits. [...] When we have an update, we'll share it before anything takes effect.

The paused plan, kept on that page for reference, would have stopped counting `claude -p` against plan limits and given a separate monthly credit instead (Pro $20, Max 5x $100, Max 20x $200), with overflow "at standard API rates" only if usage credits are on, otherwise "Agent SDK requests stop until your credit refreshes". It also said: "The Agent SDK monthly credit is sized for individual experimentation and automation. Teams running shared production automation should use Claude Platform with an API key".

Reading for slopwatch. A daemon on my machine that spawns the unmodified `claude` binary, which reads my own `/login` credential, is "an end user [...] signing in to the unmodified Claude Code binary with their own Claude subscription", and `claude -p` is explicitly counted against plan limits. That is allowed. Three things would break it:

- slopwatch storing or injecting the subscription token ("may not collect, store, or intermediate Claude.ai credentials or session tokens"). A user setting `CLAUDE_CODE_OAUTH_TOKEN` in their own shell profile, which the daemon's login-shell env then passes through, is the user's choice and a documented path. slopwatch prompting for that token and putting it in its own Keychain item is the grey case. I'd avoid it.
- Usage that stops being "ordinary, individual", such as a team sharing one daemon or one login.
- A hosted slopwatch. That already sits out of scope on the map, and the docs point it at API keys.

### How the CLI picks up credentials headless

[Authentication precedence](https://code.claude.com/docs/en/authentication#authentication-precedence), condensed:

1. Cloud provider env (`CLAUDE_CODE_USE_BEDROCK`, `_VERTEX`, `_FOUNDRY`)
2. `ANTHROPIC_AUTH_TOKEN`
3. `ANTHROPIC_API_KEY`. "In non-interactive mode (`-p`), the key is always used when present."
4. `apiKeyHelper`
5. `CLAUDE_CODE_OAUTH_TOKEN`, "Use this for CI pipelines and scripts where browser login isn't available."
6. Anthropic profile or WIF
7. "Subscription OAuth credentials from `/login`. This is the default for Claude Pro, Max, Team, and Enterprise users."

Consequences:

- A stray `ANTHROPIC_API_KEY` in the login-shell env silently switches a Step to API billing under `-p`, with no prompt. The daemon passes the login-shell env to Steps (#11), so the Plugin should log which source it used. The init message reports `apiKeySource` as `ANTHROPIC_API_KEY`, `apiKeyHelper`, `/login managed key` or `none`, where `none` covers a claude.ai login ([TypeScript reference: ApiKeySource](https://code.claude.com/docs/en/agent-sdk/typescript#apikeysource)). `none` can't tell subscription from a bearer token or cloud provider, so the Plugin has to combine it with the env it passed.
- `--bare` reads neither `CLAUDE_CODE_OAUTH_TOKEN` nor the Keychain: "In bare mode, Claude Code never reads OAuth credentials or the system keychain" ([headless](https://code.claude.com/docs/en/headless)). A subscription Step must not pass `--bare`.
- The `/login` credential lives in the macOS Keychain ([authentication](https://code.claude.com/docs/en/authentication)). Whether a launchd agent can read it without a prompt is **unverified**. A LaunchAgent runs in the user's GUI session, so it probably can.
- The `setup-token` token "can only make model requests", which is all a Step needs.

### Cost reporting

- `--output-format json` includes `total_cost_usd` and a per-model breakdown, and "Both figures are client-side estimates and can differ from your actual bill" ([headless](https://code.claude.com/docs/en/headless)). The SDK "computes them locally from a price table bundled at build time" and `costBasis` says which table priced it ([cost tracking](https://code.claude.com/docs/en/agent-sdk/cost-tracking)).
- For subscribers the figure "isn't relevant for billing purposes" ([costs](https://code.claude.com/docs/en/costs)). The docs never say the field is dropped under OAuth, and it is computed locally from tokens, so it is almost certainly still filled in at list price (**unverified** on a live run).
- What a subscription run actually spends is plan allowance. The SDK message stream has `rate_limit_event` with `rate_limit_info.status` (`allowed`, `allowed_warning`, `rejected`), `resetsAt` and `utilization` ([TypeScript reference](https://code.claude.com/docs/en/agent-sdk/typescript)). That `claude -p --output-format stream-json` emits the same event is **unverified**. The status line gets `rate_limits.five_hour` and `seven_day` percentages "only for claude.ai Pro and Max subscribers" ([status line](https://code.claude.com/docs/en/statusline)).
- Running out shows as "You've hit your session limit · resets 3:45pm" or the weekly variant, and "Claude Code blocks further requests until the reset time" ([errors](https://code.claude.com/docs/en/errors#youve-hit-your-session-limit)). The automatic wait for reset exists only "in an interactive session", so a headless Step fails. Error category `rate_limit` in the stream (#4).
- `--max-budget-usd` counts against the estimate, so on a subscription it caps estimated list-price spend, not plan usage.

## Codex

### What the terms say

OpenAI [Terms of Use](https://openai.com/policies/terms-of-use/) (effective January 1, 2026), which cover ChatGPT plans. Among the things you may not do:

> Automatically or programmatically extract data or Output (defined below).

> Interfere with or disrupt our Services, including circumvent any rate limits or restrictions or bypass any protective measures or safety mitigations we put on our Services.

And: "You may not share your account credentials or make your account available to anyone else".

The "programmatically extract" line reads broadly, but OpenAI's own Codex docs describe running `codex exec` on the ChatGPT login in scripts and CI, so the ToU clause doesn't seem aimed at Codex itself. I found no OpenAI page that says this outright. The docs steer automation to API keys without forbidding the subscription.

[Authentication](https://learn.chatgpt.com/docs/auth):

> Use API key authentication for programmatic Codex CLI workflows, such as CI/CD jobs. Don't expose Codex execution in untrusted or public environments.

> When you sign in with an API key, Codex uses standard API pricing instead of included ChatGPT plan credits.

[Non-interactive mode: Authenticate in automation](https://learn.chatgpt.com/docs/non-interactive-mode#authenticate-in-automation):

> `codex exec` reuses saved CLI authentication by default.

> Read this if you need to run CI/CD jobs with a Codex user account instead of an API key, such as enterprise teams using ChatGPT-managed Codex access on trusted runners or users who need ChatGPT/Codex rate limits instead of API key usage. API keys are the right default for automation because they are simpler to provision and rotate. Use this path only if you specifically need to run as your Codex account.

[Maintain Codex account auth in CI/CD (advanced)](https://learn.chatgpt.com/docs/auth/ci-cd-auth) lists when the subscription path applies, including "the runner is trusted private infrastructure" and "only one machine or serialized job stream will use a given `auth.json` copy", and says "Do not use this workflow for public or open-source repositories."

ChatGPT Enterprise also has [Codex access tokens](https://learn.chatgpt.com/docs/auth#use-codex-access-tokens-for-enterprise-automation), "intended for trusted scripts, schedulers, and private CI runners". Not relevant for a personal plan.

Reading for slopwatch: a local daemon running `codex exec` on the user's own Mac with their saved ChatGPT login is the default behaviour of `codex exec` and fits every condition in the advanced CI guide. It is allowed. OpenAI still recommends an API key.

### How the CLI picks up credentials headless

- `codex exec` uses the saved login from `$CODEX_HOME/auth.json` (default `~/.codex`) or the OS credential store, per `cli_auth_credentials_store = file|keyring|auto|ephemeral` ([auth](https://learn.chatgpt.com/docs/auth#credential-storage)).
- `CODEX_API_KEY=<key> codex exec ...` overrides it for one run ([non-interactive](https://learn.chatgpt.com/docs/non-interactive-mode)). The docs warn against putting `OPENAI_API_KEY` or `CODEX_API_KEY` in a job-wide env when repo code runs in the same job.
- "For sign in with ChatGPT sessions, Codex refreshes tokens automatically during use before they expire" ([auth](https://learn.chatgpt.com/docs/auth#login-caching)). The refresh writes back to `auth.json` when `last_refresh` is older than about 8 days, and on a 401 ([CI guide](https://learn.chatgpt.com/docs/auth/ci-cd-auth#why-this-works)). Concurrent Steps on one Mac share one `auth.json`. Whether two runs refreshing at once can clobber each other is **unverified**. The CI guide's "serialized job stream" condition hints that it can.
- `forced_login_method = "chatgpt" | "api"` makes Codex log out and exit if the active credential doesn't match ([auth](https://learn.chatgpt.com/docs/auth#enforce-a-login-method-or-workspace)). A Step could pass `-c forced_login_method=...` to pin the billing source. That this works as a `-c` override is **unverified**.

### Cost reporting

- `turn.completed` carries `usage{input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens, reasoning_output_tokens}` and nothing else, with no USD and no rate-limit data, under either login ([non-interactive](https://learn.chatgpt.com/docs/non-interactive-mode), [exec_events.rs](https://github.com/openai/codex/blob/main/codex-rs/exec/src/exec_events.rs)).
- On ChatGPT login the run draws "included ChatGPT plan credits", and API token prices "are separate from subscription usage; don't use them to estimate included tasks" ([pricing](https://learn.chatgpt.com/docs/pricing)). So a USD figure from the daemon's own price table means nothing on a subscription.

## Gemini CLI

[Terms of Service and Privacy Notice](https://github.com/google-gemini/gemini-cli/blob/main/docs/resources/tos-privacy.md):

> Directly accessing the services powering Gemini CLI (for example, the Gemini Code Assist service) using third-party software, tools, or services (for example, using OpenClaw with Gemini CLI OAuth) is a violation of applicable terms and policies. Such actions may be grounds for suspension or termination of your account.

Google login maps to Gemini Code Assist for individuals (Google ToS) or, with Google AI Pro or Ultra, to the Google ToS plus Google One terms. API keys map to the Gemini API terms.

[Authentication: Running in headless mode](https://github.com/google-gemini/gemini-cli/blob/main/docs/get-started/authentication.mdx):

> Headless mode will use your existing authentication method, if an existing authentication credential is cached.

The same page's table recommends an API key or Vertex for headless use. Spawning the real `gemini` binary is not "third-party software" reaching the service directly, so a cached Google login is fine. Quotas on a Google login are per request, not per token: 1,000/day on Code Assist for individuals, 1,500 on AI Pro, 2,000 on AI Ultra ([quotas and pricing](https://github.com/google-gemini/gemini-cli/blob/main/docs/resources/quota-and-pricing.md)). Cost reporting is tokens only (#4).

## opencode

[Providers](https://opencode.ai/docs/providers/), Anthropic section:

> There are plugins that allow you to use your Claude Pro/Max models with OpenCode. Anthropic explicitly prohibits this. Previous versions of OpenCode came bundled with these plugins but that is no longer the case as of 1.3.0

And next to it: "you can use the following subscriptions in OpenCode with zero setup: ChatGPT Plus, Github Copilot, Gitlab Duo". This matches Anthropic's terms above: opencode is a third-party app, not "the unmodified Claude Code binary". An opencode Step therefore needs an Anthropic API key for Claude models. Using ChatGPT Plus/Pro through opencode is offered by opencode. I found no OpenAI primary source that permits or forbids it (**unverified**).

opencode reports `cost` per step from list prices (#4). On a subscription that figure is not a charge.

## Cursor CLI

[Authentication](https://cursor.com/docs/cli/reference/authentication):

> For automation, scripts, or CI environments, use API key authentication [...] Generate a user API key from Cursor Dashboard → API Keys.

The key is a user key on the same Cursor account, not a separate API product, and the [pricing page](https://cursor.com/docs/models-and-pricing) describes only plan usage plus on-demand usage "at the same API rates". So a headless Cursor Step always spends the user's Cursor plan. That a user API key bills to the plan's included usage is inferred from the pricing page, not stated (**unverified**). The JSON `result` event has no usage or cost fields ([output format](https://cursor.com/docs/cli/reference/output-format)).

## What this means for the Secrets decision (#30)

- slopwatch doesn't need to hold any agent credential for a Step to work. Each CLI reads its own stored login. The secret store only matters for API keys the user chooses to use, such as an Anthropic key for opencode or an OpenAI key for Codex.
- For Claude, slopwatch must not offer its own "sign in with Claude" or store the subscription token. Passing through the user's own env, or letting `claude` read its Keychain entry, is fine.
- Billing source should be visible per Step, because precedence can flip it silently: `ANTHROPIC_API_KEY` beats the subscription under `-p`, and `CODEX_API_KEY` beats `auth.json`. The Plugin should report which source it used in the Outcome.
- Budgets differ by billing source. On an API key the cost figures are real spend estimates. On a subscription the meaningful number is plan utilization, which only Claude exposes (`rate_limit_event`) and only as a percentage. Codex, Gemini and Cursor expose nothing. A subscription Step's budget can only cap tokens or wall-clock time.
- Plan exhaustion is a hard failure mid-Run. Claude blocks until the reset time, and the reset time is in the error. The Verdict should be error with that reset time, not a retry loop.

## Open points

- Anthropic's paused change would put `claude -p` on a separate monthly credit and stop requests when it runs out. The Help Center says they'll "share an update before anything takes effect". Watch that page.
- Whether `claude -p --output-format stream-json` emits `rate_limit_event`, and whether `total_cost_usd` is populated under OAuth. One cheap live run settles both.
- Whether a launchd agent reads the Claude Keychain credential without a prompt.
- Concurrent Codex Steps sharing one `auth.json` during a token refresh.
- OpenAI has no primary-source statement on ChatGPT login inside third-party tools such as opencode.
