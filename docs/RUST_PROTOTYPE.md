# Rust prototype

Implemented 2026-09-07; extended 2026-09-14 with a socket daemon, the `agent`
client, two provider families, and file tools. The immediate performance target
was identical synthetic streaming work below 40 MiB total sampled RSS at 32 agents.
That target does not imply production capacity or a claim to be the world's
fastest harness. The [first measurements](RUST_MEASUREMENTS.md) passed it.

## Build and validate

Development builds use Rust 1.98.0, pinned in `rust-toolchain.toml` with Clippy
and rustfmt. The declared minimum supported Rust version remains 1.92.
Dependencies are locked; build outputs and the optional repository-local Cargo
cache stay under ignored `.local/`.

```sh
CARGO_HOME=.local/cargo cargo build --release --locked
CARGO_HOME=.local/cargo cargo test --locked
CARGO_HOME=.local/cargo cargo clippy --locked --all-targets -- -D warnings
AGENT_TEST_RUNTIME=1 AGENT_BENCH_TEST_ENGINES=1 .local/venv/bin/python -m unittest discover -s tests -v
.local/venv/bin/python -m bench run --engine rust --out .local/bench/rust
.local/venv/bin/python -m bench.matrix --out .local/bench/three-engines
```

The same checks run on Linux amd64 through Errand, with the build tree and
the Python venv kept as named runner caches so a job pays for a build once
(`.errand.toml`; the `live` profile also forwards the provider keys in
`.env.local` for checks that spend tokens):

```sh
errand --profile cabal -- sh -c 'test -x .local/venv/bin/python || python3 -m venv .local/venv; .local/venv/bin/pip -q install psutil; cargo build --release --locked && cargo test'
errand --profile cabal -- sh -c 'AGENT_TEST_RUNTIME=1 .local/venv/bin/python -m unittest discover -s tests'
```

The Python dependencies and Pi installation are described in [BENCHMARKS.md](BENCHMARKS.md).
The matrix now defaults to Pi, Codex, and Rust. `--engines pi rust` selects a pair.
Build before collecting results. The Rust target records the release binary and
Cargo.lock hashes; observer fingerprints continue to cover the benchmark tools.

## Software first

Programs are the clients. Every command below emits JSONL events on stdout by
default; exit codes report the turn outcome. `--pretty` renders the same stream
for a human watching a terminal and is never the input protocol. The daemon's
socket protocol is the contract; the `agent` client is one consumer of it.

## Running work

```sh
export ANTHROPIC_API_KEY=...          # or OPENAI_API_KEY / OPENROUTER_API_KEY
agent run --new --model anthropic/claude-sonnet-4-5 --bot Bob -- "Add a failing test for the parser bug, then fix it"
agent run --bot Bob -- "Now run the full suite"          # same bot, next turn, same conversation
agent run --bot Bob --model anthropic/claude-opus-4-1 -- "Review the diff"   # this turn only
agent follow --bot Bob --after 0                         # replay, then live events
agent fork --source Bob --checkpoint 12 --bot Bob-alt      # workspace comes with each later run
agent interrupt --bot Bob
agent ls
agent shutdown
```

`run` connects to the daemon socket next to the store (`~/.agent/state.sqlite.sock`
by default; `--store` or `AGENT_STORE` selects another) and starts a daemon when
none is running. If the canonical store path is too long for a Unix socket,
the CLI uses a stable hash of that path in a private `/tmp/agent-<uid>` directory.
This also works before a new store exists. `--socket` explicitly selects an
endpoint; otherwise `AGENT_SOCKET` applies only when `--store` was not supplied.
Explicit socket paths are used as given and must fit the OS address limit.
The daemon runs the providers and limits of the client that started it,
registers every tool this build knows, and announces all of it in `ready`.
It supplies no agent behavior: `create` names the bot's model, instructions,
and tools, the bot keeps all three, a fork inherits them, and a later
client's environment changes nothing about an existing bot. The CLI resolves
`--model` or `AGENT_MODEL`, `--instructions` or its built-in text, and
`--tools` or `shell,read,write,edit,wait,history` before it asks. A bot's
tools are what the model is shown and what dispatch allows: a call to any
other tool is answered with a `tool_not_available` result, whatever the
model was told. Tool definitions live once in the daemon and their request
encodings once per distinct selection, shared by every bot that made it. The store binds none of that:
it opens under any provider set, so providers can be added, removed, and
brought back between runs. New submissions validate the effective provider,
including the bot's default, before accepting work or changing history.
An eligible steer with no model override can inherit the active turn's provider
when its own default is unavailable or incompatible. Validation errors are:
`provider_unavailable` if it is absent, `provider_family_mismatch` if its
encoding differs. Previously accepted requests still reconcile by request ID.
Queued turns recheck before starting after a restart; an invalid provider ends
the queued turn with that error without appending its prompt. Parked turns have
already changed history; a provider failure ends them through normal turn cleanup. Whether a running daemon is acceptable is the client's call: every
daemon-scoped value a client states (`--provider` and the limits; `--model`
and `--tools` belong to `run` alone) is compared with `ready`
at attach, and a difference fails with `daemon_configuration_mismatch` naming
each one, before anything is submitted. Defaults and providers implied by
environment keys never conflict; a stated provider must be registered with the
same family and URL. Limit comparisons
use effective values: `--idle-exit 0` disables idle exit, and positive context
limits below 1,024 bytes or two items are raised to those minimums.
Providers are selected explicitly with `--provider`; otherwise by
`AGENT_PROVIDER`, which holds the same specs separated by whitespace (a spec
contains commas), such as `AGENT_PROVIDER="chatgpt bedrock"`; otherwise by which
of the well-known key variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
`OPENROUTER_API_KEY`) are set. Like `AGENT_MODEL`, `AGENT_PROVIDER` is a default
for starting a daemon: a running daemon is not checked against it, since every
bot's shell inherits the daemon's environment. No other credential discovery happens. `--no-spawn` refuses to start a
daemon. Default tools are `shell,read,write,edit,wait,history`; `note`, the
carry-forward note, is in the universe and chosen per bot.

If concurrent clients race to start the daemon, losing `serve` processes exit
75 for a store/socket ownership conflict. Their clients continue polling for
the winner within the ten-second startup window, including during recovery.
That single window starts before the first connection attempt and bounds both
socket connection and the complete `ready` line, even if the listener keeps
sending partial data. Other CLI commands also allow ten seconds to connect and
receive `ready`. Expiry returns `daemon_start_timeout`; after readiness, normal
requests and event streams have no client read deadline.
Other startup failures return immediately; contention is identified by exit
status rather than text from the shared daemon log.

A blocking `run` exits 0 when it observes the turn completed, 1 when it fails,
is interrupted or loses its connection, and
2 for usage errors. A connection error means the outcome was not observed;
it does not assert that execution failed or cancel the turn. `--request-id` makes a
submission idempotent across retries; `--bot-id N` pins the retry to the bot
identity it was first made against (see below).

The `follow` CLI snapshots the running turn before subscribing and waits for
that turn's `turn_finished` event, whether delivered during replay or live.
Its exit status reflects that outcome. If the bot is idle at the snapshot,
`follow` replays through `follow_live` and exits 0. A turn finishing during
attachment therefore cannot cause the CLI to omit its final event.

A bot is an identity with a retained conversation: every turn appends to it.
`--bot NAME` continues that bot and fails with `bot_not_found` if it does not
exist; `--new --bot NAME` creates it and fails with `bot_exists` if the name is
taken; no `--bot` creates a fresh generated identity. A typo can therefore never
silently start an empty conversation under a familiar name. Names are the
address; the identity is a store-wide integer `id` that `create`, `fork`,
`resume`, `bots`, and every `submit` answer report, and that is never reused
after a delete. A fork is a new identity with an empty request namespace.

A bot is not bound to a directory. Each turn runs in the directory `run` was
invoked from (or `--workspace`), so the same conversation can continue in a new
checkout, worktree, or snapshot, and consecutive turns may use different
directories. `create` and `fork` accept an optional workspace that serves only
as the default for submissions that name none; a bot without one rejects such a
submission with `workspace_required`. The conversation does not know about
filesystem state; the caller is responsible for the workspace matching what the
history assumes, exactly as with forks.

Every named agent is a peer. Who submitted its work creates no CLI hierarchy,
ownership, special permissions, or coupled lifetime. The same create, submit,
resume, fork, inspect, and interrupt operations apply to all agents.

