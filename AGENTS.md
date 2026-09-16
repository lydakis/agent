# Project guidance

George owns this. Read README.md and docs/NEXT.md before starting work.

## Preserve the question

This is a software-facing agent runtime investigation. The defining requirement
is minimal memory, CPU, network overhead, and tail latency for many active agents,
plus named identity, resumption, historical conversation forks, and useful event
streams. Performance is the first priority; programs are the clients. Do not
turn it into another command-alias wrapper, chat UI,
workflow builder, or remote execution service.

Errand is separate. Do not modify it or implement its deferred local execution
and continuation features from this project.

## No compatibility branches

This is an experiment with no users. Optimize for the current design,
correctness, and performance; earlier Agent revisions impose no requirements.

- Breaking protocols, CLI defaults, or configuration formats between revisions
  is acceptable. Do not report those breaks as review findings.
- The runtime has one behavior. Do not add or retain legacy modes, fallback
  branches, or shims that keep earlier Agent behavior alive alongside the
  current one. Delete them when found.
- A one-way store migration is not a compatibility branch: it converts old data
  once at open and the rest of the code knows only the current format. Keep
  migrations small and tested; reject what cannot be converted clearly rather
  than resetting it silently.
- Update current callers, tests, and docs together.
- Current-format durability, resumption, and fork correctness still matter.
- Model/provider coverage is a product requirement, including older models.
  Keep provider-specific wire formats, model capabilities, and required protocol
  handling. This policy concerns Agent's own earlier versions; it is not a
  reason to drop models or providers.

## Working rules

- George explicitly selected a custom Rust core as a performance engineering
  project. Keep Pi and Codex as measured baselines, not an adoption gate. Reuse
  standard transport, storage, and profiling components. Record equivalent-workload
  evidence and remaining gaps in docs/REUSE.md.
- Keep verified source facts, documentation claims, measurements, and hypotheses
  distinguishable. Pin source revisions and record observation dates.
- Require matched feature contracts and measurement boundaries for efficiency
  claims. Cross-engine screens with unequal or unknown capabilities are exploratory
  observations, not speedup rankings. Follow docs/COMPARISON_CONTRACT.md.
- Support very long durable conversations with bounded model context and live
  memory. Compaction must preserve original history and historical fork semantics;
  do not silently truncate the transcript. See docs/LONG_HISTORY.md.
- Do not claim that an SDK eliminates subprocesses, that a shared process makes
  contexts cheap, or that thousands of stored sessions proves active capacity.
- A new model/tool loop, component reuse, native server adoption, and a harness
  fork are explicitly in scope per docs/DESIGN.md. Compare their costs before
  selecting an implementation; existing harness compatibility is not mandatory.
- Keep unsupported capabilities explicit. Never silently create a fresh session
  when asked to resume an existing one.
- Initial Agent policy is full access to caller-registered tools, with a narrow
  extensible policy boundary. This does not bypass native/host permissions or
  supply isolation. A workspace snapshot is not a sandbox. Never copy credentials
  into this repository or log their values. Auth reuse is backend-specific.
- Keep real prompts, transcripts, machine paths, and benchmark captures in the
  ignored local directory. Use synthetic, non-sensitive examples in tracked docs.
- Scope tool and approval events to the correct bot and turn. Human-readable
  rendering must not be the input protocol for software clients.
- Add behavior tests when implementation begins. Documentation-only changes need
  source/link review and diff checks, not an invented application test suite.
- Do not select a language or add runtime dependencies just to scaffold a repo.
