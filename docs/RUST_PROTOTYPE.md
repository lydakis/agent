# Rust prototype

Implemented 2026-09-07. The immediate target is identical synthetic streaming
work below 40 MiB total sampled RSS at 32 agents. That target does not imply
production capacity or a claim to be the world's fastest harness.
The [first measurements](RUST_MEASUREMENTS.md) passed this target.

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

## Core ownership and resource bounds

One process uses a Tokio I/O runtime with one scheduler thread, one shared reqwest
client, and asynchronous agent tasks. A dedicated output thread prevents a blocking
stdout pipe from blocking the scheduler. Durable mode adds one storage worker
for all bots and one input reader. These are threads, not one process per bot.

History items are immutable, reference-counted encoded JSON buffers. Appending
allocates the new item; an in-memory fork shares its prefix. Requests stream
references to these items with an explicit Content-Length. They do not rebuild
or stringify the full conversation on each turn. Provider HTTP/TLS comes from
[reqwest](https://docs.rs/reqwest/0.13.4/reqwest/struct.Body.html), scheduling from
[Tokio](https://docs.rs/tokio/1.53.1/tokio/runtime/struct.Builder.html), and disk
transactions from [rusqlite](https://docs.rs/rusqlite/0.40.2/rusqlite/).

Current limits: 8 MiB / 4,096 items of loaded history per bot, 256 KiB input
prompt, 512 KiB terminal provider output, 2 MiB SSE frame, 16 MiB response stream,
eight provider rounds per turn, and 1,024 active durable-server turns. Provider
startup admits at most 64 requests awaiting response headers, with a 60-second
admission timeout. The permit is released before reading SSE. This smooths
connection bursts while allowing more established streams, but a provider that
delays headers will limit admission; live-provider tuning remains open. The benchmark
mode accepts up to 4,096 agents, subject to its additional fixture limits. There
is no automatic compaction: exceeding a bound is an explicit error.

The output channel allows at most 64 queued packets and 2 MiB of queued/writing
bytes. Each packet is capped at 1 MiB. Producers can additionally hold a bounded
event awaiting capacity, so the queue budget is not the whole-process memory
budget. The input channel holds at most eight bounded requests; storage queues
at most 32 operations. HTTP client internal buffers, active histories, allocator
retention, and filesystem cache remain part of the memory accounting.

## Software protocol

The current service is JSONL over stdio. The caller owns its long-lived process.
Socket attachment, automatic daemon startup, and client reconnection to a running
process are not implemented. Process restart plus disk replay is supported.

```sh
.local/target/release/agent serve \
  --store .local/runtime/state.sqlite \
  --base-url https://api.example.test/v1 --model your-model \
  --key-env YOUR_PROVIDER_KEY --tools echo,shell
```

Replace the example endpoint with a Responses-compatible provider. `--key-env`
is optional and reads only the explicitly named variable. The harness does not
discover or copy another application's credentials. Test and benchmark commands
use loopback fixtures without real model calls. HTTPS is compiled in; external
provider behavior has not yet been validated.

Requests include a string or nonnegative integer `id`. Responses carry the same
`id` and either `result` or an explicit `error`. Notifications carry `event`, with
bot and turn identifiers when applicable. Example requests:

```json
{"id":1,"op":"create","bot":"Bob","workspace":"/workspaces/project"}
{"id":2,"op":"submit","bot":"Bob","request_id":"work-1","prompt":"Hello"}
{"id":3,"op":"resume","bot":"Bob"}
{"id":4,"op":"events","bot":"Bob","after":0,"limit":100}
{"id":5,"op":"fork","source":"Bob","checkpoint":2,"bot":"Alternative","workspace":"/workspaces/alternative"}
{"id":6,"op":"interrupt","bot":"Bob","turn":1}
{"id":7,"op":"item","bot":"Bob","node":2}
{"id":8,"op":"shutdown"}
```

Workspaces must already exist and be absolute. Use the actual returned checkpoint
and turn IDs, not the illustrative numbers. Names are immutable bot identities
within one store; rename/alias operations are not implemented. Fork validation
requires a completed-turn checkpoint in the source's ancestry. It references
existing stored nodes and creates no workspace or historical side effects.

Submission is idempotent on `(bot, request_id)`. An identical retry returns the
same turn without executing again, including after restart. Reusing that key
with different input fails. Duplicate reconciliation still works when all active
turn slots are occupied; capacity rejection never writes a fresh submission.
A bot permits one running turn. Interrupt requires
its exact current turn ID. A missing bot never creates a replacement implicitly.

## Durable state and recovery

SQLite WAL with `synchronous=FULL` stores bot metadata, immutable history nodes,
turns, completed checkpoints, tool intents/results, and durable event cursors.
The store allows one owning process. A second owner fails before it can mark
the first owner's work interrupted. Ownership uses the canonical database path
with an appended `.owner-lock` suffix; symlinks resolve to the same lock and
hard-linked database files are rejected. Do not replace or rename the database
or its lock while open. Stop older prototype processes before updating: their
previous lock naming scheme is not compatible with this ownership check.
Stored provider/model/tool configuration
must match at reopen; changing configuration requires a future explicit migration.

Accepted user input is committed before the submission response. Provider output
and tool plans are committed before tool dispatch. Tool intent is committed before
execution and the result before continuing the model loop. A completed checkpoint
and terminal event are committed together before emitting completion.

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
monotonic IDs, and queries are filtered to the requested bot. No retention pruning
or replay-gap policy has been implemented yet.

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

The default tool set remains `echo`, preserving existing store configuration.
`--tools echo,shell` additionally registers noninteractive `/bin/sh` execution
in the bot workspace. The registry validates tool names and arguments before
committing plans; allowed tools run without approval prompts. A store remains
bound to its original tool schema; changing it requires an explicit future
migration or a new store.

Shell arguments are `command` and optional `timeout_ms` (default 10 seconds,
maximum 30 seconds). Commands are at most 16 KiB. At most 16 shell commands run
at once across the service. stdout and stderr are each bounded to 32 KiB; results
include separate output, exit code, and success status. Nonzero exit is a recorded
tool result. Overflow and timeout fail explicitly and kill the owned process
group. Normal completion also reclaims background children. Turn cancellation
kills that group; an interrupted tool without a committed result leaves the
bot uncertain rather than repeating possible side effects.

This is Unix-only, has no terminal/background-job support, and inherits the
service environment except the selected `--key-env` variable. Provider requests
retain that credential; shell children do not receive it through that variable.
Exact occurrences of its value in echo and shell results are replaced with
`[REDACTED]` before result serialization and persistence. This handles known tool
output text, not arbitrary encodings or credentials elsewhere on the host.
Tools still run with caller OS permissions. A command that escapes its
process group, or a hard kill of the whole service, needs external process
isolation/cleanup. A workspace snapshot is not a sandbox. Dedicated file-edit
tools, MCP, dynamic external registration, and a richer permission policy remain
unimplemented.

The real-process tests cover killing/restarting the server, exact lookup, duplicate
submission, historical forks with independent continuation, Unicode/SSE fragmentation,
a complete provider/tool/provider loop, stale interruption, cancellation, rejection of a second storage owner, and missing
provider completion. Rust tests cover shared prefix allocation, history bounds,
slow-consumer queue pressure, transactional tool outcomes, and replay pagination.

The ephemeral benchmark calls the same provider/history core but bypasses SQLite
and registers no tools, matching the earlier Pi/Codex text workload. It must not
be presented as durable-server, coding-tool, TLS, or real-provider performance.
The [durable feature screen](LIFECYCLE_MEASUREMENTS.md) measures actual service
execution, including shell descendants, independently of the text-core screen.
Very long histories and compaction are specified in [LONG_HISTORY.md](LONG_HISTORY.md);
the present whole-history cap has not yet been removed.
