# Design brief

Status: proposal, 2026-09-07. These are intended semantics, not implemented APIs.

## Purpose and success condition

A controlling program should treat an agent as a named, observable service it
can start working, address again, inspect, and stop. Human-readable logs are a
view of the same activity. The command-line interface is one client.

The project earns its existence if it provides materially better active-agent
resource efficiency than independent native headless processes while preserving
useful harness behavior. Consistent names and output alone do not meet that bar.
An efficient native interface might already satisfy the need; adopting it is a
valid outcome.

## Distinct identities

| Object | Meaning |
| --- | --- |
| Bot | Durable ID and optional human name, backend binding, and configuration. |
| Session | A native conversation and its continuation reference. |
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

## Proposed client operations

The operation names below describe responsibilities, not final CLI syntax or
wire method names.

| Operation | Required behavior |
| --- | --- |
| Create bot | Bind a stable identity to a backend and explicit configuration. |
| Submit turn | Return an accepted turn identity; execution and final success are separate. |
| Inspect | Report current state, backend/session reference, and current/last turn. |
| Follow | Replay retained events from a cursor, then stream live events. |
| Resume | Use the requested native session or report why it cannot be resumed. |
| Steer | Address the expected active turn; reject stale or unsupported requests. |
| Interrupt | Request cancellation; report the final outcome separately. |
| Answer input | Correlate the response to the outstanding question or approval. |
| Fork conversation | Return a new bot/session identity where supported, without implying a workspace clone. |
| Release runtime | Retain bot history while reclaiming inactive execution resources. |

One active turn per bot is the initial policy. Parallel mutations use distinct
bots. Native subagents are descendants with their own activity and resource
accounting, not invisible additional capacity.

## State and outcomes

Distinguish idle, queued, running, waiting for input, cancelling, succeeded,
failed, and cancelled turns. A controller reconnect must be able to determine
which state actually holds. Losing a connection is not evidence that the turn
stopped. A bot may outlive many terminal turns.

Submission needs an idempotency key or equivalent durable request identity.
After uncertain dispatch, reconcile against native state before retrying.
Do not promise exactly-once external tool side effects. If recovery cannot
establish an outcome, report it as unknown rather than replaying work blindly.
A supervisor restart and a provider process crash are different recovery cases.

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
Expose a missing-session error if native state disappeared. Do not copy user
credential stores to manufacture portable sessions. Capability, auth, and
configuration discovery should avoid starting billable work or login flows.

## Performance model

Separate fixed supervisor/runtime memory, incremental loaded-session memory,
active-turn memory, retained history, and tool descendants. Track model-waiting,
tool-running, and input-waiting agents separately. All can have different costs;
thousands of queued jobs does not satisfy thousands of active conversations.

The comparison is independent native headless execution, not the interactive
TUI. Measure private/PSS memory where available, CPU, process count, file
descriptors, startup/first-event latency, event delivery delay, and throughput
with the same backend version, context, tools, and concurrency. Record peak and
steady-state values and teardown behavior. Shared RSS must not be reported as
private per-agent memory.

No numeric performance target is fixed yet. Specify the desired improvement and
acceptable latency/behavior tradeoffs before interpreting measurements. Provider
limits and expensive tool concurrency remain separate from runtime efficiency.

## Open architectural choices

- Is a thin client to one existing native server already sufficient?
- Which backend supports independently active sessions within a shared runtime?
- Are native events replayable, or must the supervisor persist them itself?
- How much configuration is per session versus per process?
- Can inactive loaded sessions be reclaimed without ending resumable identity?
- Which auth modes are supported for personal automation versus distribution?
- Does any essential feature require a backend change or a new agent loop?
- Which language and storage model fit the verified boundary with the least work?

Do not answer these by adding generic abstractions in advance.
