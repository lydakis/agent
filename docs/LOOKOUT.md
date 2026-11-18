# Lookout

`sonny lookout` is one pipe from sources to the bots that follow them,
as SNS is from topics to subscribers. Every source is the same to it: the
tasks source, which tells what happens in the bots a follower watches
(every message someone else wrote in them, each turn's end, a question
to the person, an approval only they can give, a margin note), the
sources the person sets up outside the daemon, such as a chat poller,
and Sonny's own: the commons, memory and the linked machines. Each
event is one envelope, `[source · topic · from · time]` and its text, and
every follower and every follow is the same too: each
filters, digests and steers alike. The lookout has no rule of its own
for any source; what a source knows, it puts in its events' fields.

It is a Sonny feature and a client of the daemon like any other: it
submits one note at a time to the follower, which decides itself what
matters. Nothing judges the news. Every note goes as a steer, joining
the running turn at its next round or starting one, with everything
waiting then that no digest holds. A follower's notes are one group, as
Prometheus Alertmanager groups alerts, with `group_wait` 0 and
`group_interval` 2 seconds: an event goes at once when no note went in
the last 2 seconds, and what comes within 2 seconds of a note goes
together once they pass, so a burst from many agents is a note every 2
seconds, not one each. A steer waits for none of it. It never holds news
for the follower to rest, so one kept busy still hears it. Neither the daemon nor the `agent` CLI knows of it.

The lookout knows no leads. Sonny has each lead follow its tasks when it
opens the lead's project, and that follow is what makes a lead hear
from them; a lead follows more the same way. The lookout also
answers the calls of the tools Sonny answers (`start_task`, `show_agent`
and the rest, `sonny/src/served.rs`).

## One lookout a daemon

