# Lookout

`sonny lookout` tells a bot what happened in the bots it subscribed to:
every message someone else wrote in them and how each turn ended. It is a
Sonny feature and a client of the daemon like any other: it follows every
bot and submits one short note to the subscriber as soon as it rests,
which decides itself what matters. A judge decides what cannot wait for a
busy subscriber's turn to end, and picks out a task drifting from what the
person means. Neither the daemon nor the `agent`
CLI knows of it.

The lookout knows no leads. Sonny subscribes each lead to its tasks when it
opens the lead's project, and that subscription is what makes a lead hear
from them; a lead subscribes to more the same way. The lookout also
answers the calls of the tools Sonny answers (`start_task`, `show_agent`
and the rest, `sonny/src/served.rs`).

## One lookout a daemon

Sonny starts `sonny lookout --store STORE --socket SOCKET` detached when it
opens a project, and again after the daemon restarts. A lookout holds
`STORE.lookout.lock` while it runs, so a second one prints
`{"event":"already_watching"}` and exits, unless another build of Sonny
started the one running, as an upgrade leaves it: that one is stopped, and
the new one takes its place, so the tools the new Sonny gives its agents
are answered. `STORE.lookout.sonny` says which build runs. It watches for every bot with
subscriptions on the daemon, whichever Sonny opened it, so a lead hears
from its tasks even while no window shows that project. It exits
once the daemon has been unreachable for 30 seconds; the Sonny that starts
the next daemon starts the next lookout. Its own lines go to
`STORE.lookout.log`, and every check to `STORE.lookout.jsonl`, with each
note it sends (`woke`) and each item it held and dropped because the lead
read that turn itself first (`"skipped": "the lead read it"`).

It keeps a cursor in `STORE.lookout.cursor`: the newest event it saw, or
just before the oldest one whose news it has not told yet. A lookout that
starts again, after a crash or a lost connection, takes what happened after
that cursor as news, so a task that finished while no lookout ran, or whose
news waited on a busy lead, still reaches its lead; news told already may
be told again.

## What it checks

What a lead's `tasks` subscription watches is every bot it or one of its
tasks made, and every bot named `PROJECT.*`. Another lead, which has
subscriptions of its own, is heard only by a subscription with `updates`
that names it as it is, with no `*`, as the person asks a lead to watch
another: then it is a task here in all but what it makes, which stays
its own, and the turns its own lookout's notes start. Two events count:

| Event | When |
| --- | --- |
| Said | someone other than the lead writes in a task: the person, a program, another task |
| Ended | a task's turn ends, unless the lead is waiting on it |

Each is task updates (below), told once the lead rests. Each is also one
check, with up to two questions:

- `interrupt`, only while the lead is in the middle of a turn: can it not
  wait until that turn ends, because it changes what the lead or another
  task is doing right now, or the task is blocked on something only the
  lead or the person can give? A yes (0.70 or more) steers the lead at
  once with `--delivery steer`.
- `drifts`, busy or not, of the person's own messages and of turn ends, in
  a task with a brief: is the task drifting from what the person means,
  misreading them, swinging between readings as they correct it, going in
  circles, arguing past what they asked, or doing part of it while
  presenting it as done? A yes is picked, with what to do, in the note the
  lead gets once it rests. A lead that hears every message reads a
  person's correction or the end of a turn they asked for as theirs and
  lets it pass: in `bench/lookout_turns.py` (3-run means) it caught 11 of
  24 drifting tasks on its own and 13 with drift picked, with no fewer
  results told.

The judge reads the event's text, the lead's latest message to that task
(its brief) or the person's latest words in it, for `drifts` what passed
in that task since the lead last wrote there (the person's messages and the
first line of each reply, the last six), the other tasks' briefs and latest
lines, and the project's memory indexes. Anything less than a yes is told
unjudged with the rest. A resting lead's news waits up to 15 seconds for
checks under way, so a pick and its news usually go in one note. A judge
answers at most two checks at once for one subscriber, so many busy tasks
never crowd the model pool they work in; up to 16 more wait their turn,
and past those the rest go unjudged.

Because the lookout watches the tasks, a lead no longer sets itself
triggers to check on running work; triggers stay for what the person
schedules.

## Subscriptions

Each subscriber has a lookout of its own while it has a subscription. When
Sonny opens a project it adds the lead's `tasks` subscription, unless the
lead's file already holds one by that name, which it keeps as it is:

```
{"name": "tasks", "tasks_of": ["shop.lead"], "bots": ["shop.*"],
 "on": ["said", "ended"], "updates": true}
```

A lead subscribes to more with its `subscriptions` tool, whose `add` takes
a subscription in the file's shape, and whose `remove` takes a name; each
call answers with all of them:

```
{"add": {"name": "deploys", "bots": ["ops.*"], "on": ["ended"],
  "look_for": [{"text": "a deploy finished or failed", "then": "tell the person which", "urgent": true}]}}
{"add": {"name": "review", "tasks_of": ["review.lead"], "ask": ["interrupt"], "context": "we own the parser"}}
{}
{"remove": "deploys"}
```

- `tasks_of` watches every bot one of them or one of its tasks made, and
  `bots` every bot its name matches, `*` matching any run of characters.
  A subscription needs one of them, and may have several.
- `on` picks the events, `said` and `ended`; both by default.
- `ask: ["interrupt"]`, `ask: ["drifts"]` ask those built-in questions;
  with no `ask`, a subscription asks both, unless it looks for something of
  its own, when it asks only that.
- `look_for` adds questions of the subscriber's own, each `text` with what
  it does `then`, asked of every event it watches, busy or not; a yes
  wakes it, steering when `urgent`.
- `context` is a line every check of it reads.
- `updates` adds the task updates below for the bots the subscriber made.

