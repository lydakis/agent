# Amazon Bedrock as a provider

Surveyed 2026-09-22 against AWS and Anthropic documentation; no paid call was
made, so every runtime behaviour below is documented or inferred, never
observed. Facts are labelled: **documented** cites a page, **source** cites
this repository, **unverified** is a hypothesis a smoke run must settle.

The question was whether Agent should grow a Bedrock provider covering both the
`bedrock-mantle` and `bedrock-runtime` endpoints, for Anthropic and OpenAI
models. The short answer is that Bedrock needs no new wire family and no new
adapter: it speaks the two families this runtime already encodes, over plain
SSE, with the key headers the adapter already sends. What it does not carry is
the rate-limit telemetry the pacer is built on, and its model ids break two
name-prefix rules in the request encoder. Those are the whole cost.

## The four routes

Both endpoints serve both families. Bedrock routes are therefore base URLs, not
adapters (**source**: `src/provider.rs`, `bedrock_routes_are_ordinary_base_urls_for_the_families_we_speak`).

| Endpoint | Family | Path | Model id form |
| --- | --- | --- | --- |
| `bedrock-mantle.{region}.api.aws` | Anthropic Messages | `/anthropic/v1/messages` | `anthropic.claude-sonnet-5` |
| `bedrock-mantle.{region}.api.aws` | OpenAI Responses | `/openai/v1/responses` | `openai.gpt-5.6-sol` |
| `bedrock-runtime.{region}.amazonaws.com` | Anthropic Messages | `/anthropic/v1/messages` | `us.anthropic.claude-sonnet-5` |
| `bedrock-runtime.{region}.amazonaws.com` | OpenAI Responses | `/openai/v1/responses` | `openai.gpt-5.6-sol` |

**Documented**: the Anthropic route requires `anthropic-version: 2023-06-01`
and accepts a Bedrock API key in `x-api-key`; the OpenAI route accepts the same
key as `Authorization: Bearer`. Streaming on both is ordinary
`text/event-stream` with the families' own event names. Model ids carry a
vendor prefix, and only `bedrock-runtime` accepts the cross-region inference
profiles (`us.`, `eu.`, `global.`); `bedrock-mantle` rejects them.

**Source**: `Provider::new` appends the family route to the configured base
path, and `complete_inner` already picks `x-api-key` plus `anthropic-version`
for `Family::Anthropic` and bearer for `Family::Responses`. That is exactly
what Bedrock wants, so all four routes bind today through the existing spec:

```
--provider mantle=anthropic,https://bedrock-mantle.us-east-1.api.aws/anthropic/v1,AWS_BEARER_TOKEN_BEDROCK
--provider mantle-oai=responses,https://bedrock-mantle.us-east-1.api.aws/openai/v1,AWS_BEARER_TOKEN_BEDROCK
--provider br=anthropic,https://bedrock-runtime.us-east-1.amazonaws.com/anthropic/v1,AWS_BEARER_TOKEN_BEDROCK
--provider br-oai=responses,https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1,AWS_BEARER_TOKEN_BEDROCK
```

Bedrock gets no named default beside `openai` and `anthropic`, because its URL
carries both the region and the endpoint choice and there is no useful single
value for either. Four bindings is also the right shape rather than a
concession: **documented**, mantle and runtime quotas are independent even for
the same model, so one `Pools` per binding is what the quota actually is.

The legacy `InvokeModelWithResponseStream` route, with its binary
`application/vnd.amazon.eventstream` framing and per-model body dialect, is the
one thing here that would cost a second framing decoder beside `sse::Decoder`.
Nothing needs it: every current model is reachable on the two SSE routes above,
and Claude Opus 4.7 and later are documented as Mantle-only. Do not implement it.

## Where it strains

Four real gaps, in the order they would bite.

### 1. The pacer goes blind

`pace.rs` learns each pool's per-minute allowance only from response headers,
and a bucket with no reported limit is treated as unbounded
(**source**: `Bucket::per_minute` is `None` until `learn` runs). **Documented**:
Bedrock's Anthropic endpoint does not send rate-limit response headers at all,
and the Mantle quota page publishes no remaining-quota header either. So on
Bedrock the pacer never learns anything, admits every request immediately, and
the runtime's whole proactive mechanism is inert. All that remains is the
reactive path: a 429 calls `pace.limited(retry-after)`, and with no
`retry-after` that is a flat one-second block, retried forever without backoff.
A fleet against a Bedrock quota would spin on that.

The shape of the fix is already right, which is the good news. **Documented**:
Mantle enforces separate input-tokens-per-minute and output-tokens-per-minute
quotas per model per region and no RPM quota at all, while runtime enforces one
combined TPM. Those are dimensions 1, 2 and 0 of the pacer's existing `DIMS`
array. What is missing is a way to seed a limit from configuration instead of
from headers, plus real backoff when a block repeats. That is a change to
`pace.rs`, not to the adapter.

### 2. Model ids break two prefix rules

