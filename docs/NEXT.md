# Next decisions

This is an investigation queue, not a commitment to build every capability.
No further Errand demonstration or feature work is needed for this question.

The [measurement tools](BENCHMARKS.md) now exercise Pi's real agent/model packages
and a shared Codex app-server against the same synthetic Responses endpoint.
Every request is checked for retained conversation history. The bounded matrix
varies simultaneous agents and history length. Source ownership maps for Pi and
Codex are recorded in RUNTIMES.md. The custom Rust core now runs the same
streaming workload. Continue reusing standard infrastructure and profiling tools.

The [first matrix](MEASUREMENTS.md) passed through 32 simultaneous streams for
both engines. George subsequently selected a custom Rust harness as a performance
engineering project. The [prototype](RUST_PROTOTYPE.md) implements the bounded Bob
scenario and shared streaming core. The [new screen](RUST_MEASUREMENTS.md) compares
all three engines, with unequal feature footprints. The [comparison contract](COMPARISON_CONTRACT.md)
now prevents treating these as efficiency rankings. The [durable Rust screen](LIFECYCLE_MEASUREMENTS.md)
measures the service and tool workload separately.

## 1. Compare execution cores

Compare Codex app-server, Pi's separable agent/model packages, and the smallest
new loop that meets the fundamental contract. Rust is the selected language for
that experimental core. A Codex fork is an option, not the default decision.
Use OpenCode and Claude as additional references where they answer a specific
ownership or provider question; avoid an open-ended survey before measuring.

Deliverable: a small source-backed ownership map and capability matrix. It must
show which resources are shared, which are per session, which are per turn,
and which remain unknown. Trace context copies, provider connections, scheduling,
tool/MCP lifetimes, history retention, and event delivery. Separate code verified
at pinned revisions from package documentation. If existing components meet the
performance and behavior requirements, reuse them.

## 2. Freeze a narrow software contract

Freeze the minimum operations for named identity, submit/resume, inspect/follow,
steer/interrupt, and checkpoint-based historical forks. Decide who owns history
and which boundaries are resumable. Initial tool policy is full access, with
one policy hook before execution. Keep programmatic input requests extensible;
an approval UI and a rich permission language are deferred.

Deliverable: a controller creates Bob, completes several turns, forks an earlier
checkpoint as Bob-alternative into a caller-supplied workspace, and continues
both branches independently. It reconnects from an event cursor, resumes the
same identity after release/reload, and receives explicit errors for a missing
session or invalid fork point. State where history lives and who owns its
lifetime. A conversation fork must never imply a filesystem fork or duplicate
historical tool side effects.

## 3. Establish the performance case before product scaffolding

Define numeric acceptance criteria before interpreting measurements. Build a
deterministic synthetic streaming provider and tool fixtures to isolate harness
overhead without paid model calls. Use a bounded concurrency ramp with explicit
memory, process, and run-duration limits; report achieved active concurrency
separately from configured or queued work. Measure the fixture's own resource use
separately, and flag candidates that cannot exercise the same workload.

Vary history length, fork count, stream chunk size, tool output, and follower
speed. Measure memory per loaded/active agent, CPU per event/turn, network bytes
and connection reuse, tail latency, and teardown. Include shared history retention,
slow consumers, cancellation, and recovery. Compare equivalent durability,
permissions, contexts, and tools, including total descendant resource costs.
Keep native headless CLI execution as a baseline alongside shared engines.

Deliverable: an adopt/reuse/fork/build decision grounded in active resource cost
and the feature contract. Synthetic results do not establish real-provider
capacity or model quality. Follow with small real-provider checks under a stated
call/token/spend cap. Agree on a concrete budget before large or open-ended paid
runs; thousands of active agents is not an excuse to launch a costly fleet now.

## 4. Extend the measured Rust slice

Use the measured boundary to implement the smallest core with durable identities,
branchable history, tool dispatch, cancellation, and bounded structured events.
The first slice may own the model/tool loop. Start with one model provider, then
validate a second before freezing the provider interface. Preserve provider
details and advertise unsupported capabilities. Use supported authentication and
the documented full-access policy within caller-supplied host permissions.