`agent run --detach` commits a submission and prints its bot, turn, request ID,
status, and durable cursor, then exits. Completion is independent of the
submitting client. `--delivery queue` or `steer` hands a busy bot the work
instead of getting `bot_busy` ([delivery modes](#delivery-modes)). A controller can inspect metadata through `ls` or observe events through
`follow` from outside a shell tool. Full records remain available via the protocol.

Shell tool processes receive `AGENT_BIN`, an absolute `AGENT_STORE`,
`AGENT_SOCKET` when a socket is configured, and `AGENT_MODEL`, the running
turn's effective model, so newly created peers default to its model.
Continuing an existing peer keeps that peer's stored model unless `--model`
explicitly overrides it for the turn. The client's built-in instructions use
`"$AGENT_BIN" run --detach --new --bot NAME -- TASK` to submit peer work and the
`wait` tool to collect it. `AGENT_SHELL_CONTEXT=1` tells the CLI to reject
blocking `run` and `follow` inside a shell tool: a blocked client would hold a
process-budget unit while waiting, which is exactly what `wait` avoids. This is
an execution-context guard, not a permission or agent-identity boundary. Arbitrary
shell synchronization can still block; this does not sandbox external commands.

## Deferred tool results

A tool call may finish later than the model call that made it. Two tools
produce handles, and one consumes them:

- `shell` with `background: true` starts the command and immediately returns
  `proc:N`. The command holds one unit of the process budget while it runs;
  its bounded output and artifacts are recorded when it exits.
- `agent run --detach` prints the peer turn's handle, `turn:BOT/N`, from the
  `submit` response.
- `wait` takes up to 64 handles and an optional `timeout_ms`. It parks the turn:
  the turn's task ends, the store records `waiting` with the handles, deadline,
  and any tool calls that followed the wait in the same model response, and a
  small registry entry remains. Nothing polls and no process or thread exists
  per waiter. When every handle resolves (or the deadline passes), the daemon
  spawns a task that records the wait result as the tool result, runs the
  remaining calls, and continues the turn.
- Programs outside a tool use the same mechanism through the `wait` protocol
  op, or `agent wait HANDLE...` which prints the same result and exits 1 when
  anything is pending or errored. Inside a shell tool that command is refused,
  because a blocked client would hold a process-budget unit.

A peer handle resolves from the daemon's own `turn_finished` for that turn and
reports its status, checkpoint, error, and final assistant text (up to 16 KiB).
A process handle reports the same stdout, stderr, and exit code a foreground
`shell` would. Unresolved handles at the deadline are returned as
`{"pending": true}` and remain valid for a later wait. A turn cannot wait on
itself; unknown handles resolve to errors rather than blocking.
Malformed handles return `invalid_handle`; numeric IDs use the exact decimal
form printed by the runtime, without leading zeros or a plus sign. A rejected
wait tool call does not skip the other calls in its model response. Protocol
wait results larger than the response limit return `response_size_limit`;
callers can retrieve them in smaller batches. A connection that cannot accept
the response is closed so the caller can reconnect and retry.
For a stdio owner, failed wait delivery exits the daemon with an error and
closes stdout, even if the client keeps stdin open or stops reading output.

Background commands are rows in a `processes` table with store-wide ids, so a
`proc:` handle is unique for the store's lifetime, its result is durable and
can be waited on more than once, and daemon memory holds only in-flight
commands. Completion commits the result and its overflow artifacts in one
transaction before waking waiters. A persistence failure stops the daemon with
an error and disconnects clients. After storage is repaired, restart marks an
unrecorded completion as `process_lost`. The code means supervision ended: the
daemon no longer owns the process and its result will never be recorded. It is
not evidence that the OS process stopped or that its effects did not happen. The
process-group kill runs only when the daemon itself drops the command, so a hard
kill of the daemon leaves its children running, possibly still writing to the
workspace. The daemon never reruns the command, and a controller must not read
`process_lost` as permission to start conflicting work in that workspace;
inspect the workspace first.
Waiters share immutable completed outcomes, including when they register after
completion. The last waiter releases the shared outcome; the daemon does not
cache past results indefinitely. Turn outcome queries use an index on turn,
event kind, and cursor rather than scanning unrelated agents' events.
Parked turns survive a daemon restart: they are re-registered at
startup, peer handles resolve from durable state (a peer interrupted by the
restart reports `interrupted`), and commands that were still running resolve
to `process_lost`: their supervision ended with the daemon, whether or not the
OS process did. Interrupting a parked
turn ends it as `interrupted`: the wait and any planned calls behind it get a
`cancelled` tool result in the same transaction, so the conversation stays
valid and the bot is not left uncertain. A bot whose turn is parked reports
status `waiting` and stays busy.
Interrupts reconcile durable state even when the parked task has exited but
its completion has not yet been processed by the service loop.
Live followers receive each cancellation's `tool_completed` event before
`turn_finished`, in the same cursor order as durable replay.
Completed or disconnected request waiters release their timers. Shutdown clears
pending request waiters before draining stdout; durable parked turns remain
available for recovery.

The fan-out that deadlocked when waiting held a shell slot now completes under
a process budget of two, because waiters hold nothing.

## Accounting and budgets

Provider-reported usage records a durable `usage` event (with a per-model
`models` split when a provider-side fallback ran another model and on every
summarizer call, naming the summarizer's provider and model, and
`cache_write_tokens` when Anthropic wrote input to its prompt cache: part of
`input_tokens`, billed above the base rate, and kept only in the event since
no budget or cache ratio needs it, with `cache_write_1h_tokens` for the part
cached for an hour; `sent_ms`, when the request was sent in Unix-epoch
milliseconds, so a cache miss can be set against the gap since the call before
it; a prompt-cache refresh's event carries
`purpose: "keep_warm"` and is not a model round), and the store keeps running
totals: per turn (`input_tokens`, `output_tokens`, `cached_input_tokens`,
`model_rounds`, `started_ms`, `finished_ms`) and per bot (`tokens_used`,
`input_tokens`, `cached_input_tokens`). Both report `cache_hit`, the share of
input tokens the provider served from its prompt cache, to three places; it
is the number the context window's hysteresis exists to keep high, and
`stats` reports the same totals and ratio for every turn since the daemon
started, from three atomics and no storage read. `create` and `fork` accept
`budget_tokens`, a lifetime cap on input plus output tokens for that bot. The
cap is checked before each model call and before each submission: a
submission on an exhausted bot fails with `budget_exhausted`, and a turn whose
next call would exceed the cap ends as `failed` with `budget_exhausted` after
any tool already planned has run, so the bot is never left uncertain. One
call may overshoot the cap. Forks start at zero with their own optional cap.
Reported usage from failed or incomplete calls is charged without accepting their
output into history. Such calls count in `model_rounds` when usage is reported.
Failed-attempt usage also counts before a retry and later tool rounds: a retry
cannot start after that usage exhausts the budget. This check uses the running
total without another store read.
Cancellation and failures before a usage report is returned can leave usage
unaccounted for; these totals are not a reconciliation of provider billing.
Successful calls retain their single atomic transcript/usage commit; budget
checks use the turn's running total without an extra database read per round.

`turns` (protocol) and `agent turns --bot NAME` list a bot's turns with status,
effective workspace and model, tokens, rounds, timing, and a prompt preview,
paged by `after`. `result` and `agent result --bot NAME --turn N` return a
finished turn's outcome in the same shape a `wait` produces, or its live status
without blocking; the command exits 0 only for a completed turn. Both query
commands restart an idle daemon using the supplied provider/tool configuration
(or provider environment defaults), honor `--no-spawn`, and refuse missing stores.
They do not submit new model work; existing parked work may resume on startup.

A truncated shell result names its retained streams as `artifacts`, for
example `["12/call_abc/stdout"]`, and the model can page through one with
`read` by passing `artifact` instead of `path`. Artifact reads allow the producing
bot or a branch containing the original tool-result node. Historical forks can
read inherited outputs, but cannot read later source turns or unrelated branches.
A stream the call never retained answers `artifact_not_found`, a turn outside
the reader's lineage `turn_not_found`, and a turn whose records retention has
removed `artifact_pruned` (see Retention).
Line pages are assembled on the storage worker; only the bounded page crosses
into the async runtime. Byte-oriented protocol pages continue to use SQL slicing.

## Limits

Three daemon limits are flags on `serve`, forwarded by the client that starts
the daemon, and reported in the `ready` event as `limits`. Zero removes a
bound; the operating system is then the only limit.

| Flag | Bounds | Default |
| --- | --- | --- |
| `--max-processes` | Child processes running at once, foreground or background. Waiting never counts. | 64 per logical CPU |
| `--max-detached` | Detached shell commands still running, daemon-wide; exited ones are reaped and stop counting. Zero is unbounded. | 16 |
| `--max-active` | Turns with a live task: a model call in flight or a foreground tool. Parked turns never count. | 4,096 |
| `--max-pending` | Submissions waiting to start: queued behind a bot's own work or ready for a slot, daemon-wide. A submission that would wait past the bound answers `pending_limit` and writes nothing; one that starts at once is never refused by it. | none |
| `--max-pending-bytes` | UTF-8 prompt bytes of those waiting submissions. | none |
| `--max-connecting` | Provider requests awaiting response headers, a Bedrock Runtime call's body digest included. Established streams are not capped. Both providers hold headers until the first token, so a permit is held for the whole time to first token; a bound of N caps throughput at N calls per first-token latency. | none |
| `--max-output-tokens` | Generated tokens per model call, including reasoning. Anthropic calls use the model's full output limit (read inside Bedrock ids) unless this is set; set, it is sent as `max_tokens`, at least 2,048 so a legacy thinking budget of 1,024 or more fits beside the answer. Bedrock deducts input plus this bound from quota when a call starts, so a bound near real output buys throughput there. | none |
| `--stall-timeout` | Seconds an established provider stream may go without a content frame before the attempt fails as `provider_stream_stalled` and is retried. Keepalives do not count. 1 to 86,400. | 120 |
| `--keep-warm` | Seconds an Anthropic prompt cache may sit unread while a turn runs a tool before it is refreshed (see [keeping the cache warm](#keeping-the-anthropic-cache-warm)). Below 300; 0 disables. | 240 |
| `--cache-ttl` | Anthropic prompt-cache lifetime, `5m` or `1h`, on both cache markers. `1h` bills each write at twice the input rate instead of 1.25 times and sends no refreshes. Responses providers are unaffected; Bedrock's acceptance of `1h` is unverified. | `5m` |
| `--idle-exit` | Seconds after which a socket daemon with no sessions, no live turns, and no running background commands exits. Parked turns are durable and resume on the next start; the client restarts the daemon on demand. | none |
| `--context-bytes` | Encoded input conversation-envelope bytes, including pinned context and separators (see [long history](#long-history-and-context-windows)). Minimum 1,024. | 8 MiB |
| `--context-items` | Input conversation-envelope items, including pinned context. Minimum 2. | 4,096 |
| `--note-turns` | Omitted turns the context note lists, newest first, with the first line of each prompt. 0 lists none. | 48 |
| `--compact-at` | Percent of either context envelope that triggers compaction, and of the byte envelope that triggers tool-result elision first. Estimated completion headroom can advance the byte trigger without reducing the input allowance. | 75 |
| `--compact-keep` | Target percent of either context envelope kept verbatim: as newest whole turns by compaction, as newest items by elision. Reduced when pinned context leaves less room. Must be below `--compact-at`. | 25 |
| `--retain-turns` | Retention policy: after each turn finishes, prune that bot to this many turns' records (see [Retention](#retention)). | none |
| (derived) `connections` | HTTP/2 connections per provider: `max-active` divided by 64 streams per connection (both providers allow 100; fewer bounds how many turns one reset connection takes with it), 1 to 256; 64 when active is unbounded. Reported in `ready`, not a flag. | 64 |

Provider requests multiplex over HTTP/2, and one connection carries at most
the 100 streams the provider advertises; the HTTP layer queues the rest, so a
single connection would serialize a fleet to 100 model calls at a time however
many turns are active. The transport therefore keeps several connections per
provider and gives each request the least-loaded one for the life of its
stream. Each connection is one TLS session and a few kilobytes.

Parked agents cost a store row and a registry entry. An agent in a model call
costs its request body read-ahead (64 items at a time), the parser buffers, and
a connection; no transcript is held in memory. Resolved waits queue until `--max-active` has capacity, including after restart.
Interrupting a queued turn cancels that turn without starting its continuation.
The 200-provider-round budget belongs to the durable turn, so parking or
restarting the daemon does not replenish it. The counter commits with each
model response, without an additional disk commit.

## Providers and models

A model reference is `PROVIDER/MODEL`. A provider spec is
`NAME[=FAMILY[,BASE_URL[,KEY_ENV]]]` with two protocol families:

| Family | Protocol | Defaults |
| --- | --- | --- |
| `responses` | OpenAI Responses API, streaming SSE | `openai` → `https://api.openai.com/v1`, `OPENAI_API_KEY`; `openrouter` → `https://openrouter.ai/api/v1`, `OPENROUTER_API_KEY`; `chatgpt` → `https://chatgpt.com/backend-api/codex`, Codex's ChatGPT login; `bedrock-openai` → `https://bedrock-mantle.$AWS_REGION.api.aws/openai/v1`, SigV4 |
| `anthropic` | Anthropic Messages API, streaming SSE | `anthropic` → `https://api.anthropic.com/v1`, `ANTHROPIC_API_KEY`; `bedrock` → `https://bedrock-mantle.$AWS_REGION.api.aws/anthropic/v1`, SigV4 |

The family `responses-ws` is the Responses API over a WebSocket per bot,
continuing from the bot's previous response where it can; for example
`--provider openai=responses-ws` or `--provider chatgpt=responses-ws` keep
those presets' endpoints and credentials. See [WEBSOCKET.md](WEBSOCKET.md).

Responses gateways can be configured as named providers, for example
`--provider gw=responses,https://gateway.example.test/v1,GW_KEY`. Model IDs may
contain slashes, so `openrouter/anthropic/claude-sonnet-4-5` is valid. A key
variable is read only when it is named in the spec or implied by a default
endpoint; a custom URL without a key field sends no credential. Compatibility
requires the request fields and streaming subset implemented by this adapter;
the family label alone does not establish support for an arbitrary gateway.

Amazon Bedrock serves both families, so a Bedrock binding is a base URL and
a way to authenticate; [BEDROCK.md](BEDROCK.md) records the survey and the
choices. `bedrock` and `bedrock-openai` take the region from `AWS_REGION`, or
`AWS_DEFAULT_REGION`, and fail at startup naming the spec when neither is set.
Any `bedrock-mantle.{region}.api.aws` or `bedrock-runtime.{region}.amazonaws.com`
URL without a key field signs every request with SigV4, for example
`--provider br=anthropic,https://bedrock-runtime.us-west-2.amazonaws.com/anthropic/v1`
with `global.anthropic.claude-opus-5` model ids. Keys come from the AWS chain in
its own order: `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` (with
`AWS_SESSION_TOKEN`) when set, otherwise whatever the AWS CLI resolves for
`AWS_PROFILE` or the default profile, SSO and assumed roles included, through
`aws configure export-credentials`. Temporary keys are re-resolved in the
background five minutes before they expire, at most every ten seconds, and
once on a 401 or 403, which retries the call as `provider_login_refreshed`;
Bedrock URLs must be https; the key id, secret and session token are
redacted from tool output and, from the environment, kept out of shells. Mantle takes
the body unsigned; runtime signs its SHA-256, so a runtime call reads its
history from the store twice, once to digest it and once as it streams. A key field
instead sends a Bedrock API key the family's own way, as
`--provider b=anthropic,https://bedrock-mantle.us-east-1.api.aws/anthropic/v1,AWS_BEARER_TOKEN_BEDROCK`;
short-term API keys last at most twelve hours and are read once. Bedrock has no
WebSocket transport, so `responses-ws` is refused for it. Bedrock publishes no
rate-limit headers, so its pools learn no allowance and are paced by refusals
alone: a 429 naming no delay closes the pool for a second, doubling to 32 while
refusals continue, until a call's stream completes.

`--provider chatgpt` uses a ChatGPT plan instead of an API key. Codex's own
source (openai/codex `15922a5`, read 2026-09-23) shows that, when signed in with
ChatGPT, Codex sends Responses requests to `https://chatgpt.com/backend-api/codex`
with the saved access token as the bearer and a `ChatGPT-Account-ID` header. The
daemon reads `tokens.access_token` and `tokens.account_id` from
`$CODEX_HOME/auth.json` (default `~/.codex/auth.json`) at startup and sends
the same two headers. It never refreshes the token itself; Codex does that
when it runs. The daemon re-reads the file in two cases: when the token's own
`exp` claim has passed, after pacing and admission but before sending, and
on a 401 for the currently held token. A concurrent 401 for an older token
reuses the login already installed. A re-read that finds a different token or
account retries the call without backoff (`provider_login_refreshed`), and
every token read this way is redacted from tool output. An unchanged login
ends the turn with `provider_login_rejected`, naming the file; an expired one
on disk answers `provider_login_expired` with the expiry time, at startup or
before a call.
Either way the remedy is any `codex` command, which signs in again, and
nothing has to restart. The token observed on 2026-09-23 was valid for ten
days from issue. A missing or unreadable file fails startup with
`provider_login_unavailable`. The login goes only to that
endpoint: a `chatgpt` spec naming another URL authenticates with the key
variable it names, or with nothing, so a local or synthetic endpoint never
receives the real token. The daemon redacts the access token from tool output
like a key. Only a synthetic
endpoint has exercised the headers; whether the ChatGPT backend accepts every
field this adapter sends has not been observed.

Responses requests explicitly include `reasoning.encrypted_content` even when
no reasoning effort is configured, because a model can reason by default.
[OpenAI's reasoning documentation](https://developers.openai.com/api/docs/guides/reasoning)
checked 2026-09-14 says stateless output already includes encrypted content and
continues accepting this legacy opt-in. We retain it for older compatible
endpoints. Gateway rejection of this field has not been reproduced here; there
is currently no provider option to omit it and no automatic retry with a changed
request. Preserve returned reasoning items verbatim for stateless continuation.

HTTP and streamed provider failures retain stable error codes and useful detail.
A rate limit the provider reports inside the stream (a Responses `error` or
`response.failed` frame with code `rate_limit_exceeded`, including the top-level
`code` on `error` events regardless of message wording) is
`provider_rate_limited` with the provider's message as detail, distinct from
`provider_incomplete`, which remains a response the model could not finish; a
rate limit refused at the HTTP layer stays `provider_http_429`. A complete
HTTP 429 body with `error.code` or `error.type` equal to `insufficient_quota`
becomes `provider_quota_exhausted`, without retrying or pausing the shared pool.
Transport failures (`provider_connection_failed`, or `provider_connection_os_N`
when an OS error code is known) carry the cause chain as detail: the URL and
the HTTP, TLS, or socket layer that failed, never a header. The selected provider's credential is redacted from decoded messages (and its
standard JSON-escaped representation) before the 512-character detail limit.
HTTP error bodies are read only up to 4 KiB. Oversized, interrupted, malformed
JSON, and unsupported JSON captures omit detail rather than publishing partial
credentials or unrecognized serialized representations. JSON string bodies are
decoded; complete plain-text messages remain supported. This does not attempt
to recognize arbitrary encodings of secrets or unknown credentials.


History items are stored in the family's native encoding and streamed into
requests by reference, without translation. A bot is therefore bound to its
provider family at creation; `create` accepts `model`, `instructions`, and
`reasoning` (`low`, `medium`, `high`), and a fork inherits its source's binding.
A turn may override the model within the same family (`submit` with `model`, or
`run --model` on an existing bot); a different family is rejected with
`provider_family_mismatch`. The override is recorded on the turn and in its
`accepted` event, and the bot's default is unchanged. Cross-family handoff of a
conversation is not implemented; it would be an explicit lossy fork that
discards provider-specific state such as thinking signatures.

Anthropic requests carry a `cache_control` breakpoint after the instructions
and top-level automatic caching for the growing history, so repeated prefixes
are read from the provider cache once they exceed the model's minimum; the
[live run](ANTHROPIC_SMOKE.md) records the effect. Anthropic caches any
byte-identical prefix, so a fork reads its source's cache with no key.
Responses requests carry a `prompt_cache_key`, also sent as the `session-id`
header that the ChatGPT backend routes on: the store's identity plus the id
of the bot whose cache the call shares. That is the bot's own id, except that
a fork shares its source's key, because a fork copies its source's
instructions and tools and so its first call repeats the source's prefix.
A summary sent as a copy of the bot's call uses the bot's key; one sent as a
request of its own adds `-summary`, since its prefix differs.

Within a turn, Responses calls over HTTP also return the ChatGPT backend's
sticky-routing token. The backend sends `x-codex-turn-state` on a turn's
first response, and each later call of that turn sends the first token back,
so the backend can route it to the server holding the turn's cache. Codex
does the same and never carries a token into another turn (openai/codex
aa38089, `core/src/client.rs`, read 2026-09-25). A new turn and a summary
request of its own start without one; a summary sent as a copy of the call
sends the turn's token, so it reaches the server that holds the call's
cache. A turn that parks on a wait or a rate limit keeps its token
in the park record, so its calls after resuming send it too. The socket path
does not carry the token. On short Terminal-Bench tasks over HTTP on
2026-09-25, 1 to 4 calls per task read nothing from the cache, while the calls
around them read the whole previous request. An A/B run that day (18 trials
per arm) found the token left those misses unchanged, but calls that read only
an older prefix fell from 21 to 4.

Newer Claude models bind each replayed thinking block to the exact
conversation before it (system prompt, tools, and earlier messages) and, for
accounts created on or after 2026-08-31, reject a block whose earlier context
changed. Anthropic requests therefore send a block only while the context in
front of the window is the one it was written under. Each request
fingerprints that context (the encoded pinned prefix and the window's first
item); when the fingerprint differs from the bot's last, the window slid or a
compaction or note landed, and
every node written before that request is sent without its thinking from
then on. Removing a leading run of blocks is allowed; later blocks keep
theirs. A summary sent as a copy of the bot's call replays what that call
would, bound the same way; a summary request of its own carries no
thinking, since its instructions differ. Each node records at write time how many bytes its thinking takes,
so a request still knows its length before it reads the items it streams;
the bot records the fingerprint and the first node still bound to it. This
costs the reasoning in the stripped blocks once per change, which already
invalidated the prompt cache from that point. Every Anthropic request opts
into the check with the `thinking-binding-controls-2026-08-01` beta header
and `thinking.block_binding.prefix_mismatch_behavior: drop_block`, so older
accounts get the same check and a mismatch the runtime missed costs the
dropped blocks rather than the turn. The response reports each drop in
`input_transformations`; the runtime publishes the count as a non-durable
`thinking_dropped` event, which should never appear. The synthetic endpoint
in the tests refuses mismatches outright, as `error` would. Empty instructions omit the system block, since empty text
cannot carry an Anthropic cache breakpoint; automatic caching remains enabled.

### Keeping the Anthropic cache warm

Anthropic's default prompt cache lives five minutes from its last read, and a
foreground tool call may run ten. On 2026-09-25 a Sonnet 5 bot idle for eight
minutes between two calls read only its 2.3k-token system and tools prefix
from cache; the 6.8k-token conversation was written again at 1.25 times the
input rate. The one-hour cache read all 9.0k, but it bills every write at
twice the input rate, idle or not. The daemon keeps the five-minute cache and
refreshes it instead. While a turn's call streams its reply or a tool runs,
once that call's cache has gone `--keep-warm` seconds unread (240 by
default), it sends that call's request again with `max_tokens: 0` and
`stream: false`. That request generates nothing, bills a cache read, and
restarts the cache's lifetime. It repeats until the reply and then the tool
finish. The lifetime is counted from when the call, or the last refresh, was
sent, so a call still waiting for its pool or startup admission is not
refreshed. Refreshing during the reply matters because a long one outlives the
cache on its own: in the 2026-09-25 Sonnet 5 rerun, six replies of 32k to
86k output tokens each took longer than five minutes, and the refresh sent
when the next tool started found the prefix gone and wrote it again. Each
refresh runs as a task of its own: one still unanswered when the reply ends
carries into the tool that follows. When the tool ends, the next model round
starts, or the turn ends or is interrupted, one not yet sent is dropped at no
cost, and one already sent is answered, so its cost is recorded and counts
toward the bot's budget before another call. A summary's call is not
refreshed.
Nothing else in the request differs, since the cache is keyed on everything it
renders. A refresh is paced and admitted like a call, but gives up waiting
for its pool and startup admission once the cache it would refresh has
expired, and after 30 seconds in any case. Usage is read per attempt, as for
a call, when a server-side fallback served it. A refused
refresh publishes a non-durable `keep_warm_failed` event and ends refreshes
until the next model call, which pays the write it would have paid anyway.

Anthropic's
[prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
guidance, read 2026-09-25 through Anthropic's API skill rather than the live
page, documents the `max_tokens: 0` request under pre-warming. It recommends
this keep-alive over the one-hour cache for five-to-sixty-minute gaps only on
Claude Fable 5.1 and Mythos 5.1, whose cache reads are nearly free (0.025
times the input rate on Fable 5.1). For other models, it picks the one-hour
cache for such gaps. Refreshing those models too is this runtime's inference,
not Anthropic's advice. During a streaming reply or a running tool the next
model call is certain, so each refresh costs a 0.1-times read of the
conversation where expiry costs a 1.25-times rewrite; a ten-minute tool takes
at most two. Whether it also
beats the one-hour cache, which covers parked waits and gaps between turns
but bills every write at twice the input rate, is not measured; `--cache-ttl
1h` selects it for that comparison. Its writes are recorded apart, from
Anthropic's per-lifetime split or, when a report has none, as the request
asked. The guidance
also lists the requests `max_tokens: 0` rejects: streaming, budgeted thinking
(`thinking.type: "enabled"`), structured outputs, and forced tool choice. So
older models with thinking on are not refreshed. Bedrock is also left out
until the same request is verified there. Refreshes cover foreground tool
calls only: a turn parked on `wait` has no live task to send them, and a bot
between turns may never be called again. The Responses cache needs none: the
ChatGPT backend still read 7.4k of 7.7k tokens after a 20-minute gap, and it
refuses `prompt_cache_retention: "24h"` with a 400.

A bot created with `--fallbacks` (`create.fallbacks`, inherited by its forks)
opts its Anthropic requests into server-side fallbacks
(`fallbacks: "default"` with the `server-side-fallback-2026-07-01` beta header);
other bots send neither, and a declined request ends their turn with
`provider_refusal` as before. It is a bot's choice because a fallback answers
with a model the caller did not pick and bills at that model's rate. The
Harbor adapter asks for it, since a benchmark task is better finished on the
recommended model than failed.
The protocol described here is from Anthropic's documentation, read
2026-09-25: [Refusals and fallback](https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback)
for the stream, the `usage.iterations` entries, billing and the replay rules,
and Anthropic's model migration guide (the Claude Opus 5 and Claude Fable 5.1
sections) for the `"default"` form and its beta header. Only model acceptance
of the field was observed live; the fallback stream itself is exercised against
a synthetic endpoint built from that description. With it, a request a safety
classifier declines is rerun, on the same stream, on the
model Anthropic recommends for that refusal category instead of ending the turn
with `provider_refusal`. A `fallback` content block marks each switch. Blocks
before the last one are a declined attempt's partial output: its non-empty text
is kept as part of the answer, while its thinking and tool calls are neither
stored, replayed, nor run. The marker is stored in place, since the API checks
the thinking around it by its position. Each switch is published as a
non-durable `model_fallback` event with `from` and `to`. When the response's
per-attempt `usage.iterations` list is present it replaces the top-level usage,
which covers only the last attempt: an attempt declined before any output is
reported but not billed, so it is not counted, and every other attempt is
billed at the rates of the model that ran it. The `usage` event then carries a
`models` list with each billed attempt, including a turn that sticky routing
(about an hour after a fallback) sent straight to the fallback model, so
clients can price them per model; its totals are their sum. A refusal that
survives the fallbacks still fails the turn with `provider_refusal`, with the
models it switched to, the refusal category and any recommended retry model as
its detail. Bedrock,
Vertex and Foundry do not offer server-side fallback, so Bedrock requests send
neither the field nor its beta header. Observed 2026-09-24:
`claude-sonnet-5`, `claude-haiku-4-5` and `claude-sonnet-4-5` accept the field,
and `claude-opus-5-5`, `claude-opus-5` and `claude-fable-5-1` answer normally
with it set; a live refusal was not reproduced, so the fallback path is covered
by parser and synthetic-endpoint tests.

`reasoning` (`low`, `medium`, `high`, `xhigh`, `max`) maps to Responses
`reasoning.effort` with summaries requested, and to Anthropic adaptive thinking
(`thinking.type: adaptive` with summarized display) plus `output_config.effort`.
Every Anthropic call sets `max_tokens` to the model's full output limit:
128,000 for Claude 4.6 and later, 64,000 for Haiku 4.5, Sonnet 4.5, Opus 4.5,
Sonnet 4 and Sonnet 3.7, 32,000 for Opus 4 and 4.1, and 8,192 for the Claude
3.5 models, and 4,096 for the other Claude 3 and Claude 2 models. A lower cap fails any answer that runs past it (a
32,768 cap cut off a Sonnet 5 benchmark task mid-answer), and Anthropic counts
only generated tokens against output rate limits, so the full limit costs
nothing until it is used ([rate limits](https://platform.claude.com/docs/en/api/rate-limits),
read 2026-09-25). The 128,000 and 64,000 values for current models come from the
[models overview](https://platform.claude.com/docs/en/about-claude/models/overview),
read 2026-09-25; the older values are Anthropic's published figures for those
models, not re-read on that date. Bedrock ids are read for the Claude model
they name, and `--max-output-tokens` replaces the limit when set. Known legacy Claude ids (Haiku 4.5, 4.5 and
older) instead get the budget form with 2,048, 8,192, or 16,384 tokens, kept
below `max_tokens`, since current models reject budgets and older ones require
them. Reasoning summaries and thinking stream as
`thinking_delta` events; Anthropic thinking blocks and signatures are stored in
the assistant item so tool-using turns continue correctly. Usage is recorded as a
durable `usage` event per model call. HTTP requests have a 10 second connect
timeout and a 120 second idle read timeout, and no total deadline. The read
timeout restarts on any byte, so a provider that sends only keepalives (Anthropic
`ping` events or SSE comments) would hold a turn indefinitely. An established
stream therefore also fails with `provider_stream_stalled` when no content frame
arrives within `--stall-timeout` (120 seconds by default); keepalives never renew
that bound, and time spent publishing deltas to followers does not count against it. Provider error
bodies are reduced to a bounded `detail` string; codes never contain URLs or keys.
Live provider behavior has been exercised only through synthetic endpoints in
tests; see [NEXT.md](NEXT.md).

## Core ownership and resource bounds

One process uses a Tokio I/O runtime with one scheduler thread, one shared reqwest
client, and asynchronous agent tasks. Each client session drains its output
independently (a thread for stdio, a task per socket). Durable mode adds one
storage worker for all bots, one storage reader, and one reader per session.
These are threads and tasks, not one process per bot. The storage reader is a
second SQLite connection in query-only mode on its own thread; it serves
reads whose result is bytes for a caller, today the batches of context items
that stream into a model request, so a long history's 8 MiB window is not
read on the thread every other bot's commit waits for. It sees a job's
writes once that job is answered; anything that decides against the store's
current state stays on the worker.

The worker commits in groups. It takes one job, then whatever else is already
queued, up to one full queue of 32, and runs them in order inside one
transaction; each job's own transaction is a savepoint in it, so a failed job
rolls back alone. One COMMIT, one sync, then every job in the group is
answered. No caller hears of
a write before it is durable, an idle daemon's group is one job and waits for
nothing, and under load the jobs that queued during one sync share the next.
If SQLite rolls back the whole transaction under a job (a full disk, an I/O
error), or the COMMIT fails, every job in the group is answered with
`storage_error`, including jobs that failed on their own, since they decided
against writes the group lost. The outcomes it announced are dropped and the
waiting-turn counts are recounted from the rows; if that recount fails, no
job runs until one succeeds.

Admissions (`create` and `submit`) reach that queue without the service
awaiting each commit: it queues one and reads the next request, up to 32
outstanding, so admissions that arrive together share a group, and one that
arrives alone is queued at once and waits for nothing. Each is answered after
its commit, in request order, and what followed the commit before (starting
the turn, flagging a steer) happens as it is answered. A turn started this way
is flagged at once when a steer for its bot is queued behind it, so its first
model call can already carry the steer. Every other request
waits until the queued admissions are answered. A submission told it may
start a turn holds one `--max-active` slot until it is answered, so queued
submissions never start more turns than the limit; when only promised slots
stand between a submission and the limit, it waits too, since whether one
frees up depends on how the earlier ones end. A burst therefore gets the
answers it would get one request at a time. Queued replies go out back to
back once their commit lands, faster than any client reads them, so an
admission also waits when its session's output queue could not take what it
and the admissions already queued for that session will send: each reply,
and its event once for each way the session follows the bot (by name,
through `*`, or as the stdio firehose). Events reach followers from the
publisher, which runs apart from replies, so an admission's event can arrive
before or after its reply; an answered admission's events keep their room
until the publisher has delivered them, and while the session has a follow
of that bot still replaying history, until that replay catches up, since the
publisher passes such a follow by and its replay delivers the event later.
An admission that finds nothing queued still waits for the publisher to pass
those events when they and it would not fit together, so it cannot commit
ahead of an overflow they cause. It does not wait for a replay, which moves
at its client's pace; past that it is handled as a lone request always was. The storage worker also tells the
publisher how far each pass reached, so events a deletion removed before
publication hold no room and no wait. Both are bounded from the
request: a creation's record repeats the request's strings, and a reply or
event adds at most a canonical workspace and a model reference. A lost group answers each of its
admissions with `storage_error`, starts no turn, and frees their slots. A
fatal error, such as the store refusing a background command's result, exits
through shutdown: queued admissions are answered first, and the turns they
start end like any other running turn. A stdio owner that cannot take one of
those replies within its five-second timeout gets no more of them, so a
stalled consumer delays shutdown once, not once per queued reply. A stdio
owner whose output fails still exits at once, since no one is left to answer
and a blocked write cannot be cancelled.

History items are immutable, reference-counted encoded JSON buffers. Appending
allocates the new item; an in-memory fork shares its prefix. Requests stream
references to these items with an explicit Content-Length. They do not rebuild
or stringify the full conversation on each turn. Provider HTTP/TLS comes from
[reqwest](https://docs.rs/reqwest/0.13.4/reqwest/struct.Body.html), scheduling from
[Tokio](https://docs.rs/tokio/1.53.1/tokio/runtime/struct.Builder.html), and disk
transactions from [rusqlite](https://docs.rs/rusqlite/0.40.2/rusqlite/).

Current limits: 8 MiB / 4,096 items of model context per request (stored
history is unbounded), 256 KiB input prompt, 64 KiB instructions, 768 KiB JSON-encoded terminal provider output
(a full 128,000-token answer, under the 1 MiB event cap), 2 MiB SSE frame,
16 MiB response stream, 200 provider rounds per turn, and the configurable
active-turn, process, and connection-startup bounds below. Provider startup
admission, when bounded, has a 60-second timeout and releases its permit when
response headers arrive. Both live providers hold headers until the first
token, so the permit covers the whole time to first token; the bound is off by
default and the derived connection count is what keeps a fleet streaming in
parallel. There is no automatic compaction: exceeding a bound is an
explicit error. Opaque reasoning items count toward the same byte and item
limits as text and tool history; reasoning-heavy conversations can reach the
limits sooner. They are not discarded to make history appear smaller. Indexed
history and bounded context selection remain the next storage work.

Each session's output channel allows at most 64 queued packets and 2 MiB of
queued/writing bytes; each packet is capped at 1 MiB. The stdio owner session
applies backpressure to producers. Socket followers never block a turn: a
follower whose queue is full has its entire session closed, including blocked
writes. This is deliberate: one session multiplexes replies and events, so
keeping its control channel open would not provide reliable delivery. Socket
RPC replies also use nonblocking enqueue and close a saturated session without
waiting in the shared command loop.
Deferred deletion and pruning replies preserve this distinction: stdio waits
up to five seconds for capacity, matching ordinary replies, while sockets
close immediately on saturation. The wait runs in the retention task.
The CLI exits with a connection error; clients reconnect and re-follow from
their last received durable cursor, or
inspect the bot and turn through the protocol. The submitted turn continues
independently. This signal does not depend on free queue capacity.
The input channel holds at most 64 bounded
requests; storage queues at most 32 operations. HTTP client internal buffers,
active histories, allocator retention, and filesystem cache remain part of the
memory accounting.

## Software protocol

The service speaks JSONL. `agent serve` without `--socket` serves the process
that owns its stdio and exits when stdin closes; that session receives every
event without subscribing (the benchmark and lifecycle tools use it). With
`--socket PATH` it is a daemon: it prints `ready` once on stdout, accepts any
number of Unix-socket sessions, and runs until `shutdown`, SIGTERM, or SIGINT.
Each socket session begins with a `ready` line and must `follow` the bots it
wants to observe.

`agent shutdown` returns once the daemon process has exited, so a caller may
copy or reopen the store: the daemon answers the request first, then cancels
active turns, commits their records and closes the database. The `ready` line
carries the daemon's `pid` for this. The command fails with
`daemon_shutdown_timeout` after 30 seconds.

`shutdown` takes an optional `grace_ms`, up to 86,400,000. With it the daemon
answers, then drains: running turns go on for up to that long and no turn
starts. A `reject` submission answers `daemon_draining`; `queue` and `steer`
submissions are recorded as usual, and queued work starts on the next daemon
start. Parked and paced turns stay parked, as on any shutdown. `stats` reports
`draining: true`. The daemon exits when no turn is running or the deadline
passes, whichever comes first. A later `shutdown` can only bring the deadline
closer; SIGTERM and SIGINT still exit at once. Turns still running at exit are
cancelled like any others at shutdown. `agent shutdown --grace SECONDS` sends
it and waits that much longer than 30 seconds.

On shutdown, committed turn events get up to five seconds to drain through
the publisher. Background commands can keep the storage stream open; when
that deadline expires, the service cancels and awaits the publisher before
joining the stdout writer. A still-running background command therefore
cannot keep shutdown waiting for the stream to close. Committed events
remain available through replay if the drain deadline is reached.

```sh
.local/target/release/agent serve \
  --store .local/runtime/state.sqlite --socket .local/runtime/state.sqlite.sock \
  --provider anthropic --provider openai
```

Requests include a string or nonnegative integer `id`. Responses carry the same
`id` and either `result` or an explicit `error` code with optional `detail`.
Notifications carry `event`; durable ones carry `cursor`, `bot`, `turn`, and
`data`, in exactly the shape `events` replays them. Durable events reach
followers in commit order: the storage worker itself hands each group's
committed events, and the outcomes of turns its jobs ended, to one publisher
before taking the next group. Tasks never publish durable events, so a
follower's cursors only rise, its greatest cursor is a complete resume
point, and a committed batch is delivered whether or not the task that
asked for it was cancelled meanwhile. Wait answers follow the terminal event
they report. Live `text_delta` and `thinking_delta` notifications keep their
own path from the turn. Example requests:

```json
{"id":1,"op":"create","bot":"Bob","workspace":"/workspaces/project","model":"anthropic/claude-sonnet-4-5","reasoning":"low","instructions":"...","tools":["shell","read","write","edit","wait","history"],"compaction_instructions":"...","created_by":"Alice","created_by_id":42}
{"id":2,"op":"submit","bot":"Bob","request_id":"work-1","prompt":"Hello","workspace":"/workspaces/project-copy","model":"anthropic/claude-opus-4-1"}
{"id":19,"op":"submit","bot":"Bob","request_id":"work-2","prompt":"Also check the docs","delivery":"steer"}
{"id":3,"op":"follow","bot":"Bob","after":0}
{"id":4,"op":"resume","bot":"Bob"}
{"id":5,"op":"events","bot":"Bob","after":0,"limit":100}
{"id":6,"op":"item","bot":"Bob","node":2}
{"id":20,"op":"history_nodes","bot":"Alternative","from":2,"limit":400}
{"id":7,"op":"artifact","bot":"Bob","turn":1,"call_id":"call_1"}
{"id":13,"op":"artifact","bot":"Bob","turn":1,"call_id":"call_1","stream":"stdout","offset":0,"limit":65536}
{"id":8,"op":"fork","source":"Bob","checkpoint":2,"bot":"Alternative"}
{"id":9,"op":"interrupt","bot":"Bob","turn":1}
{"id":10,"op":"unfollow","bot":"Bob"}
{"id":11,"op":"bots","after":null,"limit":64}
{"id":14,"op":"prune","bot":"Bob","keep_turns":8}
{"id":15,"op":"delete","bot":"Bob"}
{"id":16,"op":"follow","bot":"*","after":0}
{"id":17,"op":"wait","handles":["turn:Bob/1","turn:Alice/3"],"any":true,"timeout_ms":60000}
{"id":18,"op":"stats"}
{"id":12,"op":"shutdown"}
{"id":21,"op":"shutdown","grace_ms":30000}
```

`history_nodes` lists immutable node references in a bot's lineage, newest first,
including inherited fork history. `from` is an inclusive node ID and defaults to
the current head. The response contains `nodes: [{node: ID, turn: TURN_ID}, ...]` and
`next_from`, the next older node or null at the root. `limit` defaults to 400
and must be 1 through 400. Bodies are fetched through `item`; pagination does
not copy bodies. `min_node` optionally bounds the oldest included ID.
`oldest_first: true` selects the oldest page within that range while still
returning its nodes newest first; `next_newer` is the inclusive minimum ID for
the next forward page, or null. This lets clients retain only range endpoints.
Turn IDs are inherited from each node's nearest turn-start ancestor.
Membership validation walks the lineage, as `item` does. Both operations run
on the read worker in a consistent snapshot, avoiding the serialized writer.
The fork can read its shared prefix after its source is deleted.

`history_items` accepts `bot` and 1–400 distinct `nodes`. It validates all IDs
against that bot's lineage with one ancestry walk and returns a prefix as
`items: [{node: ID, item: VALUE}, ...]` in request order. The reply targets
768 KiB; one larger item may be returned alone if it fits the 1 MiB frame limit.
An item exceeding that limit returns `{node: ID, error: "item_too_large"}`;
other items remain readable. Callers request remaining IDs in their next batch.
IDs outside the lineage fail the whole request without returning bodies. Like `history_nodes`, this
runs on the reader connection in a consistent snapshot.

## Fleet controllers

A program driving thousands of bots needs three things a single-bot client
does not, and each is one op:

- `follow` with `bot: "*"` subscribes one socket session to every bot's
  events. Replay comes from a store-wide cursor (event ids are store-wide and
  monotonic), so a controller reconnecting after a crash resumes from the last
  cursor it saw with one request, not one per bot; `follow_live` then marks
  the switch to live delivery, and a retention gap anywhere is announced as a
  `pruned` notice first. Live delivery has the socket follower's contract:
  a lagging session is disconnected rather than allowed to hold the daemon.
  `agent follow --all [--after N]`. New identities publish their `created` or
  `forked` entry to live followers. A store-level watermark records gaps from
  pruning and deletion even after the owning bot is removed; reading this
  watermark does not scan the fleet. Schema 14 seeds it once from surviving
  retention marks and missing event IDs when opening an older store.
- `wait` with `any: true` answers on the first handle that resolves; the rest
  are reported `pending`, exactly as a timeout reports them, and stay valid
  for a later wait. A scheduler reacts as work completes instead of chunking
  handles into groups of 64 and waiting for whole groups. The `wait` tool
  accepts the same field, and a parked turn's mode survives a restart.
  `agent wait --any HANDLE...` exits 0 when a successful handle resolves,
  even with peers pending; an error or timeout without a result exits 1.
  `timeout_ms: 0` polls current outcomes and returns unresolved handles as
  pending without a timer, in both the protocol op and the wait tool.
- `stats` returns the daemon's live state without sampling its process from
  outside: open sessions, active turns against the bound, parked turns,
  running processes against the process bound and how many of them are
  still in line for a slot (`queued_processes`), turns queued or ready to
  start (`queued_turns`) with their prompt bytes (`pending_bytes`) and the
  bounds on both (`pending_limit`, `pending_bytes_limit`), daemon-wide in-flight
  requests per shared HTTP client shard (`transport.in_flight_by_shard`),
  and every provider's model pools with their learned allowance and current
  level (`providers.NAME.pools`). Shard loads count requests, not physical
  connections, and appear once even when multiple providers share transport.
  A pool's `reserved_requests` counts pacing reservations awaiting response
  headers, including admitted requests not yet dispatched; it is not the
  number of streaming requests. Receiving headers releases the reservation
  while the shared transport load continues through stream completion.
  Stats also reports the store's on-disk and WAL sizes with the storage worker's
  job count and its time queued versus time running (whether the worker or
  the disk is the bottleneck; jobs on the storage reader are counted the
  same way under their operation), the same per operation under `operations`
  (each store method's count, queued, ran and answered totals, slowest run
  and slowest answer, `storage_errors` for the jobs answered `storage_error`
  by their own SQLite failure or their group's, with the total at the top,
  and three fourteen-bucket latency histograms, `ran`,
  `queued` and `answered`, over the log-spaced bounds in `buckets_us`, so a
  controller can see which jobs make the tail and how often), and the
  handle registry's size. `answered` runs from queueing to the caller's
  answer: for a write it includes the wait for the rest of its group and
  the group's commit, which a cheap job pays when it shares a group with
  costly ones; for a read it is queued plus ran. The `commit` operation
  counts the worker's group commits, and its `ran` time is the COMMIT with
  its sync, so jobs per commit and the time spent syncing are both
  readable. Other operations' `ran` times exclude the sync, which they
  included before group commit. `groups` counts answered write groups,
  including any that could not begin: their jobs, a histogram of sizes over
  `size_bounds` (up to 1, 2, 4, 8, 16 and 32 jobs), and a latency histogram
  of each group's oldest job from queueing to its answer, with the slowest.
  A job whose group could not begin counts its whole wait as queued and
  zero as ran. `agent stats [--pretty]`. Store sizes use the canonical
  database path established at open, including when the caller used a
  symlink. The counters cost three clock reads per job, three per group, and
  one short lock per group (per job on the reader), taken just before the
  group's callers are answered; they allocate only the first time an
  operation is seen. Stats copies the records under that lock, then derives
  totals and builds JSON outside it. `draining` is
  true while a shutdown's grace period runs. The totals and
  histograms describe the same snapshot; time totals are summed before
  rounding to milliseconds.

Protocol version 3 changes `bots` to return `{bots, next_after}`. It pages by
name, with a default limit of 64, maximum 256, and a 512 KiB encoded metadata
budget. Pass `next_after` as the next request's `after` until it is null. Pages
omit instructions; `resume` returns the full individual bot record. Concurrent
creations before an already-consumed cursor require restarting the listing.
`agent ls` reads pages incrementally while preserving its JSON-array output.

Replay tasks are owned by their subscriptions and tracked by the service.
`unfollow`, replacing a follow on the same bot/session, and session closure
cancel the old replay. Notifications already queued can precede the operation's
reply, but no old replay events follow its acknowledgment. Shutdown cancels and
drains remaining replay tasks before joining the stdout worker. Socket paths have independent ownership locks. Live sockets,
symlinks, and unrelated files are never removed on startup; a refused stale
socket can be recovered. Normal shutdown removes only the socket inode the
service created. Do not replace ownership-lock files while a service runs.

`follow` replays durable events after the cursor as notifications, emits
`follow_live` with the cursor reached, and then delivers live events. The
switch happens inside a storage job, so no committed event falls between the
last replayed page and the first live delivery, and replayed cursors are never
repeated live. Non-durable `text_delta` and `thinking_delta` notifications are
delivered only to live followers and carry `durable:false`, as do the `pruned`
and `deleted` retention notices. Durable event kinds
are `created`, `forked`, `queued` (a submission waiting its turn), `accepted`
(a turn starting, with its prompt's node), `message`, `usage`, `tool_started`
(with a 2 KiB argument preview), `tool_completed` (with retained artifact
names), `turn_waiting` and `turn_resumed` (a parked turn's handles and its
wake-up), `turn_paced` (a turn parked at its model-call boundary because its
provider's pool is closed by a rate limit, with `resume_at_ms`; it resumes
through `turn_resumed` like a parked wait), `steered` (a steer's item joining
the running turn), and
`turn_finished` (with status, checkpoint, error code, and detail; a steered
turn's carries `into` and `node`). A `submit` response includes the turn's
`handle`, `turn:BOT/N`, and its `status`.

For bounded artifact retrieval, specify a `stream` from `tool_completed`, a
UTF-8 byte `offset` (default 0), and a byte `limit` (4 through 65,536, default
65,536). The response contains `text`, `next_offset`, `total_bytes`, and `done`.
Continue at the returned offset until done. Pages never split a UTF-8 character;
an offset inside a character is rejected. Only the selected page is returned
from SQLite to Rust for the response.
The original operation without `stream` returns all streams for small artifacts;
it may return `response_size_limit`, in which case use pages. Offset and limit
require a stream. This keeps even escaped, multi-stream artifacts retrievable
within the 1 MiB response bound.

`created_by` and `created_by_id` on `create` and `fork` declare the bot
on whose behalf the client acts. Supply both or neither. The CLI captures them
from `AGENT_BOT` and `AGENT_BOT_ID`, exported in every shell tool environment.
The store validates the pair in the child creation transaction and rejects a
missing, deleted, deleting, or replaced creator, so a surviving shell cannot
attribute a new child to a replacement bot after restart. The daemon also sets
`AGENT_PARENT` and `AGENT_PARENT_ID` to the running bot's recorded creator.
The client preamble uses both with `run --bot NAME --bot-id ID`, preventing
stale child-to-parent submissions after name reuse. The record, the `created` and `forked` events, and `bots` pages
carry both; the two events also carry the record's list fields (`id`,
`provider`, `model`, `workspace`, `status`, `running_turn`), so a follower
seats a new bot without a request per creation. Bots remain peers: the field is lineage for people and
clients, never authority. A fork keeps the source's binding, instructions,
and tools, and takes no text of its own; the source is never changed. Workspaces, wherever given, must already exist and be absolute. Use the actual returned checkpoint
and turn IDs, not the illustrative numbers. Names are immutable bot identities
within one store; rename/alias operations are not implemented. A fork starts
from any message in the source's history: `checkpoint` names a node id (every
`message` and `tool_completed` event carries one), and without it the source's
current head is used, which requires the source to be idle since a live head
is still moving. The point must leave no tool call unanswered, or the fork
fails with `fork_point_has_open_tool_calls`; a node outside the source's
lineage fails with `node_not_in_source_history`. A Responses reasoning node
cannot be separated from its following output item; selecting it fails with
`fork_point_splits_reasoning`. Valid forks retain the opaque reasoning unchanged.
Fork validation uses an index on checkpoint heads and inspects only the suffix
after the nearest completed or previously validated checkpoint, one item at a
time. After checking the selected item's type, a known checkpoint skips the
history scan and copies no transcript into Rust. The first fork
inside an uncheckpointed turn still scans its suffix; this is not constant-time
validation for arbitrary nodes. Forking references existing stored nodes and
creates no workspace or historical side effects.

Submission is idempotent on `(bot, request_id)`. An identical retry returns the
same turn without executing again, including after restart. Reusing that key
with a different prompt, workspace, or model fails. A retry may carry `bot_id`,
the identity the name had when the request was first made: if the name has
since been deleted and recreated, the retry answers `bot_not_found` with the
current identity in `detail`, instead of starting fresh work on the namesake.
Without `bot_id` a submission addresses whoever holds the name now. `workspace` and `model` on
`submit` are optional per-turn overrides of the bot's defaults; the `accepted`
event records the values actually used. Duplicate reconciliation still works when all active
turn slots are occupied; capacity rejection never writes a fresh submission.
A bot permits one running turn. Interrupt requires its exact current turn ID.
A missing bot never creates a replacement implicitly.

### Delivery modes

`delivery` on `submit` says what happens when the bot is busy or the daemon
is at `--max-active`. It is one field with three values; every mode returns
the turn's id and handle at once, and `wait`, `result`, `turns`, and
`interrupt` work on the turn unchanged.

- `reject` (default): `bot_busy` while a turn runs or is parked,
  `active_agent_limit` when no slot is free. Nothing is written. The
  `bot_busy` detail names the running turn and the ways past it as flags
  to copy (`--delivery steer --turn N`, `--delivery queue`, or
  `fork --source NAME --checkpoint C --bot NEW` to ask without interrupting,
  where `C` is the head the running turn started from; a first turn has no
  such point, so no fork is offered). A model
  calling `agent run` does not discover them otherwise, and in benchmark runs
  it ignored a prose description of them.
- `queue`: the turn is a durable row that starts when the bot is free and a
  slot is open. The response reports `status`: `running` when it started at
  once, `queued` behind the bot's own work, or `ready` when only a slot is
  missing. A bot's line runs in submission order; only its head is `ready`,
  and ready turns across bots start oldest first, one per loop iteration so
  requests interleave with a long backlog. `--max-pending` and
  `--max-pending-bytes` bound the waiting work; past either, a submission
  that would wait answers `pending_limit`. A `queued` event records the
  submission; the `accepted` event comes when the turn actually starts, with
  the node its prompt became. Queued turns survive restart: recovery ends
  the interrupted running turn and the line moves at once.
- `steer`: a queued turn the bot's running turn may absorb. At the running
  turn's next round boundary, after its tool results are recorded and before
  the next model call, a snapshot of queued steers joins the lineage as
  user items, in submission order, and each delivered steer finishes as `steered`
  with `into` naming the turn that took it and `node` its item; the absorbing
  turn records a `steered` event. A steer that arrives while the final model
  call is in flight keeps that turn going for one more round rather than
  going unheard; one that arrives after the last boundary starts as an
  ordinary turn when its place in the line comes, as does a steer on an idle
  bot. Waiting on a steer's handle resolves at absorption; wait on `into`
  for the answer. Rounds spent on steers count toward the turn's round limit.
  An explicit workspace or model must match the running turn's effective
  value; omitted overrides inherit that running turn for absorption. A
  mismatching steer stays queued for its own turn with its requested values,
  and later steers cannot overtake it. The snapshot is drained in batches of
  at most 32 steers and 256 KiB of UTF-8 prompt bytes, releasing each batch
  before another storage call. Batching adds no model calls; arrivals beyond
  the snapshot wait for the next boundary. An interrupt completes any in-flight
  batch's commit, event publication, and waiter notifications before stopping;
  it does not drain further batches. Unabsorbed work stays durable.
  Absorption is budgeted against the context: a boundary takes steers,
  oldest first, only while what the view must send ahead of the running
  turn (its summary, pinned context, notes, and the context note without
  the previews of omitted turns, which yield to the turn), the turn's own
  items, and each
  encoded steer stay within three quarters of `--context-bytes` and
  `--context-items`, the target the window itself keeps, so a burst of
  large steers cannot make the running turn exceed its context and fail
  with `context_limit`. A boundary builds the view first, so a note a tool
  wrote during the round counts, and a steer that goes in sends the view
  back through the same overflow, elision, and compaction steps before the
  model call. A steer that does not fit stays queued, and later steers do
  not overtake it. Once elision or a summary makes room in the running
  turn, the same boundary tries it again, and so does a later boundary
  whose view sends less ahead of the turn, a note cleared or shrunk, so a
  correction reaches a long task that compacts; one still queued when the turn ends starts as its
  own turn when the line moves (a strict steer, which names that turn,
  fails with `stale_turn`).
  Usage comes from cumulative byte and depth totals at the head and the parent
  of the turn's first node, found through a partial `nodes(turn)` index. This
  takes a fixed number of indexed lookups regardless of current-turn length;
  the same accounting serves the `history` tool. The index has one entry per
  started turn and is built once on first open of an existing unindexed store.
  A partial queued-steer index keeps ordinary queued work out of the scan.
  Each live turn carries one flag, set when a steer is queued for its bot
  or queued cancellation can expose steers behind a blocker, and answered
  exactly by the job that starts or resumes the turn, cleared
  by the boundary before it reads; a boundary with nothing waiting costs
  one atomic swap and no storage round trip, and one bot's pending steer
  costs unrelated bots nothing.
- strict steering: `expected_turn` with `steer` says the message is for
  that running turn or nobody. A different or finished turn, or an idle
  bot, answers `stale_turn` with nothing written. A strict steer that
  misses its last boundary is never absorbed by a later turn and never
  starts as new work: when its place in the line comes it ends as `failed`
  with `stale_turn`. `agent run --delivery steer --turn N`. The
  steer-or-queue behavior stays the default.

A queued or ready turn that cannot start when its place comes (for example,
the bot's budget is spent) finishes as `failed` with that
error, and the next in line takes its place. `interrupt` on a queued or ready
turn ends it as `interrupted` and answers `queued: true`; interrupting the
running turn does not touch the line behind it. `delete` refuses a bot with
queued work as `bot_busy`. `stats` reports queued and ready turns together
as `queued_turns`, a count the storage worker keeps at each transition
and recounts at open, so admission and `stats` cost the store nothing.
`agent run --delivery MODE` sets the field, defaulting
to `AGENT_DELIVERY` in the client's environment, never in the daemon; with
`--detach` a peer can hand a busy bot work or a mid-turn message without
racing on `bot_busy`.

## Durable state and recovery

SQLite WAL with `synchronous=FULL`, committed in groups (see Core ownership),
stores bot metadata and provider binding,
immutable history nodes, turns, completed checkpoints, tool intents/results,
retained tool artifacts, and durable event cursors. On macOS a plain fsync
leaves writes in the drive's cache, so both connections also set `fullfsync`
and `checkpoint_fullfsync`: every commit and checkpoint is an F_FULLFSYNC and
survives a power cut. Other platforms ignore both. The writer sets
`temp_store=MEMORY`: a job's savepoint journal (the pages it changed, kept so
it can roll back alone) stays in memory and is freed when the job ends,
instead of spilling past 64 KiB to a temporary file. Without it every
admission created, wrote and deleted such a file, and a full disk refused it
mid-job. The journal holds rewritten table and index pages, not the large
values a deletion frees, and completion retention works a piece at a time, so
no job holds a long history's pages. The store allows one owning
process. A second owner fails before it can mark the first owner's work
interrupted. Ownership uses the canonical database path with an appended
`.owner-lock` suffix; symlinks resolve to the same lock and hard-linked database
files are rejected. Do not replace or rename the database or its lock while
open. The schema version lives in SQLite's `user_version`; a store created
before versioning is refused with `store_schema_unsupported`, one written by a
newer binary with `store_schema_newer`, and an older versioned store is
migrated inside the opening transaction when its data can be converted without
guessing. Schema 18 requires each bot's tool selection. Older stores with bots
but no recorded selection are refused with `store_migration_tools_unknown`;
their data and schema version remain intact. Keep those stores and use a new
store path. Empty stores can migrate. Schema 19 rebuilds cache counters from
retained usage events, checking them against durable turn and bot totals. If
pruning or invalid records make those totals unrecoverable, opening fails with
`store_migration_usage_unavailable` and leaves data and schema intact; keep
that store and use a new store path. Usage events are streamed through their
turn index, without loading the transcript. Schema 21 copies existing bot row IDs
in one pass, preserving creation order and gaps from deletion, and starts the
identity sequence above the highest assigned ID (zero for an empty store).
Schema 26 shares started prompts of at least 4 KiB with their immutable user
node; queued prompts remain inline until start, and small prompts stay inline
to avoid reference/index overhead. Idempotency and turn listings resolve the
same original text. Absorbed steers share their own user node. Migration shares
exact indexed matches; older steers without that mapping keep their inline
text. The prompt-node foreign key has a partial index for deletion checks.
Schema 29 adds [tool-result elision](#tool-result-elision) without reading
stored items: results recorded before it have no stub and are always sent
whole. A backfilled saving would change the cumulative savings of every
later node on its lineage, rewriting most of the store at open. Every bot
starts with no elision floor. Schema 30 adds the prompt a [cut inside a
turn](#cuts-inside-a-turn) keeps; earlier cuts are all at a turn's prompt,
so none needs one.

New artifacts larger than 64 KiB, up to the existing 1 MiB output bound, may
use lossless LZ4 blocks. Each remains one SQLite BLOB with a small offset
directory and independent 16 KiB blocks. A 4 KiB sample and a 12.5% saving
threshold leave incompressible output raw; tiny artifacts stay raw too.
Byte paging decodes only intersecting blocks, and all existing authorization,
retention, UTF-8, and fork rules still apply. Native transcript JSON and provider
request prefixes do not change. Existing artifacts migrate as raw BLOBs, without
a startup recompression pass. Freed SQLite pages can be reused; this migration
does not vacuum or promise to shrink an existing database file. See
[storage measurements](STORAGE_GROWTH.md).

The migration is the only code that
knows an earlier format. The store records no daemon-wide provider set or
toolset; each bot retains its tools, and its provider is checked by family
when its turn starts.

Accepted user input is committed before the submission response. Provider output,
usage, and tool plans are committed before tool dispatch. Tool intent is
committed before execution and the result before continuing the model loop. A
completed checkpoint and terminal event are committed together before emitting
completion.

On recovery, unfinished active turns are marked interrupted and their bots
accept new work. Every unanswered tool call receives a durable result: a
planned call is cancelled before execution; an executing call with no committed
result receives `tool_outcome_unknown`. Its effects may already have happened,
and execution may still be running. The model sees this in history and can
inspect current state before deciding what to do. The same rule applies to
explicit cancellation and other terminal failures. Uncertainty never blocks the
named bot, and no external request or tool is automatically repeated. Existing
queued work remains eligible to start. Explicitly stopped turns stay stopped.

An interrupted turn's `turn_finished` error names the cause, so a client can
decide whether to resubmit: `cancelled` for a client's `interrupt`,
`daemon_shutdown` when the daemon shut down around it (`shutdown` or a
signal), and `process_interrupted` when the daemon died with it running and the
next open ended it. The daemon never continues such a turn on its own.

`resume` restores access to the exact identity and reports its state; it does not
automatically continue an interrupted network request. A new submission is an
explicit new turn. Partial streaming text is marked `durable:false` and can be
lost on crash. Completed messages survive. Paged `events` replay returns durable
records; message/tool-result records reference stored nodes retrievable through
`item`, avoiding another transcript copy in the event table. Cursors are store-wide
monotonic IDs, and queries are filtered to the requested bot. History loads and
lineage checks use recursive queries, so `item` and `fork` cost one query each
instead of a walk per node. Retention is explicit; see [Retention](#retention).

Event pages stop at either the requested count or a 512 KiB encoded-entry budget.
Continue from `next_cursor` even when a nonempty page has fewer than `limit`
entries; an empty page means no later retained events. A single entry exceeding
that budget returns `event_page_item_limit`. Responses exceeding the 1 MiB wire
limit return a correlated `response_size_limit` error without stopping the service.

Histories stay on disk whether or not the bot is active. A model request streams
its context window from the store in batches of at most 64 items and 256 KiB
(an item larger than that travels alone), so the memory an in-flight request
holds is a number rather than a function of its items, with an exact
Content-Length; the daemon never holds a transcript. SQLite's configured cache is
2 MiB, not a total bound on storage-related or OS memory. Durable performance
needs its own benchmark.

## Pacing and retries

A provider's allowance is the scarce resource in a fleet, so every model call
passes through one pace per provider and pool: a fair FIFO gate holding a
bucket for requests per minute and one for each token dimension the provider
limits. The pool key is the family's idea of a quota, not the model string:
a dated snapshot shares its alias's pool (`gpt-5.6-luna-2026-05-01` with
`gpt-5.6-luna`, `claude-sonnet-5-20260401` with `claude-sonnet-5`), and
`stats` lists pools by that key. A shared quota the provider does not name is
still corrected by every response's headers, bounded by what is in flight.
The pool is unbounded until the provider reports its limits in headers
(`x-ratelimit-*` on OpenAI, `anthropic-ratelimit-*` on Anthropic).
There is no extra cold-start cap: caller-selected local resource limits
still apply, and a 429 is feedback to the shared pool. A first burst may
therefore need retries. Once limits are known, a call is admitted only when
every learned bucket can afford it, debited by an estimate
(request bytes divided by four as input, plus the output cap as output).
OpenAI publishes one token limit and the estimate's total is paced against
it; Anthropic also publishes input-token and output-token limits, and each
share of the estimate is paced against its own, so an output-heavy call
waits on the output bucket while input-heavy work proceeds. Reported token
balances are reduced by outstanding reservations before taking the minimum
with the local balance, so stale high headers cannot replenish spent tokens.
Token headers release their request's reservation immediately, in the same
locked update, so other requests can start while that response streams.
Completion cannot refund it again. Without token headers, final reported
usage replaces the estimate. Request-only headers do not suppress that correction.
Cancellation or admission timeout before HTTP dispatch refunds both token and
request allowance. Both buckets reconcile server balances net of outstanding
reservations, including calls admitted before limits were known. New lower
request balances therefore survive cancellation refunds. Request reservations
are resolved at response headers; without a request balance, their debit
remains spent. After dispatch, cancelled streams retain an estimated token
charge when no balance was reported.
Releasing a reservation wakes the FIFO waiter immediately. This uses two
counters per pool and a small guard per call, with no per-request allocation
or additional database operation.

Callers wait in arrival order and are released individually. Estimates and
continuous refill are heuristics, not a guarantee of exact provider-limit
utilization. A refusal for pace, a 429 with
`Retry-After`, an Anthropic 529, or a rate limit named inside the stream
(`provider_rate_limited`, with "try again in N" parsed from the message), holds
the pool until that time; the turns behind it wait rather than fail. A turn
whose admission finds the pool closed by a rate limit for 250 ms or more
does not wait with a live task and an active slot: it parks at its
model-call boundary as a durable `paced` row with a resume time
(`turn_paced`), holding nothing, and the service resumes it when due
(`turn_resumed`) to continue the call. This includes callers already waiting
when another request closes the pool. The unfinished call's attempt count
and retry-time budget persist with the park; a successful call resets them
for the next tool round. Cumulative retries count only dispatched retries,
separately from that call-local state, and commit with the park. Admission
waiting spends no attempt. A persisted park-start timestamp measures actual
elapsed waiting through resumption or interruption, including downtime and
capacity delays after its wake-up deadline. The elapsed time is added once
when the park ends; `paced_ms` does not precharge future waiting. `stats`
reports such turns as `paced_turns`. These long rate-limit waits therefore
release `--max-active` capacity for healthy providers
([measured](DAEMON_MEASUREMENTS.md#paced-turns-and-active-slots)). Shorter
blocks are waited out in place. The gate adds a lock and a few arithmetic
operations on an unpaced call. See the
[matched follow-up](DAEMON_MEASUREMENTS.md#pacing-review-fixes) for CPU, memory,
latency, and the measurement limits.

An estimate above the learned per-minute token limit fails with
`provider_pacing_limit`, without consuming allowance or blocking the next
request. This is a local estimate limit, not a provider refusal; reduce the
context or configured output cap before resubmitting. Unknown pools remain
unbounded until headers teach them a limit. A provider that publishes no
limits imposes no inferred rate bound, though explicit rate-limit refusals
still pause its pool.

A model call has no side effects, so a failed one is retried by rebuilding
the request from the store: within 5 minutes, up to 8 attempts for capacity
(any 5xx, as `_should_retry` in anthropic-sdk-python 4421d56 treats it,
observed 2026-09-26), transport failures, or a stalled stream and up to 64 for refusals for pace, which the pool
spaces and which are not the request's fault; never for a response the model
could not finish (`provider_incomplete`) or a client error. The providers'
reset headers are only how long a full refill takes; the pool learns the level
and refills continuously, so it never idles waiting for a reset. Rate limits wait on
the pool; other transient failures back off from 250 ms doubling to 30 s with
a deterministic spread of up to a fifth, which matters only when several
daemons share one key. A tool is never rerun. Each retry is a non-durable
`retry` notification (attempt, error, detail, delay) to live followers, and a
turn's record carries `retries` and `paced_ms` in `turns`. `retries` counts
additional attempts that entered HTTP dispatch; cancelling during backoff or
pacing does not count an unsent retry. `paced_ms` includes a partially elapsed
pool wait on interruption. Counters flush once per execution segment, on
completion, failure, explicit interruption, or parking, and accumulate across
resumption. A hard process kill can lose the current segment's unflushed
counters. Retries are on by default because they cannot repeat an effect; what they can repeat is
billing for a failed attempt, which is recorded as failed usage. Successful
model responses and failures with reported usage share the durable limit of
200 model rounds per turn, including across parking and restart. Reaching
that limit stops the next model call; already committed tool plans still run.
Unbilled failures remain bounded by the attempt and time limits above.

The transport keeps 64 streams per HTTP/2 connection rather than the 100 the
providers allow, so a connection the provider's edge resets takes fewer turns
with it.

## Long history and context windows

Stored history has no length limit. What a model sees per request is a
context window: the newest whole turns of the bot's lineage that fit
the full input allowance of `--context-bytes` and `--context-items`. Completion
headroom can advance the compaction trigger; it does not reduce the hard input
allowance. The allowance counts the encoded summary, retained prompts, carry-forward
note, omission item, transcript items, and JSON separators. These are runtime
conversation-byte/item bounds; system instructions, tool schemas, provider JSON
framing, and model token limits are separate, not inferred from byte counts.
For compaction planning, a known output-token cap is estimated at four bytes
per token, capped at a quarter of the envelope. The earlier of that headroom
threshold and `--compact-at` triggers compaction. An unset Responses cap adds
no estimate; Anthropic uses its configured wire cap. This estimate is not a
guarantee about encoded completion size or model token capacity.
The window starts at a turn boundary
(a user prompt), so a model never sees a tool call without its result or a
reply without its prompt. Its start is persisted per bot (`context_start`) and
only moves when the window overflows; it then jumps back to the oldest turn
boundary within three quarters of the available transcript budgets, so the request prefix stays
byte-identical across many turns and provider prompt caches keep hitting. A
fork inherits the lineage, not the start; its first request computes its own
window over the shared history.

When turns are omitted, the request begins with one user item:
`[context note] N earlier turn(s) with M messages are not shown.` When the
bot has the `history` tool, the note adds `Use the history tool with a turn
number from 1 to N to read any of them.` It then lists,
newest first, the ordinal and the first line (up to 120 bytes) of each
omitted turn's prompt, at most `--note-turns` of them (default 48, 0 lists
none), and a line naming the older turns the list left out. The optional
listing is additionally bounded so pinned context plus the listing targets at
most two thirds of the input byte allowance. On overflow, previews shrink further
to leave room for the current turn; requests that already fit keep their prefix. Mandatory summaries and notes are
never clipped to achieve that target. The allowance stays fixed while requests fit, so normal appends do not resize
the listing. Overflow can reduce optional previews without changing pinned blocks. The list is
data, not an instruction: it lets the model see what it is missing and
judge for itself whether a turn is worth reading. The note is
part of the request, never stored; moving the window start or shrinking previews
on overflow changes that portion of the request prefix. It is built on the storage reader from the omitted turns'
prompt nodes alone, so deleting the source bot does not remove a surviving
fork's previews. Turn numbers are ordinals along the lineage
(`turn_seq`, stored on each turn's first item and indexed), so a fork's numbering
continues its source's. The `history` tool returns one turn's prompt, replies,
tool calls, and results as provider JSONL with only the top-level
`encrypted_content` field removed from reasoning items. Reasoning records and
readable summaries remain, as do Anthropic thinking and signatures. Stored
items, raw `item` reads, forks, and provider replay remain unchanged. The reading
view removes insignificant JSON whitespace from multiline provider items so
each occupies one JSONL record. Single-line items need no whitespace rewrite;
whitespace inside text strings remains intact. Pages
contain at most 64 KiB of text. Optional `limit` (4–65,536 bytes) requests a
smaller page.
The runtime also limits the encoded tool result, including escaping and page
metadata, to half the input-byte allowance remaining after the current turn
and its mandatory prefix (summary, carry-forward note, and bare omission notice).
Optional prompt previews do not consume this allowance; request fitting shrinks
them only when needed to accommodate the result. Indexed turn totals and bounded
prefix reads avoid constructing the context window or traversing omitted prompts
for history admission. This leaves room
for subsequent work; the current turn's overall item and byte limits still
apply. If too little space remains, the tool returns `history_context_exhausted`.
Start with `offset: 0`, then pass each `next_offset` until
`done` is true. Offsets count UTF-8 bytes in that turn's filtered JSONL; pages end at
character boundaries but may split a JSON record. Concatenate the text to decode
complete records. `truncated` means more bytes remain, and `items` counts record
newlines completed in this page. No entry is replaced by a preview. The store
passes bounded blob slices into Rust rather than loading whole messages there.
SQLite may parse and copy a whole item to filter it; items are transformed
one at a time, outside the query that orders the turn.
A number past the lineage returns `turn_not_in_history`; an offset past the end or inside a UTF-8
character returns `invalid_history_page`. No summaries are made and nothing is
deleted; compaction with summaries remains future work in [LONG_HISTORY.md](LONG_HISTORY.md).

The window always contains the whole current turn. If that turn alone exceeds
a budget even with its answered tool results [elided](#tool-result-elision),
the turn fails with `context_limit` rather than sending a truncated request. Both limits are daemon flags forwarded by the client, reported in
`ready` as `limits.context_bytes` and `limits.context_items`, and advertised as
the `context_window` capability. Version 20 repairs previously blocked
`uncertain` bots once, appending missing tool results without rewriting original
history. If operational tool records were pruned, repair reconstructs unanswered
calls from the interrupted turn's durable transcript. Stores are schema version
30; supported migrations run at open. Store initialization and migration run in one
transaction. [Project policy](../AGENTS.md#no-compatibility-branches) allows
one-way migrations but no legacy runtime behavior for earlier Agent versions.

Each window request reads the head and saved-start accounting together, then
walks the selected suffix once. Streaming batches copy item bytes directly from
SQLite into the output buffer. History retrieval walks only the selected bot's
ancestry to find both turn boundaries; unrelated bots do not add candidate
walks. Very old reads still cost a walk from the selected head. The
[measured costs](DAEMON_MEASUREMENTS.md#long-history) at fixed context and
growing stored history are recorded separately.

## Retention

Nothing is dropped unless a caller asks. Two primitives cover what a fleet
needs, and one optional policy composes them:

- `delete {bot}` removes an idle bot and everything only it owns: its turns,
  tool intents, processes, artifacts, events, checkpoints, and the history
  nodes no other bot's lineage reaches. A fork keeps the shared prefix; the
  deleted bot's own suffix is freed by walking back from its head until a
  node is still some bot's head, saved context start, or the parent of a
  surviving branch. A running or parked bot, or one with a background command
  still running, answers `bot_busy`. A deletion runs as a series of bounded
  storage jobs, four turns of records at a time, then the turn rows, then
  the exclusive nodes in larger pieces with the head moved back as they go, so
  other bots' work interleaves with a large deletion. The first piece marks
  the bot `deleting`: from then on `submit` and `fork` answer
  `bot_not_found`, `create` under the name still answers `bot_exists`, and
  `resume` reports the status. Shutdown cancels and awaits pending retention
  tasks before closing outputs; committed pieces remain. Whether shutdown
  or a crash interrupts deletion, the next open finishes it before readiness. Live
  followers receive a non-durable `deleted` notification. `agent rm --bot`.
  The first piece marks the bot's original event range as a possible replay
  gap for both bot and global followers. Reads between pieces therefore
  report `pruned_before` even before every event in that range is removed.
  Admission captures the bot's identity before spawning the deletion task;
  each piece checks that identity against the record it already reads.
  If concurrent deletion removes the original, an old task returns
  `bot_not_found` rather than deleting a replacement with the same name.
  Queued wake-ups for interrupted or deleted turns are discarded when capacity
  opens; reusing a bot name cannot resume its old turn. This internal check
  does not change explicit `resume` requests: a missing bot returns `bot_not_found`.
  The bot's turn rows go with it, so a late retry of one of its requests (the
  same `bot` and `request_id`) answers `bot_not_found`, never the old outcome
  or a duplicate turn. If the name was recreated meanwhile, a retry carrying
  the old `bot_id` is still refused; one without it is fresh work on the new
  identity.
- `prune {bot, keep_turns}` protects the unfinished suffix (running, parked,
  ready, and queued work) and keeps the `keep_turns` finished turns preceding
  it. With no unfinished work it keeps the newest `keep_turns` turns by
  submission ID. It drops older events, tool intents, finished processes, and artifacts.
  An explicit `prune` runs as pieces of four turns, oldest first, each
  its own storage job; the prune a completion applies is one job, so whoever
  sees `turn_finished` sees the store as retention left it. An explicit prune
  interrupted by shutdown can be reissued to finish the remaining work.
  Admission captures the bot ID; every piece checks it before pruning, so a
  delayed request cannot remove records from a replacement with the same name.
  This check replaces the piece's existing existence read. Automatic retention
  still runs within its completion job, with no extra admission read.
  An artifact read for a turn retention has emptied answers `artifact_pruned`,
  whether the reader is the producing bot or a fork that inherited the output,
  through the protocol `artifact` operation or the model's `read`. The answer
  comes from the transcript nodes retention keeps, so it stays distinct from
  `artifact_not_found` (the call retained no such stream) and `turn_not_found`
  (the turn is outside the reader's lineage). Deleting the producing bot after
  a fork inherited its output answers the same way.
  Queuing new work or cancelling a later queued turn cannot prune a live
  turn's tool intents or move retention past work that has not finished.
  A completion's own retention pass never removes that turn's records:
  the storage worker publishes what the store holds after the job, so the
  terminal event stays published and replayable until the next pass. That
  turn is the one beyond `keep_turns` a pass leaves behind; older turns
  remain past it only while a backlog drains, one piece per completion
  (below), or until an explicit `prune` clears them.
  Running process rows survive so their results can commit; a later prune
  removes those results after completion. The
  transcript and the turn rows themselves stay, so the context window, the
  `history` tool, `item`, forks, and accounting are unaffected; what shrinks
  is replay and artifact retrieval. `result` and new `wait` calls for an expired
  turn outcome return `turn_result_pruned`; they never report an empty success.
  On a pruning notice, `agent run` and `agent follow` reconcile their selected
  turn through `result`, so retries of expired turns exit with that error
  instead of waiting for a terminal event that no longer exists.
  Existing waiters can still receive the completion captured before pruning.
  The bot remembers the highest pruned
  cursor: an `events` page starting before it carries `pruned_before`, and a
  `follow` from before it is preceded by a `pruned` notification, so no
  consumer replays a silent gap. `agent prune --bot --keep-turns N`.
- `--retain-turns N` on the daemon applies `prune` to a bot after each of
  its turns finishes, including cancelled or failed queued work and interruption
  while parked, before the terminal
  event is delivered. Whoever sees `turn_finished` sees the store as retention
  left it. Each completion removes one piece, the four oldest turns past N at
  most, like explicit pruning: every page a job rewrites is held in memory
  until it ends, so a backlog (retention newly enabled, or N lowered) drains
  over later turns, or at once with `agent prune`. A turn already pruned whose
  background process still runs keeps only that row and is passed over until
  the process ends, so it never holds a piece. Each turn task submits its
  completion to the storage worker, allowing
  concurrent finishes to share a commit. The bot stays durably busy until that
  commit; the worker publishes its terminal event before a successor's accepted
  event. Completion, cancellation, explicit pruning, and deletion jobs for the
  same bot cross a commit-and-publication boundary so later retention cannot
  erase an unpublished terminal event. Different bots still share commits.
  The service then retires the task, without another storage round trip.
  The completion, its outcome and its retention commit together or not at
  all. A completion the store refuses (a full disk fails its group with
  `storage_error`) left nothing durable, so the task submits the same
  completion again with backoff, 10 ms doubling to one second, while the bot
  stays durably busy and other bots go on; `stats` counts each refusal under
  the `finish` operation's `storage_errors`. An interrupt that arrives
  during the retries is honored: the next attempt ends the turn as
  interrupted. A queued turn the store cannot start stays ready, and a
  parked turn it cannot resume stays parked; each is tried again with the
  same backoff.
  Only shutdown stops the retries:
  the turn stays running in the store, the next start ends it as
  interrupted, and the daemon exits with the storage error after draining
  every other completion. Before, one refused completion ended the daemon
  and every running turn with it.
  Shutdown drains completions through the
  same path, including cancellation events and pending turn-wait results.

Turn IDs come from a durable high-water mark. Node/checkpoint IDs use the
highest surviving node ID and a durable floor saved atomically when deleting
history. Neither can be reused across bot deletion, name reuse, or daemon
restart. Stale handles and checkpoint references cannot identify new work. The
version-8 to version-9 migration initializes the turn mark from surviving turns
and fork-retained transcript markers. Version 10 initializes the node ID floor
from the highest surviving node ID without rewriting transcript rows.
Version 12 adds partial indexes on `turns` and `processes` for the
`running` and `waiting` statuses, so startup recovery, parked-turn resumption,
and the idle-exit check read only the active rows instead of scanning every
turn a store has ever recorded; the indexes hold one entry per active row and
cost nothing at rest. A query-plan audit of every store statement found no
other full scan on a growing table (see
[DAEMON_MEASUREMENTS.md](DAEMON_MEASUREMENTS.md#query-plan-audit)).
Version 11 adds a small `retained_turns` table indexed by `(bot,turn)`.
Pruning looks up only the selected bot's candidates, then removes them unless
they still own running background commands. Late command results remain
eligible for the next prune. Migration builds candidates from surviving
operational records, without rewriting turn rows or reindexing expired history.
A separate `(bot,id)` index locates the retention boundary without scanning
older turns; the bot's active turn and existing queued/ready indexes locate
the unfinished suffix without scanning its backlog. The transcript, accounting,
and idempotency rows remain intact.

Freed pages are reused by later writes rather than returned to the
filesystem, so a bounded fleet's store stops growing instead of shrinking.
For a long-lived bot the transcript itself still grows with every turn (about
850 bytes per turn on the sustained workload); bounding that is compaction,
which is future work, or deleting the bot.

## Tools and validation scope

The daemon registers `echo`, `shell`, `read`, `write`, `edit`, `wait`, and
`history`; each bot is created with the subset it may call (`run --tools`),
which is what its model is shown and what dispatch allows. The
registry validates tool names and arguments before execution; a tool that fails
(unknown tool, invalid arguments, missing file, ambiguous edit, output
overflow) returns an error result to the model and the turn continues. Only a
closed tool scheduler fails the turn. Allowed tools run without approval prompts.
A store remains bound to its tool set; changing it requires a new store.

`shell` runs a noninteractive `/bin/sh` command in the bot workspace with
`timeout_ms` (default 120 s, maximum 600 s), which bounds the command's
running time, not any time spent in line for a process slot. Commands are at
most 16 KiB. At most `--max-processes` child processes run at once across the
service, foreground or background; waiting never counts. Background commands
the service has accepted but not started are bounded by the same number: when
that many are already in line, a background `shell` call returns
`capacity_exhausted` as its tool result and records no process, so a fleet
cannot build an invisible backlog behind the process bound. The operating
system counts processes; it never sees this line, which is why the daemon
counts it (one counter, one comparison per accepted command). `stats` reports
it as `queued_processes`, a subset of `running_processes`, which counts every
accepted command whose result is not yet recorded. stdout and stderr are each
retained up to 1 MiB; beyond 64 KiB the model receives a head and tail with the
omission stated and the full stream is stored as an artifact retrievable through
the `artifact` operation. Results include separate output, exit code, and
success status. Nonzero exit is a recorded tool result. A command's return,
timeout, or overflow kills its owned process group, so a process it started
with `&` does not outlive it; the tool description tells the model so. A
timeout is a result, not an error: the output written before the kill, with
`timed_out: true` and no exit code, since a long command's partial output is
often what the model needs next. Background commands are killed when the
daemon stops. `detach: true` is the way to leave a service running: the
command starts in a new session with stdin, stdout and stderr on
`/dev/null`, and the call returns its pid at once. The daemon writes no log
for it, since such a file would hold output that never passed credential
redaction; a command that wants its output redirects it to a file itself. No process slot, timeout, group kill, or
handle applies to it, so it outlives the turn and the daemon and is outside
`--max-processes`; stopping it is the bot's job. What does bound it is
`--max-detached`: each detached child retains a slot until its async waiter
reaps it on exit, and a new detach refuses with `detached_limit` while all
slots are held. It exists because Terminal-Bench
tasks that leave a server for the grader failed when the server died with its
command. Turn cancellation requests the same kill for a
foreground shell, but native file I/O or background commands can outlive the
cancelled turn. Without a committed result the tool outcome is unknown, not a
claim that all work stopped. The turn ends `interrupted` and the bot stays
usable; history tells the model to inspect current state before retrying.

### Tool-result elision

Most of a long tool-using turn is tool output the model has already read.
At a round boundary, after steers are absorbed and before compaction, once
the window and its pinned context hold `--compact-at` percent of the byte
envelope, or the current turn cannot fit at all, alone or beside the
summary, pinned context, and notes sent ahead of it, the daemon moves the
bot's elision floor. Every tool result at or below the floor goes to the model as
a stub instead of its output: a result for the same call id that states the
output's size, the `read` reference that returns it whole
(`artifact: "result/NODE"`), and its first and last 256 bytes. Everything
else, the model's own messages and calls, user prompts and steers, and
small results, stays verbatim, and no call is ever separated from its
result. There is no model call. The floor goes through the newest item the
verbatim tail of `--compact-keep` percent cannot take, and never past the
model's newest output, so the model reads every result whole in the request
that answers it. A move must save a sixteenth of the byte envelope, so a
context of mostly other text does not rewrite its cached prefix each round
for a little room. When the current turn cannot fit otherwise, the floor
goes to the model's newest output, whatever the keep target, and any saving
counts. Only a bot that has the `read` tool elides, since a stub names a
`read` call; any other bot's results get no stub and stay whole, and a
turn that outgrows the window ends with `context_limit`, as before. Elision works with or without compaction instructions: it is how a
single long turn outgrows the window, and it comes first because it costs
no call and keeps the model's own reasoning in view. Compaction then runs as
before if the view is still over its threshold.

A result is elidable when its stub saves at least 1 KiB. For a bot with
`read`, the stub is made when the result is recorded, with the node id its insert takes, and stored
beside it. Each node records what its stub saves and, like its byte total,
the cumulative savings along its lineage; each floor version records the
cumulative savings through its floor. A node's bytes as sent are then its
byte total less the smaller of the two, so the window, turn admission, and
compaction accounting stay constant-time lookups and read no item. A
request reads the stub in place of the result's row. The stored transcript
is never rewritten: `item`, `history`, and forks see every result whole,
and `read` with `result/NODE` returns one on the bot's own lineage. A
shell result is one JSON line, often longer than a `read` page, so that
read splits lines longer than 4 KiB into numbered pieces and pages them.
A page stays beside its call until the model answers it, where neither
elision nor a cut can take it, so it takes at most the room the running
turn and what goes ahead of it leave; with no room for one piece, the read
fails with `read_context_exhausted`.
The lineage check walks from the head counting steps down to the result's
depth, reading only each node's parent, since `depth` follows the item in
a row; a turn checks each result once, and its later pages skip the walk,
since a running turn only appends to its lineage. Compaction sizes and plans its span as sent, and the summarizer reads it
as the model last saw it, stubs included, so a long turn whose results
are several budgets as stored can still be summarized in one request.

The floor is versioned like notes and compactions: `elided` events record
each move (version node, previous version, `through`, and what the move
takes off the view: the results it newly stubs there and the bytes that
saves). A floor covers everything below it, so a move also stubs results
the view left behind before the floor reached them, summarized or outside
the budget; the view's start only moves forward, so the bot sends none of
them again, and the event does not count them. A bot's `elision`
names its current version, and a historical fork binds to the newest version at or before its
checkpoint, so it sees what its source saw there. When the boundary's move
leaves the view unable to fit and the forced move goes further before the
model answers, that move extends the version made at the same head in
place, and its event repeats the version and its previous. A version
another bot sees, bound by a fork taken between the moves or through a
version made on it, is never changed under it: that move fails with
`elision_version_shared`, and a further summary step at such a head with
`compaction_version_shared`, so the round ends with `context_limit`. Moving the floor rewrites
items the provider has cached from the first newly stubbed result on, so it
is a prompt-cache break there: the Responses WebSocket chain key includes the
floor. On Anthropic, only thinking written after that result and before the
move is sent without it afterwards. The bot records the floor its last
request was read under and, beside the floor below which the context
changed, one range of nodes whose thinking goes: from the first newly
stubbed node to the move's request. A later move extends the range; one
apart from it takes in the nodes between, which costs only some valid
thinking. Thinking before the first new stub, and after the move, keeps its
own, so the cached prefix up to that stub stays as it was sent. Between
moves the prefix is stable and the cache extends as usual. Anthropic documents
server-side tool-result clearing (context editing) as not counting as an edit
for its binding check, which would keep that thinking; it is family-specific
and has not been compared with elision (item 35). Admission for
`history` results and notes counts the current turn's stubs rather than
its raw results.

### Compaction

A bot created with `compaction_instructions` compacts, and one without never
does. At a round boundary, after steers are absorbed and before the next
model call, when the effective view since the last summary reaches
`--compact-at` percent of either envelope, the daemon summarizes everything
older than the newest boundary whose tail holds `--compact-keep` percent
verbatim, limited by the room left after pinned context. A boundary is a
turn's prompt or, [inside the newest turn](#cuts-inside-a-turn), a round
start: the first item after a tool result that is not one, a model output or
an absorbed steer. The item dimension can choose the cut even when small messages
have barely consumed the byte budget. The summary is one model call, to the
bot's own model or the `compaction_model` the client named at creation (same
family; another family's items cannot be replayed to it).

When the summarizer is the model the bot's last call ran on (the bot's, or
the turn's override), and the view as that call sent it holds the whole
span, the request is a copy of that call, as Claude Code and Codex
send theirs: the bot's instructions, tools, tool choice, reasoning, cache
key, and routing token, the prefix and window that call sent, read under
its elision floor and thinking strip, and the items since, then one user
item, the compaction request, carrying the client's compaction
instructions and the maximum summary size. All but the newest items read
from the provider cache. The copy takes what the last call sent ahead of
its window, so a note written since does not show, and the window from
before any stubs this boundary made. A summary that parks on a rate limit
keeps in its park record the floor that window was read under, where it
starts, and that prefix when the view no longer sends it, so its retry,
after a restart too, copies the same call. It does not set `tool_choice`, which on
Anthropic would invalidate the message cache, so the request asks for text
and a reply that calls a tool is billed and not installed
(`compaction_tool_call`). The summary covers everything the copy shows,
the verbatim tail included. Otherwise (a step through a backlog larger than
the budget, below; a copy that would exceed the input limit; or another
summarizer, which cannot read that call's cache, as when a turn
overrides the bot's model and the bot's summarizer summarizes it) the
request is one of its own: the compaction instructions as its instructions, the previous summary
first, if any, so the summarizer merges rather than restarts, then the
span's items as stored, then a request to write. Anthropic requests of this
form retain the bot's tool definitions because the span may contain native
tool-use/result blocks, and set `tool_choice: {"type":"none"}`; the runtime
borrows the already encoded tool selection. Responses requests of this form
send an empty tool list, which that family permits with historical calls.

Either way the call is paced, retried, billed against the bot's budget, and
counted as a model round like any other; if it parks on a closed pool, the
turn parks. Stored history is not rewritten for summarization.
The park record identifies the unfinished call as summary or ordinary model
work. Resumption, including after restart, continues that call. Once a summary
exhausts its retries, parking the following ordinary call does not restart the
summary's retry budget; a later model round may attempt compaction again.
Successful responses, including unusable empty or oversized summaries, are
charged durably before continuing. Usage events identify `purpose: compaction`;
`compaction_text_delta` and `compaction_thinking_delta` are separate from answer
streams. Budget and round limits are checked again before the normal call.

The result is recorded in one transaction: the summary, the covered turns'
user prompts verbatim (each up to 2 KiB, with a 16 KiB budget for text plus
entry metadata, the oldest and newest kept when there are more), and the cut, the prompt node the verbatim
tail starts at, which becomes the context start. A compaction stands for
everything since the first: its coverage starts at turn 1 and the kept
prompts carry over. Its version is anchored to the head at which it was generated, separately
from the cut: forks can summarize the same cut independently. A historical
fork inherits the newest version at or before its checkpoint and restores
that version's context start, preserving its cached prefix. Deleting a bot
frees only versions on its exclusive suffix. Prompt excerpts and coverage come
from the surviving history nodes, so forks can compact after their source is deleted.
Schema 24 adds the separate cut;
existing versions preserve their originally recorded anchors during migration. The
transcript is untouched; `history` reads any covered turn. The `compacted`
event carries the version, its coverage, and the sizes.

Each retained `(ordinal, String)` entry consumes its in-memory metadata size
even when its text is empty. Planning and merging use the same budget, so
repeated empty submissions cannot grow the prefix without bound. On 64-bit
targets this permits at most 512 empty entries; text reduces that count.
Trimming drains one middle range, preserving the oldest and newest excerpts
without repeatedly shifting the retained suffix. The budget is not a claim
about exact JSON wire size or allocator overhead.

The request begins with one user item `[compaction summary, version N,
covering turns A to B]` with the summary and verbatim prompts, the
carry-forward note if any, the omission notice, then the window from the cut.
Stable summary and note blocks precede the changing omission notice; Anthropic
gets explicit cache breakpoints on these blocks, alongside the existing system
and automatic tail breakpoints (at most four in total). The summary prefix stays
fixed until the next compaction; rewriting a carry-forward note or moving the
window can still invalidate the later suffix. A summarizer failure leaves the
context view unchanged, preserves any billable usage, is reported as a live
`compaction_failed` notification, and the turn continues with the window
as it is; the window's own overflow handling still bounds stored items.
Planning runs on the reader connection: nodes are immutable and only the
running turn moves its bot's head, so the reader's snapshot plans what the
worker would. Indexed byte/item accounting sizes the unsummarized span first,
without walking the transcript. A span larger than the context budget, left
by failed summaries or a round that outgrew the budget, is caught up oldest
first. Each round boundary summarizes the longest run from the previous cut
whose summarizer request fits the budget, including the previous summary
and the request marker. The cut moves to the next prompt, or to a round
inside a turn too large for one step (see [cuts inside a
turn](#cuts-inside-a-turn)), and the window keeps omitting what is still
behind it until the steps reach the tail. The `compacted` event says `catch_up`. Finding the oldest
turns means walking the lineage back from the head, since nodes only point
to their parents and forks give a node several children. That walk reads
metadata in pieces of 1,024 nodes, so other bots' reads interleave with it,
and only rows inside the budget leave SQLite; the newest turn's prompt is
the running turn's, or, with none running, the first the walk passes. A
round larger than the whole budget cannot be summarized and answers `compaction_span_limit`. Catch-up
converges while a step covers more than one round adds. The complete
summarizer input is byte-bounded including its previous summary and request
marker and the item count, and the final marker gives the summarizer its
maximum summary size in UTF-8 bytes. Oversized summaries are rejected above a third of the byte budget
or 64 KiB, whichever is smaller. At installation, verbatim prompt copies are further bounded so
they fit alongside the encoded summary within half the byte envelope. A middle
range is removed, retaining the oldest and newest excerpts when they fit. Original prompts remain available through history; installed
prefixes are unchanged until the next compaction. A replacement must reduce encoded bytes,
not increase items, and leave enough room for the active turn with its pinned
prefix; otherwise `compaction_not_smaller` or `compaction_context_limit` leaves
the old view intact. Such completed calls are still billed. `compacted` events
include `context_before`, `context_after`, `input_limit`, `reclaimed_bytes`,
`reclaimed_items`, `headroom_bytes`, and `headroom_items`. These describe the
complete logical view since the cut; headroom can be negative during catch-up,
while the actual request still uses a bounded window. These failures leave the bot usable and
original history retrievable. The
CLI ships a default compaction text for new bots and `--no-compaction`,
`--compaction-instructions`, `--compaction-instructions-file`, and
`--compaction-model` to change it. The daemon holds no such text.

Nonempty note writes are checked against the encoded context envelope, including
pinned summary metadata, the current turn, and the tool result. A write that cannot
fit returns `note_context_limit` and preserves the previous note. Clearing the note
always remains permitted. A provider output that already exceeds the current-turn
limit can still fail that turn, but its rejected note cannot block later turns.

`note` writes or replaces the bot's carry-forward note: up to 8 KiB of text
the runtime places ahead of the conversation window in every request, so it
stays in view when earlier turns leave the window. Empty text removes it.
The note is recorded in the same commit as the tool's result and versioned
by that result's node, so it belongs to the lineage like any item: a rewrite
is a new version, a fork inherits the version that existed at its
checkpoint, and deleting a bot frees only the versions on its exclusive
suffix. The window shows it as one user item, `[carry-forward note, version
N]` followed by the text, after the summary and before the context note
and window's items, so it changes the request prefix only when the bot rewrites it. The
daemon never writes a note itself and never tells the model to; whether
the tool is offered is the client's choice at creation, like any tool.

`read` returns numbered lines with `offset`/`limit` paging and a 64 KiB result
bound including paging notices (files up to 4 MiB). If the first requested line
cannot fit the page, it returns `read_line_too_long` with the line number; use a
byte-oriented tool to inspect that line. A page ending before an oversized line
still returns its preceding lines and the next offset. `write` creates parent
directories and writes up to
1 MiB. `edit` replaces an exact string that must occur once unless `replace_all`
is set. Paths are relative to the workspace unless absolute; nothing confines a
tool to the workspace, and tools run with the service's OS permissions.
Native file tools accept regular files, including symlinks to regular files.
Known pipes, devices, and other special files return `file_not_regular`. Opens
are nonblocking and the opened descriptor is checked before reading or
truncating, so replacing a checked path with a FIFO cannot block a filesystem
worker or daemon shutdown. Use the shell tool for special-file operations.

Shell children inherit the service environment except the provider key
variables, plus the CLI location/context variables described above. Exact occurrences of provider key
values in tool results are replaced with `[REDACTED]` before serialization and
persistence. This handles known tool output text, not arbitrary encodings or
credentials elsewhere on the host. A command that escapes its process group, or
a hard kill of the whole service, needs external process isolation/cleanup. A
workspace snapshot is not a sandbox. MCP, dynamic external tool registration,
and a richer permission policy remain unimplemented.

The real-process tests cover killing/restarting the server, exact lookup,
duplicate submission, historical forks with independent continuation,
Unicode/SSE fragmentation, complete provider/tool/provider loops on both
families, stale interruption, cancellation, rejection of a second storage owner,
missing provider completion, the socket daemon started by `run`, delegation from
a shell tool through `$AGENT_BIN`, follow replay then live delivery, and
artifact retention. Additional regressions cover slow RPC readers, cancellation
of replay on unfollow/replacement, explicit store selection despite an inherited
socket, and startup/reconnection with deeply nested store paths. Rust tests cover shared prefix allocation, history bounds,
slow-consumer queue pressure, transactional tool outcomes, replay pagination,
both stream parsers, file tools, and provider spec parsing.

The streaming benchmark drives the daemon through its stdio protocol on a fresh
store with the echo tool registered but never called; there is no benchmark-only
entry point and no in-memory history path. It must not be presented as
coding-tool, TLS, or real-provider performance.
The [durable feature screen](LIFECYCLE_MEASUREMENTS.md) measures actual service
execution, including shell descendants, independently of the text-core screen.
Very long histories and compaction are specified in [LONG_HISTORY.md](LONG_HISTORY.md);
stored history is unbounded with a per-request context window, and versioned
summaries compact that window as described above.

CLI syntax, option scope, output, and exit conventions: [CLI.md](CLI.md).

### Cuts inside a turn

One long task can outgrow the budget inside a single submitted turn, even
with its results [elided](#tool-result-elision): its own calls, messages,
and stubs keep growing. So the newest turn offers more boundaries than its
prompt: every model round after a completed tool exchange. The newest
boundary whose tail reaches the keep target wins, whether a prompt or a
round, so compaction leaves about `--compact-keep` percent verbatim however
the view is split into turns; a turn smaller than that is never cut. The
summarizer reads the span up to the cut, which ends with a whole exchange:
every call in it has its result, and the tail begins with the model's
next output, or with a steer absorbed after that exchange, so no call is
separated from its result on either side and a steer at the boundary goes
verbatim.

The turn's prompt stays in view whole. The compaction records it as the
version's `pinned` node, and while the window starts at that cut, the
request carries the prompt as its first item after the pinned context,
then the items from the cut. The summary header says `covering turns A to
B and the start of turn C`, or `covering the start of turn C`, and its
list of verbatim user messages leaves out turn C's prompt, which follows
in full; the list keeps it for when a later compaction covers the whole
turn. The context note counts the messages it leaves out of turn C
separately from earlier turns. A second cut in the same turn summarizes
from the first, merging the previous summary, and keeps the same prompt.
Once the turn has ended, a later cut at a turn's prompt covers it whole.
The turn keeps running for its clients throughout: events, steers,
approvals, and its checkpoint are unchanged; only what the next request
carries differs. Admission for steers, notes, and `history` results counts
the turn from its prompt and cut, as the window does. A historical fork
binds the version at or before its checkpoint and restores its cut, so a
fork from inside a split turn sees that prompt and the tail from the cut.
Steers absorbed before the cut are summarized with the rounds around them,
as a covered turn's steers are. Schema 30 adds the `pinned` column; every
earlier cut is a turn's prompt, so none has one.

A round can take the turn past its budget before compaction is due, for
example one large result after several small ones, or a turn that fits
alone but not beside its summary, pinned context, and notes. The runtime
then elides what the model has answered, as above, and if the view still
cannot fit, it
summarizes at once with a keep target of one byte, so the cut lands at the
newest round that follows a result, and the round goes on. When elision
cannot make room and the bot has no summarizer instructions, or when even
the newest round cannot fit, the turn ends with `context_limit`. A span that big usually outgrows
the summarizer's own budget, so the summary is a [catch-up](#compaction)
step. Catch-up steps end at a prompt, or at a round start inside the newest
turn, whichever leaves the longer step; a turn older than the newest that
alone exceeds the budget, for example after the budget was lowered, is cut
at its rounds too, keeping its prompt, rather than failing with
`compaction_span_limit`. A step
may leave the view over budget as long as the running turn's prompt and
newest round still fit beside the new summary; the round then takes
further steps at the same head until the view fits, a step is skipped, or
the turn reaches its round limit. Each extends the version made at that
head in place, and its `compacted` event repeats the version and its
previous. A resumed call that fits does not summarize again at a head that
has a version, and a version another bot sees is not extended, as with
the [elision floor](#tool-result-elision).

### Compaction and prompt-cache reuse

Compaction replaces part of the request, so it cannot preserve cache hits for
all of the replaced prefix. The aim is to pay that cost once per compaction,
then reuse the new prefix while turns append. Default thresholds remain 75%
trigger / 25% retained tail; changing them is a quality/cost decision, not a
free speed improvement. Keeping more tail also means reaching the next trigger
sooner. Byte percentages are runtime bounds, not estimates of model tokens.

The current request order is stable tools/instructions, summary, carry-forward
note, omission notice, and verbatim tail. Ordinary requests do not rewrite the
summary, attach timestamps/fullness counters, or invent a new version. Forks
restore the inherited context start. Anthropic summary and note blocks get
explicit cache write points; system and automatic tail caching remain enabled.
Responses gateways retain their current wire format. OpenAI's newer explicit
cache controls need provider/model capability handling before being enabled
across compatible endpoints.

The providers require identical cached prefixes, and a reusable prefix must
also have been written at an eligible cache boundary. See the primary
[OpenAI caching guide](https://developers.openai.com/api/docs/guides/prompt-caching)
and [Anthropic caching guide](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
(checked 2026-09-19). Structural prefix tests establish eligibility, not actual
provider hits: minimum sizes, expiration, routing, and model capabilities still
matter.

A summary on the model that made the last call keeps that prefix
identical and appends the compaction request (see [compaction](#compaction)). It keeps the tool
choice, since changing it invalidates Anthropic's message cache, so a reply
that calls a tool is rejected and billed rather than prevented; how often
models comply, and the hit rates providers give it, are for the
[evaluation](LONG_TASK_EVAL.md) to measure. A summary request of its own,
on another model or for a catch-up step, has different instructions and
must not be assumed to reuse the agent's cache.
Measure agent calls and summarizer calls separately, then total input/output,
cache reads/writes, latency, and objective task quality. The old evaluation lacks
successful summarizer usage and cannot establish total compaction cost.
