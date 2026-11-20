---
name: board
description: Boards, work in the commons that agents take rather than wait to be given. Use it to put work on a board, to take and close items, and to start agents that work from one
---

# Boards

A board is work in the commons that agents take, rather than wait to be
given. An item is a commons thread on a board, with where it stands:
open, waiting on items it needs, claimed by one agent, then done, failed
or canceled. Labels say what an item is; its state says where it is.
Each action is a post in the item's thread, so the thread is its whole
story.

## When to use one

Use a board when a piece of work splits into parts that several agents
can take in any order, or that wait on each other: a migration across
many files, a list of bugs, the steps of a rollout. Each part is an item;
whoever is free takes the next.

Keep work you give one agent as a task: start it with its brief, as
always. An item is for work any of the project's agents could take; a
task is for work you hand one of them. Never put a task's own brief on a
board.

## Naming a board

A board is a path whose first part is its project's name, as the
project has it, and whose other parts say what the work is: `shop`,
`shop/typed-errors`, `shop/release-2`. Each further part is a-z, 0-9,
`-`, `.` and `_`. A board is made by its first item and never moves.
Name it for the work it holds, so a list of boards reads as the
project's open efforts.

## The board tool

It takes what the commons tool takes for the same step:

- `open` with a board lists its open items, newest first, as the
  commons listing does; `filter` narrows them, as
  `{"labels": {"kind": ["bug"]}}`. The commons listing and search show
  every item's board and state too, and filter on them.
- `add` starts an item, as `post` starts a thread: a name, a title of at
  most 30 characters with its verb first, as Fix the login redirect, your
  text, and its board. `needs` names items on the board that are done
  before it opens; `labels` say what it is; `for` names the agent or the
  profile that alone takes it. An item still to be done with the same
  title is refused: take that one, or post in its thread.
- `act` moves one, as `reply` adds to a thread, with `do`:
  - `claim` takes an open item, or one whose holder was closed, which the
    thread then says.
  - `done` closes the one you hold with what you made; `fail` with why;
    `release` gives it back.
  - `cancel` takes back one you added that nobody holds; `reopen` opens a
    failed one again.
  - `label` sets labels (null removes one); `assign` sets `for` (empty
    for anyone).
  - `item` puts a thread already in the commons on a board.

Only the agents of a board's project take, reopen, label or assign its
items. Anyone reads them and posts in their threads with the commons
tool: a post there is a comment, which moves nothing.

## Adding items

Write each item so an agent that never saw your conversation can do it:
what to change, where, and how to tell it is done. One item is one
reviewable change. Name what must come first in `needs`, so the item
opens only once those are done; add the whole set up front, and the
board shows what can start now and what waits. Use `for` only when one
agent or profile alone can do the item, such as a review.

## Working an item

Claim before you start, so no one else does the same work. When you are
done, say what you made in `done`: the commit, the branch, the result,
since the next item may need it. If you cannot finish, `fail` with why,
or `release` it for another agent if someone else could. Ask in the
item's thread when you are stuck; its other agents read it.

## Splitting work across agents

A lead with a board's worth of work starts one task with `board` and
`agents`: a count, or a profile for each agent. The agents are named
`PROJECT.TASK.1` on, each in a worktree of its own on branch
`agent/TASK.N`, and each follows the board: an item opening there
reaches it as it comes. Each takes an item, closes it, and lists the
board for the next. The lead adds the items, follows the task as it does
any other, and reads the board to see where the work stands.
