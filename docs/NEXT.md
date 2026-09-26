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
   grows per turn is the original transcript. Context compaction does not
   shrink that stored history; lossless storage work is item 38. Next at
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
3. Done: store scale, in two steps. First, the
   [query-plan audit](DAEMON_MEASUREMENTS.md#query-plan-audit), covering
   93 runtime statement variants, found four full scans by unindexed `status`;
   schema 12 added partial indexes on the active statuses. Second, the
   [store-scale screen](DAEMON_MEASUREMENTS.md#store-scale): one store grown
   to 1 GB and 10 GB across 4,104 bots. In the corrected mixed workload,
   startup, recovery, and bounded paging took tens of milliseconds; sampled
   daemon RSS peaked at 55.7 MiB. A heavy text turn averaged 8 ms of storage
   work with a 7.24 MiB request body, at at most eight concurrent heavy turns.
   Deleting a bot with 596 turns and 298 shell outputs blocked the storage
   worker for 693 ms. Cache state was uncontrolled; this is not a cold-cache
   or capacity claim. Both stalls are addressed by items 29 and 30. Still
   open: the WAL under hours of writes (item 16).
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
16. The [mixed-workload soak](DAEMON_MEASUREMENTS.md#mixed-workload-soak)
    ran sixty minutes with 192 bots in seven roles on the synthetic
    provider: 224 thousand turns, 1,771 compactions, interrupts, forks, and
    injected failures, no failed turns, flat daemon memory (36.6 to 36.8
    MiB), and a SIGKILL restart with 16 turns in flight. The original slow
    followers did not throttle and its replay check could accept missing
    events. Both observers are corrected; repeat the long run before
    claiming backpressure coverage or stream equality. The store grew
    21.7 MiB per minute, predominantly history retained by design. Combined
    reader/writer execution time does not establish writer saturation;
    measure them separately before claiming a capacity limit. The other
    half, real repository tasks with objective tests, is now item 36.
17. Provider interface: decide whether cross-family handoff (thinking rendered
    as text, tool history preserved) is worth a translation step, then freeze
    the adapter contract.
   Separate runtime byte limits from model-context budgeting, and replace the
   fixed Anthropic output ceiling and the `legacy_thinking` name-prefix match
   with a small per-provider, per-model capability configuration. Preserve
   native provider state; do not force every family into identical semantics.
   [Bedrock](BEDROCK.md) is now a binding: `bedrock` and `bedrock-openai`
   sign with SigV4 through the AWS credential chain, and the two rules it
   broke were fixed in place (Claude names are read inside Bedrock ids, and
   `--max-output-tokens` reaches Anthropic's `max_tokens`). Both fixes are
   still name- and flag-shaped; a per-model capability table would absorb
   them. Live runs on 2026-09-25 carried every current feature over on
   Mantle and runtime, for Claude and GPT-5.6, except server-side fallbacks,
   which Bedrock refuses.
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
    The pending-submission bound followed as item 31.
25. Done: pacing inputs per provider. The pool key is the family's
    (dated snapshots share their alias's pool); the estimate is a cost with
    input and output shares, paced per dimension the provider publishes
    (Anthropic's input and output token limits alongside the total).
    Unknown pools admit freely within caller-selected local resource limits;
    reported limits and 429s supply pacing feedback. No hidden cold-start cap.
    No model registry: a shared quota the provider does not name is still
    corrected by every response's headers.
26. Done: absorption against context capacity. A boundary takes steers only
    while the running turn's own items plus each encoded steer stay within
    the window's three-quarter target of the context budget; the rest stay
    queued and start as their own turns when the line moves, so a burst of
    large steers can no longer make the running turn fail with
    `context_limit`. A turn whose own tool outputs outgrow the window is
    still bounded only by the 64 KiB preview and the round limit; that
    belongs with compaction (items 15 and the context work).
    The [active-steering follow-up](DAEMON_MEASUREMENTS.md#active-steering-follow-up)
    found 3.9–6.4% higher daemon CPU with flat memory and increasing absorption
    cost as the current turn grows. [Indexed accounting](DAEMON_MEASUREMENTS.md#indexed-turn-accounting)
    now replaces those walks with fixed-count indexed lookups. The matched
    follow-up returns absorption time near the pre-budget baseline, with
    overlapping CPU and memory ranges; small CPU differences remain, so this
    is not a universal non-regression claim.
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
29. Done: [retention in bounded pieces](DAEMON_MEASUREMENTS.md#retention-in-pieces-and-the-storage-reader).
    `delete` and explicit `prune` run as series of storage jobs of four
    turns each, so other bots' commits interleave with a large deletion; the
    bot is marked `deleting` from the first piece and refuses work, and an
    interrupted deletion finishes at the next open. Closes the item 12
    leftover.
30. Done: a [storage reader](DAEMON_MEASUREMENTS.md#retention-in-pieces-and-the-storage-reader)
    connection on its own thread streams context items into model requests,
    so a long history's window is no longer read on the thread every other
    bot's commit waits for. Only byte-returning reads move; decisions stay
    on the worker.
31. Done: pending-submission bounds. `--max-pending` and
    `--max-pending-bytes` bound submissions waiting to start, daemon-wide,
    answering `pending_limit` before anything is written; the storage worker
    keeps the count and prompt bytes at each transition and recounts them at
    open, so admission and `stats` cost the store nothing. A first version
    used store triggers and cost 8% daemon CPU on the 32-agent screen and
    17% burst throughput on the ten-thousand-bot screen, so it was replaced
    before commit. Unbounded by default: waiting work is durable
    rows, and the bound exists for an honest admission answer, not memory.
    Performance follow-up: the [alternating comparison](DAEMON_MEASUREMENTS.md#alternating-follow-up)
    leaves a small CPU cost unresolved; do not call this performance-neutral.
32. Compaction, the context work. Slice one is done: the
    [evaluation](DAEMON_MEASUREMENTS.md#context-quality-before-compaction)
    from item 15, rerun on the corrected evaluator: luna honors the rule
    8/8 with it retained, 0/6 on the final file and 0/26 on fillers once
    the window has dropped it, and never called `history` in 266 turns
    although every request after the window moved named it. Read with the
    [survey](COMPACTION_SURVEY.md): the daemon already has the shape the
    research favors, whole old turns dropped and recoverable by number, and
    the failure is that the model cannot see what it is missing. The
    remaining slices, each measured with the evaluation:
    - Slice two, done: the [legible omission
      note](DAEMON_MEASUREMENTS.md#the-legible-omission-note). The request
      lists the omitted turns, ordinal and the first line of each prompt,
      newest first up to `--note-turns`. Data, not an instruction. With the
      rule omitted, luna's final file went from 0/6 to 7/7 and half the
      conversations read turn 1 through `history`; the rest acted on the
      examples still in view, which the bare count had not made them do.
    - Slice three, done as a mechanism: the [carry-forward
      note](DAEMON_MEASUREMENTS.md#the-carry-forward-note), a `note` tool
      that writes or replaces a bounded text pinned ahead of the window,
      recorded with the tool result and versioned by its node, so forks
      bind to the version at their checkpoint. Offered to luna with no
      instruction, it was never called in 112 turns; the survey's finding
      holds here. Whether the client's default instructions should mention
      it is an open client-policy decision, to be made against that number.
      The fullness signal proposed for the omission note was not built:
      the listing already changed behavior, and a per-request status line
      would cost prompt-cache prefix stability.
    - Slice four, done: [compaction](DAEMON_MEASUREMENTS.md#compaction).
      Harness-triggered once the turns since the last summary hold
      `--compact-at` percent of the context budget, at a round boundary: one summarizer call under the client's compaction
      instructions, the bot's own model or a client-named one of the same
      family, over everything older than a verbatim tail, the covered turns'
      user prompts kept verbatim within bounds, versioned at the head with a
      separate cut so forks remain independent and history stays intact. No
      instructions, no compaction; the CLI ships a default text. On the
      evaluation the rule never left the request and 41 of 41 summaries
      restated it. The original cost and cache figures excluded summarizer
      usage; rerun with corrected accounting before claiming total cost
      (needs provider keys; still open).
      Stable summaries now precede changing omission notices and forks restore
      their inherited context start. A backlog larger than the budget is
      [caught up oldest first](DAEMON_MEASUREMENTS.md#compaction-backlog-catch-up),
      one summary per round boundary, each filling the summarizer's budget;
      the walk that finds the oldest turns runs on the reader in 1,024-node
      pieces, and an ancestor index was declined because every append would
      pay for it. Jev was not used: its probes showed it answers the
      judgments, but not that it beats the static rule of keeping user
      prompts verbatim, and the daemon knows structurally when a boundary
      is stable. If Jev has a place it is in permission automation, now
      [item 45](APPROVALS.md). Still open: a real 8 MiB window over a long task, Sonnet,
      the summarizer's latency at size, and an evaluation that can see a
      summary dropping something the verbatim prompts do not carry.
    Items 16, 17, and 19 follow this; item 9 is deprioritized, since the
    socket-protocol client already covers the human way in.
33. Done: budget the effective context view, including the encoded summary,
    retained prompts, carry-forward note, omission listing, separators, and
    item counts. The configured envelope remains the hard input limit. Known
    output caps supply a soft completion-headroom estimate, capped at a quarter
    of the envelope; this is not model token budgeting (item 17). Either dimension
    can trigger compaction. The tail target shrinks
    with pinned overhead, the summarizer receives a byte target, and a candidate
    that expands the view or crowds out the active turn is rejected and billed.
    Retained prompt copies fit an encoded prefix budget, preserving history
    retrieval; its allowance uses indexed active-turn and bounded prefix reads.
    Compaction events report before/after usage and remaining headroom, negative
    while a backlog still exceeds the input envelope. Context metadata/prefixes
    are prepared once per round and reused across retries; the old separate
    unsummarized-span query is gone from the turn loop. Performance evidence is
    recorded in [the matched screen](DAEMON_MEASUREMENTS.md#effective-context-budgeting).
    Within-turn reclamation remains item 34.
34. Compaction inside a running turn. Cuts land only at submitted-turn
    starts and the window must hold the whole current turn, so one long
    autonomous task with many tool rounds in a single turn still reaches
    `context_limit`. First, deterministic tool-result elision: in the
    request view replace older bulk tool outputs with a short stub naming
    the call, the size, and how to read it back, never touching the store
    and never splitting a call from its result; the observation-masking
    result in the survey makes this the baseline to beat. Then a cut at any
    completed tool exchange within the turn, the turn's prompt kept
    verbatim, the turn still running for everyone outside. (From Astra
    Pro's compaction review.)
35. Thinking-prefix compatibility. Anthropic binds preserved thinking to
    the request prefix on newer accounts; a compaction rewrites that prefix
    and the summarizer replays native items under other instructions. Read
    the current contract, test both paths on the Anthropic family with
    prefix enforcement, and either drop invalidated thinking with the loss
    reported or use the documented handling, before Fable is offered
    compaction. Benchmark a provider-native compactor behind the versioned
    view while there. (From Astra Pro's compaction review.)
36. The evaluation that challenges the summary, and the soak's second
    half in one: a single substantial repository task on luna that crosses
    several compactions, with facts that live only in tool results (a
    discovered restriction, a failed approach and why, a measured number,
    an operation with an unknown outcome) and a requirement that a later
    instruction supersedes. Branch from identical checkpoints into full
    context, omission listing, elision, and summarization with the same
    tail, plus prompt-excerpts-only. Score next actions and final tests,
    repeated investigations, replayed side effects, and failures right
    after a compaction; record the context-view version with each model
    call; measure total input and output, cache reads and writes, and
    summarizer latency per correctly completed task. Compare a few
    threshold policies on it before changing the 75/25 defaults. (From
    Astra Pro's compaction review, and the remainder of item 16.)
37. Context construction cost, measured before built. Item 33 shares an
    encoded prefix across retries, combines window metadata, removes the
    separate `unsummarized_bytes` lookup, and avoids full-window construction
    for history reads. Each model round still rebuilds stable pinned blocks
    and reruns the omitted-turns walk. Compaction validation reconstructs
    before/after/minimum views on the writer. Planning runs on the reader,
    with catch-up walks yielding every 1,024 nodes. Measure
    the walk on long tool-heavy histories and ordinary-turn latency during
    simultaneous compactions with the operation histograms; only then a
    bounded cache of encoded prefix pieces keyed by family, summary
    version, note version, window start, and listing size, and one context-plan
    operation. (From Astra Pro's
    compaction review.)

38. Lossless storage efficiency: [targeted artifact compression and shared
    large prompts](STORAGE_GROWTH.md#targeted-runtime-follow-up) are implemented
    and measured with schema 26. The 1,024-turn growth probe falls from 79.23
    to 45.68 MiB with identical node payloads. Varied-output daemon CPU and
    text-turn tails improve; ordinary lifecycle tails are essentially flat,
    with small median/RSS costs recorded explicitly. Transcript nodes stay raw.
    Next investigate cold-history storage, then repeat the corrected hour-long
    soak with periodic table attribution. Preserve exact history, historical
    forks, recovery, and bounded paging; stored transcript growth is separate
    from model context compaction.

39. Tool calls that do not hold up the model. Prompted by Unreal Agent
    ([source](https://github.com/unreallabsai/unreal-agent/tree/b7c9bf1c5c2fa4127255c07727a7c8413e23944a)
    at `b7c9bf1c5c2fa4127255c07727a7c8413e23944a`, read 2026-09-23; its
    [announcement](https://unreallabs.ai/blog/unreal-agent/), 2026-09-22),
    a Go harness benchmarked against the same Codex and Pi baselines.
    Source facts: every shell call starts at once as a background
    operation; until it finishes the model sees a fixed "still running"
    tool result; the finished result replaces that placeholder if no
    request has carried it yet, and is otherwise appended as a second
    `function_call_output` for the same call id
    ([`builder.go`](https://github.com/unreallabsai/unreal-agent/blob/b7c9bf1c5c2fa4127255c07727a7c8413e23944a/harness/contextbuilder/builder.go)).
    Results that land together share one model call, a 1 s grace after
    each response lets quick calls finish before the next call, user input
    arriving while calls run starts a model call at once, and a heartbeat
    wakes the model after ten minutes of nothing but running calls
    ([`loop.go`](https://github.com/unreallabsai/unreal-agent/blob/b7c9bf1c5c2fa4127255c07727a7c8413e23944a/harness/coordinator/loop.go),
    [`preamble.md`](https://github.com/unreallabsai/unreal-agent/blob/b7c9bf1c5c2fa4127255c07727a7c8413e23944a/harness/contextbuilder/prompts/preamble.md)).
    Its post claims up to 40% lower cost than Codex at equal or better
    pass rates; that is their documentation claim, and it mixes this with
    a 1.4 KB preamble, three tools, and no subagents without attributing
    the saving. Here, by contrast, a response's calls run one at a time, a
    background shell's result reaches the model only through a `wait`
    call, which costs a model round, and a steer waits until every call in
    the round is done. Two slices, each measured against the current loop:
    - Overlap the calls of one response that the runtime can show are
      independent: reads, and writes or edits to distinct paths. Tool calls
      carry no dependency metadata and a shell command's effects are
      unknown, so shell calls and anything touching a path an earlier call
      in the response touches stay in call order; the model already opts a
      shell into overlap with `background=true`. Results are recorded in
      call order. Today only shells take a process slot, and a read
      allocates up to the 4 MiB file limit, so overlap needs a bound over
      every tool, not only the process bound. A `wait` among the calls,
      cancellation, and restart mid-batch need defined behavior.
    - Deliver finished background results without `wait`: the call's
      tool result stays the handle (the real result for both families),
      and the finished output arrives at the next boundary as an item
      naming the call. The Responses shape above is not portable:
      Anthropic Messages, and so Bedrock, requires each `tool_use` to be
      answered once, in the next user message. A turn whose model stops
      while its background calls run parks without a slot until one
      finishes or a steer arrives, rather than ending. A fork at a
      checkpoint where a call was still running inherits neither the late
      result nor a rerun of the process. That needs enforcement, not only
      transcript placement: `proc:N` resolves by process id store-wide and
      a finished result can be waited on again, so a fork can already
      `wait` on a handle in its inherited history and receive its source's
      result. Scope process handles to the bot that started them, and
      answer an inherited one as unavailable. Compaction that covers the
      call before its result arrives must leave the late item legible.
    Measure with the synthetic provider and tool fixtures: model rounds,
    billed input, cached input, and output tokens, wall time, and daemon
    CPU and memory, on tasks with several independent commands of mixed
    duration, with and without a preamble sentence asking the model to
    issue independent calls together, since the gain depends on the model
    doing so. Ramp active bots against the shared bounds and report p95 and
    p99 turn and model-boundary latency and per-bot fairness, since one
    turn holding several slots can speed itself up by delaying others. A
    run counts only if it completes the same calls with the same tool
    results, filesystem effects, and final answer as the current loop;
    anything else fails the comparison rather than ranking as cheaper.
    Then a small real-provider check under a stated spend cap.
40. Done: a ChatGPT login that outlives one token. `--provider chatgpt`
    used to read Codex's `auth.json` once and never again, so a daemon that
    outlived the token failed every call until restarted. The login is now
    re-read when its token's `exp` claim passes, after pacing and admission,
    and on a 401 for its current token. A concurrent 401 for an older token
    reuses the login already installed. A changed token or account is retried
    without backoff, and every token is redacted from then on. An
    unchanged login is reported as `provider_login_rejected` naming the file,
    and an expired file as `provider_login_expired` naming the time.
    Codex still does the signing in. A success that names no content type
    and finishes without an SSE frame is reported as `provider_expected_sse`
    with the message it held. Transport failures and partial SSE frames remain
    retryable. Anthropic subscription use is deliberately not attempted: the
    terms are a gray area and the account is George's. Performance evidence in
    [the login screen](DAEMON_MEASUREMENTS.md#login-re-read-and-unnamed-bodies).

40. Responses over WebSocket, measured before kept. OpenAI's WebSocket mode
    keeps the latest response per lane in a connection-local cache, so a
    `store: false` call can send `previous_response_id` and only the new
    items; Codex uses it by default for API keys and the ChatGPT login.
    OpenAI reports about 40% faster loops of 20 or more tool calls, which is
    their claim, not ours. The prototype is built: family `responses-ws`,
    one connection per bot, delta input only when the request extends the
    previous one exactly, the full input on every other case including
    `previous_response_not_found`; the store stays the only history. Next,
    the matched HTTP versus WebSocket screen in
    [WEBSOCKET.md](WEBSOCKET.md#measurement-plan) under a spend cap, with a
    prompt cache key in both arms. Open: lanes to share a connection among
    bots, pacing without per-call headers, and HTTP after a failed upgrade.
41. Several daemons, moving bots, and watching them: the
    [provisional roadmap](MULTI_DAEMON.md) separates shared durability,
    identity, admission, and execution-ownership contracts from later
    mechanisms. Nothing is built. The next implementation is
    [bounded observation within one daemon](MULTI_DAEMON.md#phase-1-bounded-observation-in-one-daemon):
    a separate observer reader, bounded admission and output through socket
    delivery, bounded live fan-out, and ordered replay handoffs. Behavior
    tests and the predeclared CPU, memory, and tail-latency gates come before
    summary reads and client digests. Store identity and multi-daemon clients
    follow; cross-store forks, drain/move, and group moves remain separate
    provisional phases. Their open design questions do not block observation.
    One daemon per user stays the default until a matched screen supports
    changing it. Placement, file copying, and transport remain with callers.
42. Done: three follow-ups from the review of the 2026-09-25 merges. The
    Responses prompt-cache key uses a store-instance namespace plus the
    bot's id, not a per-daemon nonce. The durable lineage is drawn once at
    store creation (schema 28); the namespace also binds it to the physical
    file, so an idle-exit restart keeps cache affinity while a live copy
    gets its own. Both values are announced in `ready`. This is cache routing,
    not the full copy/restore cursor identity contract in MULTI_DAEMON.md.
    Anthropic server-side fallbacks are a bot's choice
    (`--fallbacks`, inherited by forks), off by default, because a fallback
    answers with a model the caller did not pick; the Harbor adapter asks
    for it. Detached shell commands are bounded by `--max-detached` (16)
    and reaped as they exit, so they neither pile up nor linger as zombies.

42. Anthropic prompt-cache refreshes during long tool calls, measured before
    kept. [Built](RUST_PROTOTYPE.md#keeping-the-anthropic-cache-warm):
    `--keep-warm` (240 seconds by default) resends the last call with
    `max_tokens: 0` while a tool runs. Anthropic recommends this over the
    one-hour cache only on Fable 5.1 and Mythos 5.1. `--cache-ttl 1h` is
    built, with one-hour writes priced at their own rate. A live check on
    2026-09-25 (Sonnet 5 with adaptive thinking and effort) found both
    accepted: each refresh read the whole prefix, with a signed thinking
    block in history too, and carried the cache over a 330-second tool that
    missed it completely with `--keep-warm 0`. The 2026-09-25 Sonnet 5 rerun
    found six replies that streamed past five minutes on their own, so a
    refresh now also runs while a reply streams. Next, a matched long-tool task
    with three arms: the five-minute cache alone (`--keep-warm 0`),
    `--cache-ttl 1h`, and refreshes. Anthropic shares a cache across an
    organization, so each arm needs bytes of its own, such as a nonce in its
    instructions, or one arm's refreshes keep another's cache warm. Open: parked
    `wait` turns, whose helpers can run past five minutes with no live task
    to refresh them, and Bedrock.

43. Forking a running bot. `fork` without a checkpoint refuses a bot whose
    turn is running, and a model has no way to find a closed node to fork
    at. [The design](FORK_MID_TURN.md) forks at the newest finished round,
    which the store keeps current per running turn so a fork reads no
    transcript, and keeps the source's window start. It adds an optional
    allowed-tools list checked at dispatch, which a fork inherits and can
    only narrow, so a fork keeps its source's tool definitions and prompt
    cache. The daemon and CLI add no text for the fork, and a fork takes
    no instructions of its own: done, it is an exact copy. Next, the store
    change with contract tests, including process handles and their stored
    output scoped to the bot that started them (item 39), then the list,
    then measurement of the cache and of the fork-or-fresh rule in the
    preamble.

44. Storage commits on a slow disk. Done: the worker commits in groups,
    one sync for the jobs that queued together, and callers are answered
    after it ([measured](DAEMON_MEASUREMENTS.md#group-commit): at a 10 ms
    sync, 64 sustained bots went from 26 to 37 turns per second; level at
    the VM's native sync). Completions now commit from their turn tasks,
    allowing independent finishes to group without blocking the service loop.
    The bot remains durably busy until commit, and publication keeps commit
    order. Same-bot retention jobs cross a publication boundary; independent
    bots still group. The [completion burst](DAEMON_MEASUREMENTS.md#completion-scheduling-and-macos-flush-attribution)
    improves substantially; ordinary streaming tails remain mixed. Next,
    admission and creation: the service still awaits their commits before
    handling another request. Letting those jobs group needs a design for
    capacity reservation, same-bot ordering, and item 21's guarantees. Client
    acknowledgements, publication, and provider execution must stay after
    commit. Also done: on macOS
    the store sets `fullfsync` and `checkpoint_fullfsync`, because a plain
    fsync there leaves commits in the drive cache. A flush costs about
    5.4 ms on an M1 Max, paid once per group; the service loop above now
    matters on a Mac as much as on slow Linux storage.

45. Approving tool calls. Every allowed call runs without a verdict today,
    and that stays the default. [The design](APPROVALS.md) adds two more
    modes, chosen per bot with `--approval` or `AGENT_APPROVAL`: `auto`,
    where a client answers from deterministic rules first, then asks Jev a
    few narrow questions per round, and denies what is dangerous or unclear
    without asking anyone; and `manual`, where a person or program answers.
    The daemon only gets an `approve` list of tools whose calls wait for an
    `answer` from any client, and an opaque approver tag: the request rides
    the plan commit, the verdict rides the call's start or its denial, and a
    verdict that has not arrived within a short hold parks the turn like
    `wait`. It has no rules, prompts, or model. It is oversight, not a
    sandbox. The Harbor tool mix (2026-09-26) sends at least 64 to 76% of
    rounds to Jev, about 1 to 2% of median trial time, and caps one Jev
    key at roughly 26 to 31 `auto` rounds a second at most; the count
    treated four git commands as read-only, which the design no longer
    does. A labeled Jev run on 341
    calls (2026-09-26, $0.07) answered in 0.26 s median with no false
    allows, but at the starting thresholds it refused 23% of benign calls;
    tuned thresholds cut that sharply. Built (2026-09-26): the daemon
    mechanism and `manual` mode with `agent approvals` and `agent answer`.
    Next, parked on 2026-09-26 behind within-turn compaction, the storage
    failure, and admission batching: the rules-only `auto` approver with
    `serve_approvals`, then Jev, and a labeled dangerous set to measure
    false allows before thresholds are fixed.

Kept out of the queue: process sandboxing, which is the host's job as the
tools section says.

## Stop conditions

- The result is only names and flags over one full process per bot.
- Savings depend on bypassing host/provider boundaries, copying credentials, or
  quietly weakening the declared durability or recovery behavior.
- Lower memory comes from silently dropping context, tools, permissions, or
  observability rather than more efficient execution.
