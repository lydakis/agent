# Amazon Bedrock as a provider

Surveyed 2026-09-22 and revised 2026-09-25 against AWS, Anthropic and OpenAI
documentation. Facts are labelled: **documented** cites a page, **source**
cites this repository, **unverified** is a hypothesis the live run below must
settle. No paid Bedrock call has been made yet.

Bedrock needs no new wire family: it serves the Anthropic Messages API and the
OpenAI Responses API over ordinary SSE, so a Bedrock binding is a base URL and
a way to authenticate. The work is authentication, and carrying over the
features that depend on the first-party APIs.

## What the vendors recommend now

- **Endpoint.** Anthropic documents Claude Opus 4.7 and later on Bedrock at
  `bedrock-mantle.{region}.api.aws/anthropic/v1/messages`, with its
  `AnthropicBedrockMantle` client. OpenAI documents Mantle,
  `bedrock-mantle.{region}.api.aws/openai/v1`, as the endpoint for new
  deployments. AWS serves both families on `bedrock-runtime` too, through
  geographic (`us.`, `eu.`, `au.`) or global (`global.`) inference profiles.
- **Authentication.** All three name SigV4 through the standard AWS credential
  chain as the production path. Bedrock API keys, sent as a bearer token or
  `x-api-key`, are for exploration; short-term ones last at most twelve hours.
- **Quotas.** **Documented**: Mantle has separate input and output
  tokens-per-minute quotas per model and region, and no requests-per-minute
  quota. Runtime counts input plus output against one tokens-per-minute quota,
  deducts `input + max_tokens` when a call starts, and burns output down at 10x
  for current Claude and GPT-5.6 models (15x for Claude 4.8). The two
  endpoints' quotas are independent even for the same model.
- **Price.** **Documented** by Anthropic: regional endpoints carry a 10%
  premium over global routing. Mantle serves Claude in-region, so the
  premium is inferred to apply there; runtime's `global.` profiles avoid it.

## What was chosen

`--provider bedrock` binds Claude on Mantle and `--provider bedrock-openai`
binds OpenAI models on Mantle, in `AWS_REGION` (or `AWS_DEFAULT_REGION`). Mantle
because both model vendors document it and because its quota shape suits a
fleet: separate input and output allowances with no output burndown and no
request cap. Runtime is one explicit URL away, for its global routing and no
regional premium; see [providers and models](RUST_PROTOTYPE.md#providers-and-models).

Any Bedrock URL without a key field signs with SigV4 (**source**:
`src/provider/aws.rs`). The signer is in-tree on `aws-lc-rs`, which rustls
already builds, so Bedrock adds no crate to the lock file. It matches the AWS
SigV4 test suite's published signatures and botocore 1.43's output for the
same requests. The AWS SDK's signer and credential chain (`aws-sigv4`,
`aws-config`) measured 39 more crates, before the HTTP client its SSO and
instance-role providers need; instead,
credentials resolve in the chain's own order: static environment keys first,
then the AWS CLI's `configure export-credentials`, which is the chain itself
(profiles, SSO, assumed roles, container and instance roles) in the standard
`credential_process` format. Temporary keys are re-resolved five minutes before
expiry, single-flight, and once on a 401 or 403, which retries the call; a
refused fleet re-runs the CLI at most every ten seconds.

Request bodies stream from the store and are never held, so the payload is
signed as `UNSIGNED-PAYLOAD` over TLS. **Unverified**: whether Bedrock accepts
that. botocore sends it only for operations modelled as unsigned, and none of
bedrock-runtime's are. If Bedrock refuses it, the alternative is hashing each
request by reading its history from the store twice; the live run decides.

## Features, carried over or not

