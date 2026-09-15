# Agent

A performance-first agent execution engine for software clients.

The goal is to let a program create named bots, give them work, steer or resume
that work, fork earlier conversation checkpoints to explore alternatives, and
follow activity as an ordered stream of events. Programs are the consumers.
Human-readable service logs are an optional view of that stream.

The named agent is the single execution unit. In the intended design, an agent
delegates by using the same CLI or protocol as any other program to create or
fork another named agent. All named agents are peers; origin does not confer
ownership, special permissions, or a different lifecycle. Models choose their tools and coordination strategies;
the minimal core supplies durable lifecycle, provider access, and bounded execution.

The reason to build this is efficient execution of many **active** agents. A
common command syntax around one full harness process per bot is not enough.
The ambition is thousands of active conversations; no capacity claim has been
established.

**Status: experimental Rust execution core, a Unix-socket daemon, and the
`agent` client, with Pi/Codex/FX comparison tooling.** The prototype supports
named bots, restart/resume, historical checkpoints, cancellation, replay with
live follow, shell/read/write/edit tools with retained artifacts, delegation to
other named agents through the same client, and two provider families (OpenAI
Responses and compatible gateways, Anthropic Messages) with streamed thinking
and usage. Its protocol is experimental. The daemon has completed a bounded
[live OpenAI run](docs/OPENAI_SMOKE.md#daemon-live-run) covering file and shell
tools, stored reasoning items across turns, and delegation through detach and
wait; Anthropic checks still use synthetic endpoints. Compaction,
cross-provider handoff, and MCP are not implemented.

## What matters

- Performance first: minimal memory, CPU, network overhead, and tail latency
  across many active agents, with bounded resources under load.
- Fundamental behavior: named bots, explicit resumption, historical conversation
  forks, tool execution, and independent continuation of each branch.
- Live activity and replay after reconnecting: messages, tool calls and results,
  errors, state changes, approvals, and usage where the backend exposes them.
- Programmatic steering, interruption, and completion with clear semantics.
- Broad model support through provider adapters, with explicit capabilities and
  supported authentication paths. The prototype implements subsets of Responses and Anthropic Messages.
- Full-access tool execution initially, with an extensible permission boundary.

The selected experiment is our own compact Rust model/tool loop, using Tokio,
reqwest, and SQLite. Pi and Codex remain comparison baselines. Resource claims
require measured equivalent work, including achieved active concurrency.

## Scope

Agent owns agent identity, execution lifecycle, and observability. Callers
supply workspaces and decide where execution happens. Workspace provisioning,
Git branching, diff application, and machine placement belong to callers or
other tools.

Errand could run Agent, but is an optional consumer. Local execution and
continuation inside Errand are separate, deferred features. This project does
not implement them. A workspace snapshot is not process isolation. Initial
full-access mode runs within the caller's existing OS/container permissions;
Agent does not provide a sandbox.

## Start here

- [Design brief](docs/DESIGN.md): the proposed contract and open choices.
- [Runtime investigation](docs/RUNTIMES.md): evidence, alternatives, and gaps.
- [Prime Intellect investigation](docs/PRIME_INTELLECT.md): Prime Agent, long-history design, and evaluation-tool reuse.
- [Next decisions](docs/NEXT.md): remaining implementation and measurement work.
- [Rust prototype](docs/RUST_PROTOTYPE.md): build, protocol, storage, and limits.
- [Rust measurements](docs/RUST_MEASUREMENTS.md): exploratory observations, with unequal feature footprints.
- [FX measurements](docs/FX_MEASUREMENTS.md): native embedded FX versus Rust with identical conversation content and explicit protocol differences.
- [Comparison contract](docs/COMPARISON_CONTRACT.md): feature inventory and enforced comparison rules.
- [Long histories](docs/LONG_HISTORY.md): full retained conversation versus bounded model context.
- [Durable measurements](docs/LIFECYCLE_MEASUREMENTS.md): Rust feature costs and matched regressions.
- [Daemon measurements](docs/DAEMON_MEASUREMENTS.md): shell execution, socket followers, recovery, and allocation fixes.
- [Reuse assessment](docs/REUSE.md): buy/adopt/reuse versus build for the engine
  and its measurement tools.
- [Performance tools](docs/BENCHMARKS.md): run and compare engines against a synthetic provider,
  with explicit measurement limits.
- [Initial measurements](docs/MEASUREMENTS.md): Pi/Codex resource costs through
  32 simultaneous streams and the provisional reuse decision.

Next: run one bounded real-provider task on each family, then separate
long-term history storage from bounded model context. Keep matched regression
workloads as tools and durable lifecycle behavior expand.
