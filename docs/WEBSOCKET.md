# Responses over WebSocket

Question (2026-09-24): would OpenAI's WebSocket mode for the Responses API make
this runtime faster? Short answer: probably yes for tool-heavy turns on the
`openai` and `chatgpt` providers, mostly through server-side work it lets
OpenAI skip, not through the socket itself. The size of the win for this
runtime is unmeasured. It needs a paid, matched screen before it counts as a
result. The [prototype](#prototype) exists so that the comparison that matters,
this daemon over HTTP against this daemon over WebSocket, can be run.

## Source facts

Observed 2026-09-24.

- OpenAI's [WebSocket mode guide](https://developers.openai.com/api/docs/guides/websocket-mode):
  a WebSocket to `/v1/responses` carries `response.create` events whose body is
  the HTTP create body minus `stream` and `background`. The server keeps the
  latest response of each lane in a connection-local in-memory cache, so the
  next request can send `previous_response_id` plus only the new items (tool
  outputs, the next user message). This works with `store: false` and zero
  data retention; with `store: false` an uncached id fails with
  `previous_response_not_found` and there is no persisted fallback. A
  connection lasts at most 60 minutes (`websocket_connection_limit_reached`).
  `stream_id` names ordered lanes: one lane runs FIFO, different lanes run
  concurrently, at most 16 responses in flight per connection and at most 32
  distinct named lanes per connection (`websocket_stream_limit_reached`).
  `generate: false` preloads tools and instructions without generating.
  Server-side compaction continues across `previous_response_id`. The guide
  says nothing about rate-limit headers or events on the socket, or about
  billing differences.
- OpenAI's [launch post](https://openai.com/index/speeding-up-agentic-workflows-with-websockets/)
  (launch 2026-04-22) attributes the gain to server-side work: rendered tokens
  and model configuration cached in memory so tokenization and some network
  calls are skipped, fewer intermediate service hops, and faster safety
  classifiers. Reported numbers are vendor claims on their workloads: about
  40% faster end to end for loops of 20 or more tool calls, close to 45% better
  time to first token, and partner figures of 30% to 40%.
- Codex, pinned at
  [`e0ef5a1`](https://github.com/openai/codex/tree/e0ef5a1a0f6421601baaa679fb37eddaa4e9c8c1/codex-rs)
  (2026-09-24): the built-in OpenAI provider sets `supports_websockets: true`
  (`model-provider-info/src/lib.rs`), and the same provider serves both API
  keys and the ChatGPT login at `https://chatgpt.com/backend-api/codex`. The
  rollout feature flags `responses_websockets` and `responses_websockets_v2`
  are marked `Removed` (`features/src/lib.rs`), so WebSocket is Codex's default
  transport. It sends `OpenAI-Beta: responses_websockets=2026-02-06`, keeps
  `store: false`, holds one cached connection per session, and falls back to
  HTTP for the rest of the session on a transport failure (`core/src/client.rs`,
  `force_http_fallback`). It sends a delta only when every non-input field
  matches the previous request and the new input extends the previous input
  plus the previous response's output items (`get_incremental_items`);
  otherwise it sends the full input. Codex does not use `stream_id`.
- Inference: the ChatGPT backend accepts WebSocket mode, because Codex uses it
  there by default. This thread made no live call to confirm it.

## What this runtime does today

Read from `src/provider.rs` at `e1d5f8c`.

- Every model call is a fresh HTTP request with `store: false` and
  `include: ["reasoning.encrypted_content"]`. The full history streams from
  the store into the request body on each call, encrypted reasoning included.
- Connections are already persistent: `Transport` holds HTTP/2 client shards,
  each pooled per host with 64 streams per connection. TLS and TCP setup are
  already amortized, so the socket alone buys little here.
- A turn with N model rounds uploads the whole history N times, so per-turn
  upload grows with the square of the round count, and the server re-renders
  the prefix each round. Prompt caching saves compute on that prefix but not
  the upload, the tokenization, or the service hops the launch post names.
- Each bot's calls carry its prompt cache key; see the last section.

## Where the gain would come from

1. Server side, and probably most of it: the provider skips re-rendering and
   re-tokenizing history it already holds, and the launch post says the
   socket path has fewer hops. That lowers time to first token on every round
   after the first, which is the dominant latency term in tool loops.
2. Client side: request bytes per round drop from the whole history to the new
   items, and the store reads and JSON framing that feed the request body drop
   with them. That cuts daemon CPU and network per active bot, which is the
   requirement this project optimizes for, though each saving is small next to
   model time.

## What it costs a fleet

- Affinity. The cache belongs to one connection and one lane, so a bot's
  successive calls must go to the same connection and lane to get a hit. The
  current least-loaded shard lease does not give that; the transport would
  need a bot-to-lane map.
- Lane and connection bounds. With 16 in flight and 32 named lanes per
  connection, a thousand active bots need at least 63 sockets, and each
  connection can serve only 32 distinct bots in its lifetime. Lanes are
  therefore leased to active turns and released when the turn ends, and a new
  turn on a new lane pays one full send.
- Reconnects. Every connection ends by the 60-minute limit, and each reconnect
  costs every bot on it a full send. Staggering connection ages avoids a
  fleet-wide spike.
- Rate limits. The pacer learns limits from `x-ratelimit-*` response headers.
  A socket has one handshake response, not one per call, and the guide names
  no replacement event, so pacing over WebSocket would learn only from usage
  and `rate_limit_exceeded` errors until that is established.
- Stall guard and interrupt. Both carry over: a stalled lane is a stall, and an
  interrupt closes the lane or sends `response.cancel`. Which of the two the
  server honors is unverified.

## Correctness stays in the store

The cache is an optimization the runtime can lose at any time without changing
behavior. The store remains the only history. A delta is sent only when the
request is the previous request plus the previous output plus new items, with
every other field equal, which is the check Codex makes. Every other case
sends the full input with no `previous_response_id`: a fork at a historical
checkpoint, a resume after daemon restart, a compaction that rewrites the
prefix, a changed tool set or reasoning level, a reconnect, and any
`previous_response_not_found`. That last case retries once with the full
input, and that retry is not a provider failure. Nothing about the response id
is made durable.

## Scope

- In: the `openai` and `chatgpt` Responses presets. Azure OpenAI documents the
  same mode.
- Out: Anthropic Messages and Bedrock have no equivalent, so their loop is
  unchanged. OpenRouter and other Responses-compatible endpoints stay on HTTP
  unless they document the mode.
- One behavior per provider, per AGENTS.md: a provider that supports the
  socket uses it, with the full-send path above as the defined cache-miss case
  rather than a legacy HTTP mode. Whether to keep HTTP for a provider whose
  socket fails to connect, as Codex does, is a decision for the prototype.

## Prototype

Built 2026-09-24 on this branch. `--provider NAME=responses-ws[,URL[,KEY_ENV]]`
selects it; `openai=responses-ws` and `chatgpt=responses-ws` keep those
presets' endpoints and credentials. HTTP stays the default for every preset
until the screen below says otherwise. `ready.providers` names the transport,
a client attaching to a daemon compares it like the family and URL, and
`stats` reports open sockets, idle or in a call.

- One connection per bot, opened on its first call and kept between calls
  (`src/provider/socket.rs`). A task holding only a weak reference closes
  connections idle for 60 s or older than 55 minutes, checking every 5 s, and
  a call never starts on one past that age. Deleting a bot closes its
  connection at once. Lanes
  (`stream_id`) are not used: the guide does not show how events on a shared
  connection name their lane, Codex does not use them, and this thread has no
  key to find out. So a fleet holds one TLS connection per active bot rather
  than one per 64 streams, and the screen has to report that cost.
- The request is the HTTP body's fields, less `stream`, as a
  `response.create` event. A
  call continues only when its fields and context head hash to the previous
  call's, and its window ids start with the previous window plus the node ids
  the turn stored for the previous response's items. The turn names those
  ids after its append (`Provider::recorded`), so a response the store did
  not accept is never continued from. The input is then the window's
  remaining items with `previous_response_id`.
- `previous_response_not_found` resends the full input on the same
  connection inside the same call, so it is not a retry. The refused
  continuation settles at zero, and the resend takes its own pacing
  reservation, since the provider counts both as requests. Any other
  `error` event made no response, so the connection still holds the
  previous one and the retry continues from it; every other failure sends
  the next call in full. A failure in the
  middle of a response closes the connection, since its remaining events
  would reach the next call. `websocket_connection_limit_reached` becomes
  the retryable `provider_socket_expired`.
- An `error` event's `status` and `headers` stand in for an HTTP response's:
  a 429 or 529 closes the pool with its `retry-after`, `insufficient_quota` is
  `provider_quota_exhausted`, other statuses are `provider_http_N`, and the
  pacer learns limits from these headers. The upgrade response's headers
  predate the call, so they set the pool's balance without settling the
  call, which is charged its own usage. A
  refused upgrade is treated the same way, so its `retry-after` holds the
  pool. A refusal with no usage settles at zero, since no inference ran, and
  so does a request that could not be written to a connection the provider
  had closed.
- A ChatGPT login is read as each call starts, and a new connection
  sends its token and account on the upgrade. A 401 upgrade re-reads the
  login as a 401 does on HTTP. From the login re-read (NEXT.md item 40) to
  2026-09-25 the upgrade sent neither token nor account, so
  `chatgpt=responses-ws` could not authenticate. The
  socket does not yet carry the ChatGPT sticky-routing token that HTTP calls
  send back within a turn ([RUST_PROTOTYPE.md](RUST_PROTOTYPE.md)). Codex
  sends that token in each create event's `client_metadata` and reads it
  from `response.metadata` events.
- Accounting follows the HTTP path's boundaries: the reservation is
  dispatched only when the create event is about to be sent, so a connection
  that never opened is refunded, and the startup permit bounded by
  `--max-connecting` is held until the provider's first event, not a control
  frame, and taken again for a full resend. The stall bound covers writing
  the request as well as each read, so a provider that stops reading cannot
  hold a turn or its permit.
- A socket message is whole, so the input is assembled in memory before it
  is sent, unlike the streamed HTTP body. Incoming messages and frames are
  bounded at 16 MiB, the HTTP path's response bound. A continuation is small; a full
  send holds one copy of the window.
- Not built: warmup with `generate: false`, lanes, and HTTP after a failed
  upgrade; a failed connection is retried like an HTTP connection failure.

Tests: `provider::socket::tests` covers when a call continues and how error
events are classified. `tests/test_responses_socket.py` runs the daemon
against a minimal WebSocket server for two turns with a tool call each, and
checks one connection, the beta header, no `stream` field, delta input on
every continuation, a full resend with no retry after the server forgets a
response, and the open-socket count. With continuation disabled it fails. A
second test refuses the first upgrade with a 429 and `Retry-After: 1` and
checks the turn waits it out; without the pacing it retries at once and fails.
A third refuses a continuation with a 429 event and checks that the retry
continues from the same response, and that deleting the bot closes its
connection.

## Measurement plan

The launch numbers are OpenAI's; this runtime needs its own. Follow
[COMPARISON_CONTRACT.md](COMPARISON_CONTRACT.md): same daemon build, model,
reasoning level, tools, prompts, and fixture repository, with only the
transport changed.

- Workload: tool-heavy tasks of 5, 20, and 50 model rounds, a few runs each,
  from the synthetic fixtures.
- Per round: time to first token, time to the terminal event, request bytes,
  input, cached, and output tokens. Per turn: wall time, p50, p95, and p99,
  daemon CPU time and RSS.
- Fleet: a ramp of active bots to show lane leasing, reconnect cost, and
  whether the missing rate-limit headers hurt pacing.
- Report cache hit rate (delta sends over total sends) so a miss-heavy run is
  not read as a transport loss.
- Needs an API key and a stated spend cap; the ChatGPT preset repeats a small
  subset under a plan login. This thread has no key, so these runs are
  George's.

## Prompt cache key

`prompt_cache_key` lets OpenAI route requests with the same prefix to the same
cache. The Harbor benchmark thread found that only 21% of this runtime's input
hit OpenAI's prompt cache on a Terminal-Bench run against 94% for Codex, and
added a per-bot key in its own change (PR #12). That change reports two
further facts: the key alone raised the hit rate only to 35% on the ChatGPT
backend, and, per Codex's source as that thread read it, the ChatGPT backend
takes cache affinity from a `session-id` request header, so PR #12 sends the
key in that header too.

On the socket, `prompt_cache_key` needs no work: the create event is built
from the same request fields as the HTTP body, so it carries the key. The
header does: a socket has one set of request headers, the upgrade's. Because
each bot has its own connection, the bot's key goes as `session-id` on the
upgrade, and every call on the connection carries it
(`tests/test_responses_socket.py` checks both). A fork that keeps its
source's instructions shares the source's key, since its first call repeats
the source's prefix. The screen must still send the same key and header in
both arms: a
cache hit-rate gap alone could explain much of the latency difference, and it
would be credited to the wrong change.
