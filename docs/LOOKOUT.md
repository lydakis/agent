# Lookout

`sonny lookout` wakes a project's lead when one of its tasks needs it, and
tells it what its tasks did. It is a Sonny feature and a client of the
daemon like any other: it follows every bot, asks a cheap decision model a
few yes-or-no questions about each event in a lead's tasks, and submits one
short note to the lead. Neither the daemon nor the `agent` CLI knows of it.

## One lookout a daemon

Sonny starts `sonny lookout --store STORE --socket SOCKET` detached when it
opens a project, and again after the daemon restarts. A lookout holds
`STORE.lookout.lock` while it runs, so a second one prints
`{"event":"already_watching"}` and exits. It watches every lead on the
daemon (every bot named `PROJECT.lead`) whichever Sonny opened it, so a
lead hears from its tasks even while no window shows that project. It exits
once the daemon has been unreachable for 30 seconds; the Sonny that starts
the next daemon starts the next lookout. Its own lines go to
`STORE.lookout.log`, and every check to `STORE.lookout.jsonl`.

## What it checks

A lead's tasks are every bot it or one of its tasks made, and every bot
named `PROJECT.*`. Each of these is one check:

| Event | When | Questions |
| --- | --- | --- |
| Said | someone other than the lead writes in a task: the person, a program, another task | redirects, sibling_needs, conflict, remember |
| Ended | a task's turn ends, unless the lead is waiting on it | sibling_needs, conflict, off_brief, stuck, lead_next, result |
| Waiting | a task has waited on the person's approval for 2 minutes | stuck |
| Quiet | a task's running turn has printed nothing for 30 minutes | stuck |

A turn waiting on the person, on other turns or for a slot is not quiet,
and a silence is checked once; printing again starts a new one. The judge
reads the event's text, the lead's latest message to that task (its brief),
the other tasks' briefs and latest lines, and the project's memory indexes.

A high answer (0.70 or more; 0.85 for a result) wakes the lead: at once with `--delivery
steer` when the question is urgent (redirects, conflict), else queued, at
most every 2 minutes. A middle one (0.35 or more) waits for the next note,
or goes alone after 30 minutes. The rest are only counted.

Because the lookout watches the tasks, a lead no longer sets itself
triggers to check on running work; triggers stay for what the person
schedules.

## Subscriptions

What a lead's lookout watches is its `tasks` subscription: the lead's tasks,
every event, every question above. Any bot can subscribe to more, and a bot
that is no lead gets a lookout of its own while it has a subscription:

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
- `--on said,ended,waiting,quiet` picks the events; all four by default.
- `--ask ID` asks only the built-in questions named; with no `--ask`, a
  subscription asks every one that fits the event, unless it looks for
  something of its own, when it asks only that.
- `--look-for TEXT --then ACTION` adds a question of the subscriber's own;
  a high answer wakes it, steering with `--urgent`, and a middle one waits
  for the next note.
- `--context TEXT` is a line every check of it reads.

A bot's shell sets `AGENT_BOT`, so a bot changes only its own
subscriptions; the person names one with `--for BOT`. `add` under a name
already taken does nothing when it says the same and is refused when it
does not, so `rm` it first. Each subscriber's are one file,
`STORE.lookout/BOT.json`, which a running lookout reads again every 15
seconds. A lead's file that holds a `tasks` subscription replaces its own.

The note names a subscription's answer as `deploys look-for 1 0.90` and
says "do what your deploys look-for 1 says (sonny lookout ls)", where
`ls` numbers each subscription's look-fors from 1: the
subscriber's own words stay in its file, since the note reaches the
approver as the person's. `/lookout` in Sonny lists the window's agent's
subscriptions, and how many checks each asked for in the last day.

## Task updates

Some news needs no judge, and every lead gets it, with a judge or none:

- a turn the lead asked for and did not wait on, or another agent or a
  trigger asked for, once it ends;
- an approval only the person can give;
- turns the person asked for in a task themselves, listed apart as theirs;
- what was saved to or removed from the person's and the project's memory,
  by path, less what the lead saved itself.

A turn the lead waits on is not news, nor its own forks. This goes with the
next note, or on its own once the lead rests, at most every 10 minutes;
memory alone never sends one.

## The note

One message, queued (or steered, for an urgent answer), with no origin:

```
Task updates: what your tasks did since you last heard; the wait tool reads each turn by its handle.
May need you:
- calc.docs: turn:calc.docs/3, the person wrote in it (redirects 0.97; pass it to every task it applies to): "Use tabs."
Ended or waiting for an approval:
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
approver's setting too), else `openai-decisions/gpt-6-luna` when
`OPENAI_API_KEY` is set (about $0.0002 a check), else the newest Haiku the
lead's provider lists. `typesafe/jev-*` picks Jev, and any other
`provider/model` runs through the daemon on a fork of an idle, tool-less
`lookout.HASH` bot, as the approver's judge does, so Bedrock alone is
enough. Each fork is deleted once it answers, and one a stopped lookout
left behind is deleted by the daemon after a minute idle. `off` judges nothing, and the lead gets its task updates alone. A
judge whose account is out of quota stops judging until the lookout
restarts.

`sonny lookout --replay --judge SPEC[,SPEC]... [--limit N]` runs the same
checks over the store's retained history and prints each as a JSON line,
waking no one; `bench/lookout_score.py` scores such a run against labels.