Add behavior tests for independent branches, retained shared history, exact
resumption or explicit failure, stale steering, tool-call policy correlation,
cancellation, and replay gaps. Use the synthetic workload as a repeatable
performance regression fixture. Human-readable logs remain a thin optional view.

## Immediate queue

The [Prime Intellect investigation](PRIME_INTELLECT.md) adds Prime Agent as a
long-history reference and Verifiers v1 as a candidate evaluation adapter. It
does not establish resource measurements or change the selected Rust core.

The [FX native screen](FX_MEASUREMENTS.md) adds a pinned embedded-core baseline
and possible upstream benchmark contributions. Gateway/Responses protocol and
resident feature differences remain explicit; this does not change the selected
Rust experiment or establish parity for durable/tool-equipped agents.

Implemented: explicit comparison profiles, sanitized benchmark failure codes,
opt-in bounded shell execution with process-group cancellation, bounded request
startup, and a Rust-only
lifecycle/feature screen. Historical measurements remain labeled exploratory.

The first Interrogate review is complete. Its four accepted fixes cover canonical
store ownership, byte-bounded replay, duplicate reconciliation under admission
pressure, and selected provider-credential filtering for tools. Before expanding
the protocol, also settle durable/live event alignment, slow-consumer handling,
store versioning, and total versus idle provider deadlines. Conservative uncertain
tool outcomes remain explicit; caller-directed resolution is future work.

The 2026-09-14 slice (daemon, `agent` client, two provider families, file
tools, per-turn workspace and model, review fixes) is described in
[RUST_PROTOTYPE.md](RUST_PROTOTYPE.md), with measurements in
[DAEMON_MEASUREMENTS.md](DAEMON_MEASUREMENTS.md) and the bounded live check in
[OPENAI_SMOKE.md](OPENAI_SMOKE.md). Git history carries the per-fix detail.

