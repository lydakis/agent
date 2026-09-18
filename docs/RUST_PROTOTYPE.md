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
Explicit socket paths are used as given and must fit the OS address limit. The daemon inherits the providers, tools, and default model of
the client that started it and binds them to the store; later clients that pass
different `--provider` or `--tools` values are ignored while that daemon runs,
and a restart with different values fails with `store_configuration_mismatch`.
Providers are selected explicitly with `--provider`, or implied by which of the
well-known key variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`)
are set. No other credential discovery happens. `--no-spawn` refuses to start a
daemon. Default tools are `shell,read,write,edit,wait,history`.

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
is interrupted, leaves an uncertain tool outcome, or loses its connection, and
2 for usage errors. A connection error means the outcome was not observed;
it does not assert that execution failed or cancel the turn. `--request-id` makes a
submission idempotent across retries.

The `follow` CLI snapshots the running turn before subscribing and waits for
that turn's `turn_finished` event, whether delivered during replay or live.
Its exit status reflects that outcome. If the bot is idle at the snapshot,
`follow` replays through `follow_live` and exits 0. A turn finishing during
attachment therefore cannot cause the CLI to omit its final event.

A bot is an identity with a retained conversation: every turn appends to it.
`--bot NAME` continues that bot and fails with `bot_not_found` if it does not
exist; `--new --bot NAME` creates it and fails with `bot_exists` if the name is
taken; no `--bot` creates a fresh generated identity. A typo can therefore never
silently start an empty conversation under a familiar name.

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
and durable cursor, then exits. Completion is independent of the submitting
client. A controller can inspect metadata through `ls` or observe events through
`follow` from outside a shell tool. Full records remain available via the protocol.

Shell tool processes receive `AGENT_BIN`, an absolute `AGENT_STORE`, and
`AGENT_SOCKET` when a socket is configured. Default instructions use
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
unrecorded completion as `process_lost`; it never reruns the command, whose
external effects may already have happened.
Waiters share immutable completed outcomes, including when they register after
completion. The last waiter releases the shared outcome; the daemon does not
cache past results indefinitely. Turn outcome queries use an index on turn,
event kind, and cursor rather than scanning unrelated agents' events.
Parked turns survive a daemon restart: they are re-registered at
startup, peer handles resolve from durable state (a peer interrupted by the
restart reports `interrupted`), and commands that were still running resolve
to `process_lost` because they died with the daemon. Interrupting a parked
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

Provider-reported usage records a durable `usage` event, and the store keeps running
totals: per turn (`input_tokens`, `output_tokens`, `model_rounds`, `started_ms`,
`finished_ms`) and per bot (`tokens_used`). `create` and `fork` accept
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
Line pages are assembled on the storage worker; only the bounded page crosses
into the async runtime. Byte-oriented protocol pages continue to use SQL slicing.

## Limits

Three daemon limits are flags on `serve`, forwarded by the client that starts
the daemon, and reported in the `ready` event as `limits`. Zero removes a
bound; the operating system is then the only limit.

| Flag | Bounds | Default |
| --- | --- | --- |
| `--max-processes` | Child processes running at once, foreground or background. Waiting never counts. | 64 per logical CPU |
| `--max-active` | Turns with a live task: a model call in flight or a foreground tool. Parked turns never count. | 4,096 |
| `--max-connecting` | Provider requests awaiting response headers. Established streams are not capped. Both providers hold headers until the first token, so a permit is held for the whole time to first token; a bound of N caps throughput at N calls per first-token latency. | none |
| `--max-output-tokens` | Generated tokens per Responses call, including reasoning. Anthropic calls keep their fixed `max_tokens`. | none |
| `--idle-exit` | Seconds after which a socket daemon with no sessions, no live turns, and no running background commands exits. Parked turns are durable and resume on the next start; the client restarts the daemon on demand. | none |
| `--context-bytes` | Encoded bytes of stored items in one model request's context window (see [long history](#long-history-and-context-windows)). Minimum 1,024. | 8 MiB |
| `--context-items` | Items in one model request's context window. Minimum 2. | 4,096 |
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
`NAME[=FAMILY[,BASE_URL[,KEY_ENV]]]` with two families:

| Family | Protocol | Defaults |
| --- | --- | --- |
| `responses` | OpenAI Responses API, streaming SSE | `openai` → `https://api.openai.com/v1`, `OPENAI_API_KEY`; `openrouter` → `https://openrouter.ai/api/v1`, `OPENROUTER_API_KEY` |
| `anthropic` | Anthropic Messages API, streaming SSE | `anthropic` → `https://api.anthropic.com/v1`, `ANTHROPIC_API_KEY` |

