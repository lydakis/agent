# Runtime investigation

Observed 2026-09-07. Source structure is evidence of architecture, not a resource
benchmark. Documentation may describe a newer or different implementation;
resolve discrepancies against pinned versions before selecting dependencies.

The separate [initial engine measurements](MEASUREMENTS.md) now cover ephemeral
text conversations through 32 simultaneous streams. Those installed package/binary
versions are recorded independently from the source-audit revisions below.

The scope now includes a new model/tool loop or a harness fork. The initial
comparison is Codex app-server, Pi's reusable core, and a minimal experimental
loop. Performance and the fundamental software contract decide which to use;
preserving every existing coding-harness feature is not a goal.

## Current assessment

| Candidate | Evidence | Assessment |
| --- | --- | --- |
| Prime Agent / nano-rlm | [Pinned investigation](PRIME_INTELLECT.md): daemon workers, lazy Python kernels, context compaction, and retained history objects; nano-rlm explicitly omits restart/load. | Direct harness references, especially for long histories. Resource costs unmeasured; no efficiency ranking. |
| Codex app-server | Shared services and thread/turn protocol; 0.153.1 completed the synthetic concurrency matrix. | Shared native baseline. Durable/tool workload cost and active capacity beyond the screen remain unmeasured. |
| Pi agent core / model layer | Separate per-agent state; published 0.85.1 completed the same matrix with lower sampled costs at 8/32 streams. | Comparison baseline; earlier reuse recommendation superseded by the custom-core decision. Durable capacity remains unmeasured. |
| New minimal core | Rust prototype with shared provider I/O, immutable encoded history, and a durable lifecycle service. [Measurements](RUST_MEASUREMENTS.md). | Selected experiment. Durable performance and full coding-tool/provider coverage remain open. |
| Codex TypeScript SDK | The inspected execution path spawns the native executable. | Convenient interface; not evidence of lower process overhead. |
| OpenCode server | Session CRUD/fork/abort, asynchronous prompts, and event endpoints. | Existing programmable server worth examining before building a new one. |
| Claude Agent SDK | Rich programmable interface; inspected Python default transport launches Claude CLI. | Pin the actual SDK architecture. No shared-runtime saving established. |
| T3 Code backend | Mature provider routing; inspected Codex session startup creates an app-server runtime. | Integration reference, not a proven high-density runtime shortcut. |
| ACP / acpx | Session/event protocol and existing headless client with named sessions. | Potential interface reuse. Protocol compatibility alone does not share processes. |
| Grok | Candidate named in the project brief. | Native protocol and runtime ownership not yet inspected; no capability claims. |

## Codex: shared native baseline and possible fork

