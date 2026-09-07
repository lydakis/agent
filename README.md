# Agent

An agent runtime for software clients.

The goal is to let a program create named bots, give them work, steer or resume
that work, and follow their activity as an ordered stream of events. Humans
should be able to read the same stream as useful service logs.

The reason to build this is efficient execution of many **active** agents. A
common command syntax around one full harness process per bot is not enough.
The ambition is thousands of active conversations; no capacity claim has been
established.

**Status: design and source investigation. There is no executable yet.** Agent
is a working name. No language, public API, or runtime dependency is selected.

## What matters

- Stable bot identities and explicit conversation resumption.
- Low incremental memory and CPU cost per active bot.
- Live activity and replay after reconnecting: messages, tool calls and results,
  errors, state changes, approvals, and usage where the backend exposes them.
- Programmatic steering, interruption, and completion with clear semantics.
- Reuse of existing harness behavior and supported authentication paths.

The starting hypothesis is a small supervisor over shared native runtimes,
with a software API and a thin CLI client. Sharing must be demonstrated in the
backend; calling something an SDK does not establish it.

## Scope

Agent owns agent identity, execution lifecycle, and observability. Callers
supply workspaces and decide where execution happens. Workspace provisioning,
Git branching, diff application, and machine placement belong to callers or
other tools.

Errand could run Agent, but is an optional consumer. Local execution and
continuation inside Errand are separate, deferred features. This project does
not implement them.

## Start here

- [Design brief](docs/DESIGN.md): the proposed contract and open choices.
- [Runtime investigation](docs/RUNTIMES.md): evidence, alternatives, and gaps.
- [Next decisions](docs/NEXT.md): bounded work before choosing an implementation.

Current lead: investigate the Codex app-server directly. Its source manages
multiple threads with shared services. That is evidence of a reusable boundary,
not proof of a particular concurrency or memory saving.
