---
name: swarm
description: Start, watch and stop a swarm, several agents on one goal talking through a board, when a goal's pieces must talk to each other
---

# Swarms

A swarm is several agents working on one goal, talking through a board. Its
members are ordinary agents named `SWARM-1`, `SWARM-2` and so on. Each one
gets this folder's `member.md` as its rules at the top of its first message.
`swarm` is the script that does everything on the board. It's Python 3 with
only the standard library, and it talks to your daemon. Run it by its path
beside this file.

## Starting one

From a project coordinator's shell:

```
SKILL/swarm start [--agents N] [--budget MILLIONS] [--in-project] [--row MODEL,SHARE[,IDENTITY[,EFFORT]]]... -- GOAL
```

- Give the shell call `timeout_ms` 600000: a worktree's `.agents/setup` may
  take ten minutes. A start the shell gives up on still finishes or undoes
  itself, and `"$AGENT_BIN" ls` then shows whether its agents exist.
- Without `--row`, every agent runs your model at your effort as a plain
  agent. Each `--row` gives:
  - a model `"$AGENT_BIN" models` lists;
  - its share of the agents in percent (the shares add up to 100);
  - optionally an identity, a role listed under Profiles that keeps `shell`
    in its tools (empty for a plain agent);
  - optionally an effort: low, medium, high or xhigh, or max on Claude.
- `--agents` defaults to 4 and the total budget to 10 million tokens per agent;
  `--budget` sets the total in millions. Every call counts its input again,
  cached input included, so warn before going below 10 million per agent for
  repository-wide work.
- The agents share one new worktree of this folder, `~/.agent/worktrees/SWARM`
  on branch `agent/SWARM`. `--in-project` keeps them in this folder, which a
  folder without git needs.
- The goal is all they are told. Put every requirement from the request in it,
  and let the swarm decide the output's form.

It prints the swarm, its agents and its board. The person watches it, posts to
it and stops it in the app, or asks you to.

## Watching and stopping

- A message starting `[swarm NAME]` carries its final result, or says nothing
  is running and there is no final result.
- `SKILL/swarm status --swarm NAME` gives the details: members, tokens, tasks
  and the result. `result.outcome` is achieved, partial or failed. A top-level
  `outcome` of completed only means a handoff exists.
- `SKILL/swarm add --swarm NAME [--row N]` adds an agent with its own share of
  the budget. `SKILL/swarm stop --swarm NAME` ends every turn its agents and
  their helpers have running. A stopped swarm refuses its agents' posts until
  the person posts again.
- Deleting a member's agent takes it, and its share of the budget, out of the
  swarm.

## The board

`board.jsonl` in the swarm's folder, `~/.agent/swarms/STORE/SWARM/`, is the
whole record: one JSON line per post, assignment, claim, submission, review,
handoff or budget notice. `state.json` beside it is what the board adds up to,
and `swarm.json` lists the members, pinned by bot id. The script changes them
only under an exclusive lock on the board. To change how a swarm works, copy
this folder into `.agents/skills/` under another name and edit the copy. The
thresholds sit at the top of `swarm`, and the rules are in `member.md`. The
app's buttons run `~/.agents/skills/swarm/swarm`, not a copy's script, so a
change the app should follow belongs in that one.
