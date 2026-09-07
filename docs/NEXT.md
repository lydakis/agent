# Next decisions

This is an investigation queue, not a commitment to build every capability.
No further Errand demonstration or feature work is needed for this question.

## 1. Resolve the reusable execution boundary

Start with Codex app-server source. Trace how two independently active threads
share runtime services, allocate context, execute tools, own permissions and MCP
connections, and publish events. Compare that boundary with OpenCode's native
server. Reconcile Claude's current SDK documentation with its concrete transport.

Deliverable: a small source-backed ownership map and capability matrix. It must
show which resources are shared, which are per session, which are per turn,
and which remain unknown. If a native server plus a small client already meets
the need, prefer that outcome over creating a general-purpose supervisor.

## 2. Freeze a narrow software contract

Select the minimum operations for named identity, submit/resume, inspect/follow,
steer/interrupt, and explicit input requests. Decide which identities and history
the native backend can own. Specify unsupported capabilities and recovery
behavior. Keep speculative CLI syntax and a universal provider abstraction out
of the initial commitment.

Deliverable: one realistic controller interaction, including reconnect and a
failed resumption. State where code lives and who owns its lifetime. A native
session fork must never silently imply a filesystem fork.

## 3. Establish the performance case before product scaffolding

After source work identifies an actual sharing mechanism, define the acceptance
criteria and a bounded measurement plan. Compare against native headless CLI
execution with equivalent contexts, tools, permissions, and model settings.
Measure active turns separately from idle records and queued work. Include
private/PSS memory, children, event buffering, tail latency, and failure domains.

Deliverable: a go/no-go decision grounded in active resource cost. Thousands of
active agents is the ambition, not an excuse to launch a costly fleet now.
Avoid large or open-ended paid runs; agree on a concrete budget before them.

## 4. Implement only the justified slice

If the evidence supports a useful gap, choose a language and implement one
backend with the smallest durable identity/event layer it requires. Add a second
backend only to validate a real cross-harness boundary. Preserve supported native
authentication and permissions. Add a human log renderer to the same event stream
rather than introducing a separate interactive agent UI.

## Stop conditions

- The result is only names and flags over one full process per bot.
- Existing native APIs already meet the practical requirement.
- Savings require replacing the agent loop or credential behavior beyond the
  intended scope; return to the design decision instead of hiding that expansion.
- Lower memory comes from silently dropping context, tools, permissions, or
  observability rather than more efficient execution.