Implemented next: deferred tool results (`wait` as a tool, a protocol op, and a
CLI command; `shell` `background`; turn and process handles with durable
process results; parked turns that survive restart and resume their remaining
calls) and the three configurable daemon limits in place of fixed constants; see
[RUST_PROTOTYPE.md](RUST_PROTOTYPE.md#deferred-tool-results). Still to measure:
bytes per parked turn versus per live process, on the lifecycle screen.

1. Done: the [live fleet check](LIVE_FLEET.md) submitted batches of up to
   1,024 bots on gpt-5.6-luna and 256 on Sonnet 5 through one daemon. With
   sharded HTTP/2 connections and no startup bound, the 1,024-bot luna batch
   reached 805 overlapping turns, 1.924 s median turn latency, and 41.23 MiB
   peak daemon RSS. The Sonnet batch reached 256 overlapping turns with
   2.050 s median latency. These bursts do not establish those costs at
   1,024 simultaneous provider streams. Failures included one 503 per
   1,024-bot run and a burst of transport failures whose cause was not
   captured; transport failures now retain their cause chain as detail.
   A [sustained run](LIVE_FLEET.md#sustained-load) of 64 bots for five
   minutes (9,050 turns, 60 model calls per second) showed no drift in daemon
   memory, threads, or open files and no provider rate limit; the store grew
   2.8 KB per turn. Retention is now explicit: `delete` frees a bot and its
   exclusive history, `prune` keeps the newest N turns' records, and
   `--retain-turns N` applies prune after every turn; see
   [RUST_PROTOTYPE.md](RUST_PROTOTYPE.md#retention) and the measured growth in
   [LIVE_FLEET.md](LIVE_FLEET.md#retention-under-sustained-load). What still
   grows per turn is the transcript itself, which is compaction's job. Next at
   scale: hours rather than minutes.
2. Done: stored history is unbounded; each request carries a
   [context window](RUST_PROTOTYPE.md#long-history-and-context-windows) of
   whole turns with a persisted, hysteretic start, an explicit omission note,
   and a `history` tool that reads any earlier turn by ordinal along the
   lineage. [Measured](DAEMON_MEASUREMENTS.md#long-history) at 1k, 10k, and
   100k stored items with a fixed 64 KiB window. Remaining: compaction, in
   the layers of the [compaction plan](LONG_HISTORY.md#compaction-plan):
   first elide old tool results into artifact stubs (no model call), then a
   pinned agent-owned note the model rewrites with what it will need, and
   only if the task-quality evaluation (item 15) still fails, incremental
   model summaries as versioned context views that forks bind to. Read the
   prior art named there first (Prime Agent, Pi, Codex, Claude Code, FX).
   Every layer is measured on the same long conversation for tokens per
   turn, cache-hit ratio (item 13), compaction cost, and task quality; a
   compaction that rewrites the prefix every turn can cost more in cache
   misses than it saves in tokens. Also remaining: checkpoint indexes so
   forks and reads of very old turns stop walking node metadata.
3. Store scale, in two steps. First, done: the
   [query-plan audit](DAEMON_MEASUREMENTS.md#query-plan-audit), now covering
   93 runtime statement variants, found four full scans by unindexed `status` (startup
   recovery, parked-turn resumption, the idle-exit check); schema 12 adds
   partial indexes on the active statuses, taking each from tens of
   milliseconds per million rows to microseconds. Second, after compaction: a
   store-scale screen that grows one
   store with the synthetic provider to 1 GB and then 10 GB across thousands
   of bots and, at each size, measures daemon start and recovery, submit to
   finish latency, window construction, `bots` and `turns` paging, a fork, a
   delete, and a migration, with RSS and WAL size sampled throughout. The
   2 MiB page cache means the hot indexes eventually stop fitting; the screen
   should find where that cliff is and how WAL checkpoints behave under hours
   of writes. Seeding costs no provider spend, only background time.
4. Done: the [ten-thousand-bot screen](LIVE_FLEET.md#ten-thousand-bots)
   through the protocol. Synthetic: 10,000 bots created in 1.5 s, all
   submitted with 1,024 in flight throughout at 1,470 turns per second,
   5,000 parked on one anchor for 0.8 KB each, a restart with 840 in flight
   ready in 154 ms. The [heap profile](DAEMON_MEASUREMENTS.md#heap-profile-at-the-fleet-peak)
   at that peak attributes 34 MiB live: three quarters is HTTP/1.1
   per-connection state that real HTTP/2 providers do not incur, the
   daemon's own per-active-turn cost is a 3.1 KB task future, and RSS
   exceeds the live heap by about 25 MiB of allocator retention and mapped
   code. Live on gpt-5.6-luna the burst spent this organization's
   allowance, 5,000 requests and 4,000,000 tokens per minute, in five
   seconds; 8,039 of 10,000 turns were refused, and the provider's edge
   reset streams en masse, which the HTTP/2 client's flood protection turned
   into whole-connection failures. In-stream rate limits are now
   `provider_rate_limited`. The next fleet run should follow item 10, not
   precede it. The shaving order, if a workload ever needs it: the turn
   future's size, HTTP/1.1 buffers only for such a provider, then the
   allocator.
5. Done, and smaller than first written. Background commands accepted but
   not started are bounded by the process bound itself: one counter and one
   comparison, `capacity_exhausted` as the tool result beyond it, the count
   in `stats`. The operating system bounds processes; it never sees this
   line, which is the only reason the daemon has to. The durable dispatcher
   is not built: nothing measured needs a backlog that survives restart, and
   it would be a subsystem. `timeout_ms` stays a bound on running time, since
   the line is now bounded by count rather than by the clock. Context
   read-ahead batches are capped at 256 KiB as well as 64 items. Regression:
   one process slot, a running command, one in line, the next refused, the
   accepted ones resolving in order.
6. Done: [fleet controller ergonomics](RUST_PROTOTYPE.md#fleet-controllers).
   `follow` with `bot: "*"` subscribes one socket session to every bot with
   replay from a store-wide cursor; `wait` with `any: true` answers on the
   first resolved handle and leaves the rest valid, in the op, the tool, and
   across restart; `stats` reports sessions, turns against their bounds,
   daemon-wide in-flight requests per shared client shard, every pool's
   learned allowance and level, store and WAL size, the storage worker's queued versus running
   time, and the handle registry. CLI: `follow --all`, `wait --any`, `stats`.
   The bench drivers can now read the daemon instead of sampling it; moving
   them over is a follow-up when one is next touched.
7. Never silently ignore explicitly requested daemon configuration. Today the
   first client's `--provider` and `--tools` bind the daemon, and a later
   client's different values are ignored while it runs; only a restart with
   changed values fails. A program that asks for one configuration and runs
   against another has been misled. The client should compare every
   daemon-scoped option it was given against the effective configuration the
   daemon reports at attach and fail with the difference named; per-turn model
   and workspace overrides stay as they are.
8. Done: [delivery modes](RUST_PROTOTYPE.md#delivery-modes) on `submit`.
   `reject` is the old behavior. `queue` is a durable turn row in `queued`
   or `ready` state, started by the service when the bot and a slot are
   free; the line is per bot in submission order, one ready head per bot,
   and ready heads start oldest first. `steer` is a queued row the running
   turn absorbs at its round boundary as a user item, finishing the steer as
   `steered` with `into`; a steer that misses the last boundary starts as a
   turn. Both survive restart. Not built: a way to flush a bot's whole line
   in one call, and a per-bot cap on queued work; `turns` lists the line
   and `interrupt` ends one entry at a time.
9. An ACP bridge: a separate process speaking the Agent Client Protocol to an
   editor or agent client on one side and the daemon's socket protocol on the
   other, with no daemon changes. Create, submit, streamed text and thinking
   deltas, tool events, interrupt, and resume are all already in the
   protocol. This is the human way in; software keeps the protocol. Queued
   here because it is wanted soon, not because it changes capacity.
10. Done: [pacing and retries](RUST_PROTOTYPE.md#pacing-and-retries), the
    flood-control slice. Per-provider, per-model pools learned from the
    providers' own rate-limit headers, a fair FIFO gate at the model-call
    boundary, estimate-then-correct token accounting, refusals that hold the
    pool rather than fail the turn, bounded retries at the call boundary
    with backoff and spread, `retries` and `paced_ms` per turn, and 64
    streams per connection. The [matched follow-up](DAEMON_MEASUREMENTS.md#pacing-review-fixes)
    records CPU, memory, latency, and measurement noise after the review fixes; the
    live ten-thousand-bot rerun is recorded in
    [LIVE_FLEET.md](LIVE_FLEET.md#ten-thousand-bots-paced). The original
    design notes follow. Provider failure policy and pacing, the flood-control slice. 503s and one
   burst of transport failures each became a failed turn for the caller to
   resubmit. The fundamental concept is per-provider pacing: a rate and an
   in-flight cap per provider that every call, first attempt or retry, passes
   through, so one scheduler spaces the fleet deterministically instead of
   thousands of agents retrying in lockstep. On top of it: a model call that
   fails before any tool ran has had no effect and is retried by construction
   with exponential backoff bounded by attempts and total time; one that
   fails after a tool ran is not, and stays a failed turn. A 429 with
   `Retry-After` drops that provider's pace to zero until the time passes,
   with queued turns waiting rather than failing; a 429 that signals an
   exhausted quota rather than a rate, or one with no `Retry-After` that keeps
   recurring past the backoff bound, fails the affected turns promptly instead
   of pausing forever. A restart with thousands of resumable turns ramps
   through the same pace instead of firing at once.
   Jitter is only needed where daemons are the independent clients, several
   hosts on one provider key, and a small random spread on retry and resume
   delays covers that. No rate limit has been reached yet; the 10,000-bot
   screen may find one, and that run should come first so the policy is
   shaped by an observed limit: gpt-5.6-luna on this organization allows
   5,000 requests and 4,000,000 tokens per minute, reported inside the
   stream as `rate_limit_exceeded` with "please try again in N ms" in the
   message and no header; the pace has to parse that. Retries happen at the model-call boundary,
   never by replaying a turn, and are observable: attempt counts, provider
   request ids, and retry delays in the turn record. Leaving retries off by
   default is acceptable while the policy is new.
11. Make `process_lost` unmistakably different from a stopped process. Shell
   cleanup relies on a process-group guard and kill-on-drop, which cover normal
   cleanup and cancellation but not a hard kill of the daemon; a child can
   keep running and writing after its parent dies. The tools section says
   so, but other recovery text and the store-initialization comment say
   background commands "died with" the daemon while initialization only marks
   their rows `process_lost`. Reconcile the contract to: supervision ended, the
   command may still be running or may already have had effects, and a
   controller must not read `process_lost` as permission to start conflicting
   work in that workspace. Extend the restart test to observe a filesystem
   write after killing only the daemon, not just the recovered handle's status.
12. Retention correctness follow-ups. `prune` keeps turn rows, so submission
   deduplication by `(bot, request_id)` survives pruning, and an expired event
   cursor is answered with `pruned_before` and a `pruned` notice rather than an
   empty page. Two intersections remain open: `delete` removes a bot's turn
   rows with it, so a late retry of a deleted bot's request gets
   `bot_not_found` rather than a duplicate (acceptable, but state it); and
   `prune` drops a bot's old artifacts even though a fork reading through its
   lineage could still ask for them, so either artifacts referenced by
   surviving branches stay alive or the fork's read answers with an explicit
   retention error. Retention and expensive historical reads should run in
   bounded pieces on the storage thread once the queue-wait instrumentation
   exists.
13. Cache-hit accounting. The window's hysteresis exists to keep provider
   prompt caches warm and the daemon already receives cached-token counts,
   but records only cache-inclusive input tokens. Record and report the hit
   ratio per turn and per bot; it decides whether the three-quarters rule is
   right, with a live long-conversation run as the check.
14. Mass interrupt. Interrupting one bot is tested; stopping a thousand at
   once, how long until their processes are gone and their turns durable, is
   not. Cancellation latency is on the unmeasured list and matters most for
   fleets.
15. Make context management accountable for task quality, not only cost. The
   window, the omission note, and the `history` tool answer whether context is
   cheap to build; they do not answer whether the agent finishes correctly
   when what it needs has left the window. Build a small evaluation where a
   constraint appears early, enough work follows to push it out of the window,
   and a later decision depends on it; measure whether the agent retrieves it
   and acts on it, not whether the bytes are reachable. Run it before and
   after compaction lands, since compaction changes what the model sees.
   Summaries stay versioned context views, never replacements of history, and
   historical forks bind to the view valid at their checkpoint.
16. A mixed-workload soak, replacing the single-purpose slow-follower and
   parked-agent screens. Synthetic provider, no spend: large contexts, noisy
   shell output that overflows into artifacts, background-command bursts,
   parked parents waiting on children, slow socket followers, historical
   forks, and injected provider failures, all at once for an hour. Measure
   actual provider streams separately from active turns, storage-queue wait
   and execution time, cancellation latency, pending background work, and
   descendant-process resources, not only daemon RSS. Alongside it, a small
   set of real repository tasks with objective tests on a controlled model
   and starting state: completion, tokens, wall time, and recovery behavior,
   so the results say something about the harness and not the model. Idle
   exit, schema versioning, budgets, turn listings, `result`, and
   model-facing artifact reads are implemented and belong in that soak.
17. Provider interface: decide whether cross-family handoff (thinking rendered
    as text, tool history preserved) is worth a translation step, then freeze
    the adapter contract.
   Separate runtime byte limits from model-context budgeting, and replace the
   fixed Anthropic output ceiling and the `legacy_thinking` name-prefix match
   with a small per-provider, per-model capability configuration. Preserve
   native provider state; do not force every family into identical semantics.
18. Extend measured tools and recovery semantics, slow-reader and
    sustained-load tests. Profile CPU/allocations to explain regressions;
    compare matched revisions.
19. Add equivalent lifecycle adapters for Pi/Codex only where native semantics
    can satisfy the same contract. Unsupported guarantees remain an explicit
    gap.

Kept out of the queue: process sandboxing, which is the host's job as the
tools section says.

## Stop conditions

- The result is only names and flags over one full process per bot.
- Savings depend on bypassing host/provider boundaries, copying credentials, or
  quietly weakening the declared durability or recovery behavior.
- Lower memory comes from silently dropping context, tools, permissions, or
  observability rather than more efficient execution.
