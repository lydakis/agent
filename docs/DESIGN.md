# Design brief

Status, 2026-09-07: broad design contract. The [Rust prototype](RUST_PROTOTYPE.md)
documents the implemented subset and its experimental APIs; the remaining
semantics below are proposals.

## Purpose and success condition

A controlling program should treat an agent as a named, observable service it
can start working, address again, inspect, and stop. Human-readable logs are a
view of the same activity. The command-line interface is one client.

Performance is the first priority: minimize memory, CPU, network overhead, and
tail latency while preserving the fundamental behavior below. The target is the
smallest useful harness for many active agents, consumed entirely by programs.
Compatibility with an existing coding harness is a means, not a requirement to
inherit all its features. An existing engine that meets the need is a valid outcome.

Design direction, 2026-09-07: a new model/tool loop and a backend fork are now
explicit options alongside component reuse and native server adoption. The
working hypothesis is an embeddable core with a thin service interface. Provider
adapters, tools, persistence, and permissions should have narrow boundaries;
they must not require a chat UI or a process per bot.

## Distinct identities

| Object | Meaning |
| --- | --- |
| Bot | Durable ID and optional human name, backend binding, and configuration. |
| Session | A conversation owned by Agent or a native harness, with its continuation reference. |
| Checkpoint | An immutable, addressable conversation prefix with enough state to continue. |
| Turn | One unit of requested agent work, with observable progress and outcome. |
| Runtime | The process or service that executes one or more sessions. |
| Workspace reference | Caller-owned working directory and optional lineage metadata. |

A bot is not a process. A bot can be inactive without reserving a process.
A process may host many active bots only where the backend actually supports
independent sessions. Names resolve within an explicit namespace; durable IDs
remain stable when names change.

A conversation fork and a filesystem fork are different operations. Resuming a
session restores conversation, not code. The caller supplies the intended
workspace and is responsible for ensuring it contains the intended state.
Switching harnesses is a handoff with explicit context, not native resumption.
Switching models on Agent-owned history requires provider compatibility checks;
provider-specific state must be preserved or its incompatibility reported.

## Proposed client operations

The operation names below describe responsibilities, not final CLI syntax or
wire method names.

| Operation | Required behavior |
| --- | --- |
| Create bot | Bind a stable identity to a backend and explicit configuration. |
| Submit turn | Return an accepted turn identity; execution and final success are separate. |
| Inspect | Report current state, backend/session reference, and current/last turn. |
| Follow | Replay retained events from a cursor, then stream live events. |
| Resume | Continue the requested session with its recorded history or report why it cannot be resumed. |
| Steer | Address the expected active turn; reject stale or unsupported requests. |
| Interrupt | Request cancellation; report the final outcome separately. |
| Answer input | Correlate the response to the outstanding question or approval. |
| Fork conversation | Create a new bot/session from an explicit retained checkpoint; preserve parent lineage and leave the parent unchanged. |
| Release runtime | Retain bot history while reclaiming inactive execution resources. |

One active turn per bot is the initial policy. Parallel mutations use distinct
bots.

## One agent primitive

A named agent (called a bot in the prototype protocol) is the unit of execution.
"Subagent" describes a delegation relationship between ordinary named agents;
it does not require a separate execution class or a built-in recursive planner.
Bob can use the CLI to create Alice with fresh history, or fork one of Bob's
retained checkpoints as Alice. Both use the same submit, inspect, follow,
resume, and interrupt operations available to any controlling program.

The CLI must attach to the shared service so delegation does not start another
full harness per agent. The current stdio prototype does not yet provide this
attachment path. A fork shares immutable conversation ancestry and starts an
independent continuation; it does not clone a process or a workspace.

Record delegation provenance separately from fork ancestry: a freshly created
helper has a creator but no inherited conversation. Delegated work must remain
visible in resource accounting and subject to admission limits. Define explicit
cancellation scope before exposing group operations; interrupting Bob must not
silently imply interrupting Alice. A parent relationship grants no additional
permissions.

Keep a small set of execution and context-access primitives. Models can build
scripts and tools in the supplied workspace and choose how to coordinate agents.
A tool catalog, workflow engine, or per-agent language kernel is not a core
requirement. The harness still owns durable state, provider protocol fidelity,
event delivery, cancellation, and resource bounds. Those guarantees must survive
the model stopping or failing.

Borrow programmatic context-access and evaluation patterns from the
[Prime Intellect investigation](PRIME_INTELLECT.md), then measure their cost and
task quality here. Optional context-processing tools can use ordinary named
agents for delegation; they do not need a second agent lifecycle.

