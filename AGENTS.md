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

## Keep the daemon fast and light

The daemon is one `current_thread` runtime that every bot shares. A change to
it keeps these, and says in its PR how:

- Nothing blocks the runtime thread. File and SQLite work runs off it
  (`tokio::fs`, the storage threads). Work that grows with a payload, such as
  decoding, hashing or copying megabytes, stays bounded and off hot paths.
- A waiting bot holds nothing live. A wait of a second or more (a closed
  pool, an allowance refill, a retry backoff, `wait`, an approval, an answered
  tool) parks durably: no task, no active slot, no timer of its own. The run
  loop owns the one wake-up schedule. So does a client's request that waits
  (`wait`, `approve_step`): its deadline is an entry in the handles'
  schedule, which the run loop sleeps on.
- Bots never block each other. Shared bounds (active slots, the process
  budget, a provider pool, a session's output queue) are taken in fair order
  and never held while waiting on something else; one bot's wide round does
  not take every slot.
- An idle bot costs about a kilobyte. Large content (tool output, artifacts,
  pictures) lives in SQLite once and streams from it; nothing kept per bot
  grows with its history.
- Store writes on a turn's path join a group commit: submit independent jobs
  without awaiting each other. Count store jobs per turn when adding one.
- The pacer learns from everything a provider says (rate-limit headers,
  `retry-after-ms` and `retry-after`, named delays, usage) and estimates what
  the provider counts. It invents no caps and refuses only what can never fit.
- Every provider error has a class: a rate limit (429, slow down) closes the
  pool and parks; an overload (529, overloaded) backs off its own call and
  closes the pool only when it names a delay or overloads cluster; transient
  (5xx, stalls, connections) backs off and parks; the rest are final. A new
  error code gets a class and a test.
- A change on a hot path carries a matched before/after screen against main,
  recorded in docs/DAEMON_MEASUREMENTS.md: `bench.lifecycle` (32-bot socket
  echo), `bench.completion_burst`, `bench.admission_burst`, and
  `bench.footprint` for memory.
  The Perf workflow screens every pull request that touches `src/` against
  its base this way (`bench/perf_gate.py`): more store work per turn fails
  it, and CPU, latency or memory past 15% with no overlap warns.

## Working rules

- A pull request that adds, changes or removes a Sonny feature updates its
  row in sonny/FEATURES.md (status, history, why) in the same pull request.
- The role files in sonny/agents have byte budgets (`BUDGETS` in
  sonny/src/project.rs, checked by a test). Text added there cuts as much
  elsewhere: merge or drop a sentence, or leave to memory what the person
  can teach. Raise a budget only with George's say.
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
