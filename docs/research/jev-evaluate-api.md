# Jev evaluate API for PR judging

Research for [#3](https://github.com/jnsdls/slopwatch/issues/3). Sources read on 2026-09-30. No paid calls were made; every claim below comes from docs, the public model catalog, or the parallax reference client.

## Short answer

- Two gateway routes serve Jev. `POST https://ai-gateway.vercel.sh/v1/evaluate` is Vercel's native shape (`boolean` questions, `probability` answers, camelCase usage). `POST https://ai-gateway.vercel.sh/typesafe/v1/systemone` is TypeSafe's own shape (`noul` questions and answers, snake_case usage). Vercel recommends `/v1/evaluate` for new code. Parallax uses it.
- Three question types: `boolean`, `choice` (up to 255 options), `score` (ordered levels, at least 2). All can share one request and one state.
- Limit per request: 32,000 tokens for state plus the longest single question, and 64,000 tokens for state plus all questions. No output limit.
- Price: $0.042 per million input tokens, output free. A full 32k-token state costs about $0.0013.
- Rate limits are not stable. TypeSafe lists 40 requests/s and 100K tokens/s and warns they change without notice. Parallax lost 25% of calls to 429 "at capacity" at 8 in flight on 2026-09-24, with no `Retry-After`.
- Latency: TypeSafe's cookbook shows 0.27 s for 13 questions over a ~13k-token article. Parallax recorded single calls from 0.55 s to 18 s.
- PR evidence: a PR description, linked issue and review findings fit easily. A diff fits up to roughly 1,000 to 1,500 changed lines. In a 100-PR sample from parallax, the median PR (276 lines) and the 75th percentile (892 lines) fit and the 90th percentile (3,381 lines) does not.

## Endpoints and auth

| Route | Question types | Answer field | Usage keys | Source |
| - | - | - | - | - |
| `POST https://ai-gateway.vercel.sh/v1/evaluate` | `boolean`, `choice`, `score` | `probability` for boolean | `inputTokens`, `outputTokens` | [Vercel: Evaluation](https://vercel.com/docs/ai-gateway/modalities/evaluation#http-api) |
| `POST https://ai-gateway.vercel.sh/typesafe/v1/systemone` | `noul`, `choice`, `score` | `noul` | `input_tokens`, `output_tokens` | [Vercel: TypeSafe API](https://vercel.com/docs/ai-gateway/sdks-and-apis/typesafe) |
| `POST https://api.typesafe.ai/v1/systemone` (direct, not the gateway) | `noul`, `choice`, `score` | `noul` | `input_tokens`, `output_tokens` | [TypeSafe: API reference](https://docs.typesafe.ai/api) |

Both gateway routes take `Authorization: Bearer <AI Gateway API key or Vercel OIDC token>`. The model slug on the gateway is `typesafe-ai/jev`. Direct TypeSafe uses `jev-latest` or a pinned `jev-1.13.0` ([TypeSafe: Models](https://docs.typesafe.ai/models#aliases)). The gateway catalog lists only `typesafe-ai/jev`, released 2026-09-15, and its example response echoes `"model": "typesafe-ai/jev"` rather than a versioned ID. I found no documented way to pin a Jev version through the gateway.

The AI SDK exposes the same capability as `experimental_evaluate` in AI SDK 7+. The OpenAI, Anthropic and Cohere compatible gateway endpoints do not support evaluation ([Vercel: Evaluation](https://vercel.com/docs/ai-gateway/modalities/evaluation)). A Rust daemon would call the HTTP route directly.

## Request shape (`/v1/evaluate`)

```json
{
  "model": "typesafe-ai/jev",
  "state": "string | object | array",
  "questions": {
    "<your id>": {
      "type": "boolean",
      "instructions": "string | object | array",
      "criteria": { "true": "string | object | array", "false": "string | object | array" }
    }
  },
  "providerOptions": {
    "gateway": { "zeroDataRetention": true, "only": ["typesafe-ai"] }
  }
}
```

- `state` accepts a string, object or array. Text only, no images or binaries ([TypeSafe: Models](https://docs.typesafe.ai/models)).
- Question ids are yours. TypeSafe says the key is not sent to the model and has no effect on inference ([TypeSafe: API](https://docs.typesafe.ai/api#request-body)). The question text has to carry the meaning.
- `instructions` and `criteria` accept free-form JSON. TypeSafe's docs use the keys `question`, `inspect` (names the state field to look at), `what`, `not_for` and `examples` as a convention ([TypeSafe: Advanced structure](https://docs.typesafe.ai/primitives/advanced)). Parallax sends exactly that shape: `instructions: {question, inspect}` and `criteria: {true: {what, not_for, examples}, false: {...}}` (`~/code/parallax/parallax/jev.py`, classes `Instructions`, `Option`, `BoolCriteria`).
- Questions in one request are independent. One answer never becomes context for another. A dependent question needs a second request ([TypeSafe: Primitives](https://docs.typesafe.ai/primitives)).

### Other question types

| Type | `criteria` | Answer | Notes |
| - | - | - | - |
| `boolean` | optional `{true, false}` | `{type, probability}`, P(true) in [0, 1] | No `confidence` field. The probability is the whole distribution ([TypeSafe: Noul](https://docs.typesafe.ai/primitives/noul)). |
| `choice` | required map of option to description (or null), max 255 options | `{type, choice, probabilities}` | TypeSafe's direct API also returns `confidence`. The gateway `/v1/evaluate` example omits it, but gateway fallback conditions read it. |
| `score` | required array of level labels, lowest first, at least 2 | `{type, score, probabilities}` | `score` is probability-weighted and can land between levels. Direct API adds `legend` and `confidence`. |

Sources: [Vercel: Evaluation](https://vercel.com/docs/ai-gateway/modalities/evaluation#question-types), [TypeSafe: API](https://docs.typesafe.ai/api#question-types).

TypeSafe warns that a boolean's value is the probability of "yes", not a degree. "Is the candidate strong in Python?" returned 0.81 for two years of daily use and 0.92 for eight. For a graded judgment, use `score` ([TypeSafe: Noul](https://docs.typesafe.ai/primitives/noul)).

## Response shape (`/v1/evaluate`)

```json
{
  "model": "typesafe-ai/jev",
  "answers": { "refund": { "type": "boolean", "probability": 0.98 } },
  "usage": { "inputTokens": 275, "outputTokens": 20 },
  "providerMetadata": {
    "gateway": {
      "routing": { "originalModelId": "typesafe-ai/jev", "resolvedProvider": "typesafe-ai", "canonicalSlug": "typesafe-ai/jev", "finalProvider": "typesafe-ai" },
      "cost": "0.00001155",
      "marketCost": "0.00001155",
      "surchargeCost": "0",
      "gatewayCost": "0.00001155",
      "generationId": "gen_..."
    }
  }
}
```

Source: [Vercel: Evaluation, HTTP API](https://vercel.com/docs/ai-gateway/modalities/evaluation#http-api). `cost` is a decimal string in USD.

Parallax also reads a top-level `warnings` array that the gateway docs don't mention, and treats any warning as "don't trust this answer" (`jev.py` `_Reply`, `triage.py` `fold_jev`). It validates that the answer keys equal the question keys exactly and that each probability is a finite float in [0, 1].

## Limits

| Limit | Value | Source |
| - | - | - |
| State plus the longest single question | 32,000 tokens | [TypeSafe: Models](https://docs.typesafe.ai/models), [gateway `/v1/models`](https://ai-gateway.vercel.sh/v1/models) (`context_window: 32000`) |
| State plus all questions | 64,000 tokens | same |
| Output tokens | no limit, not billed | same |
| Choice options | 255 | [TypeSafe: API](https://docs.typesafe.ai/api#choice) |
| Streaming | not supported | gateway `/v1/models` description |
| Input | text only; English is the strongest language | [TypeSafe: Models](https://docs.typesafe.ai/models#language-support) |

The docs don't say what an oversized request returns. It is probably a 422 validation error, but I did not test it. Parallax does not rely on that error. It estimates tokens as bytes × 0.25 before sending and caps triage state at 24,000 bytes (`triage.py` `PROMPT_CAP_BYTES`).

Size also costs accuracy. TypeSafe's jaggedness page for jev-1.13 says accuracy falls as state grows with detail unrelated to the question, and recommends filtering in code and sending only the fields a question needs ([TypeSafe: Jev 1.13 jaggedness](https://docs.typesafe.ai/model-jaggedness/jev-1.13#large-state-full-of-irrelevant-detail)). The same page lists other weak spots that matter for PR judging: literal reading, arithmetic, date comparison, multi-hop indirection, and adversarial content. PR text written by an agent counts as adversarial content.

## Pricing

- Jev: $0.042 per million input tokens, output free. TypeSafe's list price ([TypeSafe: Models](https://docs.typesafe.ai/models)) matches the gateway catalog (`pricing.input: "0.000000042"`, `output: "0"`).
- The gateway adds no markup on the paid tier ([Vercel: Pricing](https://vercel.com/docs/ai-gateway/pricing)). Per-request `only` is free. Per-request `zeroDataRetention` is free but limited to Pro and Enterprise teams.
- Worked numbers: a full 32k-token call costs $0.00134. A Run that asks 20 questions over 5 scoped states of 10k tokens each costs about $0.002. Parallax's measured Jev-only batches agree: ten calls cost $0.0032 in total (`~/code/parallax/docs/jev-judge.md`, v18 diagnostic).
- Batching matters for cost because the state is billed once per request. TypeSafe's cookbook put 13 questions over one article in a single call and paid 12.2x less than 13 separate calls, with identical answers ([TypeSafe: Parallel questions](https://docs.typesafe.ai/cookbooks/parallel_questions)).

At these prices Jev is not where a cost budget bites. Any LLM fallback is. Parallax's fallback supplied 99.17% of one audit's cost (`jev-judge.md`, v13 ablation).

## Rate limits and errors

- TypeSafe direct: 40 requests/s and 100K tokens/s, adjusted dynamically without notice ([TypeSafe: Models](https://docs.typesafe.ai/models)).
- Gateway free tier: a lower per-model limit. Paid tier: no gateway limit, provider limits still apply ([Vercel: Rate limits](https://vercel.com/docs/ai-gateway/rate-limits)).
- `429` for rate limits, sometimes with `retry-after`. Parallax saw none on Jev's 429s and uses jittered backoff (1, 2, 4, 8, 16, 30 s) with 4 calls in flight. At 8 in flight it lost 25% of calls on 2026-09-24 (`jev.py` constants and comments).
- TypeSafe direct also returns `529 Overloaded`. Parallax retries 429, 500, 502, 503 and 504.
- `422` for a malformed request. `402` with `quota_for_entity_exceeded` when a gateway budget is hit ([Vercel: Rate limits](https://vercel.com/docs/ai-gateway/rate-limits#rate-limits-versus-budgets)). Gateway budgets can be set per team, project, API key or user, which gives slopwatch a per-key spend cap for free.
- TypeSafe's SDK defaults to a 10 s timeout ([TypeSafe: SDK constants](https://docs.typesafe.ai/sdk/python/api/constants)). Parallax uses 15 s.

## Latency

Neither Vercel nor TypeSafe publishes a latency figure, and the gateway catalog reports `latency_last_1h: null`. Measurements I found:

| Case | Latency | Source |
| - | - | - |
| 13 questions, one ~54k-character article | 0.27 s mean over 5 runs | [TypeSafe: Parallel questions](https://docs.typesafe.ai/cookbooks/parallel_questions) |
| 4 questions, parallax mail diagnostic | 0.695 s | `jev-judge.md` |
| Two v17 calls | 0.552 s and 18.010 s | `jev-judge.md` |

TypeSafe says questions in one request run in parallel, so adding questions barely changes response time ([TypeSafe: Speculative fan-out](https://docs.typesafe.ai/patterns/fan-out)). Expect sub-second calls with a long tail of many seconds when the provider is busy. Retries after 429 add tens of seconds.

## Gateway fallback to another model

The gateway can rerun an uncertain evaluation on another model when you put one conditional entry in `providerOptions.gateway.models`. For booleans the condition is `probabilityBetween: [lo, hi]`, inclusive, per question or across all booleans. `any` combines conditions. Both stages are billed and their latency adds up. If the fallback fails, the request fails even though the primary answered ([Vercel: Evaluation fallbacks](https://vercel.com/docs/ai-gateway/models-and-providers/evaluation-fallbacks)). Parallax does its own fallback in code instead, with a required-agreement LLM panel.

## What PR evidence fits

Budget: 32k tokens for state plus the longest question. Parallax estimates 0.25 tokens per byte. Code usually tokenizes denser than prose, so I planned with 3 to 4 bytes per token, giving roughly 96 to 128 KB of state. The real ratio for Jev's tokenizer is unpublished; the gateway returns the billed `inputTokens`, so a first real call can calibrate it.

Measured from the last 100 PRs in `agent-labs-dev/parallax` (via `gh pr list` and `gh pr diff`):

| Evidence | Typical size | Fits? |
| - | - | - |
| PR title and description | median 3.1 KB, p90 5.4 KB, max 16 KB | yes |
| Linked issue body | similar scale, a few KB | yes |
| Review findings (agent or human comments) | a few KB per review | yes, unless there are many rounds |
| Unified diff | about 65 to 75 bytes per changed line (sampled PRs: 147 lines = 10.6 KB, 335 lines = 22.7 KB) | median 276 lines (~19 KB) yes; p75 892 lines (~60 KB) yes; p90 3,381 lines (~230 KB) no; max 84,407 lines no |

So one call can carry the description, issue, findings and diff for about three quarters of PRs. The rest need a different representation: per-file calls, a file list with stats, or a diff filtered to the files a question is about. The jaggedness guidance argues for scoped states per question group anyway, not one state with everything in it. With parallel requests at $0.042/Mtok, splitting costs almost nothing extra.

## Open points

1. ZDR is unclear. Parallax sends `zeroDataRetention: true` with `only: ["typesafe-ai"]` and its calls succeed per its docs. The live gateway endpoint listing (`https://ai-gateway.vercel.sh/v1/models/typesafe-ai/jev/endpoints`) shows a single provider named `digitalocean` with `has_zdr: false`, and `/v1/models` shows `"zdr": "none"`. Vercel's ZDR page lists TypeSafe AI as ZDR-compliant, and says a request with `zeroDataRetention: true` fails with 400 `no_providers_available` when no compliant provider exists. TypeSafe's own docs offer ZDR only to enterprise customers. Sending private PR diffs makes this matter. Settling it needs one real call.
2. No version pinning through the gateway. The alias can move under slopwatch, which matters if Gate thresholds get tuned against one Jev version.
3. Oversize behavior is undocumented. Whatever error the gateway returns, the Step should measure state before sending rather than rely on it.
