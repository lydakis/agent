# Lookout

`agent lookout` wakes a project's lead when one of its tasks needs it, and
only then. It is a client of the daemon like any other: it follows every
bot, asks a cheap decision model a few yes-or-no questions about each event
in a lead's tasks, and submits a short note to the lead when an answer is
high. The daemon knows nothing of it.

## One lookout a daemon

The app and Sonny each start `agent lookout --store STORE --socket SOCKET`
detached when they connect, and again after the daemon restarts. A lookout
holds `STORE.lookout.lock` while it runs, so a second one prints
`{"event":"already_watching"}` and exits. It watches every lead on the
daemon (every bot named `PROJECT.lead`) whichever client opened it, so a
lead hears from its tasks even while no window shows that project. It exits
once the daemon has been unreachable for 30 seconds; the client that starts
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

A high answer (0.70 or more) wakes the lead: at once with `--delivery
steer` when the question is urgent (redirects, conflict), else queued, at
most every 2 minutes. A middle one (0.35 or more) waits for the next wake,
or goes alone after 30 minutes. The rest are only logged. The note names
each turn's handle, why it was picked and what to do, and quotes only the
person's own words, so it reaches the approver as the person's and the
lead can act on it without asking.

Because the lookout watches the tasks, a lead no longer sets itself
triggers to check on running work; triggers stay for what the person
schedules.

## The judge

`--judge SPEC`, else `AGENT_JUDGE` (in the shell or `~/.agent/env`, the
approver's setting too), else `openai-decisions/gpt-6-luna` when
`OPENAI_API_KEY` is set (about $0.0002 a check), else the newest Haiku the
lead's provider lists. `typesafe/jev-*` picks Jev, and any other
`provider/model` runs through the daemon on a fork of an idle, tool-less
`lookout.HASH` bot, as the approver's judge does, so Bedrock alone is
enough. `off` judges nothing. A judge whose account is out of quota stops
judging until the lookout restarts.

`agent lookout --replay --judge SPEC[,SPEC]... [--limit N]` runs the same
checks over the store's retained history and prints each as a JSON line,
waking no one; `bench/lookout_score.py` scores such a run against labels.
