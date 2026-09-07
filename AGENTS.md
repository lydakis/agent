# Project guidance

George owns this. Read README.md and docs/NEXT.md before starting work.

## Preserve the question

This is a software-facing agent runtime investigation. The defining requirement
is low overhead for many active agents, plus identity, resumption, and useful
event streams. Do not turn it into another command-alias wrapper, chat UI,
workflow builder, or remote execution service.

Errand is separate. Do not modify it or implement its deferred local execution
and continuation features from this project.

## Working rules

- Keep verified source facts, documentation claims, measurements, and hypotheses
  distinguishable. Pin source revisions and record observation dates.
- Do not claim that an SDK eliminates subprocesses, that a shared process makes
  contexts cheap, or that thousands of stored sessions proves active capacity.
- Prefer existing backend protocols and execution machinery. A backend fork or
  replacement model/tool loop requires an explicit design decision.
- Keep unsupported capabilities explicit. Never silently create a fresh session
  when asked to resume an existing one.
- Preserve native permission and credential boundaries. Never copy credentials
  into this repository or log their values. Auth reuse is backend-specific.
- Keep real prompts, transcripts, machine paths, and benchmark captures in the
  ignored local directory. Use synthetic, non-sensitive examples in tracked docs.
- Scope tool and approval events to the correct bot and turn. Human-readable
  rendering must not be the input protocol for software clients.
- Add behavior tests when implementation begins. Documentation-only changes need
  source/link review and diff checks, not an invented application test suite.
- Do not select a language or add runtime dependencies just to scaffold a repo.
