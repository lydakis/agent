# Build decision and component reuse

Decision update, 2026-09-07: George explicitly chose to build a minimal Rust
harness for the performance engineering challenge. This supersedes the earlier
requirement to prove adoption unsuitable before owning the loop. Pi and Codex
remain comparison baselines. Reuse standard transport, storage, and profilers;
own only the agent lifecycle and execution behavior being investigated.

## Baselines and implementation decision

| Candidate | Relevant evidence | What must be established before selecting |
| --- | --- | --- |
| FX native embedded core | [Pinned benchmark](FX_MEASUREMENTS.md): `libfx` 0.0.8, native addon plus Node, Gateway SSE. | Compare the same conversation workload; separately establish durability, historical forks, long histories, and total coding-tool costs before considering reuse. |
| Prime Agent / nano-rlm | [Source assessment](PRIME_INTELLECT.md): programmatic RLM context, headless operation, and explicit persistence differences. | Full process-tree costs and long-history behavior under matched contracts. Reuse design ideas; retain the selected Rust core. |
| Codex app-server | Shared services and threads in source; 0.153.1 completed the synthetic matrix through 32 simultaneous streams. Lower one-agent CPU than Pi in this setup. | History-fork granularity, unload/recovery, model coverage, and resource cost under equivalent durable/tool workloads. |
| Pi core and model packages | Published 0.85.1 completed the same matrix; lower sampled RSS and CPU at 8 and 32 streams. | Copying/allocation hotspots, backpressure, durable historical forks, and provider fidelity. |
| Claude Code CLI | [Pinned adapter](BENCHMARKS.md#claude-code-adapter): 2.1.267 native CLI, one process per agent over stream-json, Anthropic Messages SSE; completed smoke and 32-agent 64 KiB screens on 2026-09-23 (sanity runs, not matrix results). | Per-process cost is the deployment unit; establish durable resume/fork, tools, and long-history costs, and whether any shared-process arrangement exists, before treating it as more than a measured baseline. |
| OpenCode server | Documented session and event APIs. | Ownership/isolation and resource costs for the required workload; investigate if the first two candidates leave a relevant gap. |
| New Rust core | Shared streaming core and durable Bob lifecycle implemented; see [prototype](RUST_PROTOTYPE.md) and [measurements](RUST_MEASUREMENTS.md). | Durable performance, coding tools, real providers, long-lived memory, and broader platform validation. |

See [runtime evidence and pinned revisions](RUNTIMES.md). Pi's human-facing
application does not disqualify its reusable core. No engine has been rejected
on measured performance yet. A managed service can also qualify if it provides
the same control, resumption/fork behavior, data handling, and acceptable total
cost; moving compute elsewhere must not be counted as eliminating its cost.

The [initial measurements](MEASUREMENTS.md) favored further Pi reuse investigation.
That recommendation is historical; the explicit project direction is now our own
loop, using Tokio scheduling, reqwest HTTP/TLS, and rusqlite/SQLite transactions.
The streaming comparison does not establish equivalent durable or tool behavior
across engines. No claim is made that existing products cannot satisfy the need.

The decision record must compare: required capabilities, equivalent-workload
results, supported auth and deployment, license/commercial terms, integration
effort, and ongoing upgrade/maintenance cost. Prefer an adapter or a small
upstream change before a long-lived fork. State unknowns and the experiment
that would resolve them. Do not declare that nothing suitable exists from this
bounded candidate review alone.

## Measurement tools

The [storage experiments](STORAGE_GROWTH.md) reuse SQLite's `dbstat` for page
attribution. miniz_oxide 0.8.9 stays in the isolated benchmark; its partial-read
cost ruled out blanket compression. The runtime reuses lz4_flex 0.14.0 for
large artifacts only, with bounded independent blocks in one BLOB. Transcript
JSON remains unchanged. The component screen and matched daemon measurements
separate codec costs from lifecycle costs. No custom compressor or replacement
database is proposed.

Primary documentation checked 2026-09-07. These are capability assessments,
not comparative performance measurements of the tools themselves.

| Tool | Use | Decision for this slice |
| --- | --- | --- |
| [Verifiers v1](PRIME_INTELLECT.md#reuse-evaluation-infrastructure) | Tasksets, custom harness adapters, scoring, and per-call model traces. | Candidate reuse for task-quality evaluation outside the runtime. Validate the adapter after provider/usage contracts; retain separate host-resource profiling. |
| [psutil 7.2.2](https://pypi.org/project/psutil/7.2.2/) | Process identities, CPU and memory counters across platforms. | Adopt for measurement only. Replaced the prototype's custom macOS/Linux counter readers. |
| [Hyperfine](https://github.com/sharkdp/hyperfine) | Repeated command timing, warmups, parameter sweeps, and result export. | Reuse for standalone command/startup timing when an engine command is ready. Do not interpret timing a whole orchestration script as target-only timing. |
| [Samply](https://github.com/mstange/samply) | Sampling CPU profiles on macOS and Linux, including symbolized native code. | Use for hotspot investigation after a target exists. No custom stack sampler. |
| [Heaptrack](https://github.com/KDE/heaptrack) | Allocation tracing and allocation hotspots on Linux. | Use for allocation investigations on a compatible runner; validate allocator coverage for the selected engine. No custom heap profiler. |

These tools cover counters and profiling. The remaining project-specific glue
is a deterministic workload, target/provider separation, event and byte-count
validation, and comparison metadata. The initial `bench` package implements
that bounded experiment using psutil. It is not a general profiling framework
or evidence that a new agent engine is necessary. Its small repeat/summary loop
keeps resource samples, fixture counters, and validated events in one run record;
it does not attempt statistical significance testing or replace Hyperfine.

Before extending this package, check whether an existing tool can supply the
measurement. Planned historical-fork/resumption scenarios need engine adapters;
the current engine matrix exercises neither feature. No paid
service purchase, global profiler installation, or model API run is part of this
initial tooling slice.
