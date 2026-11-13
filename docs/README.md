# Documentation

Agent is the daemon and CLI. [Sonny](../sonny/README.md) is its terminal
client for coordinating tasks. Start with the [Agent quickstart](../README.md#quickstart)
or the [Sonny quickstart](../sonny/README.md#first-run).

## Using Agent

| I want to… | Read |
| --- | --- |
| Run, resume, steer, or fork a bot | [Quickstart](../README.md#quickstart) |
| Look up commands, flags, output, or exit codes | [CLI contract](CLI.md) |
| Connect a provider or choose a model | [Providers and models](RUST_PROTOTYPE.md#providers-and-models) |
| Apply project instructions, profiles, and skills | [Client policy](CLIENT.md) |
| Connect an application to the daemon | [Socket protocol](RUST_PROTOTYPE.md#software-protocol), [Rust client](../client), [triage example](../examples/triage) |
| Understand permissions | [Sandbox](RUST_PROTOTYPE.md#sandbox), [tool approval](RUST_PROTOTYPE.md#tool-approval), [security policy](../SECURITY.md) |
| Diagnose a turn waiting on credentials or provider access | [Held turns](RUST_PROTOTYPE.md#held-turns) |
| Understand persistence and conversation forks | [Durable state and recovery](RUST_PROTOTYPE.md#durable-state-and-recovery) |

## Using Sonny

| I want to… | Read |
| --- | --- |
| Install Sonny and start work | [Sonny README](../sonny/README.md) |
| Look up controls, settings, or integrations | [Sonny reference](../sonny/REFERENCE.md) |
| Set up automations | [Automations and scheduler](SONNY.md#automations) |
| Understand shared memory and skills | [Memory](SONNY.md#memory), [skills](SONNY.md#skills) |
| Connect MCP servers | [MCP setup](SONNY.md#mcp-servers) |
| Follow tasks or outside sources | [Lookout](LOOKOUT.md) |
| Understand the approval reviewer | [Reviewer](../sonny/REVIEWER.md) |
| See feature changes | [Feature history](../sonny/FEATURES.md) |

## Design and development

- [Runtime reference](RUST_PROTOTYPE.md): protocol, resource limits, providers, tools, storage, pacing, and compaction.
- [Design brief](DESIGN.md): the problem and design constraints.
- [Runtime investigation](RUNTIMES.md) and [reuse record](REUSE.md): evaluated components and implementation choices.
- [Long histories](LONG_HISTORY.md) and [storage growth](STORAGE_GROWTH.md): preserving conversations with bounded live memory.
- [Investigation queue](NEXT.md): completed work and open questions, not a release schedule.

## Performance and evaluations

Start with [current evidence](EVIDENCE.md), which separates measurements,
verified behavior, and what remains unestablished. Each measurement record
names its workload, environment, and source revision; it is evidence for that
run, not a claim about every later build.

- [Benchmark guide](BENCHMARKS.md): reproduce the workloads.
- [Comparison contract](COMPARISON_CONTRACT.md): the boundaries required for meaningful comparisons.
- [Daemon measurements](DAEMON_MEASUREMENTS.md): runtime changes and regression screens.
- [Harness measurements](HARNESS_MEASUREMENTS.md): exploratory comparisons with other harnesses.
- [Live fleet checks](LIVE_FLEET.md): real-provider runs and their limits.
- [Harbor evaluation](HARBOR.md): task evaluation setup and results.