The [app-server documentation](https://developers.openai.com/codex/app-server/)
exposes thread creation, resumption, naming, loaded-thread enumeration, streamed
items, and targeted turn steering/interruption. The documented steering request
uses an expected turn ID. These provide much of the intended software contract.

At source revision `0896bf6fc05ead454888b90044e1a08f99b6d778`,
[ThreadManagerState](https://github.com/openai/codex/blob/0896bf6fc05ead454888b90044e1a08f99b6d778/codex-rs/core/src/thread_manager.rs#L276)
holds a map of thread objects and shared auth/model/environment/skill/plugin
services. The [app-server message processor](https://github.com/openai/codex/blob/0896bf6fc05ead454888b90044e1a08f99b6d778/codex-rs/app-server/src/message_processor.rs)
constructs and distributes the manager. This supports investigating multiple
sessions in one server; it does not establish inexpensive independent concurrent
turns, per-workspace isolation, or a maximum thread count.

By contrast, the [TypeScript SDK execution path](https://github.com/openai/codex/blob/0896bf6fc05ead454888b90044e1a08f99b6d778/sdk/typescript/src/exec.ts#L181)
spawns the executable. Using that SDK alone would not demonstrate the desired
change in resource ownership.

Next source questions: per-thread allocations, active-turn scheduling,
configuration scope, MCP ownership, unloading, event recovery, and the smallest
supported app-server interface. Existing named threads might eliminate the need
for a separate bot registry in an initial Codex-only client.

If considering a fork, identify the reusable execution/permission components and
the cost of adapting provider assumptions, history forks, and process ownership.
Compare that maintenance burden with a new loop. No fork is selected here.

Limited source ownership check, 2026-09-07, at the same revision:

- [Thread spawning](https://github.com/openai/codex/blob/0896bf6fc05ead454888b90044e1a08f99b6d778/codex-rs/core/src/thread_manager.rs)
  passes shared service handles and conversation history into `Session::spawn`.
- [CodexThread](https://github.com/openai/codex/blob/0896bf6fc05ead454888b90044e1a08f99b6d778/codex-rs/core/src/codex_thread.rs)
  holds a session and its I/O, with submit, steer, and shutdown operations.
- Each [Session](https://github.com/openai/codex/blob/0896bf6fc05ead454888b90044e1a08f99b6d778/codex-rs/core/src/session/session.rs)
  has mutable session state, an active-turn slot, an input queue, event/status
  senders, and MCP refresh/prewarm state. Shared manager services therefore do
  not eliminate per-session runtime state. Resource sizes and the allocations
  underneath these fields remain unmeasured.

Adapter direction: start at app-server and trace its model transport to a
synthetic endpoint before adding an internal Rust integration. The fixture's
binary HTTP response is not compatible with the native model protocol yet.

## Pi: separable loop and model integration

At revision `b2602be77cb7b0de45dd616407fd210daa48aa75`, the
[agent package README](https://github.com/badlogic/pi-mono/blob/b2602be77cb7b0de45dd616407fd210daa48aa75/packages/agent/README.md)
describes a stateful agent with tool execution, streaming, and configurable
context conversion. Its model calls use the separate
[model package](https://github.com/badlogic/pi-mono/blob/b2602be77cb7b0de45dd616407fd210daa48aa75/packages/ai/README.md).
These are package documentation claims; the limited source check below does not
yet cover provider-adapter internals or persistence implementations.

Limited source ownership check, 2026-09-07, at the same revision:

- [Agent](https://github.com/badlogic/pi-mono/blob/b2602be77cb7b0de45dd616407fd210daa48aa75/packages/agent/src/agent.ts)
  owns transcript/tool arrays, steering queues, listeners, and an active run with
  an abort controller. Context snapshots copy top-level arrays; this is not a
  deep copy of every message payload. Listeners are awaited serially, so a slow
  callback can delay event consumption and run settlement.
- The [loop](https://github.com/badlogic/pi-mono/blob/b2602be77cb7b0de45dd616407fd210daa48aa75/packages/agent/src/agent-loop.ts)
  accepts a supplied `streamFn` and event sink. It converts context at the model
  boundary and emits shallow partial-message objects during streaming.
- The [event stream](https://github.com/badlogic/pi-mono/blob/b2602be77cb7b0de45dd616407fd210daa48aa75/packages/ai/src/utils/event-stream.ts)
  queues pushed events when no consumer is waiting; this class has no queue-size
  limit or producer-await mechanism. This identifies a slow-consumer experiment,
  not a claim that every Pi execution path has unbounded memory growth.

Adapter direction: a supplied stream function can isolate Pi's loop from model
networking first. A separate run must use the real provider adapter to measure
serialization and connection reuse; do not compare a transport-bypassed Pi loop
against full Codex networking and call the difference engine efficiency.

This is a closer component boundary for a minimal harness than a complete coding
application. Investigate allocation/copying during streaming, retained history,
subscriber backpressure, provider-specific context, and lifecycle persistence.
Measure it as a candidate before deciding to reuse or replace the loop or model
adapters. Neither TypeScript nor a small public API establishes its runtime cost.

## OpenCode: existing server rather than another wrapper

The [server API](https://opencode.ai/docs/server/) describes session creation,
forking, status, cancellation, asynchronous prompt submission, and event streams.
That is a close interface precedent. The API surface alone does not establish
parallel scheduling, incremental memory, safe sharing of directory-scoped
configuration, or durable event replay. Investigate native server ownership and
session concurrency before choosing it as a backend or replacing it.

## Claude: documentation and implementation must be reconciled

The [SDK overview](https://code.claude.com/docs/en/agent-sdk/overview) describes
an in-process agent loop with sessions, tools, permissions, and streaming. It
also documents API-key-based product authentication and restrictions on offering
consumer login through third-party SDK products. Native local CLI auth reuse
and a distributed SDK product are distinct integration questions.

The inspected Python source at
`efd4d865ef1795daffee3cd24cce45307aed8a51` selects
[SubprocessCLITransport by default](https://github.com/anthropics/claude-agent-sdk-python/blob/efd4d865ef1795daffee3cd24cce45307aed8a51/src/claude_agent_sdk/_internal/client.py#L81).
The [transport implementation](https://github.com/anthropics/claude-agent-sdk-python/blob/efd4d865ef1795daffee3cd24cce45307aed8a51/src/claude_agent_sdk/_internal/transport/subprocess_cli.py)
locates a Claude executable and manages a child process. This is direct evidence
for that default Python path, not a universal claim about every SDK version,
language, or custom transport. Do not infer memory savings from the overview.

Next: identify any supported shared or embedded runtime, pin its version, and
confirm its supported authentication and session boundaries.

## T3 Code: useful reference, inspect process ownership

At revision `0d34579d674920cc47fc5c908494f51ed3895204`, the
[Codex adapter](https://github.com/pingdotgg/t3code/blob/0d34579d674920cc47fc5c908494f51ed3895204/apps/server/src/provider/Layers/CodexAdapter.ts#L2290)
creates a runtime when starting a managed session. That
[runtime spawns an app-server](https://github.com/pingdotgg/t3code/blob/0d34579d674920cc47fc5c908494f51ed3895204/apps/server/src/provider/Layers/CodexSessionRuntime.ts#L1217).
This is not a statement that every native child subagent requires another server.

The [provider constraints](https://github.com/pingdotgg/t3code/blob/0d34579d674920cc47fc5c908494f51ed3895204/docs/internals/providers.md)
also specify a separate OpenCode chat server per thread because of connection
scope. Removing the UI would not remove these backend ownership decisions.
Study its provider event and permission handling; do not fork the whole project
without a specific reusable boundary and a license review of any copied code.

## ACP and acpx: interface reuse is separate from execution reuse

[ACP session setup](https://agentclientprotocol.com/protocol/v1/session-setup)
describes independent conversations with the same agent. It does not guarantee
how an implementation allocates processes or schedules concurrent work.

[acpx session documentation](https://github.com/openclaw/acpx/blob/main/docs/sessions.md)
describes cwd/harness/name-scoped records and separate queue owners for named
sessions. It already supplies useful headless lifecycle behavior, but that model
is not evidence of pooled active execution. Its existing capabilities are worth
evaluating before building convenience commands here, subject to the resumption
constraint below.

The documented [crash recovery](https://github.com/openclaw/acpx/blob/main/docs/sessions.md#crash-recovery)
falls back to `session/new` when resumption fails and transparently updates the
saved record. That behavior conflicts with this project's requirement to resume
the requested conversation or report failure. Before adopting acpx for session
lifecycle management, pin the assessed version and verify a strict resume path
that reports failure without creating a fresh session. No such path has been
verified here.

## What the earlier Errand run did and did not establish

Two native Codex agents on two hosts each completed an initial turn and a resumed
turn. Structured native events, result retrieval, and conversation continuation
worked. Both follow-ups used new job directories and explicitly supplied prior
code. This established feasibility, which was not the user's open question.

Rough summed-RSS peaks across those process trees ranged from about 0.5 to
0.9 GiB. The hosts, CLI versions, and loaded context differed; sampling missed
brief peaks and could double-count shared pages. Those observations are not a
per-agent memory budget, backend comparison, or active-capacity result. Raw
captures contain machine details and are intentionally not tracked here.

Do not repeat that demonstration as the next milestone. The unresolved question
is whether a software-facing runtime materially improves active resource cost
and observability over native headless execution.
