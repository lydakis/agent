# Agent

A performance-first agent execution engine for software clients.

The goal is to let a program create named bots, give them work, steer or resume
that work, fork earlier conversation checkpoints to explore alternatives, and
follow activity as an ordered stream of events. Programs are the consumers.
Human-readable service logs are an optional view of that stream.

A bot is a named agent and the single execution unit. In the intended design,
a bot delegates by using the same CLI or protocol as any other program to create
or fork another bot. All bots are peers; origin does not confer ownership,
special permissions, or a different lifecycle. Models choose their tools and coordination strategies;
the minimal core supplies durable lifecycle, provider access, and bounded execution.

The reason to build this is efficient execution of many **active** bots. A
common command syntax around one full harness process per bot is not enough.
The ambition is thousands of active conversations; no capacity claim has been
established.

**Status: experimental Rust execution core, a Unix-socket daemon, and the
`agent` client, with Pi/Codex/FX comparison tooling.** The prototype supports
named bots, restart/resume, historical checkpoints, cancellation, replay with
live follow, shell/read/write/edit tools with retained artifacts, delegation to
other bots through the same client, and two provider families (OpenAI
Responses and compatible gateways, Anthropic Messages) with streamed thinking
and usage. Its protocol is experimental. The daemon has completed a bounded
[live OpenAI run](docs/OPENAI_SMOKE.md#daemon-live-run) and a
[live Anthropic run](docs/ANTHROPIC_SMOKE.md) covering file and shell tools,
stored reasoning and thinking across turns, and delegation through detach and
wait. Stored history is unbounded; each request carries a bounded window of
whole turns with an explicit note and a `history` tool for what it omits.
Compaction summaries and carry-forward notes are implemented; long-task quality and cost evaluation remain open. Cross-provider handoff and MCP are not implemented.

This is an experiment with no users. Protocols, CLI defaults, and configuration
may break between revisions, and the runtime carries no legacy modes or fallback
branches for earlier Agent versions. Stores migrate forward one way at open.
Supporting models and providers, including older models, remains a product
requirement.

## What matters

- Performance first: minimal memory, CPU, network overhead, and tail latency
  across many active bots, with bounded resources under load.
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

Agent owns bot identity, execution lifecycle, and observability. Callers
supply workspaces and decide where execution happens. Workspace provisioning,
Git branching, diff application, and machine placement belong to callers or
other tools.

Errand could run Agent, but is an optional consumer. Local execution and
continuation inside Errand are separate, deferred features. This project does
not implement them. A workspace snapshot is not process isolation. Initial
full-access mode runs within the caller's existing OS/container permissions;
Agent does not provide a sandbox.

## Start here

- [CLI contract](docs/CLI.md): command structure, flags, output, and exit status.
- [Design brief](docs/DESIGN.md): the proposed contract and open choices.
- [Runtime investigation](docs/RUNTIMES.md): evidence, alternatives, and gaps.
- [Prime Intellect investigation](docs/PRIME_INTELLECT.md): Prime Agent, long-history design, and evaluation-tool reuse.
- [Next decisions](docs/NEXT.md): remaining implementation and measurement work.
- [Rust prototype](docs/RUST_PROTOTYPE.md): build, protocol, storage, and limits.
- [Rust measurements](docs/RUST_MEASUREMENTS.md): exploratory observations, with unequal feature footprints.
- [Anthropic live run](docs/ANTHROPIC_SMOKE.md): the daemon against Sonnet 5, Opus 5, and Fable 5.1, with the adaptive-thinking fix it forced.
- [Live fleet check](docs/LIVE_FLEET.md): up to 1,024 concurrent bots on real providers through one daemon, 64 bots sustained for five minutes with no drift, and 10,000 bots through one key at the provider's own rate with no failures. These are short-context turns; they establish a lightweight runtime, not coding-agent capacity.
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

Next: compaction with summaries, the remaining per-turn growth now that
retention bounds records and deletes idle bots. Keep matched regression
workloads as tools and durable lifecycle behavior expand.
