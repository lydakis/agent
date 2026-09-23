# Comparable work and feature costs

Decision, 2026-09-07: the earlier streaming tables compare the cost of different
configured execution paths. They do not isolate implementation efficiency from
feature costs. Do not promote their ratios as whole-harness efficiency wins.
Matching prompts and responses is necessary, but insufficient.

## What was actually running

| Capability | Our Rust prototype | Pi core/model packages tested | Codex app-server tested |
| --- | --- | --- | --- |
| Text streaming and retained history | Implemented; exercised | Implemented; exercised | Implemented; exercised |
| Named durable identities | SQLite service; exercised (one bot per agent, FULL durability) | Not provided by this in-memory Agent adapter; coding-agent session layer not loaded | Native threads; benchmark requests ephemeral threads |
| Restart/resume, historical forks | Completed checkpoints, replay, idempotent submit; behavior tested separately | Not exercised or wired into this adapter | Native resume/fork API; not exercised by this adapter |
| Tool loop | Echo; now optional bounded shell | Generic tools, execution modes, hooks and tool events available; empty tool set in screen | Native coding/tool machinery; shell execution disabled in screen |
| Steering and interruption | Exact-turn cancellation; no mid-turn steering | Abort, steering/follow-up queues | Turn steering/interrupt APIs |
| Models and modalities | One Responses subset; text and opaque reasoning preservation; no validated live provider | Broader provider routing, thinking, image input, usage accounting | Broader native model/transport/context machinery; one synthetic provider selected |
| Permissions | Full registered-tool access; caller host permissions | Generic before/after tool hooks; application policy outside measured core | Native policy/approval/sandbox architecture, configured full access for screen |
| Context management | Unbounded stored history; per-request window of whole turns (default 8 MiB/4,096 items) with an omission note and a history-read tool; no compaction | Context transformation hooks in core; application session compaction not measured | Native context/compaction machinery; compaction not triggered in short screen |
| Extensions and integrations | No MCP, skills, hooks, plugins | Coding-agent application not loaded; do not attribute its features to the tested core | Optional integrations disabled; disabled does not prove zero retained allocation |
| Client/runtime boundary | Native daemon driven over its stdio JSONL protocol by the observer; only the daemon charged | Node runtime plus Agent/core/model packages in one process | Node JSON-RPC client plus native app-server, both charged to target |

Evidence reviewed locally 2026-09-07: our source and adapters; installed Pi core
and model package READMEs/distribution at **0.85.1**; Codex source at
**0896bf6fc05ead454888b90044e1a08f99b6d778**. The tested native Codex binary is
**0.153.1**; source/documentation claims are not a claim that every API was tested
on that binary. The [pinned source evidence](RUNTIMES.md) supplies upstream links.
The installed [Pi core README](../bench/adapters/node_modules/@earendil-works/pi-agent-core/README.md)
and [Pi model README](../bench/adapters/node_modules/@earendil-works/pi-ai/README.md)
are the evidence for its broader capabilities (install the pinned packages first);
this experiment does not measure the complete Pi coding application.

We do not know how many MiB or CPU seconds each unexercised feature costs.
An import-cost probe identifies a package/runtime floor, not the allocation
cost of a particular feature. A serialization CPU profile identifies a hot path,
not whether removing all other features would close the gap.

## Enforcement

Each new streaming result records a versioned profile: exercised contract,
resident implementation boundary, and untested capabilities. `bench compare`
requires matching profiles, matching engine, matching workload/observer/host
settings, successful runs, and achieved provider concurrency before producing
regression percentages. Missing historical profiles fail closed.

`--exploratory` allows unmatched observations with explicit profile differences;
it omits percentage rankings. It never overrides failed runs, underachieved
concurrency, or incompatible measurement conditions. `bench.matrix` is explicitly
exploratory and uses this unranked output. A matching declaration is still a
reviewed assertion, not proof of every internal behavior or product feature.

The new durable lifecycle screen is **Rust-only** and has its own result schema.
Do not compare it to the ephemeral tables. Before comparing another engine,
implement the same retained history, tools/results, fork boundary, durable
acknowledgment, restart, replay, cancellation, output bounds, retry policy,
consumer pace, and process accounting. Mark unsupported semantics explicitly.
Different durability guarantees must not be relabeled equivalent simply because
both implementations write files.

## FX embedded comparison

The 2026-09-12 adapter adds the pinned native `libfx` 0.0.8 core, with Node and
its native bridge charged to the target. It exercises ephemeral text history
with no registered tools. Persistence, checkpoints, compaction, host tools, and
the CLI's broader provider/permission behavior are outside this screen.

FX uses Gateway SSE; Rust uses Responses SSE. Both fixture paths validate the
same complete synthetic conversation and deliver identical text at the same
scheduled delta intervals. Framing, terminal payloads, catalog requests, native
serialization, and residual runtime capabilities differ. The specific protocol
pair is allowed only in exploratory comparisons, with the difference listed;
all remaining compatibility fields must still match. No percentage ranking is
produced. See [FX measurements](FX_MEASUREMENTS.md).

## opencode server comparison

The 2026-09-23 adapter drives one pinned native `opencode serve` (1.18.32) with
one session per agent over its HTTP API and event stream. Node and the server
are charged to the target. It uses the same Responses fixture, with no tool
schemas and no title request. opencode's SQLite session store is still written
(WAL, `synchronous=NORMAL`), so its durability profile matches no other engine.
Its environment system block, lazy first-prompt initialization, and git project
probes are residual costs that cannot be switched off. Results are exploratory
only. See the [opencode adapter](BENCHMARKS.md#opencode-adapter) for every
known difference.

## How to attribute feature cost

1. Use the same binary/runtime configuration with one optional feature changed,
   where a supported switch exists. Keep the workload, durability, and output
   contract fixed. Report enabled-but-idle cost separately from exercised work.
2. Record what the switch actually disables, including eager initialization.
   Profile allocations/CPU to explain a difference; do not subtract unrelated runs.
3. Build a feature ladder: text core, durable service, tool round trips, recovery
   and forks, slow consumers, then long-history context construction/compaction.
   Each step is a new workload class, not a like-for-like speedup over the prior one.
4. Re-run a matched earlier profile after adding a feature to catch regressions.
   Maintain cross-engine observations as exploratory until the same contracts
   and measurement boundary have been independently verified.