## Forking from history

A caller can ask for another version of Bob from any retained resumable
checkpoint, not just the current tip. Return a fresh durable bot ID, an optional
new name, and the source session/checkpoint. The child can run independently
while Bob continues. The source history is immutable; new messages belong to
the branch. Shared history prefixes should avoid copying whole transcripts into
memory or storage for every fork. Retention must keep prefixes referenced by
surviving branches even if the original bot is released.

An event cursor is not automatically a resumable checkpoint. Advertise the
available checkpoints explicitly. The initial implementation may expose complete
turn boundaries; finer boundaries must contain valid model context and resolved
tool-call/result relationships. A partial token stream or an in-flight tool is
not an executable fork point. Reject an unsupported point rather than silently
forking the latest state or replaying an unfinished side effect. Arbitrary
instruction-level process cloning is outside this conversation-fork contract.

Forking neither undoes external actions nor re-executes tools from the copied
prefix. The caller supplies a workspace appropriate to the chosen checkpoint;
conversation history alone cannot restore historical filesystem state. Native
harness adapters must report which historical forks they actually support.

## State and outcomes

Distinguish idle, queued, running, waiting for input, cancelling, succeeded,
failed, and cancelled turns. A controller reconnect must be able to determine
which state actually holds. Losing a connection is not evidence that the turn
stopped. A bot may outlive many terminal turns.

Submission needs an idempotency key or equivalent durable request identity.
After uncertain dispatch, reconcile against the owning engine's state before retrying.
Do not promise exactly-once external tool side effects. If recovery cannot
establish an outcome, report it as unknown rather than replaying work blindly.
A service restart, provider request failure, and native harness process crash
are different recovery cases.

## Observability contract

The event envelope should identify schema version, bot, session, turn, event
sequence/cursor, timestamp, kind, and payload. Preserve backend-native identity
and detail where normalization would lose information. Ordering is defined per
bot; do not manufacture a global causal order across independent bots.

Record submission, dispatch, runtime readiness, messages, exposed reasoning,
tool start/progress/result, approval/input requests, errors, usage, and terminal
outcomes where available. Hidden reasoning is not part of the contract.
Advertise missing backend capabilities instead of emitting misleading empty
successes.

A follower joining during work should see recent activity and then live events.
An explicit cursor permits deterministic replay; reconnects must reveal any
retention gap. Software consumes framed structured events. A human renderer
adds labels, timing, and readable tool output without changing the stored facts.

Drain backend streams independently of individual readers. Use bounded memory
buffers and a disk-backed log or equivalent durable store. Disconnect or page
slow consumers rather than retaining unlimited transcripts in RAM. On a storage
failure, report loss of observability explicitly; the stop/continue policy is an
open decision. Define how acceptance and terminal events become durable before
advertising crash guarantees.

Prompts and tool output may contain secrets. "See everything" means complete
supported execution activity subject to explicit access and redaction policy;
never log authentication material or silently truncate activity. A truncated
payload must say so and identify any retained full output.

## Runtime ownership

A shared runtime should be pooled only across compatible backend versions,
accounts, trust/permission boundaries, and process-scoped configuration. A
workspace-specific setting must not silently change another bot's behavior.
Sharing introduces a correlated failure domain; pool size and recycling need
explicit limits. A single process hosting every bot is not a requirement.

Keep session metadata and event history outside ephemeral working directories.
Expose a missing-session error if required conversation state disappeared. Do not copy user
credential stores to manufacture portable sessions. Capability, auth, and
configuration discovery should avoid starting billable work or login flows.

## Initial permissions

Initial policy: full access (YOLO). Tools registered by the controlling program
run without an Agent approval prompt, within the permissions already granted to
the process by the host or container. This is an explicit product default for
caller-controlled environments, not a claim that Agent supplies isolation.
An Errand workspace snapshot separates workspace copies; it does not itself
restrict host filesystem, network, or credential access. Container provisioning
and host isolation remain caller responsibilities.

Keep one policy boundary before tool execution, carrying bot, session, turn,
tool-call identity, tool name, and arguments. The first policy allows registered
tools. Leave room for allow/deny/request-input decisions without building an
approval UI or a policy language now. Report the effective policy on inspection;
future requests and answers must remain scoped to the originating tool call.
Full access does not disable argument validation, cancellation, resource limits,
or secret-safe event handling. It does not bypass host or provider restrictions
or authorize copying credentials. Native adapters must expose any permissions
they cannot configure rather than claiming that every approval was disabled.

