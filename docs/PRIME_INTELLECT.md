# Prime Intellect: harness and evaluation investigation

Observed 2026-09-07. This is a source and documentation assessment, not a
performance measurement. No Prime software was installed or executed, and no
paid inference or hosted jobs were launched.

Prime Agent is a directly relevant harness reference, especially for long
conversations. Keep the selected Rust core; investigate programmatic history
retrieval and reuse Verifiers for task-quality evaluation when its adapter
contract fits. Neither small source size nor a bounded model context establishes
small host memory. No existing candidate is rejected on measured efficiency.

## Components and fit

| Component | Relationship to this project | Assessment |
| --- | --- | --- |
| Prime Agent | Same layer: a Pi-derived agent harness with daemon-backed execution and headless clients. | Strong reference for context management, recovery, recursive agents, and an eventual matched comparison. Its TUI does not disqualify programmatic use. |
| nano-rlm | Smaller Python harness with persistent IPython and recursive calls, exposed through ACP. | Useful reference for a minimal model-facing interface. The documented lack of restart/load support leaves a fundamental contract gap. |
| Verifiers | Evaluation infrastructure around harnesses, tasks, and traces. | Candidate to reuse outside the Rust runtime for quality and long-history evaluations. Keep host resource measurement separate. |
| Prime-RL, hosted compute, inference, sandboxes | Adjacent training and execution infrastructure. | Potential future integrations. They do not replace named-agent lifecycle or reduce local overhead merely by moving it elsewhere. |

