# Rust prototype

Implemented 2026-09-07; extended 2026-09-14 with a socket daemon, the `agent`
client, two provider families, and file tools. The immediate performance target
was identical synthetic streaming work below 40 MiB total sampled RSS at 32 agents.
That target does not imply production capacity or a claim to be the world's
fastest harness. The [first measurements](RUST_MEASUREMENTS.md) passed it.

## Build and validate

Rust 1.92 or later is required. Dependencies are locked; build outputs and the
optional repository-local Cargo cache stay under ignored `.local/`.

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
daemon. Default tools are `shell,read,write,edit`.

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
`"$AGENT_BIN" run --detach --new --bot NAME -- TASK` to submit peer work and
`"$AGENT_BIN" ls` to inspect progress. `AGENT_SHELL_CONTEXT=1` tells the CLI to
reject blocking `run` and `follow` before submitting work: otherwise waiting
callers can occupy every shell permit needed by the agents they await. This is
an execution-context guard, not a permission or agent-identity boundary. Arbitrary
shell synchronization can still block; this does not sandbox external commands.

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
The selected provider's credential is redacted from decoded messages (and its
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

`reasoning` maps to Responses `reasoning.effort` with summaries requested, and to
Anthropic extended thinking with 2,048, 8,192, or 16,384 budget tokens under a
32,768 `max_tokens` ceiling. Reasoning summaries and thinking stream as
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

Current limits: 8 MiB / 4,096 items of loaded history per bot, 256 KiB input
prompt, 64 KiB instructions, 512 KiB terminal provider output, 2 MiB SSE frame,
16 MiB response stream, 200 provider rounds per turn, and 1,024 active turns.
Provider startup admits at most 64 requests awaiting response headers, with a
60-second admission timeout. The permit is released before reading SSE. This
smooths connection bursts while allowing more established streams, but a
provider that delays headers will limit admission; live-provider tuning remains
open. The benchmark mode accepts up to 4,096 agents, subject to its additional
fixture limits. There is no automatic compaction: exceeding a bound is an
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
  --tools shell,read,write,edit
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
delivered only to live followers and carry `durable:false`. Durable event kinds
are `created`, `forked`, `accepted`, `message`, `usage`, `tool_started`
(with a 2 KiB argument preview), `tool_completed` (with retained artifact
names), and `turn_finished` (with status, checkpoint, error code, and detail).

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
within one store; rename/alias operations are not implemented. Fork validation
requires a completed-turn checkpoint in the source's ancestry. It references
existing stored nodes and creates no workspace or historical side effects.

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
open. The stored provider/tool configuration (schema 3) must match at reopen;
stores from earlier prototypes are rejected with
`store_configuration_mismatch` rather than migrated.

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
instead of a walk per node. No retention pruning or replay-gap policy has been
implemented yet.

Event pages stop at either the requested count or a 512 KiB encoded-entry budget.
Continue from `next_cursor` even when a nonempty page has fewer than `limit`
entries; an empty page means no later retained events. A single entry exceeding
that budget returns `event_page_item_limit`. Responses exceeding the 1 MiB wire
limit return a correlated `response_size_limit` error without stopping the service.

Inactive bot histories stay on disk. Active requests currently load their bounded
history into memory; fully disk-streamed context and a shared active-history cache
are future optimizations. SQLite's configured cache is 2 MiB, not a total bound
on storage-related or OS memory. Durable performance needs its own benchmark.

## Tools and validation scope

`--tools` names any subset of `echo`, `shell`, `read`, `write`, and `edit`. The
registry validates tool names and arguments before execution; a tool that fails
(unknown tool, invalid arguments, missing file, ambiguous edit, timeout, output
overflow) returns an error result to the model and the turn continues. Only a
closed tool scheduler fails the turn. Allowed tools run without approval prompts.
A store remains bound to its tool set; changing it requires a new store.

`shell` runs a noninteractive `/bin/sh` command in the bot workspace with
`timeout_ms` (default 120 s, maximum 600 s). Commands are at most 16 KiB. At most
16 shell commands run at once across the service. stdout and stderr are each
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

The ephemeral benchmark calls the same provider/history core but bypasses SQLite
and registers no tools, matching the earlier Pi/Codex text workload. It must not
be presented as durable-server, coding-tool, TLS, or real-provider performance.
The [durable feature screen](LIFECYCLE_MEASUREMENTS.md) measures actual service
execution, including shell descendants, independently of the text-core screen.
Very long histories and compaction are specified in [LONG_HISTORY.md](LONG_HISTORY.md);
the present whole-history cap has not yet been removed.