Responses gateways can be configured as named providers, for example
`--provider gw=responses,https://gateway.example.test/v1,GW_KEY`. Model IDs may
contain slashes, so `openrouter/anthropic/claude-sonnet-4-5` is valid. A key
variable is read only when it is named in the spec or implied by a default
endpoint; a custom URL without a key field sends no credential. Compatibility
requires the request fields and streaming subset implemented by this adapter;
the family label alone does not establish support for an arbitrary gateway.

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
[live run](ANTHROPIC_SMOKE.md) records the effect. Responses caching needs no
request change. Empty instructions omit the system block, since empty text
cannot carry an Anthropic cache breakpoint; automatic caching remains enabled.

`reasoning` (`low`, `medium`, `high`, `xhigh`, `max`) maps to Responses
`reasoning.effort` with summaries requested, and to Anthropic adaptive thinking
(`thinking.type: adaptive` with summarized display) plus `output_config.effort`
under a 32,768 `max_tokens` ceiling. Known legacy Claude ids (Haiku 4.5, 4.5 and
older) instead get the budget form with 2,048, 8,192, or 16,384 tokens, since
current models reject budgets and older ones require them. Reasoning summaries and thinking stream as
`thinking_delta` events; Anthropic thinking blocks and signatures are stored in
the assistant item so tool-using turns continue correctly. Usage is recorded as a
durable `usage` event per model call. HTTP requests have a 10 second connect
timeout and a 120 second idle read timeout, and no total deadline. Provider error
bodies are reduced to a bounded `detail` string; codes never contain URLs or keys.
Live provider behavior has been exercised only through synthetic endpoints in
tests; see [NEXT.md](NEXT.md).

## Core ownership and resource bounds

One process uses a Tokio I/O runtime with one scheduler thread, one shared reqwest
client, and asynchronous agent tasks. Each client session drains its output
independently (a thread for stdio, a task per socket). Durable mode adds one
storage worker for all bots and one reader per session. These are threads and
tasks, not one process per bot.