The [Prime Intellect site](https://www.primeintellect.ai/) links these projects
and services. This investigation does not assess hosted pricing, deployment,
provider coverage, or commercial service terms.

## Pinned evidence

Selected public files were inspected at these revisions; local copies are in
the ignored research directory. Pins describe inspected source, not benchmarked
release binaries.

| Repository | Revision | Root license inspected |
| --- | --- | --- |
| [Prime Agent](https://github.com/PrimeIntellect-ai/prime-agent/tree/9c8230df67b378aaedc032f90e1ae8ba687cfe4a) | `9c8230df67b378aaedc032f90e1ae8ba687cfe4a` | [MIT](https://github.com/PrimeIntellect-ai/prime-agent/blob/9c8230df67b378aaedc032f90e1ae8ba687cfe4a/LICENSE), including Pi attribution. |
| [nano-rlm](https://github.com/PrimeIntellect-ai/nano-rlm/tree/e38400695fd42c5bdc1eb133502c65c1c39da2a3) | `e38400695fd42c5bdc1eb133502c65c1c39da2a3` | [MIT](https://github.com/PrimeIntellect-ai/nano-rlm/blob/e38400695fd42c5bdc1eb133502c65c1c39da2a3/LICENSE). |
| [Verifiers](https://github.com/PrimeIntellect-ai/verifiers/tree/27bbd216df0af719a43705866b2cf6139bcc95de) | `27bbd216df0af719a43705866b2cf6139bcc95de` | [MIT](https://github.com/PrimeIntellect-ai/verifiers/blob/27bbd216df0af719a43705866b2cf6139bcc95de/LICENSE). |

Dependency licenses still need review if code or packages are actually adopted.

## Prime Agent: resource ownership

The pinned [architecture documentation](https://github.com/PrimeIntellect-ai/prime-agent/blob/9c8230df67b378aaedc032f90e1ae8ba687cfe4a/packages/coding-agent/docs/architecture.md)
describes a supervisor for routing/recovery, a catalog process for saved-session
scans, and session workers for root session trees. SDK paths can run the agent
runtime in process. Do not treat every integration as the same process topology.
The [RPC documentation](https://github.com/PrimeIntellect-ai/prime-agent/blob/9c8230df67b378aaedc032f90e1ae8ba687cfe4a/packages/coding-agent/docs/rpc.md)
exposes correlated commands and streamed events over JSONL stdio.

Verified implementation details:

- The supervisor actually [spawns session workers](https://github.com/PrimeIntellect-ai/prime-agent/blob/9c8230df67b378aaedc032f90e1ae8ba687cfe4a/packages/coding-agent/src/modes/daemon/daemon-supervisor.ts#L3111-L3134).
  Sharing a supervisor does not eliminate worker process cost.
- [Kernel startup](https://github.com/PrimeIntellect-ai/prime-agent/blob/9c8230df67b378aaedc032f90e1ae8ba687cfe4a/packages/coding-agent/src/core/kernel/repl-manager.ts#L285-L323)
  launches a Python subprocess. The [runtime documentation](https://github.com/PrimeIntellect-ai/prime-agent/blob/9c8230df67b378aaedc032f90e1ae8ba687cfe4a/packages/coding-agent/docs/rlm-runtime.md)
  specifies lazy provisioning on first REPL use, distinct child runtimes, and
  serialized ordinary cells within a kernel. It also describes daemon-retained
  children; count the observed process tree rather than assuming a fixed
  process count per logical agent. Workers and kernels are not security sandboxes.
- The [async history loader](https://github.com/PrimeIntellect-ai/prime-agent/blob/9c8230df67b378aaedc032f90e1ae8ba687cfe4a/packages/coding-agent/src/core/session-manager.ts#L716-L739)
  streams large files and yields while parsing. That avoids simultaneously
  retaining the entire input buffer and parsed graph, but still builds an array
  containing all parsed entries. [SessionManager](https://github.com/PrimeIntellect-ai/prime-agent/blob/9c8230df67b378aaedc032f90e1ae8ba687cfe4a/packages/coding-agent/src/core/session-manager.ts#L1411-L1455)
  retains those entries and an ID index. This path is not bounded-memory access
  to an arbitrarily large transcript; its retained graph grows with loaded history.
- [Kernel snapshots](https://github.com/PrimeIntellect-ai/prime-agent/blob/9c8230df67b378aaedc032f90e1ae8ba687cfe4a/packages/coding-agent/src/core/kernel/state-snapshot.ts#L1-L30)
  are best-effort per variable, with skipped values reported. Default payload
  ceilings are 256 MiB total and 16 MiB per variable. These are serialization
  limits, not measurements or live-memory caps. Persisted conversation recovery
  must not be conflated with exact recovery of arbitrary Python state or effects.

Inference: process/runtime overhead, retained transcript objects, Python values,
and snapshot work are concrete profiling targets. They do not establish a numeric
RSS floor, a CPU bottleneck, or a Rust speedup without measurements. Network
connection reuse and total allocation costs were not traced in this investigation.

## Long history: the useful idea and its limits

Prime Agent exposes computation and recursive delegation through a persistent
REPL. Older information can be accessed programmatically instead of placing it
all in each model request. The [paper, version 1](https://arxiv.org/html/2608.23552v1)
distinguishes active model context, live REPL state, and durable history. It
describes compaction retaining originals for retrieval, and evaluations organized
around task outcomes, inference expenditure, and time. Those results do not
establish our many-active-agent RSS/CPU target.

Current [compaction documentation](https://github.com/PrimeIntellect-ai/prime-agent/blob/9c8230df67b378aaedc032f90e1ae8ba687cfe4a/packages/coding-agent/docs/compaction.md)
describes summarization and rebuilding context from a summary plus retained
messages. The January [RLM exploration](https://www.primeintellect.ai/blog/rlm)
described an approach without summarization; do not carry that historical claim
over to current Prime Agent.

For our design, expose bounded history search/range reads and artifact handles,
backed by disk indexes. Make a persistent Python environment an optional tool
capability if useful, with its own budget and lifecycle. The core need not retain
the whole conversation as Python values or parsed objects to offer retrieval.
Model-driven retrieval and summarization can lose relevant information or add
calls; measure quality and all descendant/summary tokens alongside host costs.

The pinned [nano-rlm README](https://github.com/PrimeIntellect-ai/nano-rlm/blob/e38400695fd42c5bdc1eb133502c65c1c39da2a3/README.md)
documents ACP-only operation, per-session persistent engines, compaction with
the kernel kept alive, and explicit omission of `session/load` because arbitrary
live Python state cannot be reconstructed after process exit. Its disk outputs
are therefore not evidence of our required restart/resume contract. It is a
reference for interface simplicity, not a drop-in durable core.

## Reuse evaluation infrastructure

The inspected [Verifiers overview](https://github.com/PrimeIntellect-ai/verifiers/blob/27bbd216df0af719a43705866b2cf6139bcc95de/docs/overview.md)
targets `verifiers.v1` and says the legacy v0 stack was removed. Avoid building
an adapter from older RLMEnv examples without checking the pinned API.
[Version 1](https://github.com/PrimeIntellect-ai/verifiers/blob/27bbd216df0af719a43705866b2cf6139bcc95de/docs/v1/overview.md)
separates tasksets, harnesses, models/runtime policy, and traces with per-call
usage/timing/errors. The [custom harness contract](https://github.com/PrimeIntellect-ai/verifiers/blob/27bbd216df0af719a43705866b2cf6139bcc95de/docs/v1/harnesses.md)
provides setup/launch hooks, an intercepted model endpoint, and explicit MCP and
resume capabilities.

Proposed reuse: an external Python adapter launches our release binary and
translates its result into the evaluation trace. Validate endpoint/protocol and
trace compatibility first; advertise only supported capabilities. Keep Verifiers,
its interception service, and fixtures outside the measured harness boundary,
while also reporting total-system cost separately. Do not add Python to the
resident Rust daemon or replace psutil/CPU profilers with task scores.

## Next bounded experiment

1. Implement the indexed history/context split in [LONG_HISTORY.md](LONG_HISTORY.md).
   Fix the selected context and workload while increasing stored history. Measure
   resume, old-checkpoint fork, retrieval, memory, CPU, disk I/O, and teardown.
2. Add synthetic checks for an old fact, a corrected constraint, an exact artifact,
   and branch-specific facts. Require originals to remain recoverable after
   compaction and restart. Retrieval must not expose later or sibling-branch data.
3. When evaluating Prime Agent, pin the executed build and adapter separately.
   Exercise both unloaded and loaded sessions, before and after first REPL use,
   and include supervisor/workers/kernels/tools in resource accounting. Match
   durability, tools, context selection, model, and budgets; otherwise label the
   comparison exploratory under [COMPARISON_CONTRACT.md](COMPARISON_CONTRACT.md).
4. Validate a small Verifiers adapter for quality tests once the provider and
   usage contracts exist. Count all model, summary, retry, and descendant calls.
   Live model runs still need a concrete call/token/spend budget.

This adds research inputs and a measurement plan. It does not implement
compaction, add dependencies, or change the chosen Rust experiment.
