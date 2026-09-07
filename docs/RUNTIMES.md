# Runtime investigation

Observed 2026-09-07. Source structure is evidence of architecture, not a resource
benchmark. Documentation may describe a newer or different implementation;
resolve discrepancies against pinned versions before selecting dependencies.

## Current assessment

| Candidate | Evidence | Assessment |
| --- | --- | --- |
| Codex app-server | Multi-thread manager with shared services; thread/turn protocol. | Lead for investigating shared native execution. Active capacity remains unmeasured. |
| Codex TypeScript SDK | The inspected execution path spawns the native executable. | Convenient interface; not evidence of lower process overhead. |
| OpenCode server | Session CRUD/fork/abort, asynchronous prompts, and event endpoints. | Existing programmable server worth examining before building a new one. |
| Claude Agent SDK | Rich programmable interface; inspected Python default transport launches Claude CLI. | Pin the actual SDK architecture. No shared-runtime saving established. |
| T3 Code backend | Mature provider routing; inspected Codex session startup creates an app-server runtime. | Integration reference, not a proven high-density runtime shortcut. |
| ACP / acpx | Session/event protocol and existing headless client with named sessions. | Potential interface reuse. Protocol compatibility alone does not share processes. |
| Grok | Candidate named in the project brief. | Native protocol and runtime ownership not yet inspected; no capability claims. |

## Codex: use the app-server boundary, not SDK branding

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
is not evidence of pooled active execution. Treat its existing capabilities as
an alternative to building convenience commands here.

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
