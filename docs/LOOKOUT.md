# Lookout

`sonny lookout` wakes a bot when something it subscribed to needs it, and
tells it what its tasks did. It is a Sonny feature and a client of the
daemon like any other: it follows every bot, asks a cheap decision model a
few yes-or-no questions about each event a subscription watches, and
submits one short note to the subscriber. Neither the daemon nor the
`agent` CLI knows of it.

The lookout knows no leads. Sonny subscribes each lead to its tasks when it
opens the lead's project, and that subscription is what makes a lead hear
from them; any other bot can subscribe the same way.

## One lookout a daemon

Sonny starts `sonny lookout --store STORE --socket SOCKET` detached when it
opens a project, and again after the daemon restarts. A lookout holds
`STORE.lookout.lock` while it runs, so a second one prints
`{"event":"already_watching"}` and exits. It watches for every bot with
subscriptions on the daemon, whichever Sonny opened it, so a lead hears
from its tasks even while no window shows that project. It exits
once the daemon has been unreachable for 30 seconds; the Sonny that starts
the next daemon starts the next lookout. Its own lines go to
`STORE.lookout.log`, and every check to `STORE.lookout.jsonl`, with each
note it sends (`woke`) and each item it held and dropped because the lead
read that turn itself first (`"skipped": "the lead read it"`).

## What it checks

What a lead's `tasks` subscription watches is every bot it or one of its
tasks made, and every bot named `PROJECT.*`. Each of these is one check:

| Event | When | Questions |
| --- | --- | --- |
| Said | someone other than the lead writes in a task: the person, a program, another task | redirects, conflict, drifts (the person's words only) |
| Ended | a task's turn ends, unless the lead is waiting on it | conflict, drifts, stuck, result (not on a turn the person asked for in the task) |

`conflict` asks both whether the work collides with another task's and
whether it holds something another task needs. An approval the person has
not given and a turn that has printed nothing for 30 minutes need no
judge: they are task updates. The judge
reads the event's text, the lead's latest message to that task (its brief),
the other tasks' briefs and latest lines, and the project's memory indexes.
For `drifts` it also reads what passed in the task since the lead last
wrote to it: the person's messages and the first line of each reply, the
last 6. A yes asks the lead to settle what the person means and steer the
task with that reading.

A high answer (0.70 or more) to an urgent question (redirects, conflict)
steers the lead at once with `--delivery steer`. Any other high answer
goes in the next note, under "May need you", as soon as the lead rests,
as task updates do. A middle one (0.35 or more)
sends no note by itself and rides along with the next. The rest are only
counted.

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

Any bot can subscribe to more:

```
sonny lookout add deploys --bot 'ops.*' --on ended \
  --look-for "a deploy finished or failed" --then "tell the person which" --urgent
sonny lookout add review --tasks-of review.lead --ask conflict --context "we own the parser"
sonny lookout ls
sonny lookout rm deploys
```

- `--tasks-of BOT` watches every bot BOT or one of its tasks made, and
  `--bot GLOB` every bot its name matches, `*` matching any run of
  characters. A subscription needs one of them, and may have several.
- `--on said,ended` picks the events; both by default.
- `--ask ID` asks only the built-in questions named; with no `--ask`, a
  subscription asks every one that fits the event, unless it looks for
  something of its own, when it asks only that.
- `--look-for TEXT --then ACTION` adds a question of the subscriber's own;
  a high answer wakes it, steering with `--urgent`, and a middle one waits
  for the next note.
- `--context TEXT` is a line every check of it reads.
- `--updates` adds the task updates below for the bots the subscriber made.

A glob never names a judge or a side chat, nor a bot that deletes itself
once idle.

A bot's shell sets `AGENT_BOT`, so a bot changes only its own
subscriptions; the person names one with `--for BOT`. `add` under a name
already taken does nothing when it says the same and is refused when it
does not, so `rm` it first. Each subscriber's are one file,
`STORE.lookout/BOT.json`, which a running lookout reads again every 15
seconds. `rm tasks` stops a lead's lookout until Sonny next opens its
project.

The note names a subscription's answer as `deploys look-for 1 0.90` and
says "do what your deploys look-for 1 says (sonny lookout ls)", where
`ls` numbers each subscription's look-fors from 1: the
subscriber's own words stay in its file, since the note reaches the
approver as the person's. `/background` in Sonny lists the project's
agents' subscriptions, with their triggers, and how many checks each asked
for in the last day; `d` there removes one.

## Task updates

Some news needs no judge, and a subscriber whose subscription says
`updates` gets it, with a judge or none, about the bots it made, less those
with subscriptions of their own:

- a turn the subscriber asked for and did not wait on, or another agent or a
  trigger asked for, once it ends;
- an approval only the person can give, once it has waited 2 minutes;
- a running turn that has printed nothing for 30 minutes, once per
  silence; a turn waiting on the person, on other turns or for a slot is
  not silent;
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
Task updates: what your tasks did since you last heard; the wait tool reads each turn by its handle.
May need you:
- calc.docs: turn:calc.docs/3, the person wrote in it (redirects 0.97; pass it to every task it applies to): "Use tabs."
Ended, waiting for an approval, or gone quiet:
- calc.fix: turn:calc.fix/5 failed, asked by you
The person asked for these in the task themselves, so they are theirs:
- calc.ui: turn:calc.ui/2 completed, asked by the person
4 other events checked; none needs you.
A quoted message is the person's own words. Do what each line under "May need you" says where it holds; if nothing needs you, end with one line.
```

It quotes only the person's own words and names everything else by handle
and status, so it reaches the approver as the person's and the lead can act
on it without asking. A turn the judge picked is not listed again below.
Sonny shows it folded, as `↳ Task updates`.

## The judge

`--judge SPEC`, else `AGENT_JUDGE` (in the shell or `~/.agent/env`, the
approver's setting too), else the lead's own model, as the approver picks
(`agent_client::judge`); no smaller model is guessed for a provider.
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
none: add one per lead with
`sonny lookout add tasks --for LEAD --tasks-of LEAD --bot 'PROJECT.*' --updates`.
