# Several daemons, moving bots, and watching them

A roadmap for four ideas George raised on 2026-09-20. None of it is built.
Each section says what the code does today, what would have to change, the
main risks, and an order of work. Source references are to `612ae1d` and were
read on 2026-09-23. Claims about the code are verified at that revision;
everything under "would change" and "order" is a proposal, and anything
unmeasured says so.

The four ideas, in the order this document recommends building them:

1. [Watch a bot without disturbing it](#1-watch-a-bot-without-disturbing-it),
   including a summary drawn from its transcript.
2. [One daemon per workspace or project](#2-one-daemon-per-workspace-or-project),
   for isolation.
3. [One client over several daemons](#3-one-client-over-several-daemons),
   across machines.
4. [Move a bot to a daemon on another machine](#4-move-a-bot-to-another-daemon).

The order runs from what is mostly there to what needs the most new
semantics. Each later idea reuses something an earlier one adds.

Two boundaries from the design record hold throughout. Agent is not a remote
execution service (AGENTS.md, "Preserve the question"), and "workspace
provisioning, Git branching, diff application, and machine placement belong
to callers or other tools" (README, Scope). So the daemon's part in every
idea below is durable state and its import and export. Choosing machines,
copying files, and carrying bytes between hosts belong to callers, standard
transports, or a separate process.

## What the four ideas share: a daemon is its store

Today one daemon owns one store, and every identity it hands out is scoped to
that store.

- **One owner per store.** `Store::open` takes an exclusive lock on
  `<store>.owner-lock` and fails with `store_already_owned`
  (`src/store.rs:136-147`). The socket has its own ownership lock and refuses
  to displace a live listener (`src/server/socket.rs:18-64`). `serve` exits 75
  on either conflict (`src/main.rs:9`, `src/main.rs:34`).
- **The socket follows from the store.** Clients, the daemon, and the app
  resolve the socket from the store path: `<store>.sock`, or a hashed name
  under `/tmp/agent-<uid>` when that path is too long
  (`client/src/socket.rs:41-70`).
- **Every id is a per-store sequence.** Bot ids come from `bot_sequence`,
  history nodes from `node_sequence`, and turns from `turn_sequence`. Event
  cursors and process ids are `AUTOINCREMENT` rows
  (`src/store/db.rs:335-418`). Bot names are the `bots` primary key.
- **Handles carry no daemon.** `turn:BOT/N` and `proc:N` parse to a bot
  name and a store-wide turn or process id (`src/server/handles.rs:14-51`).
- **Model-visible text holds store ids too.** A truncated tool result names
  its retained output as `TURN/CALL_ID/STREAM` using the store-wide turn id
  (`src/server/turn.rs:1265-1279`, parsed at `src/tools.rs:220-235`).
  Detached runs and background shells hand the model `turn:` and `proc:`
  handles.
- **The `ready` line doesn't say which daemon this is.** It announces
  protocol 3, capabilities, limits, schema, tools, and providers
  (`src/server/mod.rs:475-486`), but no identity for the store behind it.

So two daemons can each have a bot `bob` with id 7, turn 42, and node 1000,
and nothing on the wire tells them apart.

**Shared first step (proposal): a store identity.** Generate a random id
once, when the store is created, keep it in a singleton table the way the
sequences are kept, and announce it in `ready`. It costs one small one-way
migration and one field. Every idea below needs it. Idea 3 uses it to
address a daemon, idea 4 to record where an imported bot came from, idea 2
to tell per-workspace daemons apart, and idea 1 to qualify cursors across
daemons. A copied store file keeps the same id, so the id names a store's
lineage, not a unique running process. Section 4 has to handle that; see
its risks.

## 1. Watch a bot without disturbing it

Let a person or another agent see what a bot is doing, including a summary
drawn from its transcript, without changing what the bot does or how fast
it runs.

### What exists

Observation is already a protocol capability; there is just no word for it.
These operations write nothing:

| Operation | What it gives an observer | Where it runs |
| --- | --- | --- |
| `bots`, `turns`, `result` | Identity, status, per-turn model, tokens, timing, outcomes | storage worker |
| `events`, `follow` (one bot or `*`) | Durable event log, then live deltas | worker for replay pages; hub for live |
| `history_nodes`, `history_items`, `item` | The transcript itself | storage reader |
| `artifact` | Full retained tool output | storage worker |
| `stats` | Daemon-wide live state | service loop |

(`src/server/mod.rs:87-172`; routing at `src/server/mod.rs:1005-1280`.)

Another agent can already observe from its shell tool using `agent follow
--bot X`, `agent turns`, and `agent result` ([CLI.md](CLI.md)). The app works
the same way: one `follow *` and pipelined item loads ([APP.md](APP.md)).

A slow observer cannot hold a bot back. The hub delivers live events with
`try_send`, and a socket follower that lags is closed rather than waited on
(`src/server/hub.rs:96-122`). The exception is the stdio owner. It gets a
backpressured firehose (`src/server/hub.rs:122-137`), so an observer should
never be the stdio owner.

Summaries already exist for some bots. Compaction writes a model-generated
summary, the covered turns, and their verbatim prompts to the `compactions`
table (`src/store/db.rs:343-346`, written at `src/store/db.rs:1038-1070`).
The bot's carry-forward note is versioned in `notes`
(`src/store/db.rs:340-342`). But no protocol operation returns either text.
The `compacted` event carries only sizes (`src/store/db.rs:1054-1057`), and
the summary is read only to build the next compaction and the model's
context (`src/store/db.rs:996-1002`).

A fork can already ask a bot about itself without touching it. A fork at an
explicit checkpoint is allowed while the source is running. Only a fork of
the current head requires the source to be idle
(`src/store/db.rs:2316-2329`). The fork shares history nodes and leaves the
source's rows unchanged.

### Where watching disturbs a bot today

- **Replay competes with commits.** `events`, `artifact`, and every follow
  replay page run on the storage worker (`src/server/mod.rs:1233-1237`,
  `src/server/mod.rs:1258-1275`, `src/server/hub.rs:142-160`), the one
  thread every bot's commits wait on (`src/store.rs:243-266`). Several
  observers attaching to long logs queue behind, and in front of, fleet
  commits.
- **Transcript reads compete with requests.** `history_*` and `item` run on
  the storage reader (`src/store.rs:269-290`). That same thread streams
  context items into model requests (`src/server/turn.rs:358`,
  `src/server/turn.rs:565`). A heavy transcript reader delays request
  construction.
- **A summary fork isn't read-only.** `fork` takes no tool selection
  (`src/server/mod.rs:65-77`) and copies the source's tools
  (`src/store/db.rs:2344-2356`). A "summarize what you are doing" fork
  therefore has `shell` and `write`. It also writes a `forked` event and
  shows up in the lineage tree.
- **A model summary spends the fleet's allowance.** Every model call passes
  the same per-provider pacing gate (`src/provider/pace.rs:1-10`). A summary
  a person or agent asks for is paid from the budget the watched bots are
  using.
- **Nothing makes a session read-only.** The only authorization is the
  socket file's permissions (`client/src/socket.rs:59-67`). A client that
  can `follow` can also `interrupt`, `delete`, or submit.

### What would change

1. **Name the observer set.** Document the operations above as the observe
   contract, and advertise it as a capability in `ready`. This is
   documentation plus one capability string.
2. **A `summary` read.** Return the bot's current compaction summary, its
   covered turn range, and its note, all from the reader. This costs no model
   call. It only has content for bots that compacted or wrote a note, and it
   is as fresh as the last compaction. Forks already bind to the right
   versions (`src/store/db.rs:2363-2388`), so the read is correct for forks.
3. **A digest in the client, without a model.** The client builds what a
   bot is doing now from `turns`, the current turn's events, and the last
   text: running turn, tool calls in flight, last output lines, tokens. It
   uses JSON by default and a rendered view under `--pretty`, like every
   other command. This is client policy, not daemon mechanism (NEXT item 20).
4. **Replay pages off the worker.** Move every replay page except the last
   onto the reader. Keep the final "empty page, switch to live" step as a
   worker job, since that is what guarantees no committed event falls between
   replay and live (`src/server/hub.rs:139-141`). Measure it with the
   per-operation histograms (NEXT item 27) before and after.
5. **A tool selection on `fork`.** An optional `tools` list, empty allowed,
   validated the way `create` validates it. Heterogeneous forks want this
   anyway. With it, "fork at the current node with no tools, ask it to
   summarize" becomes a safe recipe for a model-written summary on demand.
   The caller deletes the fork afterwards.
6. **Read-only sessions, once another principal needs them.** A socket
   session that refuses every mutating operation, for example a second
   socket or a flag at connect. Defer this until someone other than the
   store's owner observes. Today every client is the same uid.

### Risks

- A summary is a model's reading of a transcript. It can omit what the bot
  is really doing, and a stale compaction summary can describe work that
  finished long ago. Always return the covered range and the version with
  the text.
- An on-demand summary costs input tokens roughly the size of the window,
  from the same pool as the fleet. Many observers asking often could pace
  the watched bots. Keep any model summary behind an explicit call, never
  on a timer.
- The summary fork's first turn shares the source's prefix and model, so it
  may hit the provider's prompt cache. That is unmeasured.
- Transcripts carry whatever tools read, and summaries inherit that. Anyone
  granted observe access sees all of it.

### Order

1. The `summary` read, plus the observe capability.
2. The client digest in the CLI (JSON), then in the app.
3. Replay pages on the reader, measured on the slow-follower and
   mixed-workload screens (NEXT items 16 and 18).
4. `fork` with a tool selection, and the summary-fork recipe, with the
   cache-hit ratio measured.
5. Read-only sessions, only when a second principal appears.

## 2. One daemon per workspace or project

Run a daemon per workspace or project, so that a stall, crash, or runaway
in one does not reach the others.

### What exists

This works today by choosing a store per workspace. `--store` or
`AGENT_STORE` selects the store, and the socket follows from it
([CLI.md](CLI.md), "Connection and startup"). `run` starts a daemon for a
store on demand, in its own process group so it outlives the CLI
(`src/client.rs:469-545`). `--idle-exit` retires a socket daemon with no
sessions, no live turns, and no running commands, and parked turns resume
on its next start (`src/server/mod.rs:577-600`). Both ownership locks keep
two daemons off one store. Delegation stays inside the daemon: a bot's
shell inherits `AGENT_STORE` and `AGENT_SOCKET`
(`src/server/mod.rs:451-470`), so the `agent run` it issues reaches the same
daemon.

So `agent run --store "$PWD/.agent/state.sqlite" ...` already gives a
per-workspace daemon. What is missing is a convention, and an honest
account of what the isolation buys.

### What separate daemons do and do not isolate

They do isolate failure and resources. Each daemon has its own storage
worker, so a 693 ms deletion stall like the one measured in NEXT item 3
stops only its own store. Each has its own `--max-active`, process and
pending bounds, retention, WAL, and crash.

They do not isolate security. Every daemon runs as the same user with
full-access tools. `read`, `write`, and `edit` resolve a path by
`workspace.join(path)` (`src/tools.rs:816-818`), and an absolute path
replaces the workspace entirely, so a bot in project A can edit project B.
Process sandboxing is explicitly kept out of the queue (NEXT, end of the
queue). Per-workspace daemons are a failure domain, not a sandbox.

They also give up what one daemon shares, which is most of this project's
performance thesis:

- **Provider connections.** Each daemon has its own transport and HTTP/2
  shards, sized from its own `--max-active` (`src/server/mod.rs:275-320`).
- **Pacing.** Each daemon learns its own pools from response headers
  (`src/provider/pace.rs:1-10`). N daemons on one key are N independent
  clients, each assuming the whole allowance until it is refused. NEXT item
  10 anticipated exactly this case ("several hosts on one provider key") and
  proposed jitter. It is equally true of several daemons on one host.
- **The process bound.** It defaults to 64 per logical CPU per daemon
  (`src/server/mod.rs:300`), so N daemons oversubscribe the host N
  times.
- **Memory.** An ad hoc probe measured about 10.7 MiB RSS and four threads
  for a daemon holding 500 idle bots
  ([DAEMON_MEASUREMENTS.md](DAEMON_MEASUREMENTS.md), parked-turn table). An
  empty daemon's cost has not been measured.
- **Cross-workspace work.** A bot cannot `wait` on another daemon's handles
  or fork from another store's history. Creating a bot in another daemon from
  a bot's shell fails, because the client sends `AGENT_BOT` and
  `AGENT_BOT_ID` as the creator (`src/client.rs:572-587`), and the target
  store requires that creator to exist there (`src/store/db.rs:574-592`),
  answering `creator_not_found`.

### What would change

1. **Store identity** (the shared first step), so clients and logs can tell
   per-workspace daemons apart.
2. **Store discovery in the client.** Opt-in: walk from the workspace
   toward the root for `.agent/state.sqlite`, the way client policy already
   walks for `AGENTS.md` ([CLIENT.md](CLIENT.md)), and fall back to
   `~/.agent/state.sqlite`. This changes the client only. The daemon remains
   unaware of workspaces beyond each turn's path.
3. **A matched screen before any recommendation.** Run one daemon with N
   workspaces against N daemons on the same workload. Measure total RSS,
   CPU, connections, and 429s on a shared key. Per AGENTS.md, the screen,
   not the principle, decides.
4. **Host-level pacing, only if the screen shows it is needed.** If N
   daemons on one key storm the provider, the cheapest fix is jitter on
   retries and resumes (item 10's note). A shared pace across daemons, such
   as a lock file or a small pacing process, is a subsystem and should wait
   for evidence.

### Risks

- A store inside the workspace is one `git add .` away from being committed,
  along with its transcripts and tool output, and a workspace snapshot copies
  the store and WAL mid-write. The convention has to include ignoring
  `.agent/*.sqlite*`. A copied store is also a second store with the same
  identity (section 4's risks).
- Calling this "isolation" invites the security reading. The docs have to
  say failure domain every time.
- Many small daemons each learn pacing from nothing and each keep their own
  connection pools, so the fleet numbers in [LIVE_FLEET.md](LIVE_FLEET.md),
  all measured through one daemon, no longer describe the system.

### Order

1. Store identity.
2. Client discovery, opt-in, with the ignore rule documented.
3. The one-versus-N screen.
4. Jitter or shared pacing only if the screen calls for it.

Until the screen says otherwise, the default stays one daemon per user.
Per-workspace daemons are for people who want a failure boundary.

## 3. One client over several daemons

Let one UI or CLI manage several daemons, on this machine and others.

### What exists

The daemon already serves any number of clients. Its accept loop gives each
connection its own session (`src/server/mod.rs:492-515`), and `follow *`
gives each client the whole fleet on one subscription with a store-wide
cursor ([RUST_PROTOTYPE.md](RUST_PROTOTYPE.md#fleet-controllers)).

Every client talks to exactly one daemon at a time. The CLI resolves one
store and socket per invocation and checks that daemon's configuration
against what it asked for (`src/client.rs:398-467`). The app holds one
`Client` (`app/src-tauri/src/main.rs:24`, connected at
`app/src-tauri/src/main.rs:182`). The shared client crate connects only to
a Unix socket path and requires protocol 3 (`client/src/lib.rs:141-160`).

Remote use already works for one daemon. [APP.md](APP.md) records that
forwarding the daemon's socket over `ssh -L` runs the app locally at full
speed against a daemon elsewhere, and the client is built for that latency:
one `follow *`, pipelined item loads, nothing polled.

Authorization is the socket file's permissions and nothing else. The short
rendezvous directory must be the user's own and mode `0700`
(`client/src/socket.rs:59-67`).

### What would change

1. **Store identity in `ready`** (the shared first step). The app stores
   per-view state per socket and workspace ([APP.md](APP.md), "Closing the
   window is detaching"). A forwarded socket's path is arbitrary, so that
   key has to become the store identity.
2. **A client-side endpoint list.** A small file mapping names to socket
   paths, local or forwarded: `--daemon NAME` on the CLI, and a daemon
   switcher or a merged fleet view in the app with one `follow *` per
   daemon. A client that spans daemons qualifies everything it shows:
   `daemon/bot`, handles, and one cursor per daemon. The daemon does not
   change.
3. **Transport: reuse SSH, don't add a listener.** AGENTS.md asks for
   standard transports, and a TCP or TLS listener with authentication would
   make the daemon the remote execution service the project rules out. If a
   network endpoint is ever wanted, it should be a separate process in front
   of the socket, the same shape as the ACP bridge (NEXT item 9).
4. **Starting remote daemons stays with the operator.** `ensure_daemon`
   starts a local daemon from the current executable
   (`src/client.rs:501-522`). A client that runs `ssh host agent serve`
   would be choosing machines, which the README assigns to callers.
   Document an ssh or systemd recipe instead.
5. **Cross-daemon lineage, only when needed.** A bot that creates a peer on
   another daemon fails today with `creator_not_found` (see section 2).
   Either the client drops the creator for a remote target, which loses the
   lineage honestly, or the store accepts a remote creator (store identity,
   name, id) that it records but cannot check. Leave it until someone needs
   it.

### Risks

- A forwarded socket is full access for anyone who can reach it: submit,
  delete, and every bot's `shell`, which amounts to code execution as the
  daemon's user. Forward to a socket in a `0700` directory, never to a TCP
  port.
- Stale forwarded socket files, and a forward that drops mid-`wait`. The
  client already reports `daemon_unavailable` and re-attaches from its
  cursor. A merged view has to do that per daemon, without blanking the
  others.
- Name collisions across daemons. `bob` on two daemons is two bots, and any
  merged listing has to show which is which.

### Order

1. Store identity.
2. `--daemon` and the endpoint list in the CLI.
3. A multi-daemon view in the app, with one follow per daemon.
4. A written ssh-forwarding recipe, with the permissions above.
5. Cross-daemon lineage, only if a workload needs it.

## 4. Move a bot to another daemon

Move a bot, including one with work in progress, to a daemon on another
machine, and continue it there as the same conversation.

### What exists

Everything a bot is lives in store rows. Its row holds the head, model,
provider, instructions, tools, budget, note, and compaction version. The
node tree holds the transcript, and turns, tool intents, processes,
artifacts, and events hold the rest (`src/store/db.rs:335-418`). Durable
work survives a restart: queued, ready, and steer submissions are rows (NEXT
item 8), and parked turns (waiting on handles or paced) are reloaded at
start (`src/server/mod.rs:530-575`). A bot can be forked from any node of
its history (`fork_any_node` in `ready`) and deleted in bounded pieces (NEXT
item 29).

Some of a bot cannot move, by construction:

- **A live model call.** It is a task holding an open provider stream, and
  partial text is not durable (`"partial_text_durable":false` in `ready`,
  `src/server/mod.rs:486`).
- **Foreground tool processes.** They run in their own process group,
  which is killed when the turn's future is dropped
  (`src/tools.rs:851-895`).
- **Background processes.** They are children of this daemon. A restart
  reports them `process_lost`, meaning supervision ended and the process may
  still be running (NEXT item 11).
- **The workspace.** It is an absolute, canonicalized path that must exist
  on this host (`src/server/mod.rs:333-341`), and the model has seen those
  paths in its transcript.

So "moving a running bot" really means draining it to a durable boundary,
then moving that state. Interrupt already reaches a boundary: planned calls
are cancelled, and executing calls without a committed result become
`tool_outcome_unknown` (NEXT item 14). But that stops the work instead of
carrying it over.

### What would change

1. **Export and import, not daemon-to-daemon traffic.** An `export`
   operation writes the bot's state to a bundle file, a small SQLite file
   with a schema version and the source store's identity. An `import`
   operation reads one. The caller carries the file with scp, rsync, or
   whatever it uses. The daemon never opens a network connection to another
   daemon.
2. **What a bundle holds.** The lineage nodes from the head to the root
   (nodes are shared with the source's other forks, so they are copied, not
   moved), the bot row, turns with their request ids (so a retried
   submission stays idempotent at the target), tool intents, completed
   process results, artifacts, and note and compaction versions. Events
   only if the caller asks, since cursors will not survive anyway.
3. **Renumbering at import.** Node, turn, bot, process, and event ids are
   per-store sequences, so import allocates fresh ones and rewrites every
   reference: parents, head, context start, note and compaction chains,
   cuts, and checkpoints. Two ids cannot simply be rewritten:
   - Transcript text already holds turn ids in `TURN/CALL_ID/STREAM`
     artifact references and in `turn:` and `proc:` handles. History is never
     rewritten (AGENTS.md), so import has to keep an alias from the origin
     turn and process ids to the new ones, which `read` and `wait` consult
     for an imported bot. Otherwise those references fail with an honest
     `artifact_not_found`. Choose between the two explicitly; don't discover
     it in testing.
   - Turn ordinals, which the `history` tool uses, are per lineage and
     survive unchanged.
4. **Import checks before accepting.** The target must have a provider of
   that name and family, which is checked today only at admission
   (`src/server/mod.rs:725-746`), every tool in the bot's selection, and a
   free name (`bot_exists` otherwise, or import under a new name). A
   missing piece fails the import with an explicit error. It never fails at
   the first turn.
5. **The first slice is a cross-store fork.** Importing a copy under a new
   identity, with the source untouched, is already well defined: it is a
   fork whose history happens to live in another store. That delivers
   "continue this conversation on that machine from this checkpoint" before
   any move semantics exist.
6. **Then a real move, with an explicit state at each step.** The source
   marks the bot `moving` and refuses work, the way `deleting` does
   (`src/store/db.rs:418`). The target imports, idempotently by origin
   identity. The source then becomes a tombstone that answers `bot_moved`
   with the destination store's identity. A later submission is answered
   with that, never `bot_not_found` or a fresh bot. If the import fails, the
   source clears `moving`. A crash between steps leaves at most a `moving`
   source and an imported copy, a visible state that re-running the move
   resolves. Never two active copies.
7. **Draining a running bot.** A `drain` stops the bot at its next round
   boundary: the model call in flight finishes, its tool calls finish and
   commit, and the turn parks instead of starting the next round. Round
   boundaries already exist for steers and compaction. This adds a reason to
   park there, and nothing is cancelled or run twice.
8. **Parked turns that wait on handles.** A handle into the same store
   resolves only if the whole subtree moves together, a parent with the
   children it waits on. Otherwise import refuses with the handles named.
   Paced turns move freely, since their pacing state is per daemon and is
   relearned.

### Risks

- **Workspace divergence.** The model's transcript names absolute paths. If
  the target's workspace is elsewhere, its next tool calls go to the old
  paths. Placing the files is the caller's job (README, Scope). The move
  should take a workspace mapping and refuse a path that does not exist, and
  whether the model is told about the move is a client-policy question.
- **Side effects.** Only a drained or idle bot moves. A bot that was
  interrupted may have `tool_outcome_unknown` calls whose processes are
  still running on the source host. The move does not change that; the
  bundle should carry and show it.
- **Provider state.** Stored reasoning and thinking items move with the
  transcript. Whether a different API key or organization accepts another's
  encrypted reasoning items is unverified and has to be checked per family.
  The provider's prompt cache doesn't move, so the first turn at the target
  is a full miss.
- **Lineage.** `created_by` names a bot in the source store, so the
  target cannot validate it (`src/store/db.rs:574-592`). The moved bot's
  children that stayed behind can no longer reach it through
  `AGENT_PARENT`. This is the same cross-daemon lineage question as section
  3.
- **Copies of stores.** Two stores with the same identity, one copied from
  the other, would make "import idempotently by origin" collide. Store
  identity therefore needs a way to be reissued when a store is copied, or
  the origin key has to include something a copy does not share.
- **Size and sensitivity.** Histories of 100k items are measured
  ([DAEMON_MEASUREMENTS.md](DAEMON_MEASUREMENTS.md#long-history)), so export
  runs in bounded pieces like retention does (NEXT item 29). A bundle holds
  transcripts and tool output, so it belongs in the ignored local directory,
  never in the repository.

### Order

1. Store identity.
2. Export of an idle bot and import as a new identity: the cross-store fork.
   Behavior tests: the imported bot's next turn sees the same context as a
   local fork at the same node, retained artifacts and history reads work
   after renumbering, and a missing provider or tool fails the import.
3. The alias-or-refuse decision for ids in transcript text, with a test that
   reads an artifact reference written before the move.
4. Move with `moving` and tombstone states, and `bot_moved` answers.
5. Drain to a round boundary for a running bot.
6. Moving a parent together with the children it waits on.

## Combined order

| Step | Serves | Changes |
| --- | --- | --- |
| Store identity in `ready` | all four | store, `ready` |
| `summary` read, observe capability | watching | store read, protocol |
| Client digest (CLI, then app) | watching | client only |
| Client store discovery | per-workspace | client only |
| `--daemon` endpoint list, then app view | several daemons | client only |
| Replay pages on the reader, measured | watching | daemon |
| `fork` with a tool selection | watching, move | protocol |
| One-versus-N daemon screen | per-workspace | bench |
| Export and import as a cross-store fork | move | store, protocol |
| Move semantics, then drain | move | store, turn loop |

Not planned here: a network listener in the daemon, daemon-to-daemon
connections, starting daemons on other machines, copying workspaces, or
process sandboxing. Each would make Agent a remote execution service or a
provisioning tool. Where one of them is needed, it is a separate process or
the caller's job.

## Open questions

- Should imported ids in transcript text resolve through an alias, or fail
  honestly? The alias keeps old tool results readable after a move. Failing
  keeps the store free of a translation layer.
- Should per-workspace daemons become a default for the app, or stay an
  opt-in for failure isolation? The one-versus-N screen should come first.
- Does a model-written summary belong to the observer (a tool-less fork it
  pays for), or should the daemon offer one? This document assumes the
  observer, in keeping with NEXT item 20: mechanism in the daemon, opinions
  in the client.
