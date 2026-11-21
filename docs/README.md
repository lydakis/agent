# Documentation

**Agent** is the lightweight Rust daemon and JSON-first CLI for running many bots at once. **[Sonny](../sonny/README.md)** is the terminal workspace built on Agent for supervising parallel tasks.

Start with the overview for whichever layer you're using, then follow the guides below as you need more depth.

## Start here

| Product | What to read first |
| --- | --- |
| **Agent** (daemon & CLI) | [Agent README](../README.md#quickstart) — strengths, install, 2-minute quickstart, and performance summary |
| **Sonny** (terminal workspace) | [Sonny README](../sonny/README.md#first-run) — lead/task workflow, install, everyday controls, and permissions |

## Using Agent

| I want to… | Read |
| --- | --- |
| Look up CLI commands, flags, JSON output, or exit codes | [CLI contract](CLI.md) |
| Set up `AGENTS.md`, skills, profiles, and memory composition | [Client policy](CLIENT.md) |
| Connect providers, Bedrock, or custom endpoints | [Providers and models](RUST_PROTOTYPE.md#providers-and-models) |
| Build a program on the daemon socket or declared tools | [Socket protocol](RUST_PROTOTYPE.md#software-protocol), [Rust client](../client), [triage example](../examples/triage) |
| Understand sandboxes, approval gates, and security invariants | [Sandbox](RUST_PROTOTYPE.md#sandbox), [tool approval](RUST_PROTOTYPE.md#tool-approval), [security policy](../SECURITY.md) |
| Diagnose turns waiting on logins, keys, or quotas | [Held turns](RUST_PROTOTYPE.md#held-turns) |
| Understand durability, crash recovery, and checkpoint forks | [Durable state and recovery](RUST_PROTOTYPE.md#durable-state-and-recovery) |

## Using Sonny

| I want to… | Read |
| --- | --- |
| Look up views, keys, live margin notes, layouts, or ACP setup | [Sonny reference](../sonny/REFERENCE.md) |
| Configure automations, memory, commons/boards, MCP, workflows, or remote links | [Sonny capabilities](SONNY.md) ([automations](SONNY.md#automations), [memory](SONNY.md#memory), [skills](SONNY.md#skills), [MCP](SONNY.md#mcp-servers), [links](SONNY.md#links)) |
| Route task updates and external event sources with filters and digests | [Lookout](LOOKOUT.md) |
| Understand how Auto permissions review calls | [Reviewer](../sonny/REVIEWER.md) |
| Track what changed in Sonny and why | [Feature history](../sonny/FEATURES.md) |

## Design and internals

- [Runtime reference](RUST_PROTOTYPE.md) — daemon architecture, socket protocol, limits, storage, pacing, and compaction.
- [Design brief](DESIGN.md) — problem statement and core constraints.
- [Runtime investigation](RUNTIMES.md) and [reuse record](REUSE.md) — evaluated harnesses and component choices.
- [Long histories](LONG_HISTORY.md) and [storage growth](STORAGE_GROWTH.md) — unbounded transcripts with bounded live memory.
- [Code mode and the tool path](CODE_MODE.md) — design plan for code mode and declared tools at scale.
- [Investigation queue](NEXT.md) — completed engineering work and open questions (not a release roadmap).

## Performance and evaluations

Start with [current evidence](EVIDENCE.md), which separates verified measurements from open questions. Every measurement record pins its workload, environment, and source revision; treat each as evidence for that run, not a universal claim.

- [Benchmark guide](BENCHMARKS.md) — reproduce the synthetic workloads.
- [Comparison contract](COMPARISON_CONTRACT.md) — matched boundaries required for valid comparisons.
- [Daemon measurements](DAEMON_MEASUREMENTS.md) — runtime changes and regression screens.
- [Harness measurements](HARNESS_MEASUREMENTS.md) — exploratory cross-harness comparisons.
- [Live fleet checks](LIVE_FLEET.md) — real-provider scale tests and observed limits.
- [Harbor evaluation](HARBOR.md) — real-task evaluation runs and cost breakdowns.
