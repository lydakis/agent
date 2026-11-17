# Code mode and the tool path at fleet scale

Status: design note, third draft, 2026-11-15, approved by George the same
day to be built whole, in the order under
[What changes, in order](#what-changes-in-order). Built so far: slice 1,
tool lists stored once (protocol 77, schema 101; measured in
[DAEMON_MEASUREMENTS.md](DAEMON_MEASUREMENTS.md#declared-tool-lists-stored-once-by-digest-at-6-424-2000-and-5000-bots-protocol-77-schema-101));
slice 2, daemon steps for `shell` and `echo` (protocol 78, schema 102;
measured in
[DAEMON_MEASUREMENTS.md](DAEMON_MEASUREMENTS.md#daemon-steps-at-1000-and-10000-bots-protocol-78-schema-102),
where three of its gates are missed, why, and the derived gate proposed
for the step p99); slice 3, daemon steps for the file tools (`read`,
`write`, `edit`, `apply_patch`, `web_fetch`; no protocol or schema change).
GPT-6 Astra reviewed two
drafts the same day and approved neither slice as scoped. This draft takes
the narrower scopes it said it would approve and has not been reviewed
again; both reviews are recorded [at the end](#review-2026-11-15). It was
then rearranged to keep Agent and Sonny apart as the two products they
are, which split the gates by product and changed no mechanism, and to
say [which half of code mode is Agent's](#which-half-is-agents). Code
facts are from lydakis/agent at `e021f96a`. Other harnesses were
read on 2026-11-15 at the revisions under [Sources](#sources). What is
measured, read in source, claimed by a vendor, or only supposed is marked as
such.

## Two products

Agent and Sonny share a repository and are separate products.

- **Agent** is the daemon and its CLI, a product by itself, for any
  program. It is what stands beside Codex's core, Pi and Pi Durable here.
  It offers mechanisms and names no client.
- **Sonny** is one client of Agent. It brings its opinions (a tool host,
  MCP servers, a browser, a desktop, links, `exec`) on top of what Agent
  offers.

So "Agent has" means any client can use it, and "Sonny has" means Sonny
built it from Agent's mechanisms. What Sonny has, Agent does not thereby
have. Every change below is marked Agent's or Sonny's, Agent's stand
without Sonny, and Agent's gates are measured with no Sonny in the path.

## The question

Code mode is one tool that takes a script. The model writes JavaScript, the
script calls tools as functions, and only what it chooses to print comes
back, so twenty calls and the filtering between them cost one result and one
model round.

Agent has no code mode. Sonny built one, `exec`, in its tool host, from
Agent's declared tools and steps. The question is what, if anything, should
graduate from Sonny into Agent: what of code mode is fundamental, which
Agent should have, and what is an opinion, which a client brings. The
constraints, George's:

- **Agent stays as light and unopinionated as Pi's core, with batteries
  included.** What is fundamental it has; what is an opinion it does not;
  and a client gets a place to plug its own in.
- **One host.** Agent must not end up running a host for scripts while a
  client runs another for its own tools.
- **Fleet scale first.** Whatever is built has to hold from a few agents to
  tens of thousands running at once. Nothing the daemon's scaling rests on
  is given up: one `current_thread` runtime, a waiting bot that holds
  nothing live, an idle bot of about a kilobyte, shared bounds taken in
  fair order.
- **Clients keep their tools and opinions.** Sonny's browser, desktop and
  MCP servers keep working, and another client still brings its own.
- **Sonny's links are not blocked.** A script may run on a linked machine,
  and a tool may live on one.

## The answer in short

Code mode is two things. **A second caller for tools** is fundamental and
has one flavor, so it goes into Agent. **The engine** is an opinion with
several flavors, so a client brings it, in the one host that client
already runs ([which half is Agent's](#which-half-is-agents)). Agent gets
no engine, no script tool and no host. Four changes to Agent, in this
order:

1. **Tool lists stored once.** A client's declared tools are stored once,
   by their content, and referred to by its bots, not copied into each.
   This is not code mode, but everything else rides on declared tools, and
   it is the first thing the measurements below show breaking at fleet
   size.
2. **Steps the daemon runs.** A script's call of one of the bot's own tools
   is run by the daemon as the direct call would be, by a coordinator of
   its own, so the parked turn stays parked. Commands first.
3. **Callable but not shown.** A tool can be a step without being shown to
   the model, so a client can choose code mode only, for one bot or as its
   default for all of them.
4. **Replay, later.** A script that survives a restart, once the first
   three are measured.

Sonny follows each one with a change of its own: its host sends digests,
its `exec` reaches the bot's own tools, and a setting chooses code mode
only. None of Agent's four needs Sonny to be built, tested or measured.

Agent alone therefore has no code mode, as Pi's core has none. That price
is taken knowingly. No harness read here runs an engine in its core loop,
and Codex removed the option.

## What the others do

| | Engine | Where it runs | A script reaches | After a restart |
| --- | --- | --- | --- | --- |
| Codex | V8, a fresh isolate and OS thread per script, no heap limit enforced | A separate host process shared by every thread; the only place since 2026-07-30 | Every tool, through the registry direct calls use | Pending calls get a reset error; stored values are cleared |
| Pi | QuickJS as WASM, a worker thread per script, two per process, 256 MB | A standalone package wired in as a replaceable extension, off by default | Every tool, through the hooks direct calls pass | Lost |
| Pi Durable | The same VM | An extension of its harness | The same | Replayed: each delivered result is journaled, and the script is run again against the journal |
| Claude Code | JavaScript, runtime not stated | Not stated | Shell and edit helpers; each call passes permission rules and hooks | Not stated |
| Anthropic's API | Python | The provider's container; the client runs no engine | Tools marked `allowed_callers`; each call comes back to the client as a `tool_use` naming its `caller` | Not read |
| Agent | None | Nowhere: a client's companion runs one if the client brings one | Nothing of the daemon's; it judges and records the calls a client's tool reports | A waiting call is kept as a row; one already taken cannot be taken again |
| Sonny, on Agent | QuickJS, 4 running and 16 alive, 256 MB | Its tool host, a companion, one for every bot | MCP tools only | The call ends `tool_outcome_unknown` |

All of this is read in source except the Claude Code and Anthropic API
rows, which are their documentation. Agent and Sonny already divide as Pi
and its extension do: the core has no script tool, and the add-on has the
engine. Four facts carry the argument:

- **Pi Durable keeps the caller in its harness and the engine in a
  module.** Each of its tools says who may call it, `callers`: the model,
  other tools, or both. Its code mode is a separate export of the package
  built over that, callable by the model only, so that a script cannot
  start a script (packages/durable/src/harness/types.ts:444).

- **Codex treats code mode as the tool surface and still keeps the engine
  out of its core.** Its model catalog marks every model but one
  `code_mode_only`: the model is shown `exec` and `wait` and nothing else.
  The commit that moved V8 into its own crate removed the in-process
  fallback.
- **Pi's core has no tools.** Code mode and MCP are both extensions.
- **Only Pi Durable survives a restart.** Its module comment prices it: a
  sequential script costs one journal commit and one count commit per
  result, on top of its nested calls' own.

No repository records a measured cost. Codex's only statement is an ignored
test that "requires resources for 2,048 live V8 isolates and OS threads".

## What Agent has today

No code mode: no engine, no script tool, nothing that takes a script. What
it has are the mechanisms a client builds one from
([declared tools](RUST_PROTOTYPE.md#declared-tools)):

- **Companions.** A command a client asks the daemon to keep running,
  started again with backoff when it exits. The daemon knows nothing of
  what it does (src/server/companions.rs).
- **Declared tools a client answers.** A client declares tools on a bot,
  up to 32 and 64 KiB. One declared with `answered_by` waits for
  whichever client serves that tag: `tool_calls` lists the calls,
  `take_call` takes one so that it runs once, `tool_result` answers it.
- **A waiting call holds no task.** Past its hold the turn parks as a row.
- **Steps.** A tool declared with `steps` reports the calls it makes
  inside one call of its own. `approve_step` puts a step that asks to the
  bot's gates with the call's clock stopped (src/server/mod.rs:5544,
  src/store/db.rs:9243); a step its client only records is allowed at
  once; `step_result` records how it ended. Either way it reaches the
  audit trail. A step waiting on a gate adds one entry to the daemon's
  map of waiting steps (src/server/mod.rs:1212).
- **Lifting.** A tool declared with `lift` may run on past its turn.
- **Yielding (protocol 85).** Its client may hand such a call back with
  what it wrote so far (`yield_call`), as Codex's `exec` yields at
  `yield_time_ms`. The yield is a lift: the call becomes `proc:N`, waits on
  its answer with no deadline, and `wait` collects it or, with `terminate`,
  stops it. When to yield is the client's policy; the daemon has no timer.
- **Its own tools,** `shell`, `read`, `write`, `edit`, `apply_patch`,
  `web_fetch` and others, run in the bot's sandbox under the process
  budget, the load gate and pause.
- **Lending.** `lend` gives a client the bot's sandbox and environment to
  run a command itself.

What Agent lacks, whoever the client is:

- **A client's tool cannot call the bot's own tools.** A step is something
  the client runs and the daemon judges. Nothing lets a client ask the
  daemon to run `shell` or `read` as a step, so a script reaches only
  what its client can run itself.
- **Shown and callable are one list.** A client cannot keep a tool
  callable and out of the model's sight.
- **`ready` lists the daemon's tools by name only**
  (src/server/mod.rs:2200), so a client cannot describe them to a script
  without copying their schemas.
- **Declared tools are copied into every bot.**
  [Measured below](#measured-today).
- **A taken call has no second owner.** `take_call` records a time and
  refuses every later take (src/store/db.rs:9790), so a call cannot be
  handed back to a host that restarted.
- **An engine,** on purpose: a client with none has no code mode, and
  Agent's CLI ships none ([which half is Agent's](#which-half-is-agents)).

## What Sonny built on it

Sonny's `exec` is code mode, made from the mechanisms above. Its limits are
configured ones; none is measured against the other harnesses:

- **One host for every bot.** `sonny tool-host` is a companion. A script
  runs in it, not in a process per agent, and MCP connections are shared
  ([SONNY.md](SONNY.md#mcp-servers)).
- **`exec` is a declared tool** with `answered_by`, `steps` and `lift`
  (sonny/src/exec.rs:144), so a waiting script costs the daemon a parked
  row, and each nested call is judged, not the script.
- **Bounded memory.** Four scripts run and sixteen are alive, each under
  256 MB (sonny/src/exec.rs:37-45).
- **`browser`, `desktop` and links** are declared tools the same host
  answers.

What Sonny's lacks:

- **A script reaches MCP tools only.** `discover` lists the MCP servers'
  tools and nothing else (sonny/src/exec.rs:1034). Sonny's own `browser`
  and `desktop` it could offer a script today, with no change to Agent.
  The bot's `shell`, `read`, `edit` and `apply_patch` it cannot, which is
  the first of Agent's gaps.
- **A restart loses the script.** A call taken before the host restarted is
  answered `tool_outcome_unknown` (sonny/src/served.rs:1484). Only an
  upgrade restarts the host now ([slice M](#m-a-message-never-ends-a-scripts-call)).
- **It is about 3,400 lines** (engine, description, limits, step handling)
  that another client would write again.

## Measured today

Gathered by another agent and given by George on 2026-11-15: main
`46f0180`, release build, a 4-CPU Linux cloud container, a synthetic model.
The bench (`toolpath_bench.py`) and its raw file are not in this tree yet;
these are that run's numbers, exploratory until the bench is. The bench
ran Sonny on Agent, so every row but the first is the two together.

One tool call, from `tool_started` to `tool_completed` as the daemon wrote
them, 20 rounds, 1 ms resolution:

| Path | p50 (ms) | p95 (ms) |
| --- | ---: | ---: |
| Agent alone: `echo`, a tool the daemon runs | 1 | 1 |
| Sonny's `memory` read, answered by its tool host with a process per call | 5 | 6 |
| Sonny's `exec`, 1 MCP call | 4 | 6 |
| Sonny's `exec`, 20 MCP calls together | 12.5 | 15 |
| Sonny's `exec`, 20 MCP calls one after another | 23 | 29 |

Sonny's tool host start, by stored agents, each with Sonny's tools declared:

| Agents | Start (s) | Declared tools per agent | Store growth over two starts |
| ---: | ---: | ---: | ---: |
| 6 | 0.02 | 31.6 KB | 0.5 MB |
| 424 | 1.4 | 31.6 KB | 11.7 MB |
| 2,000 | 6.7 | 31.6 KB | 60.6 MB |
| 5,000 | 16.8 | 31.6 KB | 152.3 MB |

What they say:

- **A script in Sonny costs about 3 ms before its first step, and about
  1 ms a step after** (23 ms less 4, over 19 more steps), or about 0.45 ms
  when the steps overlap. A direct call of a daemon tool is 1 ms or less.
  So a one-step script is about four times a direct call, and a script
  pays for itself from a few calls, or as soon as it saves a model round,
  which costs seconds.
- **Starting Sonny's tool host is linear in agents: 3.3 ms each.** Both
  products have a part in it. Sonny's: its host inspects and reconfigures
  every agent at each start (sonny/src/tool_host.rs:193). Agent's:
  `configure` carries a list whole, 31.6 KB here, and each bot stores its
  own copy, so there is no cheap way to say "the same as before". At
  5,000 agents that is 17 seconds during which calls wait. At 20,000 it
  would be over a minute, by extrapolation. The store growth in the table
  is the first start only: the bench made each agent with a one-tool list,
  which that start replaced. A start over unchanged agents writes nothing
  (`set_declared` writes only a list that differs); its cost is time,
  about 2 ms an agent to `inspect` and 2 ms to `configure` with the list,
  against 0.37 ms for a `configure` without one (2,000 agents, measured
  after this note's first draft).
- **Where the 3 ms and the 1 ms go is not measured.** Read in source, not
  timed: each `approve_step` loads the calling bot's whole record in the
  store worker (src/store/db.rs:13049), declared tools included.

Agent's own cost, with any client that declares tools. Read in source and
not yet measured, the same copy is on the daemon's hot path:

- **Each bot's row holds its own copy,** up to 64 KiB
  (src/store/db.rs:3783). Ten thousand agents hold 316 MB of the same few
  lists.
- **Each live turn parses its own copy** into its bot's record
  (src/store/db.rs:136), as does every other load of a bot, a step's
  included.
- **Each start or resumption of a turn serializes that copy again** to key
  the shared encoding, before its rounds begin (src/tools.rs:2420,
  src/server/turn.rs:3129).

An idle bot is meant to cost about a kilobyte and nothing per bot is meant
to grow. Declared tools break both for any client that declares many, and
code mode makes them larger: a script tool's description carries the
declarations of the tools a script may call.

## Which half is Agent's

Code mode is two things, and the rule above (what is fundamental Agent
has, what is an opinion a client brings) gives them opposite answers.

### The engine is an opinion

The table above holds four contracts in two languages, and no two agree
on a script's surface: Codex has `exec`, `wait`, `store` and `load`, which
Sonny copies; Pi has its own; Claude Code has `REPL` with shell and edit
helpers; Anthropic's API has Python functions in a container the provider
runs. They run in four kinds of place: a host process, a worker thread, a
client's companion, the provider. Whether any of them helps a model on
Agent is a hypothesis this repository has not tested.

Nor can an engine be a battery the daemon carries for nothing, as it
carries `shell`:

- **The runtime thread.** A script is CPU-bound JavaScript. In the daemon
  it would need threads of its own and a bridge back, which is a host
  process in all but the address space.
- **Memory.** Sonny's sixteen scripts at their cap are 4 GB. The daemon's
  peak in the 32-agent screen is 27 MiB.
- **Failure.** An engine that aborts on a bad allocation or a deep stack
  takes its process with it. In a companion that is one restart with
  backoff; in the daemon it is every bot's turn.
- **Text for a model.** The tool's description, the TypeScript it
  declares, tool search and output budgets are what
  [src/AGENTS.md](../src/AGENTS.md) leaves to clients.
- **Hosts can multiply; the daemon cannot.** `take_call` is exclusive
  (src/store/db.rs:9790), so a call runs under one host. Sonny's host is
  not ready for a second: one that loses a take answers the call
  `tool_outcome_unknown` (sonny/src/served.rs:1480), which would end the
  winner's call. With that changed, script CPU and memory scale by adding
  hosts. In the daemon they would all land on one thread.

### A second caller for tools is fundamental

Under every flavor is one thing: a tool call that comes from code and not
from the model, still validated, ruled, gated, confined, counted and
recorded, whose result goes back to the code and not into the model's
context. Two of the others name it outright: Pi Durable's `callers`, and
the `allowed_callers` and `caller` of Anthropic's API.

Agent has half of it. It judges and records the calls a client's tool
reports. The other half only the daemon can supply, since only it holds
the bot's sandbox, the process budget and pause: running the bot's own
tools for a caller that is not the model ([section 2](#2-steps-the-daemon-runs)),
and a tool that is callable without being shown
([section 3](#3-callable-but-not-shown)).

### So Agent takes the caller and no engine

- **The place to plug in is the one that exists:** a declared tool its
  client answers. An engine is one more such tool, and so is a provider's
  flavor that needs no engine at all.
- **One host.** A host is a companion, which the daemon runs only because
  a client asked. Agent never runs one of its own beside a client's, so a
  client's scripts and its own tools are in one process. The other
  arrangement was weighed and not chosen: an Agent host for scripts
  beside Sonny's for `browser`, `desktop` and links would put two
  processes and the daemon between a script and each of those calls, and
  a linked machine would need both.
- **The price: Agent alone has no code mode,** as Pi's core has none. Its
  CLI, and Harbor runs through it, use direct tools.
- **When to look again:** if the evaluation under
  [Gates and screens](#gates-and-screens) shows code mode earns a place,
  the battery is a library a client's host builds in, as Pi Durable's is
  a module of its package, so there is still one host.

## Two kinds of step

To Agent a step is one of two things. It judges and records both and runs
only the first.

| Kind | Tools | Who runs it | Agent's part |
| --- | --- | --- | --- |
| Daemon step, new | The bot's own: `shell` first; later `read`, `write`, `edit`, `apply_patch`, `web_fetch` | The daemon, in the bot's sandbox | Decides, runs, records (`run_step`) |
| Client step, as today | Whatever the client declared and answers | The client, where and how it likes | Decides and records (`approve_step`, `step_result`) |

Agent knows no more than that. Which tools a client offers its scripts,
where they run, and how a call reaches them are the client's.

### How Sonny's tools map

Every one of Sonny's is a client step:

- **MCP:** unchanged. A script's call goes to the shared connection in
  Sonny's host; a call that asks goes to the bot's gate.
- **`browser` and `desktop` as direct calls:** unchanged. Each is a
  declared tool Sonny's host answers, outside the sandbox, with what acts
  put to the bot's gate as a step.
- **`browser` and `desktop` in a script:** new, and all Sonny's. Its host
  already runs both in its own process, so `tools.browser(...)` calls the
  code its direct call does, judged through `approve_step` under its own
  kind. The script gets the result as a value and passes a screenshot to
  `image()` when the model should see it. A script can drive several
  commands and return one snapshot, where today each is a model round.
  Agent is asked once per step that acts and runs nothing.
- **A linked machine's tools:** a client step Sonny's host sends over its
  link. To Agent it is one more step its client asked about
  ([Sonny's links](#sonnys-links)).
- **The bot's own tools in a script:** the one thing Sonny cannot do
  alone. It is Agent's new daemon step, below.

Another client's tools take the same shape: its host answers its declared
tools and offers them to its scripts.

## Design: Agent's changes

All four are Agent's. Where one names Sonny, it is as the example of a
client.

### 1. Tool lists stored once

- **A declared list is stored once, by its content.** The daemon keeps
  each distinct list of declared tools, in order, with all it holds
  (`inherit` counts, `answered_by`, `steps`, `lift`, `kind`), under a
  digest of its bytes. A bot's row holds the digest. A list never changes,
  so nothing a bot refers to moves under it.
- **Everything a bot does today keeps its meaning.** `create` and
  `configure` still take a bot's tools. A fork still keeps its source's
  list and cache (src/store/db.rs:13294). A bot made by another still
  takes its creator's tools, each passed on one generation fewer
  (src/store/db.rs:3720); that derived list is stored once too. `allow`
  still narrows calls without changing what is shown. The limits stay per
  bot: 32 tools and 64 KiB.
- **A client that knows a list's digest sends that instead:** `declare`
  takes either. An unknown digest is refused, and the client sends the
  list once.
- **One parsed copy per list** in the daemon, shared by every record that
  refers to it.
- **One encoding per list and selection,** keyed by the digest with what
  today's key holds (the family, grammars, the daemon tools chosen,
  whether the bot is sandboxed), without serializing the list
  (src/tools.rs:2403).
- **A list is kept while anything refers to it:** a bot, a configuration
  waiting to apply, a creation's record.
- **The migration covers every stored copy,** not only bots' rows: a
  creation's record and a waiting configuration hold lists too
  (src/store/db.rs:4259, src/store/db.rs:8149). It works in bounded
  pieces. An upgrade already copies and checks the whole store first
  (src/store/db.rs:2917), so on a large store its time and disk are that
  copy's.

What this does not do is make a fleet's refresh constant. A host still
asks about each agent; an unchanged agent then costs a digest compared and
no write, where today it costs 31.6 KB sent and stored. Moving many bots
to new tools in one step needs a name a client moves from one list to the
next, with rules for when a bot, a fork, a parked turn and a lifted call
take it. That is a separate change, specified once this one is measured.
Old tool blocks in history, which compaction pairs with the current
definitions (src/server/turn.rs:2284), are a hazard today that this does
not widen.

### 2. Steps the daemon runs

A new request beside `approve_step`:

```json
{"op":"run_step","bot":"scout","turn":12,"call_id":"c1",
 "step":"s7","name":"shell","arguments":{"command":"cargo test -q"}}
```

**Who owns a running step.** Not the turn, and not the background path as
it stands. The daemon keeps one live turn per bot; while a script runs,
that turn is holding live for its grace, parked, or over
(src/server/actives.rs:40). Reviving it for every step would cost a park
and a resume a step. A background command is the nearest work the daemon
owns outside a turn's task (src/server/turn.rs:6180), but it spawns a task
while it still waits for a place (src/tools.rs:3638), makes room by
stopping a bot's least recently used command (src/tools.rs:3751), and
records its result for `wait`, where a step has a session waiting. So a
step gets an owner of its own, **a step coordinator** in the service:

- **Its line holds requests, not tasks.** A step waiting for a place is an
  entry in a bounded queue. A task exists only while a tool runs.
- **A command takes a process place exactly once,** through the registry's
  admission, which already arbitrates with foreground commands and the
  load gate (src/tools.rs:3581). The coordinator bounds steps, so no step
  holds one permit while it waits for another.
- **It counts a bot's steps with that bot's other running work,** a lifted
  call's included.
- **It keeps running steps by call,** so whatever ends the call finds
  them.
- **It answers the session that asked** once the step's end is committed:
  a deferred reply, as `approve_step`'s is, with its lapse in the handles'
  one schedule.
- **It holds no active slot.** `--max-active` counts live turns and is
  untouched.

**What a step runs with.** A script may run for minutes while its bot's
rules, tools and folder change. A turn takes its rules once a round and
builds its environment and sandbox from the turn's own workspace, model
and lineage (src/server/turn.rs:842, src/server/turn.rs:2918). A script's
call takes the same things once, when its first step is asked, and every
step of that call is decided and run against them. A pause, an interrupt
or the call's end takes permission away; nothing adds any while a script
runs.

**The life of a step.**

| State | What holds it | Leaves when |
| --- | --- | --- |
| Asked | The request | Refused at once, or decided |
| Waiting on a gate | An approval row; the call's clock stopped; no place in line | A verdict, the gate's lapse, or the call's end |
| In line | An entry in the coordinator's queue, in memory | A place frees and the load gate admits it |
| Running | A task and a process place | The tool ends, or it is stopped |
| Ended | Its events and audit row | Kept as a direct call's are |

- **Refused at once:** the daemon is held; the call is not waiting for its
  answer; the tool is not one the bot may run as a step; the call already
  has a step in flight; the line is full.
- **Decided in one store job,** before any place is taken: the call is
  checked, the decision is made, the audit row and an event are written,
  and the step is marked started. Every daemon step writes that event,
  gated or not, in the shape a judge already reads, with its arguments;
  today a step no gate holds leaves an audit row only
  (src/store/db.rs:9304).
- **Ended in one more:** `step_completed` with how it ended and how long it
  took, committed before the reply.
- **Neither job is awaited by the run loop.** `approve_step`'s is, inside
  dispatch (src/server/mod.rs:5609), which would start steps one store
  commit at a time. Steps asked together share a commit.

What ends a step from outside:

| Event | Effect on the step |
| --- | --- |
| The call is answered | Stopped first; the answer is recorded once the stop is known |
| The call runs out of time | Stopped; the call ends `tool_unserved`. A running step does not stop the call's clock |
| The turn is interrupted | Stopped with its call |
| The call was lifted and its handle is stopped | Stopped with it. A later turn of the bot, or that turn's end, does not touch it |
| The daemon is paused or shutting down | Stopped as running commands are; new steps are refused |
| The session that asked closes, or its host cancels the step | Dropped from the line, or stopped. Replay changes this later |
| The daemon restarts | Closed at open as `tool_outcome_unknown`, as a direct call cut off is |

A command's stop is known: its process group is killed. A file tool's is
not. A write under way can finish after its task is dropped, which the
store already allows for when it closes an interrupted call
(src/store/db.rs:11155). That is one reason file tools come after
commands. None of these rows has its whole mechanism today; the
[second review](#review-2026-11-15) lists what each lacks.

**How a call's end reaches its steps, as built.** The store is the one
place an answered call's end is noted: its answer held or recorded, taken
by its turn (answered or out of time), or the call gone with its turn or
its lifted command. The group that ended it tells the run loop as it
commits, before any of its jobs is answered, so no one hears of a call's
end before its steps do. The run loop looks at those ends again each time
it lets a step start, so no step starts once its call's end was told. A
pause stops every step asking, in line or running, and one decided or
gated meanwhile before it starts; each answers `step_stopped` with the
pause as its reason, as every other stop does. A unit test runs the coordinator over every state a step
can be in (asked, deciding, gated, in line, running, ending) against every
cause that stops one (its call answered by its bot or by another, the
call's end, a pause, its asker leaving).

**The decision is the direct call's.** A step is validated, ruled, gated
and confined exactly as the same call from the model would be: the bot's
rules, the gates that name the step's own tool, `approval_policy`, its
sandbox and the way out of it, its workspace and environment, redaction.

- **One decision, every caller.** Today that logic is written three
  times in the turn's code: for a call alone, for calls together, and for
  a lifted call (src/server/turn.rs:4593, src/server/turn.rs:5065,
  src/server/turn.rs:6418). It becomes one planner over what the call
  captured, kept apart from the approval's bookkeeping and from running
  the tool, and a test puts a table of calls through every caller and
  requires the same answers. Denial counts and what a later judge reads
  are written as a step's are today (src/store/db.rs:9513).
- **The script tool's own gates are the client's choice.** A declared
  tool says whether a gate that names it also judges its daemon steps.
  Sonny's `exec` gate judges MCP steps today and would say no: a `shell`
  step is judged as `shell`.
- **A script gives a bot no power its direct calls lack.**

**Bounds.**

- **One daemon step at a time per call** in the first slice. A script's
  client steps still overlap as today.
- **A bot's daemon steps in flight, across its calls, are at most the
  width of one round** (`PARALLEL_CALLS`, and half the process budget;
  src/server/turn.rs:302, src/tools.rs:2649).
- **The line is bounded in count and bytes, daemon-wide,** as waiting
  submissions are by `--max-pending-bytes`. A step's arguments may be
  64 KiB, so ten thousand waiting could hold 625 MiB without it. A waiting
  step holds its request and nothing else: no task and no timer. With one
  step a call, the line is first come, first served across calls, and
  asking again faster gains nothing.
- **Past the bound a step is refused,** and order is no longer kept. The
  fleet gate below puts a budget on refusals, so this cannot pass as
  fairness.
- **Replies are bounded too.** A session's output queue is 2 MiB
  (src/output.rs:17), and a session that overflows is closed, which here
  would stop every step it asked for. Replies wait in the coordinator,
  counted in its bytes, until the session has room.

**First tool:** foreground `shell`, and the benches' fixture, `echo`.
Then `read`, `write`, `edit`, `apply_patch` and `web_fetch`, once two
things are specified for them: their place under the coordinator's bound
(a read takes no process place, and a confined write takes one inside
itself; src/tools.rs:3290, src/tools.rs:3351), and what stopping one
means. A step's `read` also needs a rule for paging, which a turn takes
from its context budget (src/tools.rs:3313). Not planned: a tool that
takes steps (no script inside a script), `wait`, `history` and the
tools about other bots. Never: `web_search`, which the provider runs.
Background `shell` runs as a step since 2026-11-16, and detached and
terminal `shell` and `write_stdin` since protocol 86, each command the
bot's as a turn's is, not the call's.

**The reply** is what the model would have read. Output past the preview
is an artifact under `CALL/STEP`, read in pages with `artifact`, or by a
later step's `read`, in lines, as the model's own read pages it. A
command's output is bounded or spilled to a file as it runs, as a direct
call's is (src/tools.rs:4404). A step's artifacts go when its call is
answered: the model never saw them, and what the script wants kept it
prints.

**Cost, to be confirmed, not measured.** Two store jobs for a step no gate
holds, the count a client step costs today. Direct calls run together are
cheaper per call: one start job for the batch and one per result
(src/server/turn.rs:5130), in one model round. What a step saves is the
result kept in history and read by the model, not store jobs.

**Considered and not chosen: lending.** `lend` already gives a client the
bot's sandbox and environment to run a command itself
(RUST_PROTOTYPE.md, protocol 54). A host could run a script's commands
that way and the daemon would run nothing. It would also step outside the
process budget, the load gate and pause, which exist to protect the
machine at fleet size, and every client would write its own `edit` and
`apply_patch`.

### 3. Callable but not shown

- **`create` and `configure` take `steps_only`,** a list of the bot's tool
  names. Such a tool is left out of the model's tool definitions and
  callable only as a step. A direct call of it is `tool_not_available`.
- **Some tools or all of them.** A list of names hides those and keeps
  the rest direct. The entry `"*"` stands for every one of the daemon's
  tools that can run as a step, so `["*"]` is code mode only for the
  daemon's tools, and it follows the daemon as more tools become steps.
  A client's own tools go by name beside it, since only the client knows
  which of them its scripts can reach.
- **`"*"` never leaves a model with nothing to call.** It hides nothing
  while the bot has no tool that takes steps.
- **It travels as declared tools do.** A bot made by another takes its
  creator's `steps_only` unless its own client sets one, and a fork keeps
  its source's. A change applies from the bot's next turn and changes its
  tool definitions, so its prompt cache is keyed anew, as for declared
  tools.
- **No daemon-wide switch.** A client that wants one keeps a default,
  for all its bots or by model, and sets it on each bot it creates; the
  bots those create take it from them. A flag in the daemon was weighed
  and not chosen: the daemon serves several clients at once, and would
  hide tools from bots whose client brought no script tool; flipping it
  would rebuild every bot's prompt cache together and meet histories that
  hold direct calls of tools no longer shown; and it could not differ by
  model, where Codex sets this per model and leaves one on direct tools.
- **Three separate things:** what a bot may call (`tools`, narrowed by
  `allow`), what its model is shown, and by which way a call may come.
  Hiding a tool never removes it as a step; `allow` narrows both ways.
- **Empty by default.** The daemon names no model and decides for no bot.
- **Elision follows what a bot can do, not what it is shown.** A result is
  stubbed today only for a bot whose `tools` hold `read`
  (src/server/turn.rs:3138), and the stub names a `read` call. With `read`
  callable only as a step, the stub must say so or not be written; which,
  the evaluation decides.
- **The daemon reports its tools' schemas.** `ready` lists names only
  (src/server/mod.rs:2200). A client that declares tools to a script needs
  each tool's `parameters`, and whether it can be a step: facts, not
  advice. Built as its own op, `tools`, rather than in `ready`: the
  schemas take about 25 KiB, and `ready` greets every session, a bot's
  socket included.
- **Built in protocol 81** as the setting `steps_only`, beside the others
  a bot is created and configured with. A bot whose `read` is hidden writes
  no stubs, until the evaluation says otherwise.

### 4. Replay, later

A script that survives its host or the daemon restarting. Not in the first
slices. What it needs, from the review:

- **Both orders:** the order the script made its calls, for ids and for
  checking arguments, and the order it saw results, which differs under
  `Promise.race`.
- **A result recorded before the script sees it.** Sonny reports a step
  after, in a task of its own (sonny/src/exec.rs:1926).
- **Every input, not only tool results:** tool discovery, `store` and
  `load`, the clock, random numbers, timeouts.
- **One owner at a time.** `take_call` has no generation, so a host that
  lost its connection can still be running when another replays.
- **The journal with the host.** Results are bytes; a host can keep them
  beside the store as Sonny's workflows do, with no cost to the daemon's
  log. The daemon's part is small and only it can do it: remember that a
  daemon step started and how it ended, keep the result of one that ended
  while its host was gone until a host takes it, and hand a call to a new
  owner exactly once.

Replay is also how scripts scale past memory: a script that waits on one
long step can drop its engine and be run again when the step ends, as a
turn parks. That is the reason to build it, more than restarts.

## Sonny's links

Links are Sonny's, and Agent knows nothing of them: to Agent a linked
machine's tool is a client step, and where a script runs is its client's
business. Links ask two things of Agent's changes: that a step not assume
who sends it, and that one can be cancelled by name.

Today a script whose options line names a machine runs in that machine's
host, against its servers alone, and a call that asks goes back up the
link to Sonny's host beside the daemon, which puts it to the bot's gate
([SONNY.md](SONNY.md#links)). Nothing in Agent's changes assumes a
script's engine sits beside the daemon: a step is a request that names a
bot, a turn and a call, and whoever holds the client's socket can send it.

- **A script on a linked machine keeps working unchanged.**
- **It can reach the bot's own tools.** Its `tools.shell` goes up the link
  as its questions already do, and Sonny's host beside the daemon sends
  `run_step`. One link round trip a step.
- **A script beside the daemon can reach a linked machine's tools,** one
  call over the link each, where today a script is held to one machine.
  One script can then run a test with `shell` and look at the result in
  the laptop's browser.
- **Where a script runs stays the model's choice,** by where most of its
  calls land. Twenty calls of a laptop's server belong on the laptop.
- **The link's own bounds hold:** a machine runs its `calls` at once and
  answers `link_busy` past them, and a machine that goes quiet answers
  `tool_outcome_unknown`.
- **A step is cancelled by name, not by a session closing.** The link
  carries questions up today, not results (sonny/src/link.rs:1260), and
  Sonny's host beside the daemon keeps one socket for every linked
  script. A script that vanishes on its machine leaves that socket open,
  so the host cancels the script's daemon step itself, and waiting on a
  step must not keep the link from noticing the machine is gone.

## Sonny's host at fleet size

Sonny's work, named here because Agent's part does a fleet no good if its
client's host is the bottleneck. Another client's host has a list of its
own:

- **Four running and sixteen alive is a desk's bound.** With code mode
  only, every tool call of every agent is a script. The bound becomes a
  memory budget, with a small default heap a script raises when it needs
  to.
- **Several hosts,** one per few CPUs, each with its own engines, once a
  host that loses a take lets the call go instead of answering it.
- **Start costs a digest per agent** once Agent stores lists once, and
  nothing once a fleet's tools move in one step. The host also reads each
  agent's settings on connecting (sonny/src/tool_host.rs:253), which is
  its own cost.
- **Quick tools answered in the host.** `memory` costs 5 ms because each
  call is a process. Not this note's change, but the same table shows it.

## What stays with the client

Agent takes none of these, from Sonny or from any client:

- The engine, its limits, and how many scripts and hosts run.
- The script tool's name, description, grammar, declarations and search.
- Its own tools (for Sonny: MCP servers, the browser, the desktop and
  links), and routing a script's calls between daemon steps and its own.
- `store` and `load`. Sonny keeps them in a file per bot name
  (sonny/src/exec.rs:929), so a fork appears to start without its
  source's values; unverified, and open.

## What changes, in order

Agent's changes come first and stand alone. Each is built, tested and
measured with a plain client of the socket protocol, and is there for any
client when it lands:

1. **Tool lists stored once.** Lists kept by digest, bots that refer to
   them, `declare` by digest, one parsed copy and one encoding, the
   migration over every stored copy. Nothing a bot does changes meaning.
   No code mode in it.
2. **Daemon steps: commands, one at a time.** `run_step` for foreground
   `shell` and the fixture, with the coordinator, the capture, the one
   decision, and the two tables above. Behavior tests, one per row of the
   tables, plus: a step and its direct call decided alike, in and out of a
   sandbox; a bot with a lifted call and a new turn; the line and the
   reply queue at their bounds.
3. **File tools as steps,** with their bound and their stop specified.
4. **`steps_only`,** by name or `"*"`, and in `ready` each tool's schema
   and whether it can be a step.
5. **A fleet's tools moved in one step,** if slice 1's numbers still show
   refresh as a cost. **Width,** more than one daemon step a call, if the
   screens ask for it.
6. **Replay's daemon part:** a step's started and ended marks, a result
   kept until a host takes it, and a call handed to a new owner once.
7. **The yield (slice Y), built in protocol 85.** `yield_call` lifts a
   waiting call of a `lift` tool with its output so far, which the model
   reads beside `"yielded":true`; a yielded call has no deadline (schema
   106); `wait` takes `terminate`. Tested with a plain socket client
   (tests/test_yield.py): the handle and the output so far, live or parked,
   pictures included; steps running on and paging what earlier steps kept;
   the answer collected whole past the tool's `expire_ms`; `terminate`
   stopping the call and its running step, and a background command; a
   steer ending a wait on it at once.
8. **Every tool a step (slice A), built in protocol 86.** Every daemon
   tool runs as a step but those Codex keeps out of its scripts, each
   refused with its class: `wait`, the script tool's second half;
   `history`, a history reader; `web_search`, hosted; and the agent
   tools. One exhaustive match names each tool's class
   (src/tools.rs, `Tool::direct_only`), and the `tools` op's `step`
   follows it. `shell` with `detach` or `tty`, and `write_stdin`, run as
   their direct calls: the command is the bot's, on a handle that
   outlives the script; they take no process slot and no place in line,
   and a detached command or a terminal holds a lasting slot as a direct
   call's does. Tested with a plain socket client (tests/test_steps.py):
   a terminal started, typed into and ended by steps, and one killed; a
   background command's handle refused as a terminal and killed; a
   detached command listed and stopped past its call; the `tools` op's
   marks.

Each is a hot-path change and carries the matched screens against main
that [AGENTS.md](../AGENTS.md) names. Not on the list, on purpose: an
engine, a script tool, and a host.

Sonny's changes, each after the one of Agent's it uses:

- **Now, needing nothing from Agent:** `browser` and `desktop` offered to
  a script.
- **After 1:** its host sends digests.
- **After 2:** `exec` reaches the bot's `shell`. Links carry daemon steps
  up and a linked machine's tools down, with cancelling by name. Its
  description and [FEATURES.md](../sonny/FEATURES.md) row change with it.
- **After 3:** `exec` reaches the file tools.
- **After 4:** a default, for all its agents or by model, that it sets on
  the bots it creates to run them code mode only, and the evaluation.
- **After 6:** the journal, and a second host.
- **After 7, built:** `exec` yields a script still running at its
  `yield_time_ms`, 30,000 ms by default as Codex's, with what it said so
  far headed `Script running`, and at once on `yield_control()`; the
  model then `wait`s on the handle for the whole result. A script's own
  time, its `timeout_ms` (600,000 ms at most), counts its JavaScript
  only: awaiting a server's tool, a step or a gate spends none of it.
  Until the yield, the call's own time bounds it too. A script run on a
  linked machine does not yield yet. Tested through the tool host
  (tests/test_served.py): handed back at its yield time and collected
  whole by `wait`; handed back at once by `yield_control()`; a script
  that ends first answered as it ends.
- **After 8, built:** `exec` offers `tools.write_stdin`, and `tools.shell`
  takes `detach`, `tty` and `yield_time_ms`.

## Gates and screens

Gates, stated before any run, on the machine the numbers above came from.

**Agent's gates** use a bench client that speaks the socket protocol and
nothing of Sonny's: it declares a list the size of Sonny's, takes a call,
and asks for steps itself. The "today" column borrows Sonny's run where
no Agent-only number exists yet, and says so.

| Slice | Gate | Today |
| --- | --- | --- |
| 1 | Configuring a bot with a list the daemon already holds writes nothing to the store and sends under 1 KiB | 31.6 KB sent and stored, in Sonny's run |
| 1 | 5,000 bots configured so, in under 2 s | Not measured apart from Sonny's start |
| 1 | A bot's stored declared tools under 100 bytes | 31.6 KB with Sonny's list |
| 1 | With 10,000 turns live on one list, daemon memory for declared tools is one list's; with 10,000 distinct lists it is reported | Not measured |
| 1 | The upgrade of a 10 GB store: its time, peak memory and disk headroom reported, and it finishes after being interrupted | Not measured |
| 2 | One daemon `echo` step, from `run_step` sent to its reply, p50 at most 1 ms and p99 reported at finer than 1 ms | 1 ms or less for a direct `echo` call |
| 2 | At most two store jobs for a step no gate holds, with reads, group sizes and bytes counted | Two for a client step |
| 2 | Store jobs per turn unchanged for a turn with no steps | 7.5 |
| 2 | A step in line holds its request and under 1 KiB more; running and ended-but-unsent bytes each bounded and reported | New |
| 2 | Fleet: 10,000 bots, five `echo` steps each, all finish; no step refused or expired; at 400, 2,000 and 10,000 bots, step p99 wait at most 1.6 times the fleet's size over its step rate, and the step rate at 10,000 bots no lower than main's (was: under 1 s; changed with George's agreement on 2026-11-15) | New |
| 2 | Beside that fleet, other bots' turn p99 within the perf gate's 15% | New |

The fleet row is the one that matters. A screen where steps are refused
or starved can pass every other row.

Slice 2's run missed three of these rows: the step p99 under 1 s (1.84 s
at 10,000 bots), the other bots' 15% (24 to 65% from their own process),
and the 1 KiB a step in line (about 3 KB measured). The 1 s figure was
written before any run. A fleet stepping at once waits in one FIFO line,
so its p99 is about one pass of that line, the fleet's size over the step
rate; George agreed the same day to replace it with a p99 at most 1.6
times that, with a step rate no lower than main's, the gate in the table
above. The numbers and the reasoning are in
[DAEMON_MEASUREMENTS.md](DAEMON_MEASUREMENTS.md#remeasured-after-review-a-second-process-measured-memory-and-a-derived-gate).

**Sonny's gates,** on Agent, with the bench the numbers above came from:

| After | Gate | Today |
| --- | --- | --- |
| 1 | Its host's refresh of 5,000 unchanged agents takes under 2 s, apart from its other start work | 16.8 s for the whole start |
| 2 | `exec` with twenty daemon `echo` steps one after another, from its `tool_started` to its `tool_completed`, p50 at most 23 ms | 23 ms p50 for MCP steps |
| 2 | `exec` with one daemon step, p50 at most 4 ms | 4 ms for an MCP step |
| 2 | The fleet above through Sonny's hosts: scripts a second and the hosts' CPU and RSS, reported and not gated until its bounds change | New |

Screens, on the synthetic provider and tool fixtures. Agent's, with the
bench client:

- **Fleet.** 10,000 bots, five `echo` steps each, all asked together,
  then the same with a short command. Steps a second, step p50 and p99,
  daemon CPU and peak RSS, store jobs and log bytes, and refusals and
  retries counted beside them.
- **Mixed.** Direct calls and steps together; a bot with several lifted
  calls; the active and process limits full; paused approvals; load
  holds; a call that expires with a step running; a slow reader.
- **Large results.** Steps returning 1 MiB: log bytes, checkpoint stalls,
  and the bytes the daemon holds at peak.
- **Footprint.** 10,000 parked calls, each waiting on one step in line.

Sonny's, on Agent:

- **Tool path,** the bench above, with daemon steps added beside MCP
  steps, and a breakdown of a script's first 3 ms and each step's 1 ms.
- **Fleet,** the same 10,000 bots as scripts through its hosts.

Then live, through Sonny, under a stated spend cap, and a hypothesis until
run: whether a model that Codex shows only `exec` does better here with
`steps_only` than with direct tools. The
[long-task evaluation](LONG_TASK_EVAL.md) on luna gives rounds, tokens,
cache reads and correct tasks for both arms.

## Code mode, all in, as built

The working design is the "Code mode, all in" document George set on
2026-11-16; this section records each slice as it lands, and what it found
that the document did not.

### M. A message never ends a script's call

Row 29 of the document's parity table: on 11-16, between 09:09 and 09:20
ET, scripts whose turn took a message ended `tool_outcome_unknown` with
their process gone, the error saying the tool host had restarted. The
document's candidate cause (the host stops a lifted script whose
`call_lifted` it did not register) does not hold: on main, a message during
a script's step leaves the script running and its call answerable in every
case tried (a short step, a sandboxed one, an escalated one at its gate and
running, one that outlasts its turn, one that goes on to more steps, a
message before the first step, two scripts in one round;
tests/test_served.py).

What reproduces row 29 exactly is a restart of the tool host while scripts
run: each is cut, and the next host, meeting calls already taken, answers
them `tool_outcome_unknown`, whether a message came or not. The message was
a coincidence: on a busy morning one comes during most scripts.

What restarted the host: every window, and every editor session, kept
Sonny's companions (the lookout, the tool host, the reviewer) again with
its own build and program as it started and each time the daemon came
back. The daemon restarts a companion whose definition changed. The
upgrade at 09:03 replaced the daemon, every open window reconnected, and a
window of 0.9.5 put its own host back, which the next 0.10.0 window or tab
replaced again. The reviewer went the same way, which is why escalated
calls failed at once after a cut. Read in source and reproduced in a test;
the box's own logs were not available.

Now a window or session adds only the companions the daemon keeps none of
(`Companions::Add`, sonny/src/main.rs). `sonny upgrade` puts its own build
in their place (`Companions::Replace`), and so does a window that started
the daemon itself (one that found none, or an older one it replaced),
since its companions have only just started with it. After a hand-made
upgrade (`brew upgrade`) on the same protocol, a window no longer moves
the companions to the new build; `sonny upgrade` does.
The names-folder spelling that made a companion report a link as its
program is fixed apart, in #1094.

Row 30 (a script a message moved to the background took 36 to 91 s for
milliseconds of shell work) is not traced. In source, a step's command
waits in one first-come line for the load gate (`[daemon] max_load`, 128
on that box, whose load was near 170) and for a process slot
(src/server/steps.rs `pump_steps`). That fits, and is unconfirmed.

## Open

Agent's:

- **Why configuring an unchanged bot grew the store.** Answered: it does
  not. The growth was the bench's first start replacing a one-tool list
  with Sonny's; later starts wrote nothing.
- **Moving a fleet's tools in one step:** whose names, when each kind of
  holder takes the new list, and what old tool blocks in history then
  mean to a provider.
- **A step's read returns a model-sized page.** Whether it should return
  more, or the client should page the artifact, is for the fleet screen.
- **Refusal past the line's bound** loses order. If the fleet screen shows
  refusals in ordinary use, the line needs a ticket a client waits on.
- **Whether an engine ever ships with Agent.** Not now
  ([which half is Agent's](#which-half-is-agents)). It is looked at again
  only if the evaluation shows code mode earns a place, and then as a
  library a client's host builds in, never a host of Agent's own.
- **[Item 39](NEXT.md)** overlaps the calls of one response. A script is
  the other way to the same end, and both draw on one bound.

Sonny's:

- **`store` and `load` across a fork,** above.
- **Its host's bounds at fleet size:** the memory budget, and how many
  hosts.

## Review, 2026-11-15

GPT-6 Astra read the first draft against the code, read-only. It agreed
the engine belongs in a companion and that running and enforcing a bot's
own tools belongs in the daemon, and would not approve the first slice.
Its code citations were checked in source and hold. What it found, and
where this draft answers it:

| Finding | Answer |
| --- | --- |
| Where a step runs was left open, and "one active slot per call" does not fit one live turn per bot | Section 2: a step coordinator of its own (the second draft's answer, a background command, did not survive the second review); no active slot |
| No state machine for a step across the live hold, a parked turn, a lifted call, pause, load and expiry | Section 2's two tables |
| Refusing past a bound is not a fair order | One step a call makes the line first come, first served; refusal remains past the byte bound and is counted |
| Gating by the step's tool moves the boundary, and an ungated step is invisible to a later judge | The direct call's whole decision, one function; every daemon step writes an event; the script tool's gates are the client's stated choice |
| The journal is not enough for replay, and nothing fences a second host | Replay moved to last, with those needs listed |
| Deleting with the answer is not one commit; step artifacts have no place in a fork | No journal in the first slices; a step's artifacts go with its call |
| The cost comparison was wrong | Corrected: steps save history and context, not store jobs |
| `steps_only` touches elision, compaction and inheritance | Section 3, and Open |

Where this note still differs: the review would leave all replay state to
hosts. The journal goes there, but that a daemon step started, how it
ended, and its result while no host is there to take it stay the daemon's,
since only it can say a step ran once.

### Second review

Astra read the second draft the same day and approved neither slice as
then scoped. Its citations were checked again and hold. Three statements
of that draft were wrong and are corrected above: declared tools are
serialized at a turn's start or resumption, not every round; a second
host is not safe today, since Sonny's answers a lost take; and a command's
output is bounded or spilled, not held whole.

| Finding | Answer in this draft |
| --- | --- |
| Named lists that change break `inherit` counts, forks, and what a parked or lifted call sees | Section 1: lists kept by content and never changed; moving a fleet in one step is a later, separate change |
| The migration misses other stored copies, and an upgrade copies the whole store | Section 1 covers every copy; the upgrade's cost is a gate |
| "Owned as a background command" gives neither a line without tasks nor a reply to a session, and a second permit can deadlock file tools | Section 2: a coordinator of its own; one process place a command; commands only at first |
| Dropping a file tool's task does not stop its write | File tools wait for their own slice |
| A step decided against the bot as it now stands can run an old script with new permissions | Section 2: the call captures what a round would |
| The decision is written three times, and its counters and events are part of it | One planner, every caller |
| Awaiting each step's store job in the run loop serializes starts | Neither job is awaited |
| The gates could pass while scripts are refused or starved; memory and start gates were vague | The gate table, and its fleet row |
| A 2 MiB session queue can close the host's session under a burst of replies | Replies wait in the coordinator |
| A script gone from a linked machine leaves the shared socket open | Links: cancel by name |

Still open after both reviews, and needed before slice 2 is built: the
coordinator's fairness between its queue and the registry's semaphore,
written down as an order and not as a hope; and each row of the two
tables turned into a code path. Astra's map of what each row lacks today:
the claim and byte reservation when a step is asked; the move from a
verdict to running; the queue itself; routing an end to a session; a
step's durable end state; stopping and settling a step when its call is
answered, expires, is interrupted or has its handle stopped; and closing
steps at restart.

## Sources

- **Codex:** openai/codex at `b016cd18472588606c04af565528c4cd6746a41f`
  (2026-11-15). `codex-rs/code-mode-runtime` (one thread and isolate per
  cell, `max_heap_size_bytes` forced to none), `code-mode-host` (2,048
  active cells), `core/src/code_mode_host.rs` (one child per thread
  manager, gRPC over its stdio), `core/src/tools/code_mode` (nested
  dispatch), `models-manager/models.json` (tool modes), and commit
  `97576b1794`, "Run code mode exclusively through the standalone host".
- **Pi:** earendil-works/pi at `c3cc91632d0a1950fb122605b190233f2aa0006e`
  (2026-11-15). `packages/codemode`, `packages/coding-agent/src/extensions/codemode`
  (limits, `defaultActive: false`), and
  `packages/durable/src/codemode/codemode.ts` (journal and replay, in its
  module comment), and `packages/durable/src/harness/types.ts` (a tool's
  `callers`; Pi's own `packages/agent/src/types.ts` has no such field).
- **Anthropic's API:**
  [Programmatic tool calling](https://platform.claude.com/docs/en/agents-and-tools/tool-use/programmatic-tool-calling),
  read 2026-11-15. Documentation only.
- **Claude Code:** [Code Mode](https://code.claude.com/docs/en/code-mode),
  read 2026-11-15. Documentation only; it states no runtime, limits or
  numbers.
- **Tool path numbers:** a run on main `46f0180`, 2026-11-15, given by
  George. Its bench and raw file are not in this tree.