| Feature | First-party | Bedrock | Status |
| --- | --- | --- | --- |
| Messages and Responses over SSE | yes | yes, both endpoints | carried |
| Prompt caching, explicit and automatic | yes | **documented** explicit and implicit on both | carried; the top-level `cache_control` spelling is **unverified** |
| Responses `prompt_cache_key` | yes | **documented** supported | carried |
| Responses `store: false` with `reasoning.encrypted_content` | yes | `store` documented; encrypted reasoning **unverified** | carried, pending the live run |
| Adaptive thinking with `output_config.effort` | yes | not named by Bedrock pages | carried, **unverified** |
| Thinking-binding check (`anthropic-beta: thinking-binding-controls-2026-08-01`, `block_binding`) | yes (PR #12) | not named; Bedrock pages list no beta headers | carried, **unverified**: an unknown beta may be refused |
| Server-side fallbacks (`fallbacks`, PR #13) | yes | **documented** unsupported; use client-side fallback | not carried |
| Responses over WebSocket | yes | **documented** unsupported on either endpoint | refused for Bedrock URLs |
| Rate-limit headers for pacing | yes | **documented** absent | escalating refusal backoff instead |
| Legacy thinking budgets by model name | yes | ids are vendor- and profile-prefixed | carried: `us.anthropic.claude-haiku-4-5…` reads as `claude-haiku-4-5` |
| Output cap | Responses only | quota deducted up front | `--max-output-tokens` now reaches Anthropic as `max_tokens` |

## The gaps from the first survey, on current main

1. **The pacer learned nothing without headers.** A 429 naming no delay used
   to close the pool for a flat second, forever. It now closes it for a
   second, doubling to 32 each time the pool reopens only to be refused, and
   resets on the next accepted call (**source**: `Pace::limited`). Refusals of
   calls already in flight during a block are one overload, not several. The
   turn's own jittered backoff still spreads retries. Proactive pacing would
   need the account's quota numbers, which Bedrock does not return per call.
2. **Model ids broke the name rules.** `legacy_thinking` now reads the Claude
   model inside a Bedrock id, so a Haiku 4.5 id on Bedrock gets a budget and
   current models get adaptive thinking. Pool keys are left as given: geo and
   global profiles are separate quotas.
3. **The fixed 32k `max_tokens`.** Still the default, but `--max-output-tokens`
   now sets it, with a legacy thinking budget clamped to leave 1,024 tokens for
   the answer. On both Bedrock endpoints the bound is charged against quota up
   front, so it is the throughput knob.
4. **Twelve-hour tokens read once.** SigV4 through the chain replaces them:
   keys refresh themselves. A Bedrock API key in a key field still works and
   is still read once.

## The live run

To settle every **unverified** row, in the shape of
[ANTHROPIC_SMOKE.md](ANTHROPIC_SMOKE.md), on one Claude and one OpenAI model:

- SigV4 with `UNSIGNED-PAYLOAD` on Mantle and on runtime.
- A tool-using turn with thinking on a current Claude model, which exercises
  the thinking-binding beta header, `block_binding`, adaptive thinking, effort
  and automatic caching together; and one on Haiku 4.5 for the budget form.
- A two-turn Responses conversation, which shows whether encrypted reasoning
  comes back and is accepted when replayed.
- Whether Mantle accepts a `global.` profile id.

## Sources

Observed 2026-09-22 to 2026-09-25.

- [Endpoints supported by Amazon Bedrock](https://docs.aws.amazon.com/bedrock/latest/userguide/endpoints.html)
- [Inference using Anthropic Messages API](https://docs.aws.amazon.com/bedrock/latest/userguide/inference-messages-api.html)
- [Quotas for the bedrock-mantle endpoint](https://docs.aws.amazon.com/bedrock/latest/userguide/quotas-mantle.html)
- [Quotas for the bedrock-runtime endpoint](https://docs.aws.amazon.com/bedrock/latest/userguide/quotas-runtime.html)
- [How tokens are counted in Amazon Bedrock](https://docs.aws.amazon.com/bedrock/latest/userguide/quotas-token-burndown.html)
- [Claude Opus 5 model card](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-anthropic-claude-opus-5.html)
- [Claude Sonnet 5 model card](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-anthropic-claude-sonnet-5.html)
- [Claude in Amazon Bedrock](https://platform.claude.com/docs/en/build-with-claude/claude-in-amazon-bedrock)
- [OpenAI models in Amazon Bedrock](https://developers.openai.com/api/docs/guides/amazon-bedrock)
- botocore 1.43.102 `auth.py` and the bedrock-runtime service model, read for
  how it signs payloads.
