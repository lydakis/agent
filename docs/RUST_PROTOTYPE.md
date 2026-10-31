# Rust prototype

Implemented 2026-09-07; extended 2026-09-14 with a socket daemon, the `agent`
client, two provider families, and file tools. The immediate performance target
was identical synthetic streaming work below 40 MiB total sampled RSS at 32 agents.
That target does not imply production capacity or a claim to be the world's
fastest harness. The [first measurements](RUST_MEASUREMENTS.md) passed it.

## Build and validate

Development builds use Rust 1.99.1, pinned in `rust-toolchain.toml` with Clippy
and rustfmt. The declared minimum supported Rust version is 1.95, which the
atomics' `try_update` needs.
Dependencies are locked; build outputs and the optional repository-local Cargo
cache stay under ignored `.local/`.

```sh
CARGO_HOME=.local/cargo cargo build --release --locked
CARGO_HOME=.local/cargo cargo test --locked
CARGO_HOME=.local/cargo cargo clippy --locked --all-targets -- -D warnings
AGENT_TEST_RUNTIME=1 AGENT_BENCH_TEST_ENGINES=1 .local/venv/bin/python -m unittest discover -s tests -v
AGENT_TEST_RUNTIME=1 .local/venv/bin/python -m tests.run --jobs 4   # the same modules side by side
.local/venv/bin/python -m bench run --engine rust --out .local/bench/rust
.local/venv/bin/python -m bench.matrix --out .local/bench/three-engines
```

Every pull request and push to main runs the format, Clippy and Rust test
checks and, after a release build of the daemon and Sonny, the runtime suite
with `tests.run` on Linux ([test.yml](../.github/workflows/test.yml)).

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
agent run --new --model anthropic/claude-sonnet-5 --bot Bob -- "Add a failing test for the parser bug, then fix it"
agent run --bot Bob -- "Now run the full suite"          # same bot, next turn, same conversation
agent run --bot Bob --model anthropic/claude-opus-5-5 -- "Review the diff"   # this turn only
agent follow --bot Bob --after 0                         # replay, then live events
agent fork --source Bob --checkpoint 12 --bot Bob-alt      # starts in Bob's folder
agent run --fork Bob --bot Bob.side --allow '' --expires 15m -- "Where are you?"   # ask a copy; Bob carries on
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
`--tools` or `shell,write_stdin,read,write,wait,history` with `apply_patch` for a
Responses model and `edit` otherwise, plus `web_search` where the provider
offers it and `web_fetch` for any, unless `web_search = "disabled"` in `~/.agent/config.toml`, before it asks. A bot's
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
daemon. Default tools are `shell,write_stdin,read,write,wait,history` and the file editor
the model's family is trained on, `apply_patch` (Responses) or `edit`, and
`web_search` where the provider offers it, and `web_fetch`; `note`, the
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
address; the identity is a store-wide integer `bot_id` that `create`, `fork`,
`inspect`, `bots`, every `submit` answer, and the `created` and `forked` events
report, and that is never reused
after a delete. A fork is a new identity with an empty request namespace.

