# Workflows

A workflow is a plan script an agent writes and a runner executes outside the
agent's turn. Each `agent()` call in the plan starts a fresh bot with a clean
context, the plan passes what they return from one to the next in code, and
the agent that started the run hears back once, when the plan ends. It ships
as a skill, [app/skills/workflow](../app/skills/workflow/SKILL.md), with no
daemon or CLI change.

## Why

Claude Managed Agents put the same idea in public beta as "dynamic workflows"
(Claude Devs on X, 2026-10-09; docs read the same day:
`platform.claude.com/docs/en/managed-agents/workflow-runs`). A lead writes a
program that runs many agents in phases; results move between them
programmatically, and the lead is not woken per worker. Anthropic reports a
planted-bug test where a single agent found 14 to 27 of 70 bugs and a
workflow found 66 in each of three runs. That is their claim, with no
published method; nothing here reproduces it.

Two findings in this project point the same way. Scripting tool calls cut
input tokens 10 to 17 times against one model call per step (the code-mode
evaluation, 2026-09-29, on George's Mac). And a coordinator is woken for
every task that ends a turn, which costs it a turn each time. A workflow
moves fan-out, routing and counting into code the model writes once.

## How it works

`workflow.py start PLAN.py --name NAME` runs the plan with these defined:
`agent()`, `parallel()`, `pipeline()`, `phase()`, `log()`; the plan sets
`result`. SKILL.md is the reference an agent reads.

- **An agent is a bot.** `agent()` runs `agent run --new --detach --no-spawn`
  with the prompt on stdin, composed with `--agents` unless the plan names a
  profile or instructions. Inside a bot's shell the CLI declares that bot as
  the creator and its turn as the prompt's author, as it does for any peer.
  A lead's agents are named `LEAD-NAME.LABEL`, under the lead's name like its
  forks and side chats, which the app leaves out of a coordinator's task
  updates (the run reports instead). Without a lead, `NAME.LABEL`.
- **One connection waits for all of them.** The runner reads the daemon's
  socket from `agent start` and sends one `wait` request per agent's turn on
  a single connection; the daemon answers each when that turn ends, with its
  status and final text (16 KiB at most). A connection lost to a daemon
  restart is reopened and the pending waits sent again, since handles are
  durable; meanwhile commands wait for the daemon instead of starting one.
- **Detached, and only detached.** A bot starts a run with the shell tool's
  `detach: true`, so the run outlives the call and holds a detached slot,
  not a process slot. The runner refuses to start in a bot's foreground or
  background shell (`detach_required`): the daemon would kill it when the
  call returns. The CLI refuses a blocking `wait` anywhere in a tool shell,
  detached ones included, because in the foreground it would hold a process
  slot; the runner sends the protocol's `wait` itself, which holds only its
  connection.
- **Reporting once.** At the end the runner queues one message to the lead
  (`run --detach --delivery queue`, pinned by `--bot-id`): status, counts,
  tokens used, the result up to 8 KiB, and the run folder.
- **Schemas.** With `schema`, the prompt asks for a bare JSON value; the
  reply is parsed and checked (type, enum, required, properties, items) and
  sent back to the same bot with what was wrong, `retries` times.
- **Failures stay in place.** An agent that fails returns a `Result` with
  `ok` false; an exception inside `parallel` or `pipeline` becomes one. Only
  a stop, the timeout or `--max-agents` ends a run early, as a
  `BaseException` a plan's `except Exception` does not catch.
- **The record.** `~/.agent/workflows/NAME/` holds `run.json` (state now,
  written at most every 250 ms), `events.jsonl` (an event per phase, agent
  start and end, reply asked for again, and log line), `result.json`, `log`
  and `lock`. The runner holds `lock` (an flock, with its pid) while it
  runs, so a second runner of the same run is refused (`run_running`), and
  `status` and `stop` know the runner from the lock, never from a pid alone
  that the system may have reused.
- **Tokens.** The runner keeps the turn of every handle it waits on, earlier
  runners' included. At the end it reads each agent's turns from the first
  of those (the protocol's `turns`) and sums theirs: one small request per
  agent, not a listing of the store, and a turn someone gives an agent
  after the run is not counted.
- **Resuming.** Starting the same plan with the same `--name` reads
  `events.jsonl`: a labelled agent whose prompt and settings hash the same
  and finished `ok` is reused without a model call, one still running when
  the last runner ended is waited on again (the latest reply asked for, so a
  schema retry is not asked twice), and others get a new bot (`LABEL.2`).
  Each agent is recorded before it is asked for, with a request id of the
  run's; if a runner ended between asking and recording the handle, the
  next one finds the turn as that bot's first with that id and waits on it,
  takes the name again if the bot was never made, and otherwise leaves the
  bot alone and makes `LABEL.2`.
  The settings include the defaults a new agent would take: the folder the
  runner starts in, `AGENT_MODEL` and `AGENT_REASONING`. The contents of
  AGENTS.md and skills are not hashed. Labels made up by the runner follow
  call order, which threads make unstable, so only labelled agents are
  reliably reused. `--max-agents` counts every agent the run has made,
  earlier runners' included, so resuming a failing plan does not start
  another hundred.
- **Stopping.** `workflow.py stop NAME` sends the runner SIGTERM; it
  interrupts every running agent and ends `stopped`. The timeout (default
  24 h) does the same, ending `failed` with `timeout`. The plan runs on its
  own thread; one busy outside `agent()` gets 5 s to reach it, and then the
  run ends without it. Reaching `--max-agents` stops the run the same way,
  at once, whatever the plan's other branches are doing.
- **No nesting.** An agent of a running run cannot start one.

## Cost

Synthetic Responses model (`bench/synthetic_model.py`), release build at
`f1325c6`, Linux container, 4 x86_64 cores, Python 3.13, 2026-10-09,
`python3 -m bench.workflow_overhead --agents N --parallel 64 --delay-ms D`. The `workflow` arm runs the plan; the `direct` arm
does the same commands from a bare loop with one connection, including the
runner's request ids and its one `turns` read per bot for its tokens at the
end, so the difference
is the runner's own bookkeeping. Two rounds each, order
alternated.

| Agents, at once, reply delay | Arm | Wall | Runner CPU | `agent` CLI CPU | Runner peak RSS | Daemon CPU |
| --- | --- | --- | --- | --- | --- | --- |
| 200, 64, 500 ms | workflow | 2.29, 2.27 s | 0.45, 0.41 s | 0.60, 0.50 s | 24.1, 24.2 MiB | 0.56, 0.53 s |
| 200, 64, 500 ms | direct | 2.30, 2.26 s | 0.38, 0.37 s | 0.62, 0.58 s | 23.6, 23.7 MiB | 0.57, 0.55 s |
| 1,000, 64, none | workflow | 3.42, 3.35 s | 2.08, 1.97 s | 2.65, 2.73 s | 39.2, 38.6 MiB | 2.44, 2.37 s |
| 1,000, 64, none | direct | 3.21, 3.04 s | 1.40, 1.45 s | 2.78, 2.76 s | 35.1, 34.9 MiB | 2.36, 2.23 s |

Per agent, the runner adds about 0.6 ms of CPU over the bare loop; the
`agent run` process it starts costs about 2.7 ms, and the daemon about 2.4 ms,
of which about 0.2 ms is the token read at the end (asking for them all at
once measured no faster). With 200 agents of 500 ms in waves of 64, both
arms finished 0.3 s after the 2.0 s the waves alone take; at 1,000 instant
agents the run took 0.3 s longer than the loop. Peak RSS is the Python
process with a thread per item, 39 MiB at 1,000. Both arms ran slower on
the host than at `b28a54c` (0.5 ms then); the record written before each
agent is asked for costs about 5 µs. These hold for this
workload and host only; macOS is not measured.

## Gaps

- **No run-wide token budget.** Each agent can have one (`budget_tokens`,
  `--agent-budget-tokens`), which the daemon enforces. A run total needs each
  agent's usage while it runs; the turn view in the CLI plan (C3) brings it.
- **The app lists each agent under its lead.** A run is one row with phases
  in the new UI's plans, not yet.
- **A process per agent.** `agent run` composes instructions client-side,
  which the runner reuses rather than reimplementing; it is the largest
  per-agent cost above.
- **Text only, 16 KiB.** Larger outputs go to files named in the reply.
- **Real-model value is unmeasured.** The swarm comparison (flat, council,
  single lead, workflow) on George's Mac is where that gets measured.