A glob never names a judge or a side chat, nor a bot that deletes itself
once idle.

A call changes only the calling bot's own subscriptions. `add` under a
name already taken does nothing when it says the same and is refused when
it does not, so remove it first. Each subscriber's are one file,
`STORE.lookout/BOT.json`, which a running lookout reads again every 15
seconds. Removing `tasks` stops a lead's lookout until Sonny next opens its
project.

The note names a subscription's answer as `deploys look-for 1 0.90` and
says "do what your deploys look-for 1 says (your subscriptions tool lists
it)", numbering each subscription's look-fors from 1: the
subscriber's own words stay in its file, since the note reaches the
approver as the person's. `/background` in Sonny lists the project's
agents' subscriptions, with their triggers, and how many checks each asked
for in the last day; `d` there removes one.

## Task updates

A subscriber whose subscription says `updates` gets this news, with a
judge or none, about the bots that subscription watches, less those with
subscriptions of their own:

- every message someone other than the subscriber writes in one: the
  person's quoted, anyone else's named by who wrote it and the turn's
  handle;
- a turn the subscriber asked for and did not wait on, or another agent or a
  trigger asked for, once it ends;
- an approval only the person can give, once it has waited 2 minutes;
- a running turn that has printed nothing for 30 minutes, once per
  silence; a turn waiting on the person, on other turns or for a slot is
  not silent;
- a task's turns its goal (SONNY.md) held, once the goal ends: only the
  last, with how it ended (``turn:calc.fix/4 completed, asked by goal,
  and met its goal: `cargo test` exits 0``);
- turns the person asked for in a task themselves, listed apart as theirs;
- what was saved to or removed from the person's and the project's memory,
  by path, less what the subscriber saved itself: the folders its own
  memory indexes name.

A turn the subscriber waits on is not news, nor its own forks. A note goes
as soon as the subscriber rests, or at once with an urgent answer; memory
alone never sends one. News settles for 2 seconds first, and up to 15
while a check of it is under way, so turns that end together and the
judge's answers on them go in one note. A busy subscriber hears once its
turn ends. When a note has waited a minute for a subscriber the events
say is busy, the lookout asks the daemon, so a missed event never holds
the note until the next turn.

## The note

One message, queued (or steered, for an urgent answer), with no origin:

```
Task updates: what happened in your tasks since you last heard; the wait tool reads each turn by its handle.
- calc.docs: turn:calc.docs/3, the person wrote: "Use tabs."
- calc.api: turn:calc.api/4, calc.fix wrote in it
- calc.fix: turn:calc.fix/5 failed, asked by you
The person asked for these in the task themselves, so they are theirs:
- calc.ui: turn:calc.ui/2 completed, asked by the person
A quoted message is the person's own words.
Read the turns you need with one wait call (timeout_ms 0), and act where something changes the work. Tell the person, briefly, what they would want to hear: the outcome of work you started for them, once, when it ends, not again when they write in that task; what only they can decide; and where their latest words contradict what they asked before. If nothing, end your turn with an empty reply.
```

What the judge picked is listed first, under "Picked by your lookout:",
with why and what to do (`drifts 0.82; settle what the person means and
steer the task with that reading`). A note that steers a busy lead says
`interrupt 0.90; it cannot wait for your turn to end` and ends "Handle what
cannot wait, then carry on with your turn." A turn the person asked for in
the task is listed apart as theirs only once it completed: one that failed
or was stopped is listed with the rest.

It quotes only the person's own words and names everything else by handle
and status, so it reaches the approver as the person's and the lead can act
on it without asking. Sonny shows it folded, as `↳ Task updates`, and a
turn it started that ends with an empty reply stays there: no line under
it and no sound. Anything the lead does write shows as any reply.

## The judge

`--judge SPEC`, else `[judge] model` in `~/.agent/config.toml` (the
approver's setting too), else the lead's own model, as the approver picks
(`agent_client::judge`); no smaller model is guessed for a provider.
`[judge] effort` sets a model judge's effort, as for the approver; unset,
the model's own.
`openai-decisions/MODEL` with `OPENAI_API_KEY` asks OpenAI's Decisions API
(about $0.0002 a check with `gpt-6-luna`), and any other
`provider/model` runs through the daemon on a fork of an idle, tool-less
`lookout.HASH` bot, as the approver's judge does, so Bedrock alone is
enough. Each fork is deleted once it answers, and one a stopped lookout
left behind is deleted by the daemon after a minute idle. `off` judges nothing, and the lead gets its task updates alone. A
judge whose account is out of quota stops judging until the lookout
restarts.

`lead` judges with a fork of the lead itself, which reads the lead's whole
conversation: the fork has no tools, is told the check comes from the
lookout, answers the same questions in one JSON object, and is deleted once
it answers. It is slow (it reads everything the lead has) and costs what the
lead's model costs, so it is for comparing judges, not a default. `SPEC+lead`
gives any other judge the lead's last dozen messages and commands as a
`lead` field beside the event.

`sonny lookout --replay --judge SPEC[,SPEC]... [--limit N]` runs the same
checks over the store's retained history, for each bot whose subscriptions
are saved beside it, and prints each as a JSON line,
waking no one; a `lead` judge forks each lead where it stood when the event
happened. `bench/lookout_score.py` scores such a run against labels, by
category, and scores each other judge as the first stage of a two-stage
judge whose doubtful checks go on to the lead's fork.
`bench/lookout_cases/run.sh OUT` builds a store of 26 small scripted
projects, with 126 labelled checks (73 the lead should hear about, 24 of
them a task drifting from what the person means), and replays and scores it
with the decision model, the lead's fork, and the decision model reading
the lead's words. A store from before Sonny wrote leads' subscriptions has
none: write one per lead to `STORE.lookout/LEAD.json`, as above.