A bot keeps its folder but is not bound to it. A new bot starts in the
directory `run` was invoked from (or `--workspace`), and later turns run there
wherever `run` is invoked. A submission that names a workspace runs there and
moves the bot, so the same conversation can continue in a new checkout,
worktree, or snapshot. Work already queued keeps the folder it was sent to; a
steer that names none joins the running turn wherever it runs, and a steer's
folder never moves the bot. A fork starts in
its source's folder unless `fork` names another. A bot without a folder rejects
a submission that names none with `workspace_required`. The conversation does not know about
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
reports its status, checkpoint, error, and final assistant text (up to 16 KiB);
a client's `wait` gets the whole turn view (below) with them, and a model's
`wait` tool gets only those. A process handle reports the same stdout, stderr,
and exit code a foreground `shell` would. Unresolved handles at the deadline
are returned as pending and remain valid for a later wait: a process as
`{"pending": true}`, a turn, for a client, as its view with `pending: true`,
read in one job for all of them. A turn that finished between the deadline
and that read answers with its outcome instead. A turn cannot wait on itself; unknown handles
resolve to errors rather than blocking.
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
commands. A bot's `wait` tool resolves only the commands that bot started:
another bot's `proc:N`, such as one a fork inherited in its history, fails
with `handle_unavailable`. A command's stored output is readable by the bot
that started it, and by another branch only when its history holds the wait
result that delivered it. The `wait` protocol op and `agent wait` still
resolve any handle: their caller is a program holding the handle it was given.
This is not a sandbox. Completion commits the result and its overflow artifacts in one
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
OS process did. A daemon that stops cleanly does not wait for them: it kills
each one's process group and records `process_lost` itself. Interrupting a parked
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
`models` split when a fallback ran another model and on every
summarizer call, naming the summarizer's provider and model, and
`cache_write_tokens` when Anthropic wrote input to its prompt cache: part of
`input_tokens`, billed above the base rate, and kept only in the event since
no budget or cache ratio needs it, with `cache_write_1h_tokens` for the part
cached for an hour; `sent_ms`, when the request was sent in Unix-epoch
milliseconds, so a cache miss can be set against the gap since the call before
it; `served_model`, the model the provider named in its response, which can
differ from the one requested (a dated snapshot, a reroute, and after a
fallback the last attempt's); `context_window`, the model's context window
in tokens on a model round's event, so a client can say how full the
conversation is; a prompt-cache refresh's event carries
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
`submit` also accepts `budget_tokens`, a cap on the input plus output tokens
of the turn it starts, checked where the bot's cap is. A turn past it ends as
`failed` with `turn_budget_exhausted` and the facts `turn_budget_tokens` and
`turn_tokens_used`; when both caps are reached the bot's `budget_exhausted`
is named, since it outlasts the turn. The cap is stored on the turn (`turns`
lists it as `budget_tokens`), so a resumed turn counts what it spent before
parking, and a resend naming another cap is an `idempotency_conflict`. A
steer joins a turn that already has its cap: `budget_tokens` with
`delivery: "steer"` is `invalid_delivery`. A summary held beside a call
counts toward both caps.

`submit` also accepts `images`, pictures sent with the prompt: up to 20 of
`{"media_type", "data"}`, the type one of `image/png`, `image/jpeg`,
`image/gif` or `image/webp` and the data standard base64 of an image of that
type, at most 5 MiB together once decoded (`image_limit`; anything else is
`invalid_image`, with the `index` it was at). They go in the user item after
the prompt's words, as `input_image` data URLs for Responses and base64
`image` blocks for Anthropic, in a turn of their own or a steer. The turn keeps
them, so a resend with other pictures is an `idempotency_conflict`. History
reads (`history_items`) return a person's prompt with each picture's bytes
left out, its `omitted_bytes` counted; the model is sent them whole. A request
line may be 8 MiB, room for a prompt and its pictures.

A turn reads the same wherever a client meets it. Its view is `bot`,
`bot_id`, `turn`, `handle`, `request_id`, `status`, `waiting_on`,
`input_tokens`, `cached_input_tokens`, `output_tokens`, `model_rounds`,
`retries`, `paced_ms`, `started_ms` and `finished_ms`, all from the turn's row.
`waiting_on` is what a parked turn waits for, as its `turn_waiting` or
`turn_paced` event said (`call_id` and `approval` or `handles`, or
`resume_at_ms`, null with `paused: true` for a turn the pull cord parked), and
null otherwise. A `wait` result is the view, plus the
outcome once finished; an `interrupt` reply is the view plus
`interrupt_requested`; a `turns` item is the view plus the fields below.
`submit` answers with the view's identity and status, without a second read.

`turns` (protocol) and `agent turns --bot NAME` list a bot's turns as views
with effective workspace, model and effort, delivery, cache hit, and a
prompt preview, oldest first and paged by `after` (`next_after`). With
`newest: true` a page runs newest first from the end of the bot's turn index
and is paged by `before` (`next_before`), so a bot's last few turns cost one
indexed read however many it has; `after` and `before` bound either direction
(`agent turns --newest --limit N`). A finished turn's outcome comes from `wait` on
its handle; `timeout_ms: 0` answers at once, pending while the turn runs. `agent turns`
restarts an idle daemon using the supplied provider/tool configuration (or
provider environment defaults), honors `--no-spawn`, and refuses missing stores.
It does not submit new model work; existing parked work may resume on startup.

A truncated shell result names its retained streams as `artifacts`, for
example `["12/call_abc/stdout"]`, and the model can page through one with
`read` by passing `artifact` instead of `path`. Artifact reads allow the producing
bot or a branch containing the original tool-result node. Historical forks can
read inherited outputs, but cannot read later source turns or unrelated branches.
A stream the call never retained answers `artifact_not_found`, a turn outside
the reader's lineage `turn_not_found`, and a turn whose records retention has
removed `artifact_pruned` (see Retention).

<a id="retained-outputs"></a>A bot with an `outputs` folder keeps each such
stream there whole instead, as `TURN-CALL_ID-STREAM`, and its result lists
those paths as `files` beside `artifacts`, so any command or tool reads them.
The store keeps one row per stream naming the file and none of its bytes
(schema 68), and `artifact` and `read artifact=` read the file. Each file is
made new (0600, never through a link) and synced with its folder before the
store records it, off the runtime thread, and only while
the bot's sandbox cannot write the folder, checked after the folder is made: a
command that could write there could put a link for the daemon to follow. An unconfined bot's
commands can already write whatever the daemon can. A stream whose file cannot
be written stays in the store, and a turn stopped while its files are written
removes them. Retention and deletion remove a turn's files once removing its
records commits; a file removed by hand reads `artifact_not_found`. A daemon
that dies between writing a file and recording its call leaves the file, which
no record names.
Line pages are assembled on the storage worker; only the bounded page crosses
into the async runtime. Byte-oriented protocol pages continue to use SQL slicing.

## Limits

Three daemon limits are flags on `serve`, forwarded by the client that starts
the daemon, and reported in the `ready` event as `limits`. Zero removes a
bound; the operating system is then the only limit.

| Flag | Bounds | Default |
| --- | --- | --- |
| `--max-processes` | Child processes running at once, foreground or background. Waiting never counts. | 64 per logical CPU |
| `--max-lasting` | Processes that outlive their call, still running, daemon-wide: terminals and detached shell commands together. A terminal holds its slot until its whole session is gone, a detached command until it is reaped. Zero is unbounded. | 4 per logical CPU |
| `--max-active` | Turns with a live task: a model call in flight or a foreground tool. Parked turns never count. | 4,096 |
| `--max-pending` | Submissions waiting to start: queued behind a bot's own work or ready for a slot, daemon-wide. A submission that would wait past the bound answers `pending_limit` and writes nothing; one that starts at once is never refused by it. | none |
| `--max-pending-bytes` | UTF-8 bytes of those waiting submissions: their prompts and the words they quote, which wait with them. | none |
| `--max-readers` | Store reader connections, each on its own thread, for the reads clients ask for that decide nothing ([observation reads](#observation-reads)), which take all but one, and for turns' own reads. At least 1. | one per logical CPU |
| `--max-connecting` | Provider requests awaiting response headers, a Bedrock Runtime call's body digest included. Established streams are not capped. Both providers hold headers until the first token, so a permit is held for the whole time to first token; a bound of N caps throughput at N calls per first-token latency. | none |
| `--stall-timeout` | How long an established provider stream may go without a content frame before the attempt fails as `provider_stream_stalled` and is retried. Keepalives do not count. Whole seconds, `1s` to `24h`. | `2m` |
| `--idle-exit` | How long, in whole seconds, before a socket daemon with no sessions, no live turns, and no running background commands exits. Parked turns are durable and resume on the next start; the client restarts the daemon on demand. | none |
| (derived) `connections` | HTTP/2 connections per provider: `max-active` divided by 64 streams per connection (both providers allow 100; fewer bounds how many turns one reset connection takes with it), 1 to 256; 64 when active is unbounded. Reported in `ready`, not a flag. | 64 |

### Bot settings

A bot's context, compaction, retention, approval hold and model-call
settings are its own:
`create` takes them as a `settings` object, the bot keeps them, a fork
copies its source's, and `inspect` reports every one as it applies, defaults
included. A setting left out takes the default below; a value out of range
answers `invalid_setting` naming it, and nothing is created. `agent run`
sets them on a new bot with the flags shown; an existing bot keeps its own.
Each bot can choose differently, so one daemon serves bots on models with
different context windows or output limits. These were daemon flags before
schema 40; a bot from before then takes the defaults. A value its provider's
family refuses, such as an Anthropic output bound below 2,048, is refused at
`create` with that provider's error. A turn calls through the shared provider,
or through a copy shaped by the bot's own output bound, cache lifetime and
keep-warm when it sets any of them.

| Setting (flag) | Meaning | Default |
| --- | --- | --- |
| `context_bytes` (`--context-bytes`) | Encoded input conversation-envelope bytes, including pinned context and separators (see [long history](#long-history-and-context-windows)). Minimum 1,024. | 8 MiB |
| `context_items` (`--context-items`) | Input conversation-envelope items, including pinned context. Minimum 2. | 4,096 |
| `note_turns` (`--note-turns`) | Omitted turns the context note lists, newest first, with the first line of each prompt. 0 lists none. | 48 |
| `compact_at` (`--compact-at`) | Percent of either context envelope that triggers compaction, and of the byte envelope that triggers tool-result elision first. Estimated completion headroom can advance the byte trigger without reducing the input allowance. | 75 |
| `compact_keep` (`--compact-keep`) | Target percent of either context envelope kept verbatim: as newest whole turns by compaction, as newest items by elision. Reduced when pinned context leaves less room. Must be below `compact_at`. | 25 |
| `compact_tokens` (`--compact-tokens`) | Input tokens per request that start elision and compaction too, whichever of this and `compact_at` comes first (see [compaction](#compaction)). 0 leaves only `compact_at`; otherwise at least 1,024. | from the model's context window |
| `max_rounds` (`--max-rounds`) | Model calls each turn may make; the last goes with no tool call allowed, and the turn fails `tool_round_limit` past it. | none |
| `keep_turns` (`--keep-turns`) | After each of its turns finishes, prune the bot to this many turns' records (see [Retention](#retention)). | none |
| `approval_hold_ms` (`--approval-hold`) | Milliseconds a gated call waits live for its verdict before its turn parks (see [tool approval](#tool-approval)), and a terminal read waits live for its output before its turn parks. 0 parks at once; at most 3,600,000. | 2,000 |
| `max_output_tokens` (`--max-output-tokens`) | Generated tokens per model call, including reasoning, summaries included. Anthropic calls use the model's full output limit (read inside Bedrock ids) unless this is set; set, it is sent as `max_tokens`, at least 2,048 so a legacy thinking budget of 1,024 or more fits beside the answer. Bedrock deducts input plus this bound from quota when a call starts, so a bound near real output buys throughput there. | none |
| `keep_warm` (`--keep-warm`) | Seconds an Anthropic prompt cache may sit unread while a turn runs a tool before it is refreshed (see [keeping the cache warm](#keeping-the-anthropic-cache-warm)). Below 300; 0 never refreshes. | 240 |
| `cache_ttl` (`--cache-ttl`) | Anthropic prompt-cache lifetime, `5m` or `1h`, on every cache marker of a request: the instructions', the request's own, and the pinned compaction summary's and note's, since the API refuses a longer lifetime after a shorter one. `1h` bills each write at twice the input rate instead of 1.25 times and sends no refreshes. Responses providers are unaffected. Bedrock takes `1h` for Claude 4.5 and later and may refuse it for older Claude (its prompt-caching guide, read 2026-10-28), so calls to older Claude there keep five minutes whatever this says, also after `configure` switches a bot to one. A bot whose summarizer or fallback is such a model keeps five minutes on every call, since those calls carry the same pinned blocks. `inspect` reports a lifetime never set as `null`, so a client's own default can tell it from `5m` someone chose. | unset (`5m`) |
| `speed` (`--speed`) | The tier the bot's own model's calls are served at: `standard`, `fast` or `ultrafast`, each faster one at a higher price per token. The Claude API serves `fast`, its fast mode (sent as `speed` with the `fast-mode-2026-02-01` beta; Opus 4.8 and later, twice the price, its own rate limit, so its own pool, which learns `anthropic-fast-input-tokens-*` and `anthropic-fast-output-tokens-*`); Bedrock serves Claude at standard only. OpenAI's API and the ChatGPT plan serve `fast` (sent as `service_tier: priority`) and `ultrafast`; Bedrock serves GPT models `ultrafast` alone (OpenAI's fast-mode guide and Bedrock's GPT-6.1 Sol and GPT-6 Astra cards, read 2026-10-30). A tier the binding does not serve is refused `invalid_speed` with its `speeds`; which models take one is the provider's to say, and a call on one that does not fails there. Switching it misses the prompt cache once. Calls on another model (a `compaction_model`, a fallback) go at standard. `inspect` reports `standard` when unset. | `standard` |
| `sandbox` (`--sandbox`) | How the bot's commands, writes and edits are confined, by Codex's names: `read-only`, `workspace-write` or `danger-full-access` (see [sandbox](#sandbox)). | none |
| `writable` (`--writable`, repeatable) | Absolute folders a `workspace-write` bot may write besides its own; each must exist and is stored by its real path. At most 64, 32 KiB as JSON. | none |
| `network` (`--network`) | A `workspace-write` bot's commands may use the network. | off |
| `hide_senders` (`--hide-senders`) | Its model reads what other bots and clients send it without the line naming the sender (see [fleet controllers](#fleet-controllers)); `senders = "hidden"` in `~/.agent/config.toml` sets it on the bots the CLI and Sonny make. | off |
| `outputs` (`--outputs`) | An absolute folder that holds each tool output stream cut for the model whole, one file per stream, in place of the store's copy (see [retained outputs](#retained-outputs)). The daemon makes it, owner-only, and writes there only while the bot's sandbox cannot write it; otherwise the stream stays in the store. `agent run --new` and Sonny name `outputs` beside the store unless told otherwise. | none |
| `unsandboxed` (`--unsandboxed`, repeatable) | Commands a sandboxed bot runs outside its sandbox without asking, each by its first words, such as `mcpx` or `docker compose` (see [sandbox](#sandbox)). At most 64, each at most 1 KiB. | none |
| `unix_sockets` (`--unix-socket`, repeatable) | Absolute Unix sockets, or folders of them, the only ones a sandboxed bot's commands reach, such as a Docker or SSH agent socket (see [sandbox](#sandbox)). Each is stored by its real path while it exists. At most 64. | none |

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
| `responses` | OpenAI Responses API, streaming SSE | `openai` → `https://api.openai.com/v1`, `OPENAI_API_KEY`; `openrouter` → `https://openrouter.ai/api/v1`, `OPENROUTER_API_KEY`; `chatgpt` → `https://api.openai.com/v1`, the ChatGPT plan login `agent login chatgpt` saved; `bedrock-openai` → `https://bedrock-mantle.{region}.api.aws/openai/v1`, SigV4 or `AWS_BEARER_TOKEN_BEDROCK` |
| `anthropic` | Anthropic Messages API, streaming SSE | `anthropic` → `https://api.anthropic.com/v1`, `ANTHROPIC_API_KEY`; `bedrock` → `https://bedrock-runtime.{region}.amazonaws.com/anthropic/v1`, SigV4 or `AWS_BEARER_TOKEN_BEDROCK` |

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
choices. `bedrock` binds Claude on Bedrock runtime, whose cross-region
inference profiles (`global.anthropic.claude-opus-5-5`, or `us.`, `eu.`,
`apac.` and the other geographies) route each call to whichever region
holds the model, from any source region the profile supports (AWS lists them
per profile, and `ListInferenceProfiles` names only the profiles callable from
the region asked), and `bedrock-openai` binds OpenAI and other models on Bedrock Mantle, as
Claude Code and Codex do by default. Both take the region in Claude Code's
order: `AWS_REGION`, `AWS_DEFAULT_REGION`, the `region` of the active profile
(`AWS_PROFILE`, else `default`) in the AWS shared credentials file and then
the config file, else us-east-1; the first value set must be shaped like a region,
or the spec is refused naming where it came from. Both use a Bedrock API key when `AWS_BEARER_TOKEN_BEDROCK` is set, and
SigV4 otherwise; a key takes no us-east-1 fallback, since a short-term key works
only in the region that made it, so with nothing naming a region the spec is
refused (Codex requires a region for keys too). `provider_models` lists runtime's models as the active
system-defined inference profiles the Bedrock control plane in its region
names (`ListInferenceProfiles`), and Mantle's as its host's `/v1/models`.
Any `bedrock-mantle.{region}.api.aws` or `bedrock-runtime.{region}.amazonaws.com`
URL without a key field signs every request with SigV4, for example
`--provider mantle=anthropic,https://bedrock-mantle.us-east-1.api.aws/anthropic/v1`
for Claude on Mantle with in-region `anthropic.claude-opus-5-5` ids. Keys come from the AWS chain in
its own order: `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` (with
`AWS_SESSION_TOKEN`) when set, otherwise whatever the AWS CLI resolves for
`AWS_PROFILE` or the default profile, SSO and assumed roles included, through
`aws configure export-credentials`, asked once at start for every SigV4
binding and waited on at most three seconds. Keys the CLI cannot resolve
by then (no login yet, an expired SSO session, a stalled CLI) do not stop
the daemon: the binding's `ready.providers` entry (its `family`, `url`,
`transport`, the `efforts` its family takes, the `speeds` it serves
faster than standard and whether it offers `web_search`) carries `unresolved` with
the reason (its detail clipped to 200 bytes), and its next call resolves
again, so `aws sso login` fixes it without a restart.
A call with no usable keys fails before it is paced, admitted or its body
read.
Temporary keys are re-resolved in the
background five minutes before they expire, at most every ten seconds, and
once on a 401 or 403, which retries the call as `provider_login_refreshed`.
A 401 or 403 whose `x-amzn-ErrorType` is one of the AWS SDKs' clock-skew
codes (`RequestExpired`, `InvalidSignatureException` and the rest), as for
a request signed before the machine slept, retries at once as
`provider_signature_expired`, signed again with the time now.
Bedrock URLs must be https; the key id, secret and session token are
redacted from tool output and, from the environment, kept out of shells, as
are the `AWS_PROFILE`, `AWS_DEFAULT_PROFILE`, `AWS_REGION` and
`AWS_DEFAULT_REGION` that picked them: a bot's AWS commands choose their
own. Mantle takes
the body unsigned; runtime signs its SHA-256, so a runtime call reads its
history from the store twice, once to digest it and once as it streams. A key field
instead sends a Bedrock API key the family's own way, as
`--provider b=anthropic,https://bedrock-mantle.us-east-1.api.aws/anthropic/v1,AWS_BEARER_TOKEN_BEDROCK`;
short-term API keys last at most twelve hours and are read once. Bedrock has no
WebSocket transport, so `responses-ws` is refused for it. Bedrock publishes no
rate-limit headers, so its pools learn no allowance and are paced by refusals
alone: a 429 naming no delay closes the pool for a second, doubling to 32 while
refusals continue, until a call's stream completes.

`--provider chatgpt` spends a ChatGPT plan instead of an API key, through
OpenAI's Sign in with ChatGPT for open-source tools
([developers.openai.com/siwc](https://developers.openai.com/siwc/token-sharing-open-source),
read 2026-10-20; a preview). `agent login chatgpt` signs in, in the CLI's
own process: it registers a client for the user and workspace on the first
sign-in (`client_id=dynamic_agent_client`, PKCE, a `127.0.0.1` callback,
default port 1455), checks the ID token against OpenAI's keys and the plan
scope `chatgpt.tokens.use.direct`, and writes `~/.agent/chatgpt.json`
(`AGENT_CHATGPT_LOGIN` names another file) with mode 0600. Later sign-ins
reuse the issued client and must return the same account (`sub`), else
`login_account_changed`; a first sign-in without the plan scope is still
saved, so the next one reuses its client. The printed address leaves out
the `id_token_hint` the browser is given. Over SSH the browser is not opened
and the callback port has to be forwarded. A per-machine host id is kept beside it in
`host-id`, so a login copied to another machine (OpenAI's documented way to
use a VM) does not carry this machine's id. Delete the login file to
register anew.

The daemon reads that file at startup and sends the access token as the
bearer to `https://api.openai.com/v1/responses`; OpenAI says to use this
endpoint, not ChatGPT's `backend-api`. Access tokens last an hour and refresh
tokens are single-use, so the daemon is the one refresher: two minutes before
expiry, after pacing and admission but before sending, it refreshes under a
lock (in-process, and `flock` on `chatgpt.json.lock` across daemons sharing
the file) and writes the rotated pair back before using it. A file holding
another token than the one in memory (a new sign-in, another daemon's
refresh) is taken as it is. A 401 for the held token refreshes it once and
retries without backoff (`provider_login_refreshed`); a 401 moments after a
refresh ends the turn with `provider_login_rejected`, naming the file. A
refused refresh answers `provider_login_expired`, naming the file and
`agent login chatgpt` (for `invalid_client`, to delete the file and register
again); a 5xx or 429 from the token endpoint keeps the token
set and is retried with backoff (a 429 as `provider_login_throttled`). A
sign-in takes the same lock before writing. Every token in the file is
redacted from tool output. Requests on the plan group the bot's tools in one namespace, `agent`, since
the route takes function tools only grouped or as `additional_tools`; a call
comes back naming it and is replayed as it came. They carry no
`max_output_tokens`, which it refuses,
and its usage limit (`subscription_sharing_usage_limit_exceeded`, a 429 or a
failed stream) ends the turn as `provider_quota_exhausted` rather than being
paced and retried; a stream that fails with
`subscription_sharing_usage_unavailable` or `_user_unavailable` is retried
with backoff as `provider_plan_unavailable`. Not yet measured on a real plan: prompt-cache hit rates
at this endpoint, `responses-ws`, and which request fields it accepts beyond
those OpenAI lists.
A new sign-in needs no restart: the daemon takes the file's token set at its
next refresh or 401. A missing, unreadable or plan-less file fails startup
with `provider_login_unavailable`. The login goes only to that
endpoint: a `chatgpt` spec naming another URL authenticates with the key
variable it names, or with nothing, so a local or synthetic endpoint never
receives the real token.

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
`effort` (`low`, `medium`, `high`, `xhigh`, and on Anthropic `max`), and a
fork inherits its source's binding.
A turn may override the model within the same family (`submit` with `model`, or
`run --model` on an existing bot); a different family is rejected with
`provider_family_mismatch`. A turn may likewise run at another effort level
(`submit` with `effort`, or `run --effort` on an existing bot); a level
the family does not take is `invalid_effort`. Each override is recorded
on the turn and in its `accepted` event, and the bot's default is unchanged. A
steer that names a level joins only a running turn at that level. Cross-family handoff of a
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
changed. A signature is also only valid for the model that wrote it, on the
endpoint that holds its key. Anthropic requests therefore send a block only
while the context in front of the window, the model, and the endpoint are the
ones it was written under. Each request fingerprints all three (the encoded
pinned prefix, the window's first item, the provider's URL, and the model);
when the fingerprint differs from the bot's last, the window slid, a
compaction or note landed, a turn or fork runs on another model, or the
provider name now points at another endpoint, and
every node written before that request is sent without its thinking from
then on. Other harnesses do the same by recording the producer on each
message (pi-mono d5b3de2 `transform-messages.ts`, opencode 1743d1a
`to-llm-message.ts`); here every node above the bot's strip was written by
requests under the current fingerprint, so the bot's record says it once. Removing a leading run of blocks is allowed; later blocks keep
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
in the tests refuses mismatches outright, as `error` would.

A provider can also fail to read a replayed block back with nothing in front
of it changed. On 2026-10-11, Opus 5.5 and Fable 5.1 on Bedrock
answered such requests with server errors about restoring the reasoning (the
exact text was not kept), often in a sub-agent's long tool-using turn. The
same body fails every time, so retries never cleared it and every later turn
of that bot repeated it; switching models only helped because a block from
another model is dropped unread.

The runtime reacts only to error types the providers publish, never to
message text. Neither Anthropic nor Bedrock publishes one for unreadable
thinking: a replayed block whose signature is altered or cannot be decrypted
is documented to fail with a plain 400 `invalid_request_error`, and a 400 is
final for its body. So after a 400 on a request that replays thinking, the
call sends it once more without that thinking (its `retry` event carries
`thinking_withheld: true`, within the normal retry and token budgets). Only
if that request is accepted is the strip recorded on the bot as a binding
change, so later requests leave those blocks out and keep the thinking
written after; a 400 the thinking did not cause fails the second time too
and changes nothing. The cost is one extra request on such a 400, and on a
recovery the withheld reasoning and a cache miss from the first stripped
node, once. Server errors, including an in-stream `api_error`, are retried
with the body unchanged, so the 2026-10-11 failures, if they recur in that
form, are not recovered: a bot switches model to get past them. The error
text, if it is seen again, is worth keeping. Why Bedrock could not read the
blocks is not known; global cross-Region profiles (#99) are an unverified
suspect.

Empty instructions omit the system block, since empty text
cannot carry an Anthropic cache breakpoint; automatic caching remains enabled.

### Keeping the Anthropic cache warm

Anthropic's default prompt cache lives five minutes from its last read, and a
foreground tool call may run ten. On 2026-09-25 a Sonnet 5 bot idle for eight
minutes between two calls read only its 2.3k-token system and tools prefix
from cache; the 6.8k-token conversation was written again at 1.25 times the
input rate. The one-hour cache read all 9.0k, but it bills every write at
twice the input rate, idle or not. The daemon keeps the five-minute cache and
refreshes it instead. While a turn's call streams its reply or a tool runs,
once that call's cache has gone the bot's `keep_warm` seconds unread (240
by default), it sends that call's request again with `max_tokens: 0` and
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
a call. A refused
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
but bills every write at twice the input rate, is not measured; a bot's
`cache_ttl` of `1h` (`--cache-ttl 1h`) selects it for that comparison. Its writes are recorded apart, from
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

A bot created with `--fallback PROVIDER/MODEL` (`create.fallback`, a known
provider's model in the bot's family, inherited by its forks) sends a request
a safety classifier declined (`stop_reason: "refusal"`) to that model, on the
same body, and only that request: the next goes to the bot's own model again,
with the fallback's answer in its history. Other bots end the turn with
`provider_refusal` as before, and so does a refusal the fallback repeats. It
is a bot's choice because a fallback answers with a model the caller did not
pick and bills at that model's rate. A client may name one for each model:
the CLI and Sonny give a new bot that names none the person's entry for its
model from `[fallbacks]` in `~/.agent/config.toml`.

The retry is published as a `retry` event with the refusal as its error and
`fallback` naming the model, and the answer as a non-durable `model_fallback`
event with `from` and `to`; its `usage` event names the fallback's provider
and model in `models`, so it is priced at that model's rates. On the Claude
API, when the fallback is on the bot's own provider, the retry redeems the
refusal's `stop_details.fallback_credit_token` (the
`fallback-credit-2026-07-01` beta), so the fallback's cache writes are priced
as if it had run there all along; a 400 to the credit sends the same retry
without it. Bedrock requests send no credit: the beta is documented there, but
Mantle answered a beta it did not take with a 400
([BEDROCK.md](BEDROCK.md)), and this one has not been tried there.

Anthropic's server-side fallback (`fallbacks: "default"`) is not asked for:
its sticky routing sends the same conversation to the fallback model for about
an hour, where this one goes back to the bot's model at once. Its
`server-side-fallback-2026-07-01` beta is still sent on the Claude API, so a
history that holds an answer it served replays: a `fallback` content block
marks each switch. Blocks before the last one are a declined attempt's partial
output: its non-empty text is kept as part of the answer, while its thinking
and tool calls are neither stored, replayed, nor run. The marker is stored in
place, since the API checks the thinking around it by its position, and the
response's per-attempt `usage.iterations`, when present, replace its top-level
usage.

The protocol described here is from Anthropic's documentation, read
2026-10-29: [Refusals and fallback](https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback)
and [Handle streaming refusals](https://platform.claude.com/docs/en/test-and-evaluate/strengthen-guardrails/handle-streaming-refusals),
which lists Bedrock. A refusal may follow partial output, which is discarded;
its `stop_details` names a `category` (kept in the error's detail and facts)
and may name a recommended model and a credit. The fallback's thinking blocks
are dropped by the API when the bot's model reads them back
(`input_transformations` reason `model_binding_mismatch`), which is not
counted as a binding drop; only `prefix_binding_mismatch` is. A live refusal
was not reproduced, so the fallback path is covered by parser and
synthetic-endpoint tests.

A turn that ends with `provider_refusal` leaves what the declined request
added since the model last answered (the prompt, when it was the turn's first
request, or the latest tool results and any steer) out of later requests, as
Anthropic's [streaming refusals](https://platform.claude.com/docs/en/test-and-evaluate/strengthen-guardrails/handle-streaming-refusals)
guidance says to: "you must reset the conversation context before
continuing... Attempting to continue without resetting will result in
continued refusals." Each item becomes a note naming the refusal's category,
so the model knows something was declined: a tool result keeps its call with
`[This tool result is left out: ...]`, and a message becomes `[A message here
is left out: ...]`. A note is padded with spaces to the size of the item it
replaces, so the context's byte accounting does not change; a shorter wording
is used where the full one does not fit, and an item too small for any keeps
its own. Everything before it is unchanged, so no thinking binding or prompt
cache is touched. The originals stay in the `declined` table (schema 61):
turn listings and the history reads (`history_items`, `history_read`,
`history_search`, `result`) show them, and a retry with the same request id
still matches the person's prompt. A node another bot also reaches, as a fork taken while the
request was out does, is that bot's history too: it and what came before it
stay as they are.

`effort` (`low`, `medium`, `high`, `xhigh`, `max`) maps to Responses
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
they name, and a bot's `max_output_tokens` replaces the limit when set. Known legacy Claude ids (Haiku 4.5, 4.5 and
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
(a full 128,000-token answer, under the 1 MiB event cap; tool call ids and
names count toward it), 64 KiB JSON-encoded
tool call id without NUL, unique within its turn (a longer one, one with a
NUL, or one an earlier call of the response or turn used fails the response
with `invalid_tool_call_id`, so every gated call can be listed and answered
by a command naming it alone), 2 MiB SSE frame,
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

Each session's output channel is bounded by bytes, as NATS and Redis bound a
slow consumer: at most 2 MiB queued or being written, each packet counting
64 bytes besides its own, and each packet capped at 1 MiB. A burst of many
small events fits as well as a few large ones. The stdio owner session
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
requests from all sessions together, each session's in its own queue, and
the service takes the sessions with one queued in turn, one request each, as
Redis and NATS serve their clients: a client that pipelines thousands of
requests holds every other session up by one of them, not by all it sent
first. Each session reads at most 8 requests ahead, so a few busy sessions
never fill the queue for the rest. What one waiting
for an [observation read](#observation-reads) has queued stops counting
against the queue, and so does what it reads, up to its 8, while it waits:
any number of sessions waiting on reads leave the queue to the others, and
each still reaches the lease renewals it sends behind its requests. A session's own
requests keep their order. Storage queues at most 32
operations.

### Observation reads

Reads that decide nothing run beside the service loop, not in it:
`history_nodes`, `history_items`, `history_search`, `turns`, `bots`,
`events`, `usage` and `prompts`. Each runs on one of the store's reader
connections (`--max-readers`, one per logical CPU by default; WAL readers
do not block each other or the writer). They take at most all but one of
the connections, the rest waiting in the order they came, so a turn's own
reads (its context, a compaction plan) always find one free, as Kubernetes'
API Priority and Fairness keeps seats for each priority level. Meanwhile the loop serves every other
session, so one client's deep history read or search across every bot
holds up no other client's submit, interrupt or pause. The session that
asked waits for its read's answer before its next request is taken, so its
own requests keep their order and each session has at most one read
running or waiting. At shutdown, reads still running get a few seconds to
answer. A read whose task fails answers `observation_failed`. HTTP client internal buffers,
active histories, allocator retention, and filesystem cache remain part of the
memory accounting.

## Software protocol

The service speaks JSONL. `agent serve` without `--socket` serves the process
that owns its stdio and exits when stdin closes; that session receives every
event without subscribing (the benchmark and lifecycle tools use it). With
`--socket PATH` it is a daemon: it prints `ready` once on stdout, accepts any
number of Unix-socket sessions, and runs until `shutdown`, SIGTERM, or SIGINT.
Each socket session begins with a `ready` line and must `follow` the bots it
wants to observe. `ready` carries the protocol version, and the CLI, the app
and `agent_client` refuse a daemon of any other with
`daemon_protocol_mismatch`: the daemon and its clients ship together, and
none keeps an earlier protocol working. `agent start` and `agent run` never
start a second daemon for the same socket: they refuse a newer one, and
replace an older one once its running turns end (docs/CLI.md).

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
cancelled like any others at shutdown. `agent shutdown --grace DURATION` sends
it and waits that much longer than 30 seconds.

Protocol 24 also adds the pull cord: `pause` stops every agent until
`resume`, and holds across restarts, since the store keeps it (schema 64;
since schema 72 the `pauses` table keeps every pause, held or lifted). `{"op":"pause","reason":TEXT,"by":NAME}` needs a
reason of 1 to 4096 bytes. `by` is who pulled it, 1 to 128 bytes, or the
request answers `invalid_by`; on a bot's own socket it is that bot, whatever
the request says. It parks every running turn at once,
as a closed pool parks it (`turn_paced` with `paused: true`; status `paced`),
to continue on `resume`. The reply it was writing is dropped and asked for
again then. Each call the turn had not answered gets a result, `{error:
"daemon_paused", detail, pause}` (`pause` is `{reason, by, at_ms}`), saying the pause stopped it before its result
was recorded (it may have had effects) or came before it ran, and none is run
again: the model decides what to redo, as Codex answers the calls an interrupt
cut off. The pause ends every background command (whose handle then answers
`daemon_paused`, saying the same), terminal, and detached command this
daemon started (the process group each leads, with what it left running
there after it exited, and on Linux a sandboxed one's whole pid namespace;
one started before a restart, or an unsandboxed process that left that
group, is no longer the daemon's to find), and from
then no turn starts: a `reject`
submission answers `daemon_paused`, `queue` and `steer` submissions are
recorded and wait, triggers and notes with them, and parked and paced turns
stay parked. An `attach` is taken as usual. The answer is `{pause, changed,
parked}`, where `pause` is `{id, reason, by, at_ms, parked}` and the
reply's `parked` counts the running turns it parked. The pause's own `parked`
is `{bots, total}`: the agents whose running turns it parked, the first 64 by
name, and how many in all, so whoever looks can stop one before letting the
line go on. While a pause holds,
another changes nothing and answers the first with `changed: false`.
`{"op":"resume","by":NAME?}` lifts it and answers `{pause, by, changed}` with
the pause it lifted, or null when none held, and who lifted it (`by`, 1 to
128 bytes, as `pause` takes it). A lifted pause adds `resumed_by` and
`resumed_ms`; what waited,
and the turns the pause parked, start as capacity allows. Time parked by a
pause is not counted in `paced_ms`. Protocol 33 parks running turns on a
pull; before it, they ended `interrupted`. Protocol 34 keeps every pause:
`{"op":"pauses","after":ID?,"before":ID?,"limit":N?,"newest":BOOL?}` reads
them, held and lifted, oldest first (`newest` turns it), at most `limit` (1
to 256, default 64, and no more than half a reply's bytes) after the pause
`after` and before `before`, answering `{pauses, more}`; a bad page is `invalid_pause_page`. `agent pauses` prints
them. A store migrated from schema 71 keeps the pause that held, with no
agents named as parked. A
sandboxed bot's own socket refuses it with `not_permitted`, so a command
left running in a sandbox cannot let the line go on. Each
change is told to every session as a `paused` or `resumed` event, `{event,
pause, durable: false}`, a `resumed` one with `by` too. `ready` and `stats`
carry `paused`, the pause or null. `agent pause --reason TEXT` and `agent
resume` send `by` as `AGENT_BOT` when run in a bot's shell.

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
A refusal a program acts on also carries its facts as fields beside them:
`bot_busy` from `submit` reports `running_turn` and `fork_point`, and
`invalid_effort` lists the model's `efforts`, and `invalid_speed` the
provider's `speeds`. A line that is not JSON
is `invalid_json` with a null `id`; JSON of the wrong shape is
`invalid_request`, answered with its `id` when that parses, and its detail
names the unknown or ill-typed field.
Notifications carry `event`; durable ones carry `cursor`, `bot`, `turn`, and
`data`, in exactly the shape `events` replays them, and `written_ms`, the Unix
ms the event joined the store, set by the insert that writes it and never
earlier than the event before it (should the clock go back, events keep the
last time until it catches up, as a log's time index does); events from
before schema 69 have none. Durable events reach
followers in commit order: the storage worker itself hands each group's
committed events, and the outcomes of turns its jobs ended, to one publisher
before taking the next group. Tasks never publish durable events, so a
follower's cursors only rise, its greatest cursor is a complete resume
point, and a committed batch is delivered whether or not the task that
asked for it was cancelled meanwhile. Wait answers follow the terminal event
they report. Live `text_delta` and `thinking_delta` notifications keep their
own path from the turn. Example requests:

```json
{"id":1,"op":"create","bot":"Bob","workspace":"/workspaces/project","model":"anthropic/claude-sonnet-5","effort":"low","instructions":"...","tools":["shell","read","write","edit","wait","history"],"compaction_instructions":"...","created_by":"Alice","created_by_id":42}
{"id":2,"op":"submit","bot":"Bob","request_id":"work-1","prompt":"Hello","workspace":"/workspaces/project-copy","model":"anthropic/claude-opus-5-5"}
{"id":19,"op":"submit","bot":"Bob","request_id":"work-2","prompt":"Also check the docs","delivery":"steer"}
{"id":3,"op":"follow","bot":"Bob","after":0}
{"id":4,"op":"inspect","bot":"Bob"}
{"id":24,"op":"configure","bot":"Bob","bot_id":1,"instructions":"...","model":"anthropic/claude-opus-5-5","effort":"high"}
{"id":5,"op":"events","bot":"Bob","after":0,"limit":100}
{"id":20,"op":"history_nodes","bot":"Alternative","from":2,"limit":400}
{"id":6,"op":"history_items","bot":"Bob","nodes":[2]}
{"id":31,"op":"history_search","bot":"*","query":"checkpoint","before":null}
{"id":13,"op":"artifact","bot":"Bob","turn":1,"call_id":"call_1","stream":"stdout","offset":0,"limit":65536}
{"id":8,"op":"fork","source":"Bob","checkpoint":2,"bot":"Alternative","request_id":"alt-1"}
{"id":28,"op":"fork","source":"Bob","bot":"Bob.side","allow":[],"expire_ms":900000,"request_id":"side-1"}
{"id":9,"op":"interrupt","bot":"Bob","turn":1}
{"id":11,"op":"bots","after":null,"limit":64}
{"id":22,"op":"bots","after":null,"limit":64,"name":"project.*","active":true}
{"id":23,"op":"turns","bot":"Bob","newest":true,"limit":3}
{"id":30,"op":"usage","name":"project.*","since_ms":1793000000000,"bucket_ms":21600000,"limit":64}
{"id":14,"op":"prune","bot":"Bob","keep_turns":8}
{"id":29,"op":"compact","bot":"Bob","bot_id":1}
{"id":15,"op":"delete","bot":"Bob","bot_id":1}
{"id":16,"op":"follow","bot":"*","after":0}
{"id":32,"op":"follow","bot":"*","since_ms":1793000000000,"kinds":["accepted","message"]}
{"id":17,"op":"wait","handles":["turn:Bob/1","turn:Alice/3"],"any":true,"timeout_ms":60000}
{"id":18,"op":"stats"}
{"id":19,"op":"provider_models"}
{"id":21,"op":"create","bot":"Carol","workspace":"/workspaces/project","model":"openai/gpt-6-luna","instructions":"...","tools":["shell","read","write","edit","wait","history"],"approve":["shell","write","edit","read"],"approver":"manual","settings":{"context_bytes":1048576,"keep_turns":16}}
{"id":22,"op":"approvals","bot":"Carol","limit":64}
{"id":23,"op":"answer","bot":"Carol","turn":7,"call_id":"call_1","request":1,"decision":"deny","reason":"not on main","by":"cli"}
{"id":24,"op":"serve_approvals","tag":"auto","lease_ms":5000,"limit":64}
{"id":25,"op":"renew_approvals","tag":"auto","lease":1790460421345}
{"id":26,"op":"answer","bot":"Carol","turn":7,"call_id":"call_2","request":1,"tag":"auto","decision":"allow","by":"approver","lease":1790460421345}
{"id":27,"op":"submit","bot":"Dave","request_id":"child-1","prompt":"Summarize the build log","from":{"bot":"Carol","turn":7}}
{"id":28,"op":"prompts","bot":"Carol","turn":7,"bytes":65536}
{"id":12,"op":"shutdown"}
{"id":21,"op":"shutdown","grace_ms":30000}
```

`history_search` is the history tool's query for clients: literal text, ASCII
case ignored, in `bot`'s turns, or with a GLOB of `*`, `?` and `[...]` in every
matching bot's, newest first. It answers as the tool does, `{bots or bot, query,
matches: [{bot, turn, handle, type, text}], done, next_before}`, up to 20 matches;
pass `next_before` as `before` for older ones. Like `usage` it reads off the
dispatch path. Sonny's commons uses it to find a bot whose history touches a new
thread.

`history_nodes` lists immutable node references in a bot's lineage, newest first,
including inherited fork history. `from` is an inclusive node ID and defaults to
the current head. The response contains `nodes: [{node: ID, turn: TURN_ID}, ...]` and
`next_from`, the next older node or null at the root. `limit` defaults to 400
and must be 1 through 400. Bodies are fetched through `history_items`; pagination does
not copy bodies. `min_node` optionally bounds the oldest included ID.
`oldest_first: true` selects the oldest page within that range while still
returning its nodes newest first; `next_newer` is the inclusive minimum ID for
the next forward page, or null. This lets clients retain only range endpoints.
Turn IDs are inherited from each node's nearest turn-start ancestor.
Each node, here and in `history_items`, carries `written_ms`, when it joined
the history (a queued prompt when its turn started, a steer when its turn
absorbed it), set by the insert that writes it; nodes from before schema 67
have none.
`workspaces` lists each folder the page's turns ran in once, as
`{"folder", "turns"}`, so a client resolves a path a message names where it was
written even after the bot moved; a turn whose rows went with a deleted fork
source is absent. One query reads them for the whole page.
Membership validation walks the lineage, as `history_items` does. Both operations run
on the read worker in a consistent snapshot, avoiding the serialized writer.
The fork can read its shared prefix after its source is deleted.

`history_items` accepts `bot` and 1–400 distinct `nodes`. It validates all IDs
against that bot's lineage with one ancestry walk and returns a prefix as
`items: [{node: ID, item: VALUE}, ...]` in request order. A prompt a bot's turn
wrote also carries `from: {bot, turn, bot_id}`, with `bot_id` the identity the bot's name
held when it wrote it, and one a client sent with an `origin` carries that, so a
client can say who each message came from without reading every turn. A prompt
sent with `delivery: "attach"` that a call took in also carries `attached: true`:
it rode along on that call rather than reaching the bot as a message of its own,
so a client can draw it apart from steers. The store
keeps these with the prompt's node (`senders`), so they last as long as the item:
after the turn's rows go with a deleted fork source, and after its sender is
deleted or its name reused. The reply targets
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
  `agent follow --all [--after N]`. With `kinds`, a list of one or more
  event kinds (`invalid_follow_kinds` otherwise), the follow receives only
  those, in replay and live; `follow_live` and `pruned` still come. A
  controller that never reads streamed text leaves the deltas, most of all
  events, out of its session's queue. New identities publish their `created` or
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
- `provider_models` asks each provider for its own model listing (`GET
  .../models` with the provider's credentials, four providers at a time,
  10 seconds each and 15 for the whole request) and
  answers `providers.NAME.models` as `{id, name?, context_tokens?,
  output_tokens?}`, or `providers.NAME.error` for a provider that would not
  list. It runs off the dispatch path, only when asked, under the same
  connection admission as a turn, and each provider keeps its answer, a
  refusal included, five minutes, while the listings (kept as their JSON
  text) and refusals kept across all providers stay within 2 MiB (past that
  an answer is given, not kept). A refusal for missing, expired or rejected
  credentials (a 401 or 403 included) is not kept, so a login made meanwhile
  lists on the next ask.
  The reply fits one event: a listing past
  what is left of it is `provider_models_limit` for that provider. Bedrock
  Mantle is asked at the host's `/v1/models`, and each binding offers only
  its own family (`anthropic.` models on the Messages route, the rest on
  Responses, except gpt-oss, which Mantle lists but refuses on Responses). OpenAI's own listing leaves out speech, embedding, image,
  moderation and search models by base model name (a fine-tune by the model
  it was tuned from). Explicit output modalities must include text when
  the provider supplies them; unknown custom model names are kept. A
  ChatGPT plan login asks `https://api.openai.com/v1/models` with its
  bearer and keeps `models[]` entries listed as `visibility: "list"`, by
  `slug`. Nothing else reads it: a turn runs whatever model it names.
  `agent models --discover` uses it once to write `~/.agent/models`.
- `stats` returns the daemon's live state without sampling its process from
  outside: open sessions, active turns against the bound, parked turns,
  running processes against the process bound and how many of them are
  still in line for a slot (`queued_processes`), turns queued or ready to
  start (`queued_turns`) with their prompt and quoted bytes (`pending_bytes`) and the
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
omit instructions; `inspect` returns the full individual bot record. With `bot_id`,
a bot that no longer holds that identity is `bot_not_found`, as on `submit`. Concurrent
creations before an already-consumed cursor require restarting the listing.
`agent ls` reads pages incrementally while preserving its JSON-array output.
Protocol version 14 adds `configure`, which changes what a bot's client
chose for it when it was made: its `instructions` (up to 64 KiB, as on
`create`), and its `model` with an optional `effort` (absent is the model's
default). A bot's instructions are otherwise fixed when it is made, so a
client recomposes them (skills, memory, folder rules added since) and sends
them here. A model must be of the bot's family, as a turn's own model must,
since its history is kept in that family's encoding
(`provider_family_mismatch`). A gate (`approve`, `approver` and optionally
`approve_expire_ms`, as on `create`) replaces the bot's gates, and
`approve: []` alone removes them, including the gates a bot took from the
bot that made it: whoever configures a bot decides, as a person overriding
one does, and bots it already made keep theirs. The gate is checked before
anything is written. `sandbox`, with `writable`, `network`, `unsandboxed` and
`unix_sockets` (as `create`'s settings name them), replaces the bot's sandbox,
and those left out go back to none, off, none and none; a bot another bot made keeps no weaker a sandbox than its
creator's, as when it was made, and a bot's own socket may not configure
at all. `cache_ttl` (`5m` or `1h`, as `create`'s settings name it) sets the
bot's prompt-cache lifetime from its next model call, and `speed`
(`standard`, `fast` or `ultrafast`) its speed tier, refused `invalid_speed`
where the provider it is on, or moves to with `model`, does not serve it. It answers `{bot, bot_id, changed}`, `changed` naming the fields
(`model`, `instructions`, `gates`, `declared`, `sandbox`, `cache_ttl`, `speed`) that differed; the same request again
changes nothing, so a resend is harmless. A change is told to the bot's
followers as a live `configured` event with the same fields, which is not
kept, so a client that attaches later reads the bot as it is. It is refused with `bot_busy`
while a turn runs or waits parked, and the next turn sends the
new instructions: the provider's cache of the old ones is not read again,
replayed thinking written after them is withheld as when any context in
front of it changes, and a fork keys its own prompt cache from then on.
Protocol version 15 adds `"decision":"ask"` to `answer`: a gate's holder
hands the call to a person instead of deciding it. The gate stays open and
the call keeps waiting; the request's row keeps one `{tag, reason, by,
at_ms}` per gate that asked, which `approvals` and served listings show as
`asked` while that gate is open, and the bot's followers get an `approval_asked` event with the call,
request, tag, reason and `by`. A person then answers it like any other
call, without a lease. Only a gate with no expiry can wait on a person: on
one with `approve_expire_ms` the ask is refused with
`approval_ask_expiring`, and the holder decides instead. A call announced
again is asked afresh.
Protocol version 12 lets `bots` filter on the daemon side: `name` keeps the
bots whose name matches a SQLite GLOB pattern (1 to 256 bytes; case-sensitive
like names), read from the name index's range for the pattern's literal
prefix, and `active: true` keeps those with a turn running, read from a partial
index of only them (`bots_running`). Filtered pages page and bound like any
other. `bots` and `turns` run on the store's reader connections, beside the
service loop ([observation reads](#observation-reads)), so a listing never
waits behind, or holds up, the writer or another client.

Protocol 23 adds `usage`, what bots' turns spent, also a read on the reader
connection: one grouped statement over `turns`, so one snapshot. `name` keeps
the bots a `bots` GLOB would (absent is every bot), and `since_ms` the turns
started at or after it (absent or 0 is all of them); a turn counts wholly where
it started, compaction and keep-warm calls included, since they are recorded on
it. The answer has the totals (`turns`, `input_tokens`, `cached_input_tokens`,
`output_tokens`; cached is part of input), `bots` with the `limit` (1 to 256,
default 64) that spent most input plus output first and `other_bots` summing
the rest with how many (`null` when none), and `models`, the turn's model as
`turns` reports it: a different compaction model's or an Anthropic fallback's
tokens count under the turn's model, since only the usage events keep that
split and events are pruned, and `origins`, by the `origin` a client gave
the turn at `submit` (`null` for none), so a client can tell what its own
features cost. With `bucket_ms` it adds `series`, one row for each bucket
of that width from `since_ms` that has turns, at its `at_ms`; it needs a
`since_ms` and fewer than 1024 buckets to now (`invalid_usage_window`). Usage
is read from `turns`, so a deleted bot's turns, and the judge's own copies once
deleted, no longer count. Its cost grows with the turns it matches; like every
observation read it is answered beside the service loop, and other sessions'
requests go on while it reads. As an observation, not
a screen: the same statement in Python's SQLite over a synthetic store of 164k
turns took about 0.3 s for every bot's month and 0.07 s for one project's day.
Sonny's `/usage` draws it, reading again once a minute while it is open.

Protocol 24 lets a bot declare up to 32 tools, from 16, under the same 64 KiB
of declarations: a Sonny lead with MCP servers already declared 15 of Sonny's
and `exec`, so one more of Sonny's, the commons, or one of a client's own,
went past the bound. The count is a sanity bound; the bytes are the real one.

Replay tasks are owned by their subscriptions and tracked by the service.
Replacing a follow on the same bot/session and session closure
cancel the old replay. Notifications already queued can precede the operation's
reply, but no old replay events follow its acknowledgment. Shutdown cancels and
drains remaining replay tasks before joining the stdout worker. Socket paths have independent ownership locks. Live sockets,
symlinks, and unrelated files are never removed on startup; a refused stale
socket can be recovered. Normal shutdown removes only the socket inode the
service created. Do not replace ownership-lock files while a service runs.

`follow` takes the cursor as `after`, or as `since_ms` (a Unix ms; giving
both is `invalid_event_page`): it then starts from the first event written at
or after that time, so a client can replay the last hour without knowing its
cursors. Cursors and times rise together, so the daemon finds that cursor by
halving the cursor range, a few point reads however many events are kept;
events with no time count as older. It is the `usage` op's word for a start
time, and `agent follow --since 1h` sends it.
`follow` replays durable events after the cursor as notifications, emits
`follow_live` with the cursor reached, and then delivers live events. The
switch happens inside a storage job, so no committed event falls between the
last replayed page and the first live delivery, and replayed cursors are never
repeated live. Non-durable `text_delta` and `thinking_delta` notifications are
delivered only to live followers and carry `durable:false`, and an `item`
naming the output item they stream, a number that grows through the turn:
deltas with the same `item` are one message (or one block of thinking) as
its `message` event keeps it, and a larger `item` starts the next, as Codex
streams a response's items apart. A call retried or resumed after a park
streams its items again under the same numbers. The `pruned`
and `deleted` retention notices carry `durable:false` too. Durable event kinds
are `created`, `forked`, `queued` (a submission waiting its turn), `accepted`
(a turn starting, with its prompt's node), `message`, `usage`, `tool_started`
(with a 2 KiB argument preview, and `approvals` and the planning `node`
for a gated call), `tool_completed` (with retained artifact names; `failed`
for a call that failed, `denied` for a denied one, which also carries
the refused call's `name` and argument preview as `tool_started` would),
`approval_requested` (a round's gated calls, see [tool approval](#tool-approval)),
`turn_waiting` and `turn_resumed` (a parked turn's handles, or the call whose
verdict it waits for, and its wake-up), `turn_paced` (a turn parked at its model-call boundary because its
provider's pool is closed by a rate limit, with `resume_at_ms`, or because the
pull cord stopped it, with `resume_at_ms` null and `paused: true`; it resumes
through `turn_resumed` like a parked wait), `steered` (a steer's item joining
the running turn: `steer` names the steer's turn, `node` its item, `from`
or `origin` its sender as on `accepted`, and `attached: true` when it was sent
with `delivery: "attach"`), and
`turn_finished` (with status, checkpoint, error code, and detail; a steered
turn's carries `into` and `node`). A `submit` response includes the turn's
`handle`, `turn:BOT/N`, and its `status`.

`artifact` reads one retained stream a page at a time: a `stream` from
`tool_completed` (required), a UTF-8 byte `offset` (default 0), and a byte
`limit` (4 through 65,536, default 65,536). The response contains `text`,
`next_offset`, `total_bytes`, and `done`. Continue at the returned offset until
done. Pages never split a UTF-8 character; an offset inside a character is
rejected. Only the selected page is returned from SQLite to Rust for the
response, so any artifact stays retrievable within the 1 MiB response bound.

`created_by` and `created_by_id` on `create` and `fork` declare the bot
on whose behalf the client acts. Supply both or neither. The CLI captures them
from `AGENT_BOT` and `AGENT_BOT_ID`, exported in every shell tool environment.
The store validates the pair in the child creation transaction and rejects a
missing, deleted, deleting, or replaced creator, so a surviving shell cannot
attribute a new child to a replacement bot after restart. The daemon also sets
`AGENT_PARENT` and `AGENT_PARENT_ID` to the running bot's recorded creator,
and `AGENT_TURN` to the turn the shell runs in.
The client preamble uses both with `run --bot NAME --bot-id ID`, preventing
stale child-to-parent submissions after name reuse. The record, the `created` and `forked` events, and `bots` pages
carry both; the two events also carry the record's list fields (`id`,
`provider`, `model`, `workspace`, `status`, `running_turn`), so a follower
seats a new bot without a request per creation. Bots remain peers: the field is lineage for people and
clients, never authority.

`from: {bot, turn}` on `submit` says a bot's model wrote the prompt, in
that turn; a prompt without it is a person's. The CLI sends it from
`AGENT_BOT` and `AGENT_TURN`, and refuses a shell environment that has one
without the other (`author_turn_required`). The store checks that the turn
is the bot's (`invalid_from`), keeps it on the new turn's row, a steer's
included, counts it in the request's idempotency, and reports it on
`accepted`, `queued` and `steered`, with the sender's identity as `from.bot_id`. Like the
creator, it is declared, not verified. A client that sends a prompt on its own,
not from a bot's turn, may name itself with `origin` (a name's characters,
else `invalid_origin`), mutually exclusive with `from`, stored and reported the same way; the daemon gives it
no meaning, but a prompt with an `origin` is no person's word to the
approver. The sender's identity is resolved at submit and kept on the turn
(`turns.from_id`), so a queued message whose sender is deleted before it
starts still names it. The model reads the sender too, as a text part after
the prompt's words and before its pictures, so it does not take another
agent's words for a person's: `[Written by the agent NAME in
turn:NAME/TURN, not by a person.]` for `from`, `[Sent by ORIGIN.]` for
`origin`, and nothing for a person's prompt. `content[0]` stays the words.
A bot made with `hide_senders` reads the words alone.

A client that speaks for a person other than the daemon's own may name
them with `person` (a name's characters, else `invalid_person`), exclusive
with `from` and `origin`. Like `origin` it is the client's word, kept and
reported the same way, and read by the model as `[Written by PERSON.]`; a
prompt with a `person` is no consent to the approver, which answers to the
daemon's own person only. A command run from a bot's sandboxed tool cannot
send one (it would speak as a person from inside an agent).

`quotes`, a list of `{bot, turn, reply?}`, copies other turns'
words into the prompt: a turn's prompt, or with `reply: true` the last
message the finished turn ended with. The daemon copies them at submit, so
what a quote says is what that turn holds, never what the sender retyped,
and keeps who first wrote them: a quoted prompt's own quotes come along
with their writers. Each quote is a text part after the sender's line,
behind a line naming it, `[Quoted from turn:BOT/TURN, written by a
person:]` or `[Quoted from the reply of turn:BOT/TURN, written by the agent
BOT:]` (`sent by ORIGIN`, `written by PERSON`, `written by the agent NAME
in turn:NAME/TURN`); `hide_senders` leaves the lines out too. A turn that
does not exist, a steer's reply, or a turn that ended without a reply
(or whose events were pruned before schema 70) is `quote_not_found`; a
reply of an unfinished turn `quote_unfinished`; more than 64 quotes, those
quoted prompts bring counted (marked `nested` in the record, which a retry
leaves out), or words and quotes past 256 KiB together, each quote's line
counted at 256 bytes, `quote_too_large`. The CLI's `run --quote
BOT/TURN` (repeatable; `BOT/TURN:reply` for a reply, `turn:` prefix
optional) sends them. `prompts` and `history_items` list a prompt's
quotes as `quotes`, each with its writer (`from`, `origin` or `person`,
none for a person's) and, in `prompts`, its `text` within the read's
`bytes`, each record counted as an entry; quotes past them are left out,
and their prompt says `quotes_more` and `truncated`; Sonny's approver reads a quote of the person's own words as
consent only in a prompt the person sent, and any other as asking for
nothing.
`{"op":"prompts","bot","turn","bytes"?}` reads a turn's words and calls
as an approver judges them, within `bytes` of text (default 64 KiB, at most
256 KiB): the turn's prompt and each steer it absorbed, in order, with
`from` or `origin`, and `prompts_more` when steers were left out; the calls it started, each with its argument preview,
`done`, `failed` when it failed, and `node`, with `calls_more` when some
were left out; and the bot's earlier prompts, newest first, with `more`
when some were left out. A prompt or steer, this turn's or an earlier
one's, carries the calls its turn was refused after it and before the
next steer (`refused`, each `{name, arguments}`, `arguments_truncated`
when cut), so a reader can tell a go-ahead that followed a refusal from
one that came before it. This turn's prompt and steers carry `reply`,
the end of the bot's last message before them within 2 KiB
(`{text, truncated?}`), with secrets redacted before the end is kept: its
last of the turn before for the prompt, and one since the previous words
for a steer; absent when there is none. It is the question a bare "yes"
answers. Refused calls and
`reply` count against the words' `bytes`. The words and the calls each have `bytes` of
their own, so a long prompt still leaves the calls (and the files they
wrote) to read. Text that does not fit is cut and marked
`truncated`, and each steer, call, and earlier prompt also counts 64
bytes, so many short entries stay within `bytes` too. A fork keeps the source's binding, instructions,
and tools, and takes no text of its own; the source is never changed. Workspaces, wherever given, must already exist and be absolute. Use the actual returned checkpoint
and turn IDs, not the illustrative numbers. Names are immutable bot identities
within one store; rename/alias operations are not implemented. A fork starts
from any message in the source's history: `checkpoint` names a node id (every
`message` and `tool_completed` event carries one). Without it, an idle
source's head is used, and a running or parked turn's newest finished round:
the last result of its newest fully answered round, a steer absorbed at a
round boundary, a final answer not yet finished, or the turn's prompt before
any round finishes. The store keeps that node current as the turn appends, so
this fork reads no transcript. The upgrade to schema 36, which began keeping
it, ends a turn left running or parked as interrupted, so every turn has
one. A fork keeps its source's window start when the fork
point is at or after it and carries the source's current compaction, so its
first request can read the source's cache. `allow` narrows which of the
source's tools the fork may call: absent keeps the source's list, `[]` allows
none, and `null` fails with `invalid_allow`. A tool outside the source's list
fails with `allow_not_in_source`, so a fork never widens. The fork is still
shown every tool, since changing the definitions would rebuild the provider
cache; a call outside the list is refused at dispatch with
`tool_not_available` and never waits for an approver. `inspect` and `forked`
report `allowed` when a bot has one (schema 37). The point must leave no tool call unanswered, or the fork
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

`create` and `fork` take an optional `request_id` for the same purpose. The bot
keeps the key and the request as sent, so a resend with the same key and
fields returns the bot it made, with `duplicate: true` and no `cursor`, and
writes nothing; a fork's resend is answered from the fork even after its source
moved or was deleted. The same key with any field changed is
`idempotency_conflict`, naming the field in `detail` and `field`. A name taken
by an unkeyed creation or another key is `bot_exists`, with the holder's
`bot_id`. Once the bot a key made is deleted, any request with that name and
key is `bot_deleted`, with the deleted bot's `bot_id`, and never makes the bot
again. Their replies are the
bot record (`name`, `id`, ...) plus `duplicate`. `delete` takes an optional
`bot_id`: resent while the first deletion runs or after it finished, it
succeeds with `duplicate: true`, and an identity that name never held is
`bot_not_found`. Its reply carries `bot`, `bot_id`, `duplicate` and the counts
it freed. Protocol 7 adds these fields.

`interrupt` answers with the turn view and `interrupt_requested`. A running
turn's view shows it still running; its `turn_finished` follows. A queued,
ready or parked turn ends in place, so its view already shows `interrupted`.
A turn that had already ended answers with how it ended and
`interrupt_requested: false`, so a repeated interrupt, or one that crossed the
turn's own end, succeeds. A turn the bot never had is `turn_not_found`; a
turn that is starting, or that is not the one running, is `stale_turn`.
Protocol 8 adds the turn view.

### Delivery modes

`delivery` on `submit` says what happens when the bot is busy or the daemon
is at `--max-active`. It is one required field with four values, since
what a busy bot does with a message is the client's choice
(`invalid_delivery` without it); every mode returns
the turn's id and handle at once, and `wait`, `turns`, and
`interrupt` work on the turn unchanged.

- `reject`: `bot_busy` while a turn runs or is parked,
  `active_agent_limit` when no slot is free. Nothing is written. The
  `bot_busy` detail says what is in the way (`turn N is running`, or
  `earlier work is waiting`) and which requests get past it, in this
  protocol's fields: `submit` with `delivery` `steer` and `expected_turn`
  N, `submit` with `delivery` `queue`, or a `fork` of the bot to ask
  without interrupting, which starts at the running turn's newest finished
  round. `agent run` says the same as flags to copy
  (`--delivery steer --turn N`, `--delivery queue`, or
  `run --detach --fork=NAME --bot NEW --allow '' --expires 15m -- QUESTION`),
  built from the refusal's `running_turn` and `fork_point`, taken in the
  refusing transaction, so it asks nothing more and never names a turn that started
  since. `fork_point` is where a fork without a checkpoint starts, the head
  of an idle bot or that newest finished round. A model calling `agent run`
  does not discover them otherwise, and in benchmark runs it ignored a prose
  description of them.
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
  turn then records a `steered` event. A steer that arrives while the final model
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
  encoded steer stay within three quarters of `context_bytes` and
  `context_items`, the target the window itself keeps, so a burst of
  large steers cannot make the running turn exceed its context and fail
  with `context_limit`. A boundary builds the view first, so a note a tool
  wrote during the round counts, and a steer that goes in sends the view
  back through the same overflow, elision, and compaction steps before the
  model call. A steer that does not fit stays queued, and later steers do
  not overtake it. Once elision or a summary makes room in the running
  turn, the same boundary tries it again, and so does a later boundary
  whose view sends less ahead of the turn, a note cleared or shrunk, so a
  correction reaches a long task that compacts. For a bot with
  compaction instructions and a summarizer the daemon serves in the bot's
  family, a steer still queued after a boundary's elision and compaction
  steps, or after a final reply, is measured against the whole
  `context_bytes` and `context_items` instead: when
  one large result in the newest round fills the turn, no stub or summary
  can take it, and the steer would otherwise wait out the task. Beside
  what no summary takes, the view's prefix and the turn's prompt, it
  still keeps to three quarters, so only rounds a summary can take go
  past them. Such a steer goes to the model in the next call, before any
  summary, so no summary spends the round or the budget it went in for.
  What it adds may put the view past the compaction threshold, and the
  boundary after that call summarizes the rounds behind it, as it does
  for any view that size. A bot without such a summarizer keeps three
  quarters, since nothing could take those rounds and its next round
  would fail with `context_limit`; the summarizer itself refuses a
  provider now bound to another family, with `provider_family_mismatch`.
  Admission counts the comma the request puts before each item. A steer
  joins only a turn that can call again: after a summary or a final
  reply that spent the budget or the last round, counting a cache
  refresh the reply left in flight, it stays queued. One still queued
  when the turn ends starts as its own turn when the line moves (a
  strict steer, which names that turn, fails with `stale_turn`).
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
- `attach` (protocol 18): a message that needs no reply. It never starts a
  turn, busy bot or idle: its row is `attached`, it holds no slot, and no
  pending bound counts it. It joins the bot's next model call as a steer
  does, through the same absorption and room checks: at the running turn's
  next round boundary, or at the first boundary of the bot's next turn,
  after that turn's prompt. Unlike a steer it never keeps a turn going: one
  that arrives while a final reply is in flight waits for the next turn,
  unless a steer keeps this one going, whose call it then joins. It
  finishes as `steered` with `into` naming the turn that took it; the
  submission's event is `attached`. It takes no `workspace`, `model`,
  `effort`, `budget_tokens` or `expected_turn` (`invalid_delivery`), since
  it joins whatever call comes next. For the same reason the bot's default
  model is not checked and it needs no folder, even on a bot without one; it
  never moves the retention boundary past itself. `interrupt` withdraws one
  that waits.
  `agent run --delivery attach` returns at once, inside a bot's shell too.

A queued or ready turn that cannot start when its place comes (for example,
the bot's budget is spent) finishes as `failed` with that
error, and the next in line takes its place. `interrupt` on a queued or ready
turn ends it as `interrupted` and answers with it ended; interrupting the
running turn does not touch the line behind it. `delete` refuses a bot with
queued work as `bot_busy`. `stats` reports queued and ready turns together
as `queued_turns`, a count the storage worker keeps at each transition
and recounts at open, so admission and `stats` cost the store nothing.
`agent run --delivery MODE` sets the field, defaulting
to `AGENT_DELIVERY` in the client's environment, then `reject`; with
`--detach` a peer can hand a busy bot work or a mid-turn message without
racing on `bot_busy`.

## Durable state and recovery

SQLite WAL with `synchronous=FULL`, committed in groups (see Core ownership),
stores bot metadata and provider binding,
immutable history nodes, turns, completed checkpoints, tool intents/results,
retained tool artifacts, and durable event cursors. An answered commit
survives a daemon crash. On macOS, plain fsync can leave writes in the
drive's cache, so an OS crash, including a kernel panic, or power loss can
lose recent acknowledged commits; see [Apple's fsync documentation](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fsync.2.html).
Per-commit `fullfsync` closed that gap at twice the daemon CPU on light
load and is off. Both connections set `checkpoint_fullfsync` to preserve
WAL/database write ordering at checkpoints. This protects database consistency,
but does not make subsequent commits durable across an OS crash or power loss.
Workspace writes have their own durability behavior and need not be lost
together with the corresponding store records. Other platforms ignore both
full-flush settings. The writer sets
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
so none needs one. Schema 33 adds [tool approval](#tool-approval): the
`approvals` table and its tag index, and `bots.gates`, which existing bots
leave empty. Schema 34 adds `turns.view_sent`, whether a turn's call or a
summary or elision changed the bot's view last; turns stored before record
neither, so a [summary](#compaction) after them never takes their call as
having sent the view. Schema 35 adds `bots.denials`, the [denial
counts](#tool-approval); none were kept before, so every bot starts from
none. Schema 36 keeps a running turn's newest finished round. That round
cannot be read back without a transcript scan, so the upgrade ends a turn
left parked the way every open ends a running one: interrupted, its calls
answered, its history kept. Schema 38 adds `turns.from_bot` and `turns.from_turn`, who wrote a
prompt; turns stored before record none, so they read as a person's.
Schema 39 records the folder each earlier turn ran in. Schema 40 adds each
bot's own [settings](#bot-settings), the daemon's flags before it, so an
existing bot takes the defaults; it also gives a turn an earlier daemon
parked the fields added to its record since, with the values that daemon ran
it with, so every parked record has one shape. Schema 41 adds `turns.origin` and
`turns.from_id`, and keeps each prompt's sender with its node (`senders`): a turn another bot
wrote is found by its first node, a steer by the node it shares or the one its
`steered` finish recorded, with the sender's identity where its turn is still
stored. A steer whose finish event was pruned names no sender. Stored `steered`
events name the steer's turn as `steer` and gain its sender as `from`, and
stored `accepted` and `queued` senders gain their `id`, so a replay reads as
live events do.

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

`inspect` reports the identity's state and changes nothing; no operation
automatically continues an interrupted network request. A new submission is an
explicit new turn. Partial streaming text is marked `durable:false` and can be
lost on crash. Completed messages survive. Paged `events` replay returns durable
records; message/tool-result records reference stored nodes retrievable through
`history_items`, avoiding another transcript copy in the event table. Cursors are store-wide
monotonic IDs, and queries are filtered to the requested bot. History loads and
lineage checks use recursive queries, so `history_items` and `fork` cost one query each
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
daemons share one key. A tool is never rerun. Each retry is a durable `retry`
event of its turn (attempt, error, detail, delay, whether it parked), and a
turn's record carries `retries`, `paced_ms` and `summary_ms` in `turns`. `retries` counts
additional attempts that entered HTTP dispatch; cancelling during backoff or
pacing does not count an unsent retry. `paced_ms` includes a partially elapsed
pool wait on interruption. `summary_ms` is how long the turn waited on
summaries: one before its call from planning to install, and one beside a
call from the call's end to its install, including a wait an interrupt
cut short. Counters flush once per execution segment, on
completion, failure, explicit interruption, or parking, and accumulate across
resumption. A hard process kill can lose the current segment's unflushed
counters. Retries are on by default because they cannot repeat an effect; what they can repeat is
billing for a failed attempt, which is recorded as failed usage. Successful
model responses and failures with reported usage share the bot's durable
limit of model rounds per turn, its `max_rounds` setting (unset, none: the
turn's token budget bounds a runaway instead), including across parking and
restart. The last
of them, a retry included when a billed failure made it the last, goes with the bot's tools shown but `tool_choice` `none` (both
families), so the model must answer in words, and the turn ends on that reply
instead of failing with its work done and unreported; a live
`tool_calls_withheld` event (`reason: "tool_round_limit"`) marks it. That call
sends no keep-warm refresh, and on Anthropic, where `tool_choice` is part of
the cached prefix, it rewrites the message cache once. A model that calls a
tool anyway has those calls run, as any committed plan's, and the turn then
fails `tool_round_limit`.
Unbilled failures remain bounded by the attempt and time limits above.

The transport keeps 64 streams per HTTP/2 connection rather than the 100 the
providers allow, so a connection the provider's edge resets takes fewer turns
with it.

## Long history and context windows

Stored history has no length limit. What a model sees per request is a
context window: the newest whole turns of the bot's lineage that fit
the full input allowance of `context_bytes` and `context_items`. Completion
headroom can advance the compaction trigger; it does not reduce the hard input
allowance. The allowance counts the encoded summary, retained prompts, carry-forward
note, omission item, transcript items, and JSON separators. These are runtime
conversation-byte/item bounds; system instructions, tool schemas, provider JSON
framing, and model token limits are separate, not inferred from byte counts.
A picture counts at its weight, not its size: four bytes for each token it is
estimated to cost (width × height / 750 once its longest side fits 1,568
pixels, at most 1,600 tokens, Anthropic's rule; OpenAI's high detail counts
fewer). Each node records what its pictures add beyond their weight, which
its byte total leaves out; only a request's length counts them, so the
pacer's token estimate and every budget see a picture at its token cost.
For compaction planning, a known output-token cap is estimated at four bytes
per token, capped at a quarter of the envelope. The earlier of that headroom
threshold and `compact_at` triggers compaction. An unset Responses cap adds
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
number from 1 to N to read any of them, or with query to search them for
text.` A search (`query`, optionally `before` a turn) walks the lineage newest
first, leaves out the turn it runs in, and lists up to 20 matches with their
turn, item type and the JSON text around each, ignoring ASCII case. It stops at
a turn boundary once it has listed 20 or read 64 MiB of items and names that
turn as `next_before`. A search whose `bot` is a GLOB pattern (`*`, `proj.*`;
no name holds `*`, `?` or `[`) searches every bot it matches at once, newest
first. It merges the bots' walks by node ID, which only falls along a
lineage, so a node two histories share, as a fork shares its source's, comes
up in both walks at once and is searched once, under the oldest bot that has
it, while a node only a later fork has is credited to it. Where walks meet,
the rest of them is the same, so only the oldest bot's goes on: each node is
read once however many bots share it, and the next node comes off a heap of
the walks' places, so a step costs O(log k) for k walks. A walk is only its
place, started when the merge reaches its head, so nothing is prepared per
bot; each match adds its `bot` and turn `handle`; `next_before` is where the
merge stopped, passed back as `before`.
The note then lists,
newest first, the ordinal and the first line (up to 120 bytes) of each
omitted turn's prompt, at most `note_turns` of them (default 48, 0 lists
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
items, raw `history_items` reads, forks, and provider replay remain unchanged.
With `bot`, it reads another agent's turn the same way, numbered along that
agent's lineage and as it stands, a running turn included; with `bot` and no
`turn`, it lists that agent's newest 20 turns, newest first, each with its
number, handle, status and its prompt's first 200 characters, so an agent can
see what another is doing without reading the store. A bot's own turns are
read by number only. Any agent of the daemon can be read: the daemon holds
one person's agents. The reading
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
32; supported migrations run at open. Store initialization and migration run in one
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
needs, and one optional policy composes them. A fork can also ask to be
deleted once idle:

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
  `inspect` reports the status. Shutdown cancels and awaits pending retention
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
  does not change explicit `inspect` requests: a missing bot returns `bot_not_found`.
  The bot's turn rows go with it, so a late retry of one of its requests (the
  same `bot` and `request_id`) answers `bot_not_found`, never the old outcome
  or a duplicate turn. If the name was recreated meanwhile, a retry carrying
  the old `bot_id` is still refused; one without it is fresh work on the new
  identity.
- `fork {expire_ms}` (1 s to 7 days; `--expires` on `agent fork` and
  `agent run --fork`) makes a fork the daemon deletes, as `delete` would,
  once it has been idle that long: no turn has run, waited, or been queued
  since it was made or its last turn ended. Its record shows `expire_ms`
  and `expires_at` (epoch milliseconds), and each turn's end moves
  `expires_at` to then plus `expire_ms`, so a follow-up keeps it alive. The
  time is stored, so a fork outlives neither a daemon restart nor the
  client that made it: the daemon looks when the earliest time comes due
  and at every start, and a fork still busy then (a command it started,
  queued work) is looked at again a minute later. Followers get the same
  `deleted` notification with `"expired":true`. This is what a side
  question needs: `agent run --fork NAME --bot NEW --allow '' --expires 15m
  -- QUESTION` asks a copy of NAME, which answers from NAME's history
  without acting and leaves NAME as it was.
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
  `history` tool, `history_items`, forks, and accounting are unaffected; what shrinks
  is replay and artifact retrieval. A finished turn's row names the message
  with text it ended on, set in the write that finishes it, so a quote of its
  reply and the `reply` a `prompts` read of the next turn carries are
  unaffected too, and read it by key rather than searching events. Schema 70
  adds it, found for finished turns from the events retention had left. New `wait` calls for an expired
  turn outcome return the turn's view with `error: turn_result_pruned`; they
  never report an empty success.
  On a pruning notice, `agent run` and `agent follow` reconcile their selected
  turn through a zero-timeout `wait`, so retries of expired turns exit with that error
  instead of waiting for a terminal event that no longer exists.
  Existing waiters can still receive the completion captured before pruning.
  The bot remembers the highest pruned
  cursor: an `events` page starting before it carries `pruned_before`, and a
  `follow` from before it is preceded by a `pruned` notification, so no
  consumer replays a silent gap. `agent prune --bot --keep-turns N`.
- A bot's `keep_turns` setting applies `prune` to it after each of
  its turns finishes, inside the same commit, including cancelled or failed queued work and interruption
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

The daemon registers `echo`, `shell`, `read`, `write`, `edit`, `apply_patch`,
`wait`, `history`, `note`, `web_search`, `web_fetch` and `write_stdin`; each bot is created with the subset it may call (`run --tools`),
which is what its model is shown and what dispatch allows. The
registry validates tool names and arguments before execution; a tool that fails
(unknown tool, invalid arguments, missing file, ambiguous edit, output
overflow) returns an error result to the model and the turn continues. Only a
closed tool scheduler fails the turn. Allowed tools run without approval
prompts unless the bot was created with a gate (see [tool approval](#tool-approval)).
A store remains bound to its tool set; changing it requires a new store.

`shell` runs a noninteractive command with `/bin/bash -c` (`/bin/sh` where
bash is not installed) in the bot workspace with
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
the `artifact` operation, or written to the bot's `outputs` folder (see
[retained outputs](#retained-outputs)). Output is redacted before it is stored or sent,
artifacts included, as every tool's result is: each credential the daemon
holds, wherever it appears; the value printed after a field name ending in
one the daemon was started with (`--redact-field`, any case, with or without
`_` or `-`, as `NAME=`, `NAME:` or `"NAME":`), when it is at least 8 bytes and
mixes letters and digits; and the body of a PEM block whose header names a
`--redact-block` label. Which names count as secrets is the client's
judgement: the CLI passes `agent_client::SECRET_FIELDS` and `PRIVATE KEY`. Results include separate output, exit code, and
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
`--max-lasting`, shared with terminals as a systemd slice holds a
service's processes: each detached child retains a slot until its async
waiter reaps it on exit, and a new detach or terminal refuses with
`lasting_limit`, naming how many terminals and detached commands hold the
slots, while all are held. Nothing waits in line for one. It exists because Terminal-Bench
tasks that leave a server for the grader failed when the server died with its
command. `tty: true` runs the command in a terminal instead, for a REPL or a
program that prompts, as Codex's `exec_command` does: the command gets a
pseudo-terminal as its stdin, stdout and stderr and its own session, and the
call returns what it wrote within `yield_time_ms` (default 1 s, at most 30 s)
with a `proc:N` handle while it runs. `write_stdin` types into that handle
(`chars`, with a control key as its character, such as `\u0003` for Ctrl-C)
and returns what the command wrote since the last read, after waiting
`yield_time_ms` (250 ms after typing, 5 s when only reading); `kill: true` ends
the command's process group. A read that waits longer than the bot's
`approval_hold_ms` waits live only for that hold; then its turn parks as
`wait` parks on the terminal's handle, holding no task and no active slot,
until the command ends or the read's time runs out, and the read's result is
what the command wrote meanwhile. A steer waiting for the boundary keeps the
read live. Output merges stdout and stderr, as a terminal
does, keeps its last 64 KiB unread, counting what it drops, and is redacted
like any other; a secret field's name, cut off at the end of a read, waits
with its value for the next one, so the value is redacted whole. A
terminal takes no process slot and no timeout: it runs until
it exits or is killed, across turns, and `wait` on its handle reports how it
ended, with what no write read. It takes a `--max-lasting` slot before it
starts and keeps it until its whole session is gone: a kill ends the
command's group, the job in the terminal's foreground, and every other
group in its session (found under `/proc` on Linux and from the process
list on macOS), so a job a shell with job control started, ignoring
SIGHUP, does not outlive it. Each bot keeps at most 16; starting another
first kills the one it used least recently, as Codex prunes its sessions,
so that one's slot is free for it. Only the
bot that started a terminal types into it (`terminal_not_running` for any
other, and for one that ended), and terminals end when the daemon stops. A
terminal never leaves its bot's sandbox (`tty` with `require_escalated` is
refused), so input to a sandboxed bot's terminal runs without review, as its
commands do; an unsandboxed bot's `write_stdin` is gated like `shell`.
Commands see the same environment, `TERM=dumb` included, which keeps REPLs to
plain prompts. Under bubblewrap the command runs in a session of its own, so
Ctrl-C does not reach it there; `kill` does. Turn cancellation requests the same kill for a
foreground shell, but native file I/O or background commands can outlive the
cancelled turn. Without a committed result the tool outcome is unknown, not a
claim that all work stopped. The turn ends `interrupted` and the bot stays
usable; history tells the model to inspect current state before retrying.

### Sandbox

A bot created with `sandbox` runs its `shell` commands, `write`s and `edit`s
confined, as Codex confines its agent: the mode names, the writable folders,
the folders kept read-only inside them, the bubblewrap arguments on Linux and
the Seatbelt profile on macOS are Codex's, ported (see NOTICE). Reading stays
open. `read-only` writes nothing; `workspace-write` writes the bot's folder
(its stored workspace, else the turn's), each `writable` folder, `$TMPDIR`
and `/tmp`. Inside each writable folder, each name the daemon was started
with as `serve --protect NAME` stays read-only and, when missing, may not be
made, and each `--protect-existing NAME` stays read-only where it exists. A
`.git` among them keeps a linked worktree's own git folder read-only too
when the folder holds it. Which names is the client's judgement: the CLI
passes `agent_client::PROTECTED` (`.git`, `.agents`, `.codex`, since git runs
hooks and config from there outside the sandbox and agents trust what is
there) and `PROTECTED_EXISTING` (`.aws`, whose profiles can name a program
to run). The daemon's store and the files beside it stay read-only too. A
protected folder that is a symlink refuses the call with
`sandbox_unavailable`, since the link could be swapped from inside. A git
folder given as a `writable` folder, so a bot may commit, keeps what git
runs read-only as `.git` does: its `hooks`, `config`, `config.worktree`,
submodules' `modules`, and the `commondir` that would point git at another
folder's hooks and config, its own and each linked worktree's under
`worktrees`. Missing `hooks` or `modules` may not be made, nor the git
folder's own missing `commondir` on macOS; on Linux, where a folder held in
its place would stop git, one the command makes is removed when it ends. So a sandboxed `git
worktree remove` of an existing worktree is refused. It is
known by its name, `.git` or `NAME.git`, which the bot can't change, not by
what is inside it. One whose config is missing, turns on per-worktree config
or includes other files stays read-only as a whole, since git reads those
files wherever they are made. Network is
off unless `network` is set. `danger-full-access`, or no `sandbox`, runs
unconfined.

A Unix socket is a way out of any sandbox: a program listening on one runs
what it is asked outside, as a browser's daemon launches a browser or Docker
runs a container. A bot with the network on reaches every Unix socket but
the daemon's and the other bots' (below), as Codex's full network does, so
Docker, ssh-agent and a tool's own daemon just work. One kept off the
network reaches only the Unix sockets its `unix_sockets` names, as Claude
Code's sandbox and Codex's managed network block them by default. Not even
those in its own folders: a hard link there to any socket, which needs no
right to the socket, would reach it, and a named folder the bot may write
lets one in the same way. On macOS Seatbelt holds an offline bot to those
paths. On Linux, where a read-only mount doesn't stop `connect`, a seccomp
filter bubblewrap loads refuses every Unix socket an offline bot's command
would make, as Codex's Linux sandbox does; a filter can't see the path, so
a bot whose `unix_sockets` names any reaches them all, as Claude Code's
`allowAllUnixSockets` does, and with the network on the filter lets them
all through. Socket pairs stay allowed, but not of datagrams, which send to
any socket's path. The filter also refuses `io_uring`, whose calls it can't
see, and ends a command that makes system calls of another architecture.

A sandboxed bot's commands reach the daemon through the bot's own socket,
never the daemon's: so `agent` and the clients built on it work in its shell,
as that bot and only as far as a sandboxed bot may go. The daemon opens the
socket, `SOCKET.d/BOT_ID` beside its own, when one of the bot's turns
needs it, and keeps it listening while any of the bot's live turns or
running commands (foreground, background, detached or a terminal) holds
it, as systemd's socket activation keeps a unit's socket. Thirty seconds
after the last one goes, or at once when the bot is deleted, it closes;
connections it already accepted keep their sessions, and the bot's next
turn opens it again. So a fleet of idle bots holds no listening sockets. On macOS Seatbelt
denies the daemon's socket and that folder and allows the bot's own,
network on or off. On Linux, where the command makes no Unix socket, the
daemon hands it connections, as a broker does: `AGENT_SOCKET_FD` names a
descriptor the command inherits, and each byte written to it is answered
with a connection to the bot's own socket, passed over it, or none when
that socket is gone. Like make's jobserver descriptors, it reaches only
the processes that keep it: a launcher that closes what it didn't open,
as Python's `subprocess` does, passes it on (`pass_fds`), or the client
says which descriptor went missing. The bot's socket is also bound over the daemon's in
the sandbox, and the folder of the bots' sockets is hidden. Either way the
command's `AGENT_SOCKET` names it, the clients connect as it says, and no
command reaches another bot's. A request on it:

| Request | On a bot's own socket |
|---|---|
| `configure`, `answer`, `tool_result`, `approve_step`, `step_result`, `serve_approvals`, `renew_approvals`, `shutdown` | `not_permitted`: any of them would let a confined command undo its confinement |
| `create`, `fork` | Made by the caller, whatever `created_by` names (another is `not_permitted`), so the bot inherits its sandbox; its workspace and `writable` folders, and a fork's source's, must be ones the caller may write. A fork with `allow: []`, which only answers, may be of any bot |
| `submit` | Reads as the caller's: `from` its turn when the request names it (the CLI does, from `AGENT_TURN`), else with its name as `origin`; `from` naming another bot is `not_permitted`. A workspace for a sandboxed bot must be one the caller may write |
| `delete`, `prune`, `compact`, `interrupt` | The caller itself, or a bot it made; another is `not_permitted` |
| Everything else | As on the daemon's socket |

The folder of these sockets stays read-only in every root, so no bot can
put its own listener in another's place. A tool a client answers, declared
for a bot the caller makes or callable by a fork it makes, must be one the
caller has, may call and passes on, answered by the same client, and taking
`steps` only if the caller's does, since a call that takes steps skips the
gates of the call as a whole; another is `not_permitted`, and one it may not call isn't inherited. A root reached
through a link, which something swapped in since, is `sandbox_unavailable`.
A socket inside a writable folder
stays read-only too. On macOS the folders between a writable folder and
anything kept inside it (a socket, the store, a worktree's git folder) can't
be renamed, so nothing kept can be moved to a name its rule doesn't cover. A
daemon on stdio, or whose socket path leaves no room for the bots' sockets,
gives a sandboxed command no way to it.

Creating a sandboxed bot checks that sandboxes run here and refuses with
`sandbox_unavailable` otherwise: on Linux, a system bubblewrap
(`/usr/bin/bwrap`, `/bin/bwrap`, `/usr/local/bin/bwrap` or
`/run/current-system/sw/bin/bwrap`, or the one a package build fixed with
`AGENT_BWRAP`, as the Homebrew formula does for its own; never one found on
`PATH`, which could lie in a folder a bot may write) must make the user, pid
and network namespaces; on
macOS, `/usr/bin/sandbox-exec` must exist. A sandboxed bot never runs
unconfined for want of one. A `writable` folder that doesn't exist is
`path_not_found`.

A confined command that touches what it may not fails as it would on a
read-only filesystem, and the model sees that result. A confined `write` or
`edit` is checked against the same folders by the target's real path and
refused with `write_denied`; the write itself then runs inside the sandbox,
as Codex runs `apply_patch`, in the agent binary started as
`agent --agent-run-as-write PATH` with the content on stdin, so a link
swapped in after the check can't carry it out. Being a process, it takes a
slot of the daemon's process budget as a command does.

To leave the sandbox, a shell call sets
`sandbox_permissions: "require_escalated"` with a `justification`, which a
sandboxed bot's `shell` schema shows with Codex's descriptions; a `write` or
`edit` leaves it by naming a path outside its writable folders. Only such an
escape is gated: a confined call runs without a verdict even under a gate,
and an escape waits on the bot's gate as any gated call does
(see [tool approval](#tool-approval)) and, allowed, runs unconfined. With no
gate on the tool there is no one to ask, so, as Codex refuses an escape under
a policy that never asks, a command asking to escalate is refused with
`escalation_refused` and a write or edit stays held to the sandbox
(`write_denied`). Whether a call escapes is read
from its arguments and the folders' names alone, so the round that announces
it and the run that starts it agree.

A command its client trusts runs outside the sandbox without asking, as a
Codex rule that allows a command prefix runs it: a shell call that doesn't
ask to escalate, whose script runs nothing but commands that begin with the
words of one of the bot's `unsandboxed` entries. So a tool with a daemon of
its own, such as mcpx, which keeps its lock and socket under
`~/.local/state` and starts its daemon on first use, works from a sandboxed
bot once it is named: started in a sandbox, the daemon would keep the
sandbox's limits, and on Linux die with the command. Only a plain script is
read, as Codex reads one: words, quoted or not, in commands joined by `&&`,
`||`, `;`, `|` or a newline. A script with anything that could run something
else (`$`, a backquote, a redirection, parentheses, `&` alone, a comment)
runs in the sandbox, and so does one with any other command in it, such as
`mcpx … | jq`; it can still ask to escalate. The command is found on the
daemon's `PATH`, so an `unsandboxed` command is only as trusted as the
folders there.

A bot that a sandboxed bot makes (its `created_by`), or forks, takes the
creator's sandbox and `writable` folders unless it names a stricter sandbox,
and reaches the network only if both do, as Codex's subagents inherit their
parent's sandbox. It runs outside the sandbox only commands its creator
does.

On Linux, bubblewrap makes an empty folder on the host to mount a missing
`.git` read-only over; the daemon removes it once no running sandbox holds
it, and a detached command holds it until it exits. A sandbox that needs
one being removed waits until it is gone.

### Declared tools

Protocol version 19 lets a client declare tools of its own on a bot:
`create` and `configure` take `declare`, a list of up to 16
`{name, description, parameters, result?}` taking at most 64 KiB, which
`configure` replaces whole (`[]` removes them). The model is shown them after
the daemon's tools, and the daemon never runs one: a call is checked against
its `parameters` (an object schema; required fields, a closed object's extra
fields, and each listed field's basic type) and recorded at once with the
tool's fixed `result` (default `ok`), or with `invalid_arguments` when it does
not fit. The call is otherwise like any other: `tool_started` and
`tool_completed` events, history items, approvals never asked. What it means
is the client's, which reads the call from events and history; a TUI's
reaction to a message is one. A name is 1 to 64 letters, digits, `_` or `-`,
taken by none of the daemon's tools nor declared twice; anything else is
`invalid_declare`. `inspect` reports `declared` when a bot has some (schema
53). A bot created by another takes its creator's declared tools unless its
own client declares others, a fork keeps its source's, and a fork's `allow`
narrows them as it does the daemon's. Changing them changes the request's
tool definitions, so a fork keys its own prompt cache from then on, as for
instructions.

Protocol version 20 adds `inherit` to a declared tool, default `true` and
reported only when `false`: a tool declared with `"inherit": false` stays
the declaring bot's own, so a bot it creates does not take it with the
rest. A fork still keeps it. A lead's client uses it for a tool only the
lead should call.

Protocol version 21 lets a client answer its own declared tools. A tool
declared with `answered_by`, a tag of 1 to 64 letters, digits, `_`, `-` or
`.` that the daemon never reads, takes no `result`: a call that fits its
`parameters` starts, and waits for the answer of whichever client acts for
that tag. Its `tool_started` names `answered_by` and `expires_ms`, when it
stops waiting: `expire_ms` after it started (1 to 3,600,000, default
60,000). A client finds such calls by following the bot, or every bot with
`*`, and lists those still waiting with
`{"op":"tool_calls","tag","after"?,"limit"?}`: oldest first, each with
`bot`, `turn`, `call_id`, `name`, `arguments` and `expires_ms`, at most
`limit` (1 to 256, default 64) and 256 KiB a page, continued from
`next_after`. It answers with
`{"op":"tool_result","bot","turn","call_id","output"}`, the text the model
reads, or with `"error":{"code","detail"?}` in place of `output`, a refusal
the model reads as `{"error":CODE,"detail":DETAIL}` (a code is 1 to 64
lowercase letters, digits or `_`; output and detail each up to 64 KiB,
redacted as command output is; anything else is `invalid_tool_result` or
`tool_output_limit`). `output` may instead be a list of parts the model
reads in that order, as providers' own tool results and MCP's take them:
`{"type":"text","text"}` and `{"type":"image","media_type","data"}`. Its
text is up to 64 KiB together, redacted the same way, and its pictures
are a prompt's: up to 20 of PNG, JPEG, GIF or WebP, 5 MiB together once
decoded, else `invalid_image` or `image_limit`. A picture counts at its
token cost, and a result with pictures elides to its text as a read
picture does. The client scales a picture; the daemon sends it as given. The first answer counts: another is
`call_already_answered`, one for a call that ended or ran out of time is
`no_pending_call` or, once its turn ended, `stale_turn`. There is no lease: a tag is the client's
to serve.

The call waits as a gated call waits for its verdict. Its turn holds live
for the bot's `approval_hold_ms`, but at most 250 ms, and an answer in that
time is held by the storage worker and written with the call's result, so
the call costs one commit. Past the hold the turn parks like `wait`: one
commit, `turn_waiting` with `{call_id, answer: TAG, deadline_ms}` (also its
view's `waiting_on`), no task, no active slot, and it survives restart. An
answer to a parked call is committed on arrival and resumes the turn. A
call still unanswered at `expires_ms` ends with
`{"error":"tool_unserved","detail":...}` and the turn goes on, since the
model can act on it; the service wakes a parked one then from its own
schedule, so a waiting call costs a row and no timer. A later call's gate that
lapses first ends the wait as `tool_unserved` and the turn as it ends one
waiting on a verdict. An interrupt ends the call as cancelled, like any
running call. A call that does not fit its
parameters is refused at once and waits for no answer.

Protocol version 22 lets a declared tool take free-form text, as
`apply_patch` does: a tool declared with `grammar`, a Lark grammar its
input follows, is declared on Responses as a custom tool whose call is the
text itself, and elsewhere as a function of that text, so its `parameters`
must take `input` as a required string (`invalid_declare` otherwise).
Either way the call's `arguments` are `{"input": TEXT}` and its answer is
the provider's custom tool output where it has one. The grammar counts
toward the 64 KiB. A client's code-running tool is one.

Protocol version 24 makes `delivery` a required `submit` field
(`invalid_delivery` names the four), since the daemon has no opinion of
its own on what a busy bot does with a message.
It adds `item` to `text_delta` and `thinking_delta`, so a
client draws each message a call streams apart, as it is kept (see
`follow` above). It also reports an unset `cache_ttl` in `inspect`'s
`settings` as `null` rather than `"5m"`, so a client tells a lifetime
nobody set from five minutes someone chose.
It also lets the client answering a call put each step of it to the
bot's gates, as Codex approves each nested call of a code-mode
script rather than the script. A tool declared with `"steps": true` (only
with `answered_by`; `invalid_declare` otherwise) is never gated as a
whole: its call starts at once, and the gates that name the tool judge its
steps instead. The client asks with
`{"op":"approve_step","bot","turn","call_id","step","name","arguments"}`:
`step` its own id for the step within the call (1 to 128 of `A-Z a-z 0-9
. _ -`, never reused for the call), `name` the step's tool (1 to 512 bytes,
no control characters), and
`arguments` an object of at most 64 KiB (`invalid_step`,
`step_arguments_limit`). The call must be the bot's and still waiting for
its answer (`turn_not_found`, `stale_turn`, `no_pending_call`,
`call_already_answered`), of a tool that takes steps (`invalid_step`).
Under no gate the reply comes at once:
`{call_id: CALL/STEP, decision: "allow", approvals: []}`. Otherwise the
step is announced in its own `approval_requested`, listed, served and
answered exactly as a gated call is, under the call id `CALL/STEP`, with
`step_of` naming its call, `node` 0 (a step has no item of its own) and
its arguments whole. The reply waits for the decision, holding no task or
timer, only the step's lapse in the daemon's one schedule:
`{call_id, decision, approvals}`, with the deny's `reason`, and
`expired` when a gate lapsed without a verdict, which denies the step
(`not reviewed: no verdict`) and nothing more: the turn never waits on a
step. Each decided step is recorded in a `step_decided` event with
`call_id`, `step_of`, `name`, `arguments` (previewed as `tool_started`
previews a call's), `decision`, `reason` and `approvals`, and `denied`
when its gates refused it, which a judge of a later call sees among the
turn's refused calls; it counts toward the bot's denial counts as a call
does. A step its gates allowed is among the turn's calls a judge of a
later call sees, as running until its client reports how it ended:
`{"op":"step_result","bot","turn","call_id","step","failed"}` records
`step_completed` (`call_id`, `step_of`, and `failed` when it did), so the
step counts as done; told twice it keeps the first, and a step no gate
allowed is `no_allowed_step`. While any step of a
call waits, the call's time stops: it cannot run out, its parked turn has
no deadline, and once the last step is decided it gets back what was left.
A step lives only as long as the request waiting on it: asking for one
already waiting is `step_waiting`; a client that closes its session takes
its waiting steps with it (`step_decided` with `its client stopped
waiting`), an answer to the call ends them (the waiting request gets
`no_pending_step`), as does the turn's end (`stale_turn`), and a restart
drops them all. Schema 63 adds the steps' and paused calls' columns. Protocol 24
also adds `history_search`, the history tool's query for clients (see
`history_nodes` above).

Protocol version 29 lets `inherit` be a count of generations as well as
`true` or `false`: a tool declared with `"inherit": 2` goes to the bots
its bot creates with `"inherit": 1`, to theirs with `false`, and no
further, as a packet's hop count runs down. `false` is no generation and
`true`, still the default and still left out, every one. A fork is a copy,
not a generation, and keeps the count its source has. A client bounding
how deep its agents delegate gives its tool for starting them that count,
so each bot's tools are right from its first turn.

### Tool approval

A bot may be created with a gate (one no bot made, which a client
created or forked, can have all its gates replaced later with `configure`,
its source's included): `approve`, the tools whose calls wait for a
verdict; `approver`, an opaque tag naming who answers, which the daemon
stores and reports but never reads; and optionally `approve_expire_ms`.
`approve` may name no more tools than the daemon registers and a bot may
declare (`invalid_gate` otherwise); a gate's tools must be among the new
bot's own, the daemon's and those its client declared or it takes from its
creator or source
(`approve_not_in_tools`). Without them none of this runs and nothing is paid. `fork` takes the same
fields. A bot keeps every gate it descends from: a fork keeps its source's,
and a `create` or `fork` whose `created_by` names a bot adds that bot's.
Each gate is kept to the new bot's tools, and gates with the same tag merge,
keeping the shorter expiry. A bot carries at most 8 gates: a `create` or
`fork` that would give it more fails with `gate_limit` and writes nothing.
`inspect`, `bots`, `created`, and `forked` report `gates`. The design and its reasoning are in [APPROVALS.md](APPROVALS.md).

- **Announcement.** The commit that records a model response with gated
  calls also writes an `approval_requested` event for the round:
  `{"calls":[{"call_id","request","announced_ms","gates","name","node"}]}`.
  A round whose calls take more than 256 KiB is announced in several such
  events, in order, so each one pages.
- **Answer.** `{"op":"answer","bot","turn","call_id","request","tag"?,"decision":"allow"|"deny","reason"?,"unreviewed"?,"by"?,"lease"?}`
  records one gate's verdict on the call's current request; `tag` may be
  left out when the call has one gate, and `reason` goes only with a deny
  (`invalid_reason` otherwise). `unreviewed: true` marks a deny nobody
  assessed (the approver's review failed or timed out): it counts toward
  no breaker, as in Codex, and goes with a deny only
  (`invalid_unreviewed` otherwise). `lease` is for a session serving `tag`
  (below): the answer renews the lease, and one under a lease that ended
  gets `approvals_lost` and changes nothing; a lease without a `tag` is
  `invalid_lease`. An answer without one is an override, whoever serves
  the tag. The reply lists the gates still
  `pending`. Errors: `no_pending_approval` (unknown call, request, or tag),
  `approval_superseded` (an earlier request), `approval_already_answered`
  (this gate answered, or the call denied already),
  `approval_expired` (the gate's expiry has passed, even if the turn has
  not yet denied the call), `approval_tag_required`, `stale_turn`, and
  `turn_not_found`.
- **Verdicts.** A call needs an allow from every gate, and the first deny
  denies it. An allow rides the call's `tool_started` commit, which gains
  `approvals: [{tag, by, allow, waited_ms}]`. A deny is the call's result,
  `{"error":"approval_denied","detail":REASON}`, and its `tool_completed`
  carries `denied: true` and the same `approvals`, every verdict the call
  got, so an allow before the deny is kept; the turn goes on.
- **Hold, then park.** A verdict for a running turn is held by the storage
  worker, which wakes the turn's task, until the call's start, its denial,
  or a park writes it. A gated call waits live for its bot's `approval_hold_ms`
  (default 2,000; 0 parks at once), then its turn parks like `wait`: one
  commit, `turn_waiting` with `approval: true`, no task, no active slot, and
  it survives restart. A verdict for a parked turn is committed on arrival
  and resumes the turn only if it parked on that call and the call is now
  decided; a verdict for a call after a `wait` waits in the store.
- **Expiry.** With `approve_expire_ms`, a call still without that gate's
  verdict that long after it was announced, however long the calls before
  it ran, is denied with "not reviewed: no verdict" (`tool_completed`
  carries `expired: true`, and its `approvals` add each lapsed gate with
  `by: null` and `allow: false`), and the turn ends
  `interrupted` with `approval_expired`. When a call has several gates, a
  deny decides it only if it came before an open gate lapsed, and an allow
  on one moves the turn's wake-up to the next gate's lapse. A turn parked
  on a `wait` or on a verdict ahead of another gated call is woken when
  that call lapses, and the lapse ends it: the wait returns every result
  stored by then and the rest as pending, and a call still waiting ends
  with the rest of the round, as not run.
  No call starts, gated or not, once a later call of its round lapsed.
- **Rounds.** A verdict is for the round as planned. When a call fails (an
  error result, a denial, or a command that did not succeed, as the tool
  reports it rather than as its output reads), every gated
  call of the round still to run is announced again in that call's
  finishing commit, with the next `request` and `failed` naming the call.
  Their earlier verdicts are dropped, and answers to the old request get
  `approval_superseded`. Not when one of them lapsed while the failing
  call ran: a new request would restart its clock, so the round keeps its
  requests and the lapse ends the turn at the next call.
- **Listing.** `{"op":"approvals","bot"?,"tag"?,"after"?,"through"?,"limit"?}` lists
  calls still waiting on a gate in announcement order, each naming only its
  unanswered gates, with `expires_ms` and `arguments`: the call's top-level
  fields, each cut to 2,048 characters on its own, so a long `content`
  hides no `path`. A string keeps its text; a longer other value becomes
  its JSON text so far. `arguments_cut` names the fields cut short and
  `arguments_omitted` counts those left out (past 8, repeated, or with a
  name over 128 bytes); `arguments` is null when they are not a JSON
  object, which no tool accepts. The previews are taken once, when the
  call is planned, so a listing reads no item. One bot's listing reads
  only its running turn's calls, and one tag's only the calls still
  waiting on that tag, through an index of each call's tags: written with
  its announcement, and a tag taken out when its answer is stored (every
  tag, for a deny). An answer the storage worker still holds skips the
  call's entry without reading its row. A page reads at most 1,024 calls
  or index entries, skipped ones included, so a page can come back short
  or empty with `next_after` set. A page holds at most `limit` (1 to 256, default 64) calls and
  256 KiB, or the one call when it alone is larger (bounded by the 64 KiB
  call id and the previews, well inside a 1 MiB line); `next_after` continues
  it, and `through` ends it at a position. Positions are never reused, and a call announced again takes a new
  one at the end, so a listing that pages on finds it. `stats` reports `approval_requests`, the calls
  announced and not yet started or denied: a count the storage worker
  keeps at each announcement, start, denial, and turn end, as it does
  `queued_turns`, so `stats` reads no approval row. Each listed call also
  carries `denials`, the bot's counts under each of its open gates (next).
- **Denial counts.** The daemon counts, per bot and gate tag, the denials
  that tag gave in the turn that gave the last one: `in_row`, ended when a
  call every gate allowed starts, and which of the turn's last 50 decided
  calls were denied, Codex's breaker window. A denial in a
  new turn starts both from none, so the breaker that ends one turn does
  not refuse the next. They are
  written in the commit that records the denial; an allowed start writes
  only while a denial is still in the window, and the count is dropped
  once none is, so a bot never denied writes nothing. A lapsed gate's
  denial, and an `unreviewed` one, are not counted: nobody judged the call.
  `inspect` reports `denials: {tag: {in_row, window, turn}}` while one is
  counted, `window` the last 50 decided calls as bits, newest lowest, set
  for a denial; a listed or served call carries `{tag: {in_row, recent}}`,
  `recent` the denials in that window, as seen from its own turn, so an
  approver that restarts or takes over counts on where the last one left
  off. Schema 54 turned a stored count of N a turn into N denials as the
  window's newest calls, since their order was never kept.
- **Serving a tag.** `{"op":"serve_approvals","tag","lease_ms","limit"?}`
  hands the session the tag. The reply is the first page of the calls
  waiting on it, as `approvals {tag}` lists them, with the `lease` number,
  `lease_ms`, and `through`, the newest position when serving began; the
  session pages the rest with `approvals {tag, after, through}`. From then
  on it receives, for each round announced for the tag, the
  `approval_requested` event's envelope with `tag`, `lease`, and
  `durable: false`, whose `data.calls` are only the calls waiting on the
  tag, each as a listing shows it, and `failed` when the round is
  announced again. A round whose calls take more than 256 KiB comes in
  several such messages, numbered by `data.part` of `data.parts`, so the
  holder knows when it has the whole round. The listing and the start of delivery are read in
  one storage job, so a call waiting on the tag is on a page or arrives
  after, not both; a listed call announced again takes a new position past
  `through` and arrives pushed. One session holds a tag: another gets
  `approvals_served` (also for duplicate registration on the same session)
  while the holder's session is open and its lease
  runs or its serve reply is not out yet. The holder keeps it with `{"op":"renew_approvals","tag","lease"}`
  or an answer under the lease, each at least every `lease_ms` (100 to
  600,000), counted from when its serve reply is queued. A renewal on the
  main socket is answered as it is read, ahead of the session's requests
  still queued for the service loop, since it touches no storage; pushes follow
  the reply, so neither the rest of a storage group nor a burst of pushes
  uses up the lease or the reply's room. An accepted leased answer holds
  the tag through storage and reply backpressure, restarting the period
  when its reply is queued, including an error. A holder whose lease ran
  out keeps the tag until any session next serves a tag, which ends the lease (and
  another may take the tag over under a new one), or until it renews or
  answers, which is refused; either way it is sent
  `{"event":"approvals_lost","tag","lease"}`, and pushes stop. A closed
  session frees its tags at once. A `limit` outside 1 to 256 is refused
  before any lease is made. Lease numbers change at
  every takeover, start from a random point each run, and stay below
  2^53; a lease holds only on its own session.
  `stats` lists the served tags in `approvers`. The stream is for the
  holder alone: bot followers get the compact event. Pushes to a holder
  wait in order for room in its output, so a group commit's burst does not
  drop a holder that is reading; one with 128 pushes waiting has stopped
  reading and is closed, like a lagging follower. A lease that ends drops
  the pushes still waiting for it, one already waiting for room in the
  output included, and sends only `approvals_lost`. Each round is held
  once, however many tags
  its calls wait on, and a tag's messages are built only when a session
  serves it.
- Interrupting a turn cancels its gated calls like any planned call.
  Anything that reaches the socket can answer, a bot's own shell included:
  this is oversight, not containment.

The CLI turns a mode into a gate. `run --new --approval manual`, or
`AGENT_APPROVAL=manual`, gates every tool of the new bot but `history`,
`wait`, `note`, and `echo`, tagged `manual`; `--approve LIST` picks the
tools instead, and `fork` takes both. `full`, the default, gates nothing.
`auto` gates the same tools but `read`, tagged `auto`, with a 105 s
expiry (the approver's 90 s review budget plus margin), and refuses with
`approver_not_serving`, before any bot is made, when no session serves
`auto`: the CLI judges nothing. The approver is a client of its own,
`sonny approver [--tag TAG] [--judge PROVIDER/MODEL] [--effort LEVEL]
[--note FILE]`, which Sonny keeps as the daemon's companion: it serves a
tag and has a judge decide every call waiting on it, printing one JSON line
per round ([APPROVALS.md](APPROVALS.md#automatic-mode)). `agent approvals [--bot NAME] [--tag TAG]` lists pending
calls, `agent answer --bot NAME --turn N --call ID --request R allow|deny
[--tag T] [--reason TEXT]` decides one and refuses to run inside a bot's
tool shell, and `run --pretty` prints each pending call with the commands
that allow or deny it.

### Tool-result elision

Most of a long tool-using turn is tool output the model has already read.
At a round boundary, after steers are absorbed and before compaction, once
the window and its pinned context hold `compact_at` percent of the byte
envelope, or the current turn cannot fit at all, alone or beside the
summary, pinned context, and notes sent ahead of it, the daemon moves the
bot's elision floor. Every tool result at or below the floor goes to the model as
a stub instead of its output: a result for the same call id that states the
output's size, the `read` reference that returns it whole
(`artifact: "result/NODE"`), and its first and last 256 bytes. Everything
else, the model's own messages and calls, user prompts and steers, and
small results, stays verbatim, and no call is ever separated from its
result. There is no model call. The floor goes through the newest item the
verbatim tail of `compact_keep` percent cannot take, and never past the
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

A result is elidable when its stub saves at least 1 KiB. A result that
carried a picture is stubbed as its text alone, with a line that the file
holds the picture if the model needs it again, and a prompt that came with
pictures as its words, with a line on how many were left out; what either
saves is counted with the pictures at their weight, and any saving makes
one. A request takes at most 100 pictures and 20 MiB of their bytes beyond
weight (Anthropic takes 100 pictures and 32 MB in a request): past either,
before a call, the floor moves, for any bot, until at most 50 pictures and
10 MiB remain, the newest, so it moves seldom. Each node records how many
pictures it has. For a bot with
`read`, the stub is made when the result is recorded, with the node id its insert takes, and stored
beside it. Each node records what its stub saves and, like its byte total,
the cumulative savings along its lineage; each floor version records the
cumulative savings through its floor. A node's bytes as sent are then its
byte total less the smaller of the two, so the window, turn admission, and
compaction accounting stay constant-time lookups and read no item. A
request reads the stub in place of the result's row. The stored transcript
is never rewritten: `history_items`, `history`, and forks see every result whole
(history reads leave out a picture's bytes, counting them in `omitted_bytes`),
and `read` with `result/NODE` returns one on the bot's own lineage. A
shell result is one JSON line, often longer than a `read` page, so that
read splits lines longer than 4 KiB into numbered pieces and pages them.
A page stays beside its call until the model answers it, where neither
elision nor a cut can take it, so it takes at most the room the running
turn and what goes ahead of it leave. A piece wider than that room shows
in part under its own number, with a notice to read that offset again when
the turn has more room, which a cut inside the turn makes; numbering pieces to
the room would make an offset name other text on a later read. With no
room for any of it, the read fails with `read_context_exhausted`.
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
`compact_at` percent of either envelope, or the view would send
`compact_tokens`, the daemon summarizes everything
older than the newest boundary whose tail holds `compact_keep` percent
verbatim, limited by the room left after pinned context. A boundary is a
turn's prompt or, [inside the newest turn](#cuts-inside-a-turn), a round
start: the first item after a tool result that is not one, a model output or
an absorbed steer. The item dimension can choose the cut even when small messages
have barely consumed the byte budget. The summary is one model call, to the
bot's own model or the `compaction_model` the client named at creation (same
family; another family's items cannot be replayed to it).

The token threshold is what Claude Code and Codex compact on; bytes alone
compacted far too late, since the 8 MiB default envelope is about two million
tokens, past any model's window. Unset, it follows the turn's model the way
they compute it: the context window less 20,000 tokens kept for the reply and
a 13,000-token buffer, and at most 90% of the window (900,000 on a 1M window,
167,000 on 200K). The window is the one the provider's model listing gives
(`context_tokens`, as `provider_models` answers) once a client has asked
for one, as a model picker does; the windows of the last listing that
answered are kept apart from it, read without parsing it again (a turn never
asks a provider for anything but its call). Otherwise, as for a provider that
lists none, Claude models get the window Anthropic publishes, 1M from Claude 5
on and 200K before, and any other model 200K, the smallest a current model
has. A
view's tokens are estimated from the bot's
last call: the bytes it sent (view, instructions and tools) against the input
tokens the provider counted for them, cache reads and writes included,
kept on the bot (`sent_bytes`, `sent_tokens`, schema 52). The threshold then
acts as a smaller byte envelope, the one whose `compact_at` percent is the
view that sends `compact_tokens`, so `compact_keep` and elision's own
threshold and saving scale with it. Before a bot's first call, or after one
whose provider ran fallbacks, there is no estimate yet and only bytes apply;
when instructions and tools alone pass the threshold, no summary can help
and only bytes apply too. The input limit stays the byte envelope.

A client may also ask for a summary with `compact` (protocol 17;
`agent compact --bot NAME`,
Sonny's `/compact`). It is kept on the bot, so a parked turn or a restarted
daemon still takes it: a running turn's next round boundary, or an idle
bot's next turn before its first call, summarizes everything older than the
newest boundary, whatever the thresholds, as an overflow would, before the
call rather than beside it. It stays pending until that summary ends, a
summary that parks on a closed pool included. The answer
names the bot, its identity and `running_turn`; a request while one is
pending answers `duplicate: true` and changes nothing; a bot without
compaction instructions answers `compaction_unavailable`. Followers get a
live `compaction_requested` event, then the usual `compacted` or
`compaction_failed`.

When the summarizer is the model the bot's last call ran on (the bot's, or
the turn's override), and a view a call sent holds the whole span, the
request may be a copy of that call, as Claude Code and Codex send theirs:
the bot's instructions, tools, tool choice, reasoning, cache key, and
routing token, the prefix and window that call sent, read under its elision
floor and thinking strip, then one user item, the compaction request,
carrying the client's compaction instructions and the maximum summary size.
The view is the last call's in this task, or, before the task's first call,
the view as the bot's call before the new prompt or wait sent it through its
newest boundary. That holds only when a call on the same model sent the view
last, with no summary or elision rewriting it since: this turn's own, or,
before this turn's first, the previous turn's, when that turn left more than
its prompt. After a turn that failed before a call, no call sent that
history. The copy takes what the call sent ahead of its window, so a
note written since does not show, and the window from before any stubs this
boundary made.

Each summary goes the cheapest of three ways, estimated from the byte sizes
the store keeps, a byte the provider's cache should hold counted at a tenth
(what Anthropic's cache reads and OpenAI's GPT-5 family bill): a copy of the
window through the span's end, a copy of the whole window, or a request of
its own. The Responses cache reads any prefix of a call it holds, so there a
copy ends at the span and reads all but the request from cache. Anthropic
reads a cache only at a breakpoint, and the call put its own where it ended,
so there a copy through the span reads only the tools and instructions,
while a copy of the call's whole window reads all but the request; the
result the model answered since stays out. A whole copy over the input
limit is trimmed to the span rather than dropped, and a catch-up step's copy
ends at its span. The estimate takes the call to be in cache; it does not
model a cache that expired while a tool ran or the bot sat idle. The
`compacted` event's `request` says which way went (`form`, `copy` or `own`,
and `items`, the window items copied) with both estimates, `null` where a
way could not be sent, and whether the summary ran `beside` the turn (below). A summary that parks on a rate limit
keeps in its park record the floor that window was read under, where it
starts, where it ends when the copy had the call's window whole, and that
prefix when the view no longer sends it, so its retry, after a restart
too, sends the same request, though the view since holds another round. A summary
the budget forced keeps that prefix, and its retry reads the window from
the saved start the call's window was read from, past the budget, since
the view it summarizes holds one round more than the call it copied. The copy does not set `tool_choice`, which on
Anthropic would invalidate the message cache, so the request asks for text
and a reply that calls a tool (`compaction_tool_call`) or has no text
(`empty_summary`) is billed and not installed. As Claude Code does, the
summary is then asked for at once in a request of its own, below, if the
bot's budget and round limit allow another call; the `compaction_failed`
event for the copy says `"fallback": true`. The summary covers everything
the copy shows, the part of the verbatim tail a whole copy sends included.
Otherwise (no view a call sent holds the span, a copy that cannot fit the
input limit, or another summarizer, which cannot read that call's cache, as
when a turn overrides the bot's model and the bot's summarizer summarizes
it, or the next turn inherits the view that model sent), or when it is
estimated cheaper, the request is one of its own: the
compaction instructions as its instructions, the previous summary
first, if any, so the summarizer merges rather than restarts, then the
span's items as stored, then a request to write. Anthropic requests of this
form retain the bot's tool definitions because the span may contain native
tool-use/result blocks, and set `tool_choice: {"type":"none"}`; the runtime
borrows the already encoded tool selection. Responses requests of this form
send an empty tool list, which that family permits with historical calls.

Either way the call is paced, retried, billed against the bot's budget, and
counted as a model round like any other; if a summary before the call parks
on a closed pool, the turn parks. Stored history is not rewritten for summarization.
The park record identifies the unfinished call as summary or ordinary model
work. Resumption, including after restart, continues that call. Once a summary
exhausts its retries, parking the following ordinary call does not restart the
summary's retry budget; a later model round may attempt compaction again.
Successful responses, including unusable empty or oversized summaries, are
charged durably before continuing. Usage events identify `purpose: compaction`;
`compaction_text_delta` and `compaction_thinking_delta` are separate from answer
streams. Budget and round limits are checked again before the normal call.

#### Summaries beside the turn

Inside a turn, behind a call this task made, a due summary runs beside the
boundary's own call rather than before it, as in Pi Durable
([announcement](https://earendil.com/posts/pi-durable/), 2026-10-01). At the
boundary it is planned, its copy chosen and the view it copies read, as
above; then its call and the turn's call go out together, the turn's
sending the view as it is. Once the turn's call is recorded, the round
waits for the summary and installs it before the call's tools run, so the
view still changes only between calls, and a park, an approval, or a
resumed turn never sees a summary running. The turn saves the shorter of
the two calls; what it pays is that one call sends the longer view. An
interrupt cancels a summary still running, like the turn's own call; one
that has landed waits outside the turn's rounds until its install is
queued on the store, and is billed and installed as the turn ends. The
install and, when it fails, the summary's bill are one store job.

It runs beside only while the view and the reply's estimated size still
fit the input limit and, beside what it holds, the round limit and the
bot's token budget leave room for the turn's call. It holds two model
rounds (a copy and a request of its own) and, under a budget, the most
those two may bill: each sending at most the view, the tools and both
instructions at a token to a byte, and generating at most the summarizer's
output bound; with no known output bound (a Responses provider without
`max_output_tokens`), a budgeted bot's summary does not run beside. A summary beside the call that an interrupt cancels still adds its
retries and pacing to the turn's. A refresh does not keep warm the view of
a call whose summary has written its replacement.
Otherwise, and at a turn's start or as a parked turn resumes, when the
cache the copy reads may have lapsed, the summary goes before the call. A
summary beside the turn uses its own connection rather than the bot's
WebSocket, which the turn's call holds, and counts toward the turn's
rounds, budget, and retries when it is joined. One that parks on a closed
pool parks nothing; a later boundary summarizes again once the pool's
retry time has passed, unless the view outgrows the limit first, which
summarizes before the call as above. A park keeps that retry time, so a
turn resumed before it goes on with its calls rather than parking on the
summarizer's pool. The time the turn waits for it counts toward the turn's
`summary_ms`.

The result is recorded in one transaction: the summary, the covered turns'
user prompts verbatim (each up to 2 KiB, with a 16 KiB budget for text plus
entry metadata, the oldest and newest kept when there are more), and the cut, the prompt node the verbatim
tail starts at, which becomes the context start. A compaction stands for
everything since the first: its coverage starts at turn 1 and the kept
prompts carry over. Its version is anchored to the head at which it was installed, separately
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
as it is; the window's own overflow handling still bounds stored items. A
summarizer the daemon no longer serves (`provider_unavailable`), or serves
in another family than the bot's stored items (`provider_family_mismatch`),
is reported the same way without a request.
Planning runs on the reader connection: nodes are immutable and only the
running turn moves its bot's head, so the reader's snapshot plans what the
worker would. Indexed byte/item accounting sizes the unsummarized span first,
without walking the transcript. A span larger than the context budget, left
by failed summaries or a round that outgrew the budget, is caught up oldest
first. Each round boundary summarizes the longest run from the previous cut
whose summarizer request fits the budget, including the previous summary
and the request marker, and without the thinking blocks such a request
leaves out: each node sums its lineage's thinking bytes, as it sums its
bytes (schema 32; nodes stored before count none). The cut moves to the next prompt, or to a round
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
still returns its preceding lines and the next offset. A PNG, JPEG, GIF or
WebP file comes back as the picture itself, after a line naming its kind, size
and bytes, in each family's result shape: a list of an `input_text` and an
`input_image` data URL for Responses, a `text` and a base64 `image` block for
Anthropic. One longer than 1568 pixels on a side is scaled down to it, and one
over 1 MiB as sent is re-encoded as a JPEG, so a screenshot fits what every
provider takes. Any other binary file is `file_not_text`, naming its kind, and
an image that cannot be decoded is `invalid_image`. One that must be scaled
but has more than 64 megapixels is `image_limit`, refused before it is
decoded; scaling runs off the daemon's thread, one picture at a time. `write` creates parent
directories and writes up to
1 MiB. `edit` replaces an exact string that must occur once unless `replace_all`
is set. `apply_patch` takes Codex's patch format: one patch adds, deletes,
updates and moves any number of files, every change checked before any file is
written, so a patch that does not apply changes nothing. Its context lines
match exactly, then ignoring trailing and surrounding whitespace, then with
typographic punctuation read as ASCII. On Responses it is a custom tool whose
call is the patch itself, constrained by its Lark grammar, and its result a
`custom_tool_call_output`; on Anthropic, a function taking the patch as
`input`. Paths are relative to the workspace unless absolute; nothing confines a
tool to the workspace, and tools run with the service's OS permissions.
Native file tools accept regular files, including symlinks to regular files.
Known pipes, devices, and other special files return `file_not_regular`. Opens
are nonblocking and the opened descriptor is checked before reading or
truncating, so replacing a checked path with a FIFO cannot block a filesystem
worker or daemon shutdown. Use the shell tool for special-file operations.

`web_search` searches the web the way the bot's provider offers it, as Codex
does. On OpenAI's API with a key it is Codex's `web.run` function in the `web`
namespace, which the daemon answers by posting the call's commands to the
provider's `alpha/search` (bounded at 1 MiB and two minutes) and returning its
`output`. Searches are paced in a pool of their own and their failures classed
as a model call's: a rate limit closes the pool, a server or connection failure
backs off, and both retry within the two minutes before the model hears of them. Bedrock's GPT models get the hosted `web_search` tool over indexed
results, and Anthropic's API gets its hosted server tool, `web_search_20260209`
for Claude 4.6 and later but Haiku, `web_search_20250305` otherwise; the
provider runs both, its blocks and citations are kept in the history as sent,
and a `pause_turn` stop asks again without a new message. A ChatGPT plan login,
Claude on Bedrock and any other gateway offer none: `create` refuses the tool
there with `web_search_unavailable`, and `ready` says per provider whether it
searches (`web_search`). It runs without approval, as Codex's does.

`web_fetch` reads one public http or https URL from the daemon, so it works on
every provider, Claude on Bedrock and a ChatGPT login included. It follows up
to ten redirects within 30 seconds, reads at most 4 MiB of the body, and
refuses a type that is not text (`web_fetch_not_text`). Text is read as UTF-8,
or as windows-1252 when the response or an HTML page's `meta` names Latin-1;
another charset is refused (`web_fetch_charset`) rather than misread. An HTML
page comes back as Markdown with its title and links made absolute, converted
off the runtime's thread and cut at 8 MiB; other text as it is. Text
past 64 KiB is shown as its start and end and kept whole as the call's `text`
artifact, up to 1 MiB, which `read` pages. A status outside 2xx marks the
result failed. Unlike search, it is gated like any other tool, and `auto`
judges it, since the URL it is given leaves the machine.

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
of replay on follow replacement, explicit store selection despite an inherited
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
round, so compaction leaves about `compact_keep` percent verbatim however
the view is split into turns; a turn smaller than that is never cut. The
summarizer reads the span up to the cut, which ends with a whole exchange:
every call in it has its result, and the tail begins with the model's
next output, or with a steer absorbed after that exchange, so no call is
separated from its result on either side and a steer at the boundary goes
verbatim.

The turn's prompt stays in view whole, and so does every message sent to
the turn before the cut: a steer or an attached message. The compaction
records the prompt as the version's `pinned` node and those messages as
its `pinned_messages`, and while the window starts at that cut, the
request carries the prompt as its first item after the pinned context,
then those messages in the order they came, then the items from the cut.
So a correction sent mid-turn never reads older than the prompt it
corrects, as it would if only the summary told of it. They count against
the room the prompt does; when the active turn would not fit beside them,
the oldest go into the summary first, since a later message may overrule
an earlier one but not the other way round. The summary header says `covering turns A to
B and the start of turn C`, or `covering the start of turn C`, and its
list of verbatim user messages leaves out turn C's prompt, which follows
in full; the list keeps it for when a later compaction covers the whole
turn. The context note counts the messages it leaves out of turn C
separately from earlier turns. A second cut in the same turn summarizes
from the first, merging the previous summary, and keeps the same prompt
and messages, with those sent since the first cut after them.
Once the turn has ended, a later cut at a turn's prompt covers it whole.
The turn keeps running for its clients throughout: events, steers,
approvals, and its checkpoint are unchanged; only what the next request
carries differs. Admission for steers, notes, and `history` results counts
the turn from its prompt and cut, as the window does. A historical fork
binds the version at or before its checkpoint and restores its cut, so a
fork from inside a split turn sees that prompt, the messages that version
kept, and the tail from the cut. A turn covered whole has its steers
summarized with its rounds. Schema 30 adds the `pinned` column; every
earlier cut is a turn's prompt, so none has one. Schema 65 adds the `pins`
table of kept messages; cuts made before it keep the prompt alone.

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