**Source**: `legacy_thinking` in `src/provider.rs` decides between an explicit
thinking budget and adaptive thinking by `model.starts_with("claude-haiku-4-5")`
and six siblings. Bedrock ids are `anthropic.claude-haiku-4-5` and
`us.anthropic.claude-haiku-4-5`, so every one of those matches fails: the
adapter would send adaptive thinking to a model that requires a budget, and the
[Anthropic smoke run](ANTHROPIC_SMOKE.md) records exactly that as a
`provider_http_400`. `Family::pool_key` has the milder version of the same
problem: it strips an eight-digit date suffix that Bedrock ids do not carry, so
it is harmless today but equally name-shaped.

This is [NEXT.md](NEXT.md) item 17's per-model capability configuration
arriving with a bill attached. A prefix strip would paper over it; the right
answer is to look capability up rather than parse it out of a name.

### 3. The fixed output ceiling costs quota

**Source**: every Anthropic request sends `max_tokens: 32_768`, and
`with_max_output_tokens` refuses to set anything else for that family.
**Documented**: Mantle checks input tokens **plus `max_tokens`** upfront against
the input-token quota. So each call reserves 32,768 tokens of input allowance it
will almost never use. Against the documented 20,000,000 input TPM default for
Claude Opus 4.7 that padding alone caps the endpoint near 610 requests a
minute before a single prompt token is counted. This is the other half of item
17, and Bedrock turns it from tidiness into throughput.

### 4. Credentials expire under a long-lived daemon

**Documented**: Bedrock bearer tokens are short-term, valid at most 12 hours;
SigV4 is the durable path. **Source**: `run` in `src/server/mod.rs` reads each
`key_env` once at startup and the `Provider` holds that string for the life of
the process. A daemon meant to run for days would begin failing every Bedrock
call after twelve hours with no refresh path.

SigV4 is the strain worth naming precisely. Request bodies here are streamed,
never assembled — `Items` yields pre-encoded history with only a byte count
known in advance — so the runtime cannot hash a payload it never holds. SigV4
over a streamed body therefore only works with `UNSIGNED-PAYLOAD`, which is the
one open question: **unverified** whether Bedrock accepts it on these routes. If
it does, SigV4 is a per-request signing step over headers and a credential
source that can refresh. If it does not, streamed bodies and SigV4 are
incompatible and Bedrock is limited to twelve-hour tokens with a re-read.

## Smaller unknowns for a smoke run

- **Unverified**: whether `include: ["reasoning.encrypted_content"]`, which the
  Responses encoder always sends and which stateless continuation depends on, is
  honoured on Bedrock. A third-party bug report describes Bedrock Runtime
  replaying encrypted reasoning, which suggests it is, but that is not a source.
- **Unverified**: whether the top-level automatic `cache_control` breakpoint and
  `output_config: {"effort": …}` are accepted. **Documented**: Mantle rejects
  `output_config.format` with a 400 and its prompt caching is model-dependent;
  neither statement settles the fields the encoder actually sends.
- **Unverified**: what Bedrock reports when output TPM runs out mid-generation.
  **Documented**: generation stops with a finish reason rather than a 429.
  **Source**: `anthropic::State::finish` turns `max_tokens` into
  `provider_incomplete` and anything unrecognised into `provider_incomplete`
  too, and neither reaches `pace.limited`. Either way a quota exhaustion
  surfaces as a turn failure with no pacing feedback, which is the sharpest
  version of gap 1.
- **Documented** and worth stating: structured outputs, `count_tokens`, server-
  side tools, Files API input sources and message batches are unsupported on the
  Bedrock Anthropic route. None is used here.

## Recommendation

Do not build a Bedrock adapter; there is nothing to adapt. The reach is already
there, and the four gaps above are all pre-existing weaknesses that Bedrock
exposes rather than causes. Take them in this order:

1. A paid smoke run on one Mantle Anthropic model and one Mantle OpenAI model,
   in the shape of [ANTHROPIC_SMOKE.md](ANTHROPIC_SMOKE.md), to settle every
   **unverified** above. Nothing else should be built before it.
2. Item 17's per-model capability configuration, which gaps 2 and 3 both need
   and which no longer has a workaround once Bedrock ids are in play.
3. Configured pool limits and real backoff in `pace.rs`, for any endpoint that
   publishes no rate-limit headers.
4. SigV4 only if the smoke run shows twelve-hour tokens are not enough, and only
   after `UNSIGNED-PAYLOAD` is confirmed.

## Sources

Observed 2026-09-22.

- [Endpoints supported by Amazon Bedrock](https://docs.aws.amazon.com/bedrock/latest/userguide/endpoints.html)
- [APIs supported by Amazon Bedrock](https://docs.aws.amazon.com/bedrock/latest/userguide/apis.html)
- [Inference using Anthropic Messages API](https://docs.aws.amazon.com/bedrock/latest/userguide/inference-messages-api.html)
- [API compatibility](https://docs.aws.amazon.com/bedrock/latest/userguide/models-api-compatibility.html)
- [Responses API (Bedrock Mantle)](https://docs.aws.amazon.com/bedrock/latest/userguide/bedrock-mantle.html)
- [Quotas for the bedrock-mantle endpoint](https://docs.aws.amazon.com/bedrock/latest/userguide/quotas-mantle.html)
- [Claude in Amazon Bedrock (Opus 4.7 and later)](https://platform.claude.com/docs/en/build-with-claude/claude-in-amazon-bedrock)
- [OpenAI models in Amazon Bedrock](https://developers.openai.com/api/docs/guides/amazon-bedrock)
