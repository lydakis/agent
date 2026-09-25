# Amazon Bedrock as a provider

Surveyed 2026-09-22 and revised 2026-09-25 against AWS, Anthropic and OpenAI
documentation, then run live on 2026-09-25. Facts are labelled: **documented**
cites a page, **source** cites this repository, **observed** comes from the
[live run](#the-live-run), **unverified** is a hypothesis still open.

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
request cap. It also takes the body unsigned, so a call reads its history
once. Runtime is one explicit URL away, for its global routing and no
regional premium, at the cost of a second store read per call to sign the
body; see [providers and models](RUST_PROTOTYPE.md#providers-and-models).

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

Request bodies stream from the store and are never held. **Observed**: Mantle
accepts the payload signed as `UNSIGNED-PAYLOAD` over TLS, so a Mantle call
reads its history once. Runtime does not: it puts the SHA-256 of the body it
received into the canonical request whatever `x-amz-content-sha256` says, and
answers 401 "The request signature we calculated does not match". A runtime
call therefore reads its history from the store twice, once to digest it and
once as it streams (**source**: `Items`, `aws::payload`). That costs a second
store read and a SHA-256 pass per call, and no memory: the body is still never
assembled. Buffering the body instead would read once but hold every in-flight
request whole, up to the context window per call, which is the wrong trade
for many active bots. The digest runs before admission, so it does not hold a
connection-start slot.

## Features, carried over or not

| Feature | First-party | Bedrock | Status |
| --- | --- | --- | --- |
| Messages and Responses over SSE | yes | **observed** on both endpoints, Claude and GPT-5.6 | carried |
| Prompt caching, explicit and automatic | yes | **documented** explicit and implicit on both; **observed** on both, top-level `cache_control` included | carried |
| Responses `prompt_cache_key` | yes | **documented** supported; **observed** cached input on both | carried |
| Responses `store: false` with `reasoning.encrypted_content` | yes | **observed** on both: returned and accepted on replay | carried |
| Responses reasoning summaries (`summary: "auto"`) | yes | **observed** empty on both for GPT-5.6 Luna at high | requested; none arrived to stream |
| Adaptive thinking with `output_config.effort` | yes | **observed** on both | carried |
| Legacy thinking budget (Haiku 4.5) | yes | **observed** on Mantle | carried |
| Thinking-binding check (`anthropic-beta: thinking-binding-controls-2026-08-01`, `block_binding`) | yes (PR #12) | **observed** on both: signed blocks replayed with no drops; on Mantle `block_binding` without the beta is a 400 | carried |
| Server-side fallbacks (`fallbacks`, PR #13) | yes | **documented** unsupported; **observed** on Mantle: the field and the beta are each a 400 | not carried: Bedrock requests send neither |
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

Observed 2026-09-25 in us-east-1, with an SSO profile resolved through the
AWS CLI, on release builds with an isolated store, in two runs: about twenty
calls on 84e10c6, then eighteen on 4cef67a after the fixes below and the rebase
onto server-side fallbacks. Prompts were synthetic (list three files and count
them, check a number for primality). Token counts are per call.

**Two transport faults**, both now pinned by tests:

- At 84e10c6 every daemon call to Mantle failed as `provider_connection_failed`
  ("stream closed because of a broken pipe"). An HTTP/2 trace showed Mantle
  answering with GOAWAY `FRAME_SIZE_ERROR` right after the request's DATA
  frames, and a standalone reqwest probe reproduced it with a single empty
  chunk in a streamed body: Mantle refuses a zero-length DATA frame that does
  not end the stream. The daemon sent one whenever a request's context prefix
  was empty. Bodies now drop empty chunks (**source**: `framed`).
- The same probe showed that an explicit `host` header on HTTP/2 breaks the
  signature (401), since the client sends `:authority` as well. The signer
  already signs `host` without sending it; a test keeps it that way.

**Mantle, Claude**, with the empty-chunk fix:

| Model, effort | Turns | Result |
| --- | --- | --- |
| `anthropic.claude-sonnet-5`, low | shell tool turn, then a turn without tools | accepted; second call of the first turn read 2,346 cached tokens; no thinking blocks at low |
| `anthropic.claude-sonnet-5`, high | tool turn, then without tools | two signed thinking blocks stored and replayed, accepted with no binding drops; cache reads 2,362 and 2,503 |
| `anthropic.claude-haiku-4-5`, low | one turn | budget form accepted, thinking streamed; no cache read, as the prompt is under Haiku's cache minimum |
| `global.anthropic.claude-sonnet-5` | one call | 404 "does not exist": Mantle takes bare in-region ids only |

The daemon's full captured body replayed with curl gave 200 with the beta
header and 400 `thinking.adaptive.block_binding: Extra inputs are not
permitted` without it, so Mantle validates fields strictly and honors the
beta. Curl also showed Mantle accepting both a hashed and an unsigned payload,
and a body streamed without a length over HTTP/2 and HTTP/1.1 chunked.

**Mantle, OpenAI**: `openai.gpt-5.6-luna` at low ran a tool turn and a turn
without tools (second turn read 1,040 cached tokens). At high it returned three
reasoning items with `encrypted_content`, and replaying them was accepted
(1,067 cached). `openai.gpt-oss-20b` is refused on this route: "does not
support the '/openai/v1/responses' API". The model list is `GET /v1/models` on
the Mantle host, not `/openai/v1/models` (404); it named the GPT-5.4 to
GPT-6 and gpt-oss families and Claude Haiku 4.5 to Opus 5.5.

**Runtime** on 84e10c6: `global.anthropic.claude-sonnet-5` through the daemon
was refused with a signature mismatch, and curl showed why: runtime signs the
real body digest. With the payload hashed it answered 200.

**Second run, on 4cef67a**, every turn at high effort, a tool turn and then a
turn without tools on one bot:

| Endpoint, model | Result |
| --- | --- |
| Mantle, `anthropic.claude-sonnet-5` | accepted; three signed thinking blocks stored in turn 1 and replayed in turn 2 with none dropped; cache reads from the second call on (3,170 of 3,588 in turn 2) |
| Mantle, `openai.gpt-5.6-luna` | accepted; two encrypted reasoning items replayed; turn 2 read 1,360 of 1,647 cached; the first prompt (1,001 tokens) is under the 1,024-token cache minimum |
| Runtime, `global.anthropic.claude-sonnet-5` | accepted on every call with the hashed payload, no signature mismatch or credential refresh; two thinking blocks replayed with none dropped; turn 2 read 2,963 of 3,336 cached |
| Runtime, `global.openai.gpt-5.6-luna` | accepted; runtime serves it on `/openai/v1/responses`, as `global.` and `us.` inference profiles; three encrypted reasoning items replayed; turn 2 read 1,472 of 1,727 cached |

A trivial prompt at high effort produced no thinking at all: adaptive thinking
skipped it. Both GPT-5.6 Luna bindings returned reasoning items whose summary
was empty, so no reasoning text streamed. Curl to Mantle with `"fallbacks":
"default"` gave 400 `fallbacks: Extra inputs are not permitted`, with or
without the `server-side-fallback-2026-07-01` beta, and the beta alone gave
400 for an unexpected `anthropic-beta` value, so both stay out of Bedrock
requests.

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