History items are immutable, reference-counted encoded JSON buffers. Appending
allocates the new item; an in-memory fork shares its prefix. Requests stream
references to these items with an explicit Content-Length. They do not rebuild
or stringify the full conversation on each turn. Provider HTTP/TLS comes from
[reqwest](https://docs.rs/reqwest/0.13.4/reqwest/struct.Body.html), scheduling from
[Tokio](https://docs.rs/tokio/1.53.1/tokio/runtime/struct.Builder.html), and disk
transactions from [rusqlite](https://docs.rs/rusqlite/0.40.2/rusqlite/).

Current limits: 8 MiB / 4,096 items of model context per request (stored
history is unbounded), 256 KiB input prompt, 64 KiB instructions, 512 KiB terminal provider output, 2 MiB SSE frame,
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
waiting in the shared command loop. The CLI exits with a connection error;
clients reconnect and re-follow from their last received durable cursor, or
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

```sh
.local/target/release/agent serve \
  --store .local/runtime/state.sqlite --socket .local/runtime/state.sqlite.sock \
  --provider anthropic --provider openai --model anthropic/claude-sonnet-4-5 \
  --tools shell,read,write,edit,wait,history
```

Requests include a string or nonnegative integer `id`. Responses carry the same
`id` and either `result` or an explicit `error` code with optional `detail`.
Notifications carry `event`; durable ones carry `cursor`, `bot`, `turn`, and
`data`, in exactly the shape `events` replays them. Example requests:

```json
{"id":1,"op":"create","bot":"Bob","workspace":"/workspaces/project","model":"anthropic/claude-sonnet-4-5","reasoning":"low"}
{"id":2,"op":"submit","bot":"Bob","request_id":"work-1","prompt":"Hello","workspace":"/workspaces/project-copy","model":"anthropic/claude-opus-4-1"}
{"id":3,"op":"follow","bot":"Bob","after":0}
{"id":4,"op":"resume","bot":"Bob"}
{"id":5,"op":"events","bot":"Bob","after":0,"limit":100}
{"id":6,"op":"item","bot":"Bob","node":2}
{"id":7,"op":"artifact","bot":"Bob","turn":1,"call_id":"call_1"}
{"id":13,"op":"artifact","bot":"Bob","turn":1,"call_id":"call_1","stream":"stdout","offset":0,"limit":65536}
{"id":8,"op":"fork","source":"Bob","checkpoint":2,"bot":"Alternative"}
{"id":9,"op":"interrupt","bot":"Bob","turn":1}
{"id":10,"op":"unfollow","bot":"Bob"}
{"id":11,"op":"bots","after":null,"limit":64}
{"id":14,"op":"prune","bot":"Bob","keep_turns":8}
{"id":15,"op":"delete","bot":"Bob"}
{"id":12,"op":"shutdown"}
```

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
are `created`, `forked`, `accepted`, `message`, `usage`, `tool_started`
(with a 2 KiB argument preview), `tool_completed` (with retained artifact
names), `turn_waiting` and `turn_resumed` (a parked turn's handles and its
wake-up), and `turn_finished` (with status, checkpoint, error code, and detail).
A `submit` response includes the turn's `handle`, `turn:BOT/N`.

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

Workspaces, wherever given, must already exist and be absolute. Use the actual returned checkpoint
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
with a different prompt, workspace, or model fails. `workspace` and `model` on
`submit` are optional per-turn overrides of the bot's defaults; the `accepted`
event records the values actually used. Duplicate reconciliation still works when all active
turn slots are occupied; capacity rejection never writes a fresh submission.
A bot permits one running turn. Interrupt requires its exact current turn ID.
A missing bot never creates a replacement implicitly.

## Durable state and recovery

SQLite WAL with `synchronous=FULL` stores bot metadata and provider binding,
immutable history nodes, turns, completed checkpoints, tool intents/results,
retained tool artifacts, and durable event cursors. The store allows one owning
process. A second owner fails before it can mark the first owner's work
interrupted. Ownership uses the canonical database path with an appended
`.owner-lock` suffix; symlinks resolve to the same lock and hard-linked database
files are rejected. Do not replace or rename the database or its lock while
open. The schema version lives in SQLite's `user_version`; a store created
before versioning is refused with `store_schema_unsupported`, one written by a
newer binary with `store_schema_newer`, and an older versioned store is
migrated forward one version at a time inside the opening transaction (a
version-6 store gains turn ordinals rebuilt from its accepted events). The
migration is the only code that knows an earlier format. The stored provider and tool
configuration must still match at reopen, or `store_configuration_mismatch`
is returned.

Accepted user input is committed before the submission response. Provider output,
usage, and tool plans are committed before tool dispatch. Tool intent is
committed before execution and the result before continuing the model loop. A
completed checkpoint and terminal event are committed together before emitting
completion.

On recovery, unfinished turns are marked interrupted. A planned/executing tool
without a committed result is conservatively marked uncertain, and further
submission on that bot is blocked. No external request or tool is automatically
repeated. The caller can inspect records and fork a known completed checkpoint;
in-place resolution of uncertain outcomes is not implemented.

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
its context window from the store in batches of 64 items with an exact
Content-Length; the daemon never holds a transcript. SQLite's configured cache is
2 MiB, not a total bound on storage-related or OS memory. Durable performance
needs its own benchmark.

## Pacing and retries

A provider's allowance is the scarce resource in a fleet, so every model call
passes through one pace per provider and model: a fair FIFO gate holding two
buckets, requests and tokens per minute. The pool is unbounded until the
provider reports its limits in headers
(`x-ratelimit-*` on OpenAI, `anthropic-ratelimit-*` on Anthropic); from then
on a call is admitted only when both buckets can afford it, debited by an
estimate (request bytes divided by four plus the output cap). Reported token
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
the pool until that time; the turns behind it wait rather than fail. The gate
adds a lock and a few arithmetic operations on an unpaced call. See the
[matched follow-up](DAEMON_MEASUREMENTS.md#pacing-review-fixes) for CPU, memory,
latency, and the measurement limits.

An estimate above the learned per-minute token limit fails with
`provider_pacing_limit`, without consuming allowance or blocking the next
request. This is a local estimate limit, not a provider refusal; reduce the
context or configured output cap before resubmitting. Unknown pools remain
unbounded until headers teach them a limit.

A model call has no side effects, so a failed one is retried by rebuilding
the request from the store: within 5 minutes, up to 8 attempts for capacity
(5xx) or transport failures and up to 64 for refusals for pace, which the pool
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
`--context-bytes` and `--context-items`. The window starts at a turn boundary
(a user prompt), so a model never sees a tool call without its result or a
reply without its prompt. Its start is persisted per bot (`context_start`) and
only moves when the window overflows; it then jumps back to the oldest turn
boundary within three quarters of both budgets, so the request prefix stays
byte-identical across many turns and provider prompt caches keep hitting. A
fork inherits the lineage, not the start; its first request computes its own
window over the shared history.

When turns are omitted, the request begins with one user item:
`[context note] N earlier turn(s) with M messages are not shown. Use the
history tool with a turn number from 1 to N to read any of them.` The note is
part of the request, never stored. Turn numbers are ordinals along the lineage
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
metadata, to half the current turn's remaining byte budget. This leaves room
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
a budget, the turn fails with `context_limit` rather than sending a truncated
request. Both limits are daemon flags forwarded by the client, reported in
`ready` as `limits.context_bytes` and `limits.context_items`, and advertised as
the `context_window` capability. Stores are schema version 12; a version-6
store is migrated at open. Store initialization and migration run in one
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
  still running, answers `bot_busy`. Live
  followers receive a non-durable `deleted` notification. `agent rm --bot`.
  Queued wake-ups for interrupted or deleted turns are discarded when capacity
  opens; reusing a bot name cannot resume its old turn. This internal check
  does not change explicit `resume` requests: a missing bot returns `bot_not_found`.
- `prune {bot, keep_turns}` keeps the newest `keep_turns` turns' records and
  drops the older turns' events, tool intents, finished processes, and artifacts.
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
  its turns finishes, including interruption while parked, before the terminal
  event is delivered. Whoever sees `turn_finished` sees the store as retention
  left it. The service commits completion and publishes its terminal event
  before accepting another turn for that bot; the task is retired before
  notifying followers and waiters. Shutdown drains completions through the
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
older turns. The transcript, accounting, and idempotency rows remain intact.

Freed pages are reused by later writes rather than returned to the
filesystem, so a bounded fleet's store stops growing instead of shrinking.
For a long-lived bot the transcript itself still grows with every turn (about
850 bytes per turn on the sustained workload); bounding that is compaction,
which is future work, or deleting the bot.

## Tools and validation scope

`--tools` names any subset of `echo`, `shell`, `read`, `write`, `edit`, `wait`, and
`history`. The
registry validates tool names and arguments before execution; a tool that fails
(unknown tool, invalid arguments, missing file, ambiguous edit, timeout, output
overflow) returns an error result to the model and the turn continues. Only a
closed tool scheduler fails the turn. Allowed tools run without approval prompts.
A store remains bound to its tool set; changing it requires a new store.

`shell` runs a noninteractive `/bin/sh` command in the bot workspace with
`timeout_ms` (default 120 s, maximum 600 s). Commands are at most 16 KiB. At most
`--max-processes` child processes run at once across the service, foreground or
background; waiting never counts. stdout and stderr are each
retained up to 1 MiB; beyond 64 KiB the model receives a head and tail with the
omission stated and the full stream is stored as an artifact retrievable through
the `artifact` operation. Results include separate output, exit code, and
success status. Nonzero exit is a recorded tool result. Timeout and overflow kill
the owned process group. Turn cancellation kills that group; an interrupted tool
without a committed result leaves the bot uncertain rather than repeating
possible side effects.

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
stored history is now unbounded with a per-request context window, and
compaction with summaries is not implemented.
