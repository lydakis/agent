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
store versioning, and total versus idle provider deadlines. Unknown tool
outcomes are explicit results in history; they never disable the named bot.

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
7. Done: a running daemon is never silently different from what a client
   asked for. The client compares every daemon-scoped value it stated with
   the daemon's `ready` and fails with `daemon_configuration_mismatch`
   naming each difference. The store no longer binds a provider set or
   toolset at all: providers come and go between runs, and a bot's provider
   is checked by family before admission and again before queued work starts.
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
11. Done: `process_lost` means supervision ended. The recovery text now
    says the daemon no longer owns the process and never records its result,
    that a hard kill of the daemon leaves children running, and that a
    controller must not read the code as permission to start conflicting work
    in that workspace. The restart test kills only the daemon and observes the
    background command's write landing afterwards, not just the recovered
    handle's status.
12. Done: retention intersections. A late retry of a deleted bot's request
    answers `bot_not_found`, stated in the retention section. An artifact
    read for a turn retention has emptied answers `artifact_pruned` for the
    producing bot and for a fork whose transcript holds the output node,
    through the protocol operation and the model's `read`; the transcript
    nodes that retention keeps decide it, so `artifact_not_found` and
    `turn_not_found` keep their meanings. Deleting the producer after a fork
    inherited its output answers the same way. Still open from the item:
    retention and expensive historical reads in bounded pieces on the storage
    thread; the operation histograms from item 27 show where they wait.
