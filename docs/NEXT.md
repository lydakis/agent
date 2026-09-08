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

1. Validate provider startup/admission on bounded real-provider workloads. The
   synthetic 1,000-stream reset reproducer, host backlog evidence, and bounded
   startup results are recorded in LIFECYCLE_MEASUREMENTS.md. Preserve failures;
   do not claim that this validates 1,000 durable/tool-equipped agents.
2. Separate long-term conversation storage from bounded model context. Implement
   indexed history access and context selection before claiming long-history
   support. Follow [LONG_HISTORY.md](LONG_HISTORY.md), including preserved fork
   ancestry and versioned compaction with originals retained.
   Add bounded model-facing history retrieval and branch-aware fact checks;
   measure fixed active context against growing stored histories before adding
   recursive context-processing machinery.
3. Add Unix-socket attachment and race-safe automatic daemon startup.
   Exercise delegation through the same client: a program running in Bob's
   workspace creates Alice fresh and forks a retained checkpoint as another
   named agent, submits work, and follows both results. Verify independent
   continuation, explicit cancellation scope, and shared admission/resource
   limits. Use ordinary agent operations rather than a separate subagent engine.
4. Validate a second provider, then add usage/context budget accounting before
   freezing the provider interface. Current live-provider behavior is unverified.
5. Extend measured tools and recovery semantics, slow-reader and sustained-load
   tests. Profile CPU/allocations to explain regressions; compare matched revisions.
6. Add equivalent lifecycle adapters for Pi/Codex only where native semantics can
   satisfy the same contract. Unsupported guarantees remain an explicit gap.

## Stop conditions

- The result is only names and flags over one full process per bot.
- Savings depend on bypassing host/provider boundaries, copying credentials, or
  quietly weakening the declared durability or recovery behavior.
- Lower memory comes from silently dropping context, tools, permissions, or
  observability rather than more efficient execution.