`sonny lookout --store STORE --socket SOCKET` runs as the daemon's
companion `sonny-lookout` (CLI.md, Companions): Sonny adds it when it opens
a project, and the daemon keeps it running for as long as it runs, starts
it again if it exits, and starts it with every later daemon on that store,
whether or not a window is open. A lookout holds
`STORE.lookout.lock` while it runs, so a second one prints
`{"event":"already_watching"}` and exits, unless another build of Sonny
started the one running, as an upgrade leaves it: that one is stopped, and
the new one takes its place; the daemon's companion takes the place of one
a window started detached, of any build. `STORE.lookout.sonny` says which build runs. It watches for every bot with
follows on the daemon, whichever Sonny opened it, so a lead hears
from its tasks even while no window shows that project. It exits
once the daemon has been unreachable for 30 seconds, and the daemon stops
it as it stops. Its own lines go to `STORE.companions/sonny-lookout.log`,
and each follower it watches (`seated`), each follower whose file it
read again after a change (`reloaded`) and each note it sends (`woke`) to
`STORE.lookout.jsonl`. Which revision of each follower's file it took
(the file's modification time, or none for a file it saw go) and when is
kept in `STORE.lookout.taken.json`, written only as one changes, and the
`follows` tool lists it as `lookout`: whether a lookout runs, when it
took the file (`took_ms`), whether what it took is the file as it is now
(`current`), and how often it reads the files again (`rereads_every_s`).
A follower whose rewritten filter, or removed last follow, is not
live yet sees `current: false`.

The lookout only watches and tells. The tools Sonny gives its agents are
answered by another companion, `sonny-tool-host` (`sonny tool-host`; see
SONNY.md, Sonny's tool host), so a tool that runs long, crashes or holds
memory, as an MCP server can, never holds up a lead's news. The lookout
follows those calls' starts and ends: when a `follows` or `commons` call
ends, it reads its files again before the next event, which the stream
puts after that end, rather than at its next reread.

It keeps a cursor in `STORE.lookout.cursor`: the newest event it saw, or
just before the oldest one whose news it has not told yet. A lookout that
starts again, after a crash or a lost connection, takes what happened after
that cursor as news, so a task that finished while no lookout ran, or whose
news waited on a busy lead, still reaches its lead. One lead's news held
back, as by a digest, holds no other lead back: the file also keeps each
lead's own mark where it is past that cursor, up to which that lead was
told, and what comes at or before a lead's mark is history to it, so it is
not told again. An event waiting for a follower is kept on disk with the
rest of what it holds (Sources, below) as soon as it is made, so the cursor
moves past it without losing it. Beside it, the file keeps where the stream starts again:
just before the oldest turn still under way among the bots its followers
watch, as a consumer commits its offset only up to its oldest record still
in work. What came between is replayed as history, so who asked for a turn,
what a follower waits on and a task's resolving are known when the turn
ends. It never reads the whole log again; only a lookout with no cursor yet
reads it all, once, as history rather than news.

It is built to stay light with thousands of bots, as a Kubernetes
controller is with a large cluster. It follows only the event kinds it
reads, so the text models stream never reaches it. Each event reaches only the
followers that watch its bot, or every one for a bot made or deleted,
and each checks that one bot alone. Each follower keeps only the bots it
watches, worked out from who made whom: one index of each bot's makings,
kept as bots are made and deleted, so a follower's start walks its own
tasks rather than every bot listed (10,000 bots: 1.9 s of CPU to start
before, 0.8 s after). Notes and status checks go to the daemon beside the stream, so no
call to it holds the events up. `bench/lookout_scale.py` measures it with
up to thousands of streaming bots. When the daemon lets go of it
(`disconnected` in its log), it connects again from its cursor.

## What it tells

What a lead's `tasks` follow watches is every bot it or one of its
tasks made, and every bot named `PROJECT.*`. Another lead, which has
follows of its own, is heard only by a follow that names it
as it is, with no `*`, as the person asks a lead to watch another: then
it is a task here in all but what it makes, which stays its own, and the
turns its own lookout's notes start. What happens in the bots a follow
watches comes from the tasks source (The tasks source, below), and what
the sources the person set up say comes from those (Sources, below), in
the same notes.

The lookout makes one choice: when. It used to ask a judge model
whether an event could not wait and whether a task was drifting from what
the person means; George dropped that on 2026-10-28 as too ad hoc to
scale. On the eval of how leads act on their notes (`gpt-6-sol`, 3-run
means), the note without the judge's picks handled 41.0 cases against 44.3
with them, caught 11.0 of 24 drifting tasks against 13.3, and sent 23.3
needless messages against 26.0.

Because the lookout watches the tasks, a lead no longer sets itself
automations to check on running work; automations stay for what the person
schedules.

### Bounced messages

The lookout also hands back what a turn never carried out, as mail
returns a letter it could not deliver. It notes which bots sent each
message (an `accepted`, `queued` or `steered` event's `from`), and when
that turn ends failed or interrupted it steers a note to each sender,
waking it when idle:
`[your message to BOT was not carried out: turn N ended CODE]`, timed
like any message. The sender knows what the message was for, so it decides
whether to send again, wait, or tell the person; nobody else could. A
sender that already hears the bot, such as a lead with its own task, gets
the turn's end from the tasks source instead, and a bot's own messages never
bounce. A cause a person can fix does not end a turn at once: the daemon
[holds it](RUST_PROTOTYPE.md#held-turns) for the bot's `hold_ms`, five
days unset, so what bounces is final, or was held that long.

## Follows

Each follower has a lookout of its own while it has a follow. When
Sonny opens a project it adds the lead's `tasks` follow, unless the
lead's file already holds one by that name, which it keeps as it is:

```
{"name": "tasks", "tasks_of": ["shop.lead"], "agents": ["shop.*"]}
```

That is the folder's first lead's. Another lead of the folder,
`shop.lead.billing`, gets `tasks_of` alone, so it hears only of its own
tasks; the first lead's `shop.*` skips the other leads and every bot they or
theirs made.

A lead follows more with its `follows` tool, whose `add` takes
a follow in the file's shape, and whose `remove` takes a name; each
call answers with all of them:

```
{"add": {"name": "deploys", "agents": ["ops.*"]}}
{"add": {"name": "review", "tasks_of": ["review.lead"]}}
{}
{"remove": "deploys"}
```

Every follow hears exactly what it names, nothing more or less: a
lead and its task following the same topic both hear it. A lead
tells a task in its brief which topics to follow, as `NAME:TOPIC`;
a lead that wants to stop hearing a topic narrows its own follow,
with a narrower `sources` or a filter. A follow is held by the bot that made it, by
its ID: a later task given the same name inherits none of it.

`tasks_of` watches every bot one of them or one of its tasks made,
`agents` every agent its name matches, `*` matching any run of characters,
and `sources` every event of the sources the person set up, by name
(below). A follow needs one of them, and may have several. A glob
never names a side chat, nor a bot that deletes itself once idle.

Three more fields say how it hears what it watches, its bots and its
sources alike (Filters, Digests and Steers, below): `filter`, which of
their events it hears; `digest`, how often at most it tells them; and
`steer`, which of them cannot wait. Every note steers; a digest batches
what it names, and what `steer` names goes at once, past any digest. A
follow with none of them is heard as it always was, and costs the
lookout nothing more: no event is built for the bots it watches.

```
{"add": {"name": "api", "agents": ["api.lead"],
         "filter": {"type": ["turn.ended"], "$or": [{"status": ["failed"]}, {"done": [true]}]}}}
{"add": {"name": "leads", "agents": ["*.lead"], "digest": "1h"}}
{"add": {"name": "migration", "tasks_of": ["shop.migrate"], "steer": {"status": ["failed"]}}}
```

Follows end as automations do, by one rule. `until`, as
`"2026-11-08 18:00"` in local time, ends one then, as an automation's
`--until`; `runs` ends one after it was told in that many notes, as an
automation's `--runs` after that many messages, so `"runs": 1` is a
one-shot alert. A note counts once it lands, however many events it
tells, and the count outlasts a lookout restart. A follow also ends
with its bot: deleting a bot ends every follow it held, and one
that watched only bots by exact name, every one of them deleted now, and
no source, ends too. Each end is a line in the lookout's log with why.

```
{"add": {"name": "deploy", "agents": ["ops.deploy"], "steer": {"status": ["failed"]}, "runs": 1}}
{"add": {"name": "launch", "sources": ["slack:launch"], "until": "2026-11-08 18:00"}}
```

A call changes only the calling bot's own follows. `add` under a
name already taken does nothing when it says the same and is refused when
it does not, so remove it first. Each follower's are one file,
`STORE.lookout/BOT.json`, which a running lookout reads again every 15
seconds. Removing `tasks` stops a lead's lookout until Sonny next opens its
project. `/background` in Sonny lists the project's agents' follows,
with their automations; `d` there removes one.

Each follow in the answer's list says under `lately` what it told
in the last 24 hours, once it has told anything: the events and tasks'
news its notes carried (`heard`), those notes (`notes`), how many of them
steered for it, by its `steer` or the source's mark (`steers`), and when
it last told (`last`). The lookout counts them by the hour as each note
lands, in `STORE.lookout/.counts/BOT.json`, written with what it holds;
a follow removed takes its counts with it. So a flood shows as a
follow with many notes, without reading the log.

## Sources

A source is news from outside the daemon: a chat, a mailbox, a feed. The
person sets one up in `~/.agent/config.toml` as a command, which the
lookout runs with `sh -c`, as the person and outside any sandbox:

```toml
[sonny.sources.slack]
command = "~/bin/slack-poll --channel eng"
every = "1m"   # how long after it exits it runs again: 30s, 5m, 1h; a minute unset
```

Every line the command prints is one event: plain text, or a JSON object
with `text` and, if it has them, `id`, `topic` (what it is about: a
channel, a thread), `from` (who wrote it there), `url` and `steer`. An
object whose `text` is empty, as `{"topic": "12", "text": ""}`, is no
event: it clears what the source last said of that topic, or of the
source itself with no topic, as an MQTT message with an empty payload
clears a topic's retained one. No follow hears it; a read (below) no
longer gives the topic. One that exits runs again `every` later, so a poller that
prints what is new and exits and a stream that never exits both work. An
event whose `id` the source printed lately (the last 1024) is not news
again; a poller keeps its own cursor in `$SONNY_SOURCE_DIR`, a folder of
its own beside the store (`STORE.sources/NAME`), and `$SONNY_SOURCE` is
its name. A line over 64 KiB is skipped, and an event's text is cut at
4,000 characters. What it writes to stderr (its first 20 lines a run), a
run that fails, and a source started or stopped go to the lookout's own
log, `STORE.companions/sonny-lookout.log`.
The lookout reads the settings again every 15 seconds: a source added or
changed starts, one removed stops, and a lookout that stops stops its
sources' commands. So does a source when a file its `watch` list names
changed, came or went since it started, as a service manager restarts a
unit on its new binary: `watch = ["~/bin/slack-poll"]`, each file from `/`
or `~/`. An edited script then runs on its new code within 15 seconds, a
stream that never exits included; the log says `source_stopped` with why.
Nothing is watched unless listed, so a cursor the command writes is no
reason to start again. Keep a source's command and the files it watches
where no agent can write: they run as the person, outside any sandbox.

A source whose last 3 runs failed (exited other than 0, or did not start)
is down: every follow of it, whatever its topic or filter, hears so
from the lookout itself, with why the last run failed and its last 5
lines of stderr, and again each day it stays down; once a run works, or
the source prints an event, they hear it works again, before the event. A
source that runs one long command never ends a run, so its first event is
what says it is back; it also starts the count of failed runs again. Its line in the note reads `- slack, said by the
lookout (for your eng): "The source slack is down: …"`, and like any
event from the source it waits for the source's digest. A source no
longer needs code of its own to say that its reads fail. The lookout keeps
its last such word in `STORE.sources/.health/NAME.json` (down or not, since
when, why, the stderr lines), so Sonny's `/background` marks each follow of
a source that is down as failed by reading one small file per source.

A source that cannot read everything, a code-review system with
thousands of changes, say, can let the follows say what to fetch.
Its `wants` names a field of its events, as `id`, `meta.repo` or
`topic`:

```toml
[sonny.sources.cr]
command = "~/bin/cr-poll"
wants = "id"
```

The lookout then writes, to the file `$SONNY_SOURCE_WANTS` names (in the
source's folder), what the follows of it want of that field: the
exact values their filters name for it, every branch of an `$or`
included, and for `topic` the topics named as `NAME:TOPIC` too. `all` is
true while any follow of it names no exact values, a `prefix` or a
bare follow say, so the command fetches everything:

```
{"field": "id", "all": false, "values": ["CR-12", "CR-14"]}
```

The file is written before the command first runs, and again whenever
the follows' wants change, which starts the command again on them,
as an edited script does. So an agent that follows with
`{"filter": {"id": ["CR-12"]}}` has the source fetch CR-12: the
follow is the setup, with no setup step that drifts from it. The
`follows` listing marks such a source with its `wants`, and says
once what that means.

Agents never set a source up; they follow one by name, or one
of its topics as `NAME:TOPIC`:

```
{"add": {"name": "eng", "sources": ["slack"]}}
{"add": {"name": "deploys", "sources": ["slack:deploys"]}}
```

Every event its sources print is told, unless the follow has a
filter. The person decides what cannot wait: an event the command marks
`"steer": true`, or one the source's `steer` pattern in the settings
matches, goes at once, past any digest; so does one a follow's own
`steer` matches (Steers, below). The rest go in the next note, as a
digest allows.

```toml
[sonny.sources.slack]
command = "~/bin/slack-poll"
steer = { from = ["boss"] }    # an event pattern, as a filter is written
```

A pattern that does not check keeps the source from starting, said in
the lookout's log as a bad `every` is. Naming a source the settings do not have is refused, with the
ones they have. An event `from` the follower itself is not news to it.
What waits to be told, and when each source
last told a follower, is kept in `STORE.sources/.held/BOT.json` until it
is, so a restart loses none.

A source's `digest` says how often at most a follower hears it, as
Slack's "send me email notifications at most once every 15 minutes or
hour": the first event after a quiet spell is told as it comes, and what
comes after waits until the digest has passed since the note that last
told that source, then all of it goes: in one note, or in notes one after
another, each as soon as the one before landed, when it is more than a
note holds (192 KiB, 64 quotes), as Alertmanager notifies a whole group at
its interval. The digest counts again from the last of them. An event
that steers never waits.

```toml
[sonny.sources.slack]
command = "~/bin/slack-poll"
digest = "15m"   # 30s, 15m, 1h; unset or 0, as events come
```

A follow's own `digest` takes its sources' place for what it
hears, counted from the note that last told what it held (Digests,
below); one with none goes by the source's.

Sonny makes one source of its own, `commons`, while agents have the
commons: it reads `~/.agents/commons` every 10 seconds in the lookout,
and each new post is an event, its thread the topic, its author `from`,
its id `THREAD/N`. Its first look tells nothing of the past. How far it
read each thread, in `STORE.sources/commons/read.json`, moves only once
every follower that hears the new posts holds them on disk, so a crash
between reading and holding reads them again; a follower already holding
one does not hold it twice. It is set
as any source is, under `[sonny.sources.commons]`, which takes `digest`
(15 minutes unless set; `0` for as posts come) and `steer`, and nothing
else: its command and `every` are Sonny's. Posting in a thread
follows it for the agent as `commons.THREAD`; a person's source may not
take the name. This follow is the only way a thread's posts reach
an agent: an invite starts one turn, and its later replies come here like
any poster's. A turn that only sources woke sends no commons
invites, as an invite's turn does not.

Sonny makes two more sources of its own the same way, every 10 seconds,
each with its cursor in `STORE.sources/NAME/read.json` and a first look
that tells nothing of the past. No agent follows either unless it asks,
with `sources: ["memory"]` or `["links"]`, and each is set under
`[sonny.sources.NAME]` with `digest`, `steer` and `icon` alone.

- `memory`: each fact saved to or removed from the person's memory or a
  project's, under `~/.agents/memory`. The fact's name is the topic, and
  `from` is the agent that saved it, as `calc.fix saved`, or `removed`.
  The text is the fact's description. Its fields are `project` (none for
  the person's own memory), `type` and `file`, the fact's file to read.

  ```
  [memory · release-steps · calc.fix saved]
  Tags are pushed from the Mac; cloud can't push tags.
  ```
- `links`: each machine linked here (`sonny link`) that connects or goes
  away; a machine that sleeps goes away once it has been silent 45 seconds. The
  machine is the topic, and `state` is `up` or `down`. A machine linked
  for the first time tells only that it connected.

  ```
  [links · laptop]
  not connected, last heard 1 min ago
  ```

### Filters

An event's line may carry any other fields beside `text`, and a
follow's `filter` hears only the events whose fields match it. The
bots it watches have events too (The tasks source, below), so one filter reads
both: a follow that names bots and sources hears, of each, what its
filter matches. A CR
poller that prints

```
{"text": "CR-12: tidy the billing tax rules", "id": "CR-12", "url": "https://cr.example/12", "author": "ana", "paths": ["src/billing/tax.rs", "docs/billing.md"]}
```

can be filtered to the changes an agent's work touches:

```
{"add": {"name": "billing-changes", "sources": ["cr"],
         "filter": {"paths": [{"prefix": "src/billing/"}], "author": [{"anything-but": "me"}]}}}
```

A filter is an event pattern as AWS EventBridge and SNS filter policies
write one. Each field the event must have maps to a list of values, any
of which will do, or to a pattern of its own for a field that is an
object. Every field must match, and a field that is a list matches when
any of its items does. A value is a string, number, boolean or null to
equal, or `{"prefix": S}`, `{"suffix": S}`, `{"wildcard": S}` (`*` any run
of characters), `{"equals-ignore-case": S}`, `{"anything-but": V}` (a
value, a list of them, or one of the string matchers), `{"numeric": [">",
0, "<=", 5]}` (whole numbers compared exactly, past the 2^53 a float
holds, as sequence IDs and nanosecond times run) or `{"exists": true}` /
`{"exists": false}`. `"$or": [P1,
P2]` beside the fields takes either pattern. A plain-text line is the
event `{"text": LINE}`. A filter is at most 4 KiB, 256 values and 6 levels
deep; nothing in it runs, and it only ever narrows what is heard.

The lookout checks a filter as each event comes, before the event waits
for the follower, so an event no follow hears wakes no one.

So an agent knows what it may filter on, the lookout keeps each source's
shape: the fields its last 200 events had (an object's own fields named
with dots, as `meta.team`), their kinds, and its last 8 events cut short
(strings at 120 characters, lists at 8 items), in
`STORE.sources/.shapes/NAME.json`, written by a thread of its own so a
source whose fields keep changing never holds up the lookout. Every
`follows` call's answer lists the sources under `sources`, each
with its shape. A field shows an example only from the newest of those
events one of the caller's follows hears, by source, topic and
filter (a filter is not trusted with an event that was cut), so no agent
reads a post from a thread or filter it does not follow; when any example
shows, `examples` says they come from outside. A source whose events name topics lists under `topics` the ones its
last 200 events named, newest first, at most 32: what a follow may
name after the colon, shown to every caller, since a topic is a name to
follow, not a post:

```
{"name": "cr", "events": 41, "fields": {"author": "string, as \"ana\"", "paths": "list of strings, as [\"src/billing/tax.rs\"]", "text": "string, as \"CR-12: tidy…\"", "topic": "string"}, "topics": ["billing", "search"]}
```

A filter naming a field no recent event of its sources had is added, and
the answer says so under `unseen`: it hears nothing until one does.

A call that adds or removes one answers with that change alone: one
added comes back under `follows` with, when it hears sources, the
same `test` a try gives (below); one removed, with `removed`. Every
follow, the sources there are and their events' recent fields come
with a call that sends neither, so twenty changes no longer repeat the
whole listing twenty times.

To try a follow before adding it, a call sends it as `add` with
`"test": true`. Nothing is added; the answer's `test` says what it would
have heard of its sources' last 24 hours: how many events there were
(`events`), how many it hears by source, topic and filter (`heard`), how
many of those steer, by the source's mark or the follow's `steer`
(`steers`), and about how many notes they would have been with its
digest or its sources' (`notes`), with the newest 3 it hears as examples,
outside content as the listing's are. For this the lookout keeps what
each source printed, a command's and Sonny's own `memory` and `links`
alike, in `STORE.sources/.recent/NAME.jsonl`, an
event a line: a day of it, at most 2,000 events and 4 MiB, an event's
fields cut as a shape's examples are past 16 KiB (`cut` then counts them,
since a filter may misjudge a cut event). Past the day, it keeps the
newest event of each topic, of the source itself (its events with no
topic) and the lookout's newest word on it, however old, as an MQTT
broker retains each topic's last message: at most 1,000 of them in 512
KiB. A clear is kept as a line with no text while it is within the day,
and past it goes with all before it, as Kafka compacts a tombstone away.
The same writer thread appends it and cuts it back once an hour or when
it grows past its bytes. The commons is not kept, since its threads are
their members'. What bots do is not kept either, so a follow that
watches bots says so under `untested`.

```
{"add": {"name": "red", "sources": ["ci:main"], "filter": {"status": ["failed"]}}, "test": true}
→ {"test": {"since": "Fri 7 Nov 05:10", "events": 412, "heard": 9, "steers": 1, "notes": 9, "examples": […]}, "examples": "The fields' examples are…"}
```

To know what a source says now without following it, a call sends a
follow as `add` with `"read": true`, as it would a `test`, and nothing
is added: Kubernetes reads a resource with `get` or follows it with
`watch` by one selector, and Alertmanager's alerts API reads the alerts
it holds by a route's matchers or a receiver's name. The add names
sources as any does, `NAME` or `NAME:TOPIC`, with a `filter` if it wants
one; an add that names nothing but its `name` reads the caller's follow
of that name, what it hears. Each source answers with its status as kept
above: the newest event of each topic it hears, and of the source
itself, newest first, as data (`topic`, `from`, `at`, `text`, `id`,
`url`, the other fields under `fields`, `cut` when it was kept cut), a
cleared topic left out; and `down`, the lookout's word, while the source
is down. `outside` says the text comes from outside, as the listing's
examples do. An answer holds what a tool's answer shows, 60 KiB, and
`more` counts the topics past it. The commons and what agents do keep nothing
here (`unread`): the commons tool and the history tool read them. A read
costs one read of one file a source, each line read for its topic alone
and whole only when it is a topic's newest word, whatever the number of
agents: about 2 ms for one topic.

```
{"add": {"name": "red", "sources": ["prs"], "filter": {"state": ["red"]}}, "read": true}
→ {"read": [{"source": "prs", "topics": [{"topic": "12", "from": "ci", "at": "Sat 14 Nov 22:10", "text": "#12 red", "id": "prs/88", "fields": {"state": "red"}}]}], "outside": "What a read gives is…"}
{"add": {"name": "leads"}, "read": true}
```

What sources said goes in the follower's next note, with what the tasks
source said (The note, below). Each event is its header, `[source · topic
· from · time]`, then its text as the source printed it, then its link
and its other fields as compact JSON. The time is when the lookout heard
it, as an Alertmanager notification carries each alert's `startsAt`: the
time of day on the 24-hour clock, as the daemon's stamp on each message
says it, after the day when it is not today and the year when not this
one (`10:39`, `Sat 14 Nov 10:39`); the note's own stamp names the zone.
An event says at most 32,000 characters of its text and as many of its
other fields, one limit for both, each cut with its two ends kept, the
text at 80 KB too:

```
[ci · main · 10:31]
deploy failed on prod
[slack · eng · ana · 10:39]
lunch?
https://example.slack.com/archives/C1/p2
```

Up to 1,024 events wait (`sonny.lookout_waiting`, 0 for no limit), on disk
with the rest of what the follower is to be told, so a restart loses none.
Past that, a newer event is not kept. One that steers takes the place of
the newest waiting event that does not. The follower's next note says how
many were not kept, from which sources and when, under a `[lookout]`
header, for example `40 more events from chat and ci came between 9
minutes ago and 1 minute ago, and they were not kept: 1024 were waiting for
you already`. That note's row shows `40 not kept`. Until a note with that
line lands, the count is kept with the rest and told again. The log names
each event that was not kept, with its source and ID, and `/status` shows
how many wait and how many were not kept today. George chose this over
dropping the oldest (11-02). Keeping every ask is what "nothing fell
through" needs, and a source's own history cannot bring back an event that
was dropped from a script source.

## The tasks source

The tasks source is the lookout's own, `tasks`, each bot it tells of its
topic; a person's source may not take the name. It tells a follower of
the bots its follows watch, less those with follows of their own:

| `type` | When | `from` in the header | Text |
| --- | --- | --- | --- |
| `wrote` | someone other than the follower writes in a watched bot | `the person`, or the agent or program that wrote | the person's words, quoted by the daemon under the note; anyone else's as they wrote them |
| `turn.ended` | a turn ends | `TASK replied to WHO, STATUS`, or `ended` when it said nothing, then `: error` and `, marked done` where so; no `to WHO` when the follower asked | what the task replied |
| `question` | a task asked the person with the `ask` tool, once the call ran | `TASK asked the person` | the question and its numbered options, or the line it asked them to run |
| `approval` | a call only the person can approve starts to wait, once a turn | `TASK waits for the person's approval` | the command, or the call and its arguments |
| `margin` | a task put or took away a margin note | `TASK put the margin note "ID"`, or `took … away` | `TASK/ID`, which the margin tool shows |

Each event's fields, for a follow's `filter` and `steer`, are `type`,
`bot` (where), `maker` (who made that bot), `from`, `turn` and `text`,
and as the kind has them `status` (`completed`, `failed`, `stopped`,
`interrupted`), `done`, `asked_by` (`you`, `the person`, an agent's name,
`automation`), `options`, `command`, `call`, `id` and `removed`. The
source marks the person's words, a question to them and an approval as
steers. Two rules live in the source, as any source keeps its own: a turn
a lookout note started tells nothing, so follows on each other cannot
wake each other in a loop, and an automation's run that completed with
nothing to say tells nothing.

It uses no timers: an event is made from what the stream says happened,
and a turn left running long tells nothing until it ends. Words and
replies are read on connections of their own beside the one the stream
and notes go on, one per logical CPU, as a database client keeps a pool,
so a busy fleet's replies are read side by side (#576). A turn that
completed ended on its reply, so that is one read; another turn's nodes
are looked through for its last words. A read gets 60 seconds, as Codex
gives a tool call: one the daemon never answers lets the event go without
the words (#388). A task's events are told in the order they happened,
though their reads end in another: one waits while an earlier one of its
task is still being read.

### Digests

A follow's `digest`, as `15m` or `1h`, says how often at most it tells
what it hears, in place of its sources' own: the first after a quiet
spell goes as it comes, and what comes after waits until the digest has
passed since the note that last told what it held, then all of it goes,
in notes one after another when one does not hold it, as a source's does.
`0` says it waits for none, though its source digests. Heard by several
follows, an event goes when the first of them allows. What steers never
waits for one. However soon a note tells what a follow heard, its digest
counts again from that note. When each follow's digest last told is kept
with what waits, in `STORE.sources/.held/BOT.json`.

### Steers

A follow's `steer`, a pattern as a filter is written, marks what it
hears that it matches: that goes at once, as the person's words do, and
waits for no digest, and the follow counts it as its steer. A source
marks its own steers, as the tasks source marks the person's words.
`/background` lists each follow with its filter, digest and `steer`.

Who sets which: an agent adds `steer` itself for what it owns, as a pull
request it opened; a `filter` or a `digest` only when the person asks for
one, since what a filter leaves out it never hears and a digest makes it
late. The `follows` tool says so. The person sets a source's own
`digest` and `steer` in the settings.

## The note

One message, steered, with the origin `lookout`, so the reviewer reads its
words as asking for nothing: each event's header and text, in the order
they came, then the trailer. The person's words in a task are not
retyped: the note quotes their turns (`quotes`), and the daemon copies
each in after the note with a line naming the turn and who wrote it, so
the lead reads the person's own words, never a retelling. The reviewer
reads them as the person's but not as their word on the lead's calls: the
lookout picked them, and only words the person sends, or quotes
themselves (not the quotes those bring along), consent. A quote the
daemon refuses (its task deleted meanwhile, or too long) sends the note
again naming those turns by their handles alone.

```
[tasks · calc.docs · the person]
(their words are quoted below, from calc.docs/3)
[tasks · calc.api · calc.fix]
Look at the schema before you change the route.
[tasks · calc.fix · calc.fix replied, completed]
Fixed avg: it skips empty lists now, with a test.
[ci · main]
deploy failed on prod
[lookout]
This note is news for you, unseen by the person. Reply when it bears on what the person is doing now or is urgent; otherwise end your turn with an empty reply.

[Message calc.lead/41, sent by lookout.]

[Quoted from message calc.docs/3, written by a person:]

Use tabs.
```

A note holds at most 192 KiB and 64 quotes, and always one event; the rest
wait for the next, which it says under a `[lookout]` header with how many
wait and when the lookout heard the first of them (`12 more events wait
for the next note, the first heard 10:31`). One note goes
at a time: what comes while one is under way waits for its answer, then
goes. A note the daemon refuses goes again after 2 minutes; one lost with
the connection goes again at once. The note reports what happened; what
the lead tells the person is its role's call (lead.md).

Every note ends with the trailer, under a `[lookout]` header: one setting,
`[sonny] lookout_trailer`, the same for every follower and every source,
read as each note goes, and `""` for none. By default it says when to
reply: when the news bears on what the person is doing now or is urgent,
and otherwise with an empty reply, so the person sees nothing. Agents
forget what their role says when news comes in a turn of its own (George,
11-17), so it comes with every note. The lookout adds nothing else: the
events stay as their sources said them.

Sonny shows it folded, one quiet row named by what it tells: `↳ Lookout ·
calc.fix completed · you wrote in calc.docs · ▲ ci`, each task's event in
a few words and each other source's counted, marked by its icon. A turn it
started that ends with an empty reply stays there: no line under it and no
sound. Anything the lead does write shows as any reply.

![A lead's window: the lookout's note folded to one row, ↳ Lookout · calc.fix-avg completed, then the lead's reply](sonny/lookout-note-folded.png)

![The same note opened: its [tasks · calc.fix-avg · calc.fix-avg replied, completed] header and the task's reply under it](sonny/lookout-note-open.png)