## Core resource strategy

These are design hypotheses to measure, not established savings:

- Keep compact live state per agent. Share immutable configuration and history
  prefixes; load context on demand and evict inactive state. Account for decoded
  history, request serialization, provider state, and stream buffers separately.
- Use asynchronous I/O and bounded execution pools. Model/input waits should
  require no busy polling. Limit tool processes and blocking work separately.
- Parse stream data once where practical; avoid repeatedly copying complete
  growing responses. Bound subscriber buffers and amortize persistence work
  without weakening the advertised durability contract.
- Reuse connections within compatible account and transport boundaries. Measure
  connection setup, request/response bytes, retries, and serialization. Use
  provider continuation or caching only where supported; a prompt-cache hit
  does not by itself prove fewer uploaded bytes.
- Acquire tool runtimes on demand. Count their memory, CPU, and teardown costs
  in total capacity, including any shared MCP servers or execution workers.

Broad model support means extensible provider adapters with capability discovery,
not identical semantics for every model. Keep model calls distinct from tools
and the scheduler. Validate a second provider before freezing the adapter
contract, including streaming, tool calls, usage, and incompatible context.
The goal is to accommodate any model through adapters without baking one model's
prompt format or tool strategy into the core. Preserve provider-specific content
and advertise supported modalities and tool capabilities; reject unsupported
operations explicitly. Broad compatibility is a design goal, not a claim about
the current Responses-only prototype.

## Performance model

Separate fixed supervisor/runtime memory, incremental loaded-session memory,
active-turn memory, retained history, and tool descendants. Track model-waiting,
tool-running, and input-waiting agents separately. All can have different costs;
thousands of queued jobs does not satisfy thousands of active conversations.

Compare an existing shared runtime, a reusable agent core, and a minimal loop;
retain independent native headless execution as a baseline. Measure private/PSS
memory where available, CPU time per turn/event, allocations, process count,
file descriptors, network bytes/connections, startup/first-event latency,
throughput, and p50/p95/p99 admission, event, and cancellation latency. Record
peak and steady-state values and teardown.
Shared RSS must not be reported as private per-agent memory.

First isolate harness overhead with a deterministic synthetic streaming provider
and fixed tool fixtures. Measure its load generator separately. Then validate
real provider behavior with bounded runs. Comparisons need equivalent contexts,
tool outputs, permissions, durability, and concurrency, with source versions
pinned. Mark unmatched harness behavior instead of attributing all differences
to efficiency. A lower-cost run that drops context or events is not a win.

The first Rust target is below 40 MiB total sampled RSS at 32 simultaneous agents
on the equivalent synthetic streaming workload. This excludes durable storage
and tools, which need separate acceptance criteria and measurements. Provider
limits and expensive tool concurrency remain separate from runtime efficiency.

## Language direction

Rust is the selected implementation language for the experimental core. Its ownership model
and lack of garbage collection suit explicit resource lifetimes and concurrent
state management ([language overview](https://rust-lang.org/), checked
2026-09-07). This is an engineering preference, not a measured speed advantage.
Zig's explicit allocation model is also relevant ([overview](https://ziglang.org/learn/overview/),
checked 2026-09-07), but there is no workload evidence justifying a second core
implementation. Choose dependencies against the measured execution boundary;
do not add frameworks merely to create scaffolding.

The current core owns the loop, reuses HTTP/TLS and SQLite, and lazily loads
bounded histories. Durable events are persisted by Agent. See the prototype
record for current configuration and recovery limits.

## Very long conversations

Lifetime conversation size must be independent of active model-context size.
Keep original history on disk; compaction creates a versioned context view rather
than deleting history. Historical forks must use the context view valid at their
checkpoint. Indexed resume/fork and bounded context construction are required
performance cases. See [LONG_HISTORY.md](LONG_HISTORY.md). The current 8 MiB cap
is a prototype limit and does not fulfill this requirement.

## Remaining architectural choices

- Which backend supports independently active sessions within a shared runtime?
- Are native events replayable, or must the supervisor persist them itself?
- How much configuration is per session versus per process?
- Can inactive loaded sessions be reclaimed without ending resumable identity?
- Which auth modes are supported for personal automation versus distribution?
- Which components are cheaper to reuse than replace at the required performance?
- Which storage model supports cheap historical forks and bounded live memory?

Do not answer these by adding generic abstractions in advance.