13. Done: [cache-hit accounting](DAEMON_MEASUREMENTS.md#cache-hit-accounting).
   `cached_input_tokens` and `cache_hit` per turn and per bot, and
   daemon-lifetime totals in `stats`. The live check on luna and Sonnet
   (0.72 and 0.85 overall) shows the ratio is set by how often
   the window start moves: every move is one full-miss turn, the turns
   between hit at 0.91 and 0.95. A lower hysteresis target would
   raise the ratio at the price of less average context; that is item 15's
   call, so the three-quarters rule stays until the quality evaluation
   exists.
14. An exploratory [mass interrupt screen](DAEMON_MEASUREMENTS.md#mass-interrupt)
   measured terminal events for a thousand bots in 0.2 s mid-request and
   0.8 s with shell commands. Repeat with tracked process identities before
   claiming that every child has stopped. The screen exposed a
   contract gap: stopped bots refused further work. Cancellation and crash
   recovery now close unanswered calls with honest results and keep the same
   named bot usable. Planned calls are cancelled; executing calls without a
   committed result report `tool_outcome_unknown`, including that execution
   may still be running. Nothing is automatically retried. Version 20 repairs
   previously blocked bots once at store open. Automatically continuing
   crash-interrupted model work remains a separate policy decision; explicit
   stops must stay stopped.
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
20. Done: the daemon supplies no implicit agent behavior. The split that pays is mechanism in the daemon and policy
    in the client: anything that must be true for every client at once
    (durable truth, shared pacing and pooling, processes, cancellation, the
    model and tool loop, the per-turn invariants, resource limits and
    protocol defaults) stays in the daemon; anything that is an opinion
    about an agent belongs to whoever is asking. Rendering, the delivery
    default (`AGENT_DELIVERY`), and configuration judgment (item 7) already
    live in the client. Three opinions remain in the daemon, and each is
    also a configuration axis that can mismatch. The distinction to keep is
    choosing a value versus retaining it: once a bot exists its model,
    instructions, and tools are durable with it, and resuming or forking it
    never depends on the environment of whichever client connects next.
    Model and instructions are already retained in the bot's row, so those
    two moves only change who supplies the value: `create` always names a
    model (the CLI keeps the user's in `AGENT_MODEL`) and always sends
    instructions. Tools are the real change: today they are daemon-wide,
    so the daemon should register the universe of tools at start and each
    bot select its set at `create`, durably, with the schemas the model
    sees filtered per bot and the selection enforced at dispatch, not just
    hidden from the model; definitions stay shared, never copied per bot.
    Heterogeneous fleets need this anyway, and it retires the toolset
    mismatch check. Afterwards daemon configuration is store, socket,
    providers, and limits; everything about an agent is stated per bot or
    per turn. Two slices: explicit model and instructions first, then
    durable per-bot tool selection. Two cautions bound this: client-side
    work is paid per invocation, and every bot's shell tool that runs
    `agent run` is a client, so shared mechanisms must not move and the
    measurement is daemon plus clients plus tool processes, including
    frequent CLI invocations and detached execution; and the screen, not
    the principle, decides whether a move was free. Removing a default
    string saves little; the benefit is heterogeneous bots sharing one
    runtime and its connections, with any performance change demonstrated,
    not assumed. (Refined with Astra's review of the item.) Done in two
    slices: `serve` takes no model, instructions, or tools; `create`
    requires all three; the CLI resolves them from `--model`/`AGENT_MODEL`,
    `--instructions`/its built-in text, and `--tools`/its default set; a
    bot's tools are shown to the model and enforced at dispatch, with
    definitions held once and encodings once per distinct selection. The
    daemon's configuration is store, socket, providers, and limits.
21. Done: durable events are published by the storage worker in commit
    order, through one publisher, with turn outcomes for waiters behind
    the events that end them. Tasks and the service no longer publish or
    resolve waiters, the absorbing turn's cancellation guard is gone, and
    a `*` follower's cursors rise strictly and equal the replay. (From the
    second Astra Pro review.)
22. Done: each live turn carries its own steer flag; a boundary with
    nothing waiting is one atomic swap, and one bot's pending steer costs
    unrelated bots nothing.
23. Done: strict steering with `expected_turn`, `agent run --delivery
    steer --turn N`.
24. Done: [paced turns and active slots](DAEMON_MEASUREMENTS.md#paced-turns-and-active-slots).
    Measured first: when a throttled provider's retrying turns filled
    `--max-active`, a healthy provider's queued turns did not start at all.
    Now a turn whose pool is closed by a rate limit for 250 ms or more
    parks at the model-call boundary as a durable `paced` row with a resume
    time, holding no task and no slot; the service resumes it when due and
    it re-enters the model call. Admission waiters park too, including those
    already queued when the pool closes. The unfinished call's attempts and
    retry-time budget persist across parks and restarts; cumulative turn
    retries stay separate and count only dispatched retries. No second scheduler.
    Still open from the item: pending submissions want their own count and
    byte bound, separate from the active-turn bound.
25. Done: pacing inputs per provider. The pool key is the family's
    (dated snapshots share their alias's pool); the estimate is a cost with
    input and output shares, paced per dimension the provider publishes
    (Anthropic's input and output token limits alongside the total).
    Unknown pools admit freely within caller-selected local resource limits;
    reported limits and 429s supply pacing feedback. No hidden cold-start cap.
    No model registry: a shared quota the provider does not name is still
    corrected by every response's headers.
26. Absorption against context capacity. A boundary drains the whole
    steer snapshot in storage batches, so the absorbed total can exceed
    what the next request carries; the same is true of any turn whose own
    items outgrow the window. Budget the boundary against encoded context,
    leaving excess steers queued, as part of the compaction work.
27. Done: storage counters by operation. Every store job is labeled by
    the method it performs, and `stats` reports per operation the count,
    queued and ran totals, the slowest run, and two fourteen-bucket
    log-spaced latency histograms, at one short lock per job. Totals and
    histograms come from one consistent snapshot, formatted outside the lock. The
    store-scale screen (item 3) reads them. (From the second Astra Pro
    review.)
28. Done: bot identities. A bot has a store-wide integer `id`, allocated
    from a sequence and never reused after delete; `create`, `fork`,
    `resume`, `bots`, and `submit` report it and the `created` and `forked`
    events carry it. `submit` accepts `bot_id`, and `run --bot-id N`: a retry
    pinned to an identity the name no longer holds answers `bot_not_found`
    with the current identity in `detail`, so a recycled name cannot absorb
    a stale retry as fresh work. A fork is its own identity with an empty
    request namespace. One primary-key lookup per submission. (From the
    item 12 discussion.)

Kept out of the queue: process sandboxing, which is the host's job as the
tools section says.

## Stop conditions

- The result is only names and flags over one full process per bot.
- Savings depend on bypassing host/provider boundaries, copying credentials, or
  quietly weakening the declared durability or recovery behavior.
- Lower memory comes from silently dropping context, tools, permissions, or
  observability rather than more efficient execution.
