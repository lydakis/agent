---
name: workflow
description: Write and run a workflow, a script that runs many fresh agents in code, when the person asks for one or the work splits into many independent pieces; read before calling the workflow tool
---

# Workflows

A workflow is a short JavaScript program you write and hand to the `workflow`
tool. Each `agent()` call in it starts a fresh agent with a clean context, in
your folder, with your permissions. The script passes results between agents
in code, and you hear back once, when it ends, in a message from the
workflow. Use one to be thorough (cover many pieces in parallel), to be sure
(independent checks before you commit to an answer), or for work too large
for one conversation (a migration, an audit, a sweep). The agents a workflow
starts have none of the tools that start agents, on purpose, so a check that
needs a new agent of its own (a trap on agent start, say) runs outside the
workflow.

Scout first, inline: list the files, scope the diff, find the sources. Then
run a workflow over that list. For larger work, run several in turn, one per
phase (understand, design, change, review), and read each result before you
write the next.

The language is Claude Code's dynamic workflow language, name for name, so a
script written for one runs on the other.

## The script

Plain JavaScript, not TypeScript: no type annotations. The body runs as an
async function, so `await` works at the top. Standard built-ins are there;
there is no file system, no Node API and no network: agents do that. It must
begin with a header that is a pure literal, with no variables, calls or
`${}`:

```js
export const meta = {
  name: 'find-flaky-tests',
  description: 'Find flaky tests and propose fixes',
  phases: [
    { title: 'Scan', detail: 'grep test logs for retries' },
    { title: 'Fix', detail: 'one agent per flaky test' },
  ],
}
```

`name` and `description` are required; `whenToUse` and `phases` are not. Use
the same titles in `phases` as in your `phase()` calls. What the body
returns is the run's result.

## The API

- `agent(prompt, opts?)` starts an agent. Without `schema` it resolves to the
  agent's final reply, a string; with `schema` (a JSON Schema) to the object
  its reply gives, checked against the schema, with one retry when it does
  not fit. It resolves to `null` when the agent fails, so use
  `.filter(Boolean)`. Options: `label` (shown with it), `phase` (its group;
  set it inside `pipeline` and `parallel` stages rather than rely on the last
  `phase()`), `schema`, `model` (leave it out: the agent runs your model),
  `effort` (`'low'` for mechanical stages, higher only for the hardest
  checks), `isolation: 'worktree'` (a git worktree of its own, costly: only
  when agents change files in parallel), `agentType` (a profile under
  Profiles). Agents are told their reply is a value, not a message, so they
  return data.
- `pipeline(items, stage1, stage2, ...)` runs each item through every stage
  with no barrier between stages: one item can be in stage 3 while another
  is in stage 1. Each stage gets `(prevResult, originalItem, index)`. A stage
  that throws drops that item to `null` and skips its other stages. This is
  the default for work in stages.
- `parallel(thunks)` runs `() => promise` thunks at once and waits for all of
  them: a barrier. A thunk that throws is `null`; the call never rejects.
- `phase(title)` groups the agents that follow; `log(message)` adds a line to
  the run's journal.
- `args` is the tool call's `args`, verbatim. Pass a list as a JSON list, not
  a string of one.
- `budget` is `{total, spent(), remaining()}`: `total` is the call's
  `budget_tokens` (null when not given), `spent()` the output tokens the
  run's agents spent. Past `total`, `agent()` throws. Guard loops on it:
  `while (budget.total && budget.remaining() > 50_000) { ... }`.
- `workflow(nameOrRef, args?)` runs another workflow inline, by a saved
  workflow's name or `{scriptPath}`, and returns what it returns. It shares
  this run's limits. One level only.

`Date.now()`, `Math.random()` and `new Date()` with no argument throw, since a
run must replay the same way: pass timestamps in through `args`, and vary
prompts by index rather than at random.

Limits: at most the CPUs less two, up to 16, agents run at once
(`[sonny.workflows] concurrency`); the rest queue. At most 1,000 agents over a
run (`max_agents`). At most 4,096 items in one `pipeline` or `parallel` call:
more is an error, never a silent cut.

## Pipeline by default

A barrier between stages is right only when the next stage needs every result
of the one before together: to deduplicate across all findings before costly
checks, to stop early when there are none, or to compare one finding with the
others. Flattening, mapping or filtering needs no barrier: do it inside a
stage. If you wrote `await parallel(...)`, then a transform, then another
`await parallel(...)`, and the transform has no cross-item step, make it one
pipeline.

```js
export const meta = { name: 'review', description: 'Find bugs per module, then refute each' }
const FINDINGS = { type: 'object', properties: { findings: { type: 'array', items: { type: 'string' } } }, required: ['findings'] }
const VERDICT = { type: 'object', properties: { real: { type: 'boolean' }, why: { type: 'string' } }, required: ['real'] }
const results = await pipeline(args,
  m => agent(`Review src/${m}/ for bugs a caller can hit. Do not edit files. Give each as file:line and one sentence.`,
    { label: `find:${m}`, phase: 'Find', schema: FINDINGS }),
  found => parallel((found?.findings ?? []).map(f => () =>
    agent(`Try to refute this claimed bug by reading the code: ${f}`,
      { label: 'verify', phase: 'Verify', schema: VERDICT, effort: 'low' })
      .then(v => v?.real ? f : null))))
return results.flat().filter(Boolean)
```

## Patterns

- Adversarial check: several independent agents per finding, each told to
  refute it; keep it only when most fail to.
- Different lenses: when a finding can be wrong in more than one way, give
  each checker its own (correctness, security, does it reproduce) rather
  than the same prompt N times.
- Judge panel: several attempts from different angles, scored by judges,
  then one that builds on the best and borrows from the rest.
- Loop until dry: for an unknown number of things to find, keep finding
  until two rounds in a row bring nothing new; deduplicate against
  everything seen, not only what was kept.
- Completeness critic: a last agent asks what is missing (a source unread, a
  claim unchecked), and what it finds is the next round.
- No silent caps: when the script covers less than everything (top N,
  sampling, no retry), `log()` what it left out.

Scale to the ask: "find any bugs" is a few finders and one check each;
"audit this thoroughly" is more finders, three to five refuters, and a
synthesis. A check only removes: what no finder brings up is lost, so when
missing something matters most, give each piece two finders with different
lenses before adding refuters.

Return what you need to act on: the kept items and the counts (found,
checked, dropped), not every step's working set. A large result reaches you
only as its outline.

## Running it

Call `workflow` with the script inline as `script`: it is saved with the
run, so write no script file in the person's folder. `script_path` runs a
file already there, and `name` a saved one in `.agents/workflows/NAME.js`
(the project's or `~/.agents/workflows/`), which you write only when the
person asks for a workflow to keep. Pass `args` when the script reads
them, and `budget_tokens` when the person gave one. It returns at once with the run's id and where its script is saved.
Carry on, or end your turn: the run's one message wakes you when it ends,
with its result, how many agents ran and failed, and where the whole result
and each agent's value are.

To change a step and go again, edit the saved script and call `workflow`
with `script_path` and `resume_from_run_id`: calls whose prompt and options
did not change are answered from that run without a model call, and the
rest run. The same script with the same args costs nothing. Call `workflow`
with `stop` and a run's id to end it early; you still hear how it ended.

The result is your agents' word. When the person asked for checked
answers, spot-check what it claims against the source before you report it.
Before you explain an empty or odd result, read the run's `events.jsonl`: it
records each agent's value, and why one was null.
