---
name: workflow
description: Run many fresh agents from a plan script you write, combine what they return in code, and hear back once
---

# Workflows

Use a workflow for work with many independent pieces: an audit across many
files, a migration, research over many sources, cross-checking findings, or a
long task that finishes sooner split up. Do one or two pieces yourself instead.

You write a plan, a Python script. `workflow.py`, beside this file, runs it
outside your turn. Each `agent()` call starts a new agent with a clean context
that sees only its prompt, your code passes results from one to the next, and
when the plan ends you get one message with its `result`. Nothing wakes you
for each agent.

## Running one

1. Write the plan to a file.
2. Start it with the shell tool and `detach: true` (it refuses to run
   otherwise):

   ```sh
   python3 /path/to/this/folder/workflow.py start plan.py --name audit-api
   ```

   Options: `--parallel N` agents at once (default 16, at most 64),
   `--max-agents N` for the whole run (default 100, at most 1000), `--timeout`
   (default `24h`), `--agent-budget-tokens N` for any agent without its own.
3. Carry on or end your turn. The result arrives as a new message, sent from
   the turn that started the run, that begins `Workflow run audit-api ended`,
   with counts, tokens used and the result; the run's folder holds
   `result.json`, `events.jsonl` and `log`.

`workflow.py status [NAME] [--agents] [--pretty]` shows progress and
`workflow.py stop NAME` stops a run, interrupting its agents.

## The plan

These are defined for the plan; set `result` before it ends.

- `agent(prompt, label=None, phase=None, schema=None, model=None, effort=None,
  budget_tokens=None, profile=None, instructions=None, workspace=None,
  tools=None, retries=1)` starts an agent and returns its `Result` when it is
  done: `ok`, `text` (its final reply), `data` (the parsed reply, with
  `schema`), `status`, `error`, `detail`, `bot`. It never raises because an
  agent failed; check `ok`. Without `model` and `effort` an agent takes yours.
  It composes AGENTS.md and the skills index like you, unless `profile` or
  `instructions` say otherwise.
- `parallel(items, fn)` runs `fn(item)` for every item at once and returns the
  results in order; `parallel([f, g])` runs callables. An exception in one
  becomes a failed `Result` in its place.
- `pipeline(items, stage1, stage2, ...)` sends each item through the stages on
  its own, so one item can be in stage 2 while another is in stage 1. A stage
  takes the previous stage's output.
- `phase(name, description='')` groups the agents that follow.
- `log(text)` adds a line to the run's events.
- `result` may hold `Result`s; it must otherwise be JSON.

```python
phase("Review", "One reviewer per module")
modules = ["auth", "billing", "search"]
FINDINGS = {"type": "object", "required": ["findings"],
            "properties": {"findings": {"type": "array", "items": {"type": "string"}}}}
reviews = parallel(modules, lambda m: agent(
    f"Review src/{m}/ for bugs a caller can hit. Do not edit files. "
    "Reply with each finding as file:line and one sentence.",
    label=f"review-{m}", schema=FINDINGS))

phase("Verify", "A second agent checks every finding")
claims = [f for r in reviews if r.ok for f in r.data["findings"]]
checks = parallel(range(len(claims)), lambda i: agent(
    f"Check this claimed bug by reading the code: {claims[i]}\n"
    "Reply real or not-real, then why.", label=f"verify-{i}"))

result = {"real": [c for c, v in zip(claims, checks) if v.ok and v.text.startswith("real")],
          "not_covered": [m for m, r in zip(modules, reviews) if not r.ok]}
```

## Rules

- An agent sees its prompt, not your conversation. Name the files, the goal
  and what to reply with.
- Agents share your folder. Make them read-only, or give each its own files.
- Up to 16 KiB of an agent's reply comes back (`truncated` says when it was
  cut). Larger output belongs in a file named in the reply.
- Give a `schema` to any reply your code reads. A reply that does not fit is
  sent back to the same agent `retries` times with what was wrong.
- Routing, filtering, deduplicating and counting are plain Python, which
  costs nothing. Keep them out of prompts.
- Label every agent when a run may need resuming. Starting the plan again with
  the same `--name` reuses finished agents whose label and prompt match,
  waits again for ones still running and reruns the rest.
- Runs do not nest: an agent of a run cannot start one.
- Every agent costs tokens. Try a small run first and set budgets.
- Each agent stays a bot, named `YOU-NAME.LABEL` after you, the run and its
  label (`status NAME --agents` lists them). Read it with `agent turns`, or
  continue or fork it after the run.
