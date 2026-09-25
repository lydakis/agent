# Several daemons, moving bots, and watching them

Status: proposed roadmap. None of the additions below is implemented. The
next implementation is bounded observation within one daemon, with behavior
tests and a matched performance screen. Multi-daemon clients and bot mobility
remain later phases; their design questions do not block that first slice.

The four goals are to watch a bot without changing its work, optionally run
a daemon per workspace, use one client across daemons, and continue a bot's
conversation on another machine. Agent owns durable state and the model/tool
loop. Callers own machine selection, workspace provisioning, file transfer,
and host isolation. Reuse SSH for remote socket access. Keep one daemon per
user as the default until a matched screen supports another recommendation.

Source observations below were checked on 2026-09-25 against
[`e1d413f`](https://github.com/lydakis/agent/tree/e1d413ffa3e2c1adf9995f19442c0469fbbe077f).
The roadmap branch through `46dc064` has identical runtime, client, and app
source. File links and symbols identify those observations; implementation
must recheck them against its own base. Acceptance limits below are proposed
engineering gates, not measured results.

## Contracts that apply across phases

| Contract | Requirement |
| --- | --- |
| Observation has a bounded cost | Bound admission, queued work, materialized bytes, socket output, and live fan-out. Measure commit and model-request latency under observation; a separate reader alone cannot prove isolation from shared CPU or disk. |
| A reference keeps its meaning | Every id is scoped to its issuing store. A client or import must resolve the intended object, or explicitly refuse it, never use a colliding local id. This applies to structured fields and every carried text a model can see or retrieve. |
| History and outcomes stay durable | Preserve original transcript bytes, context versions, historical forks, retained outputs, and known or unknown tool outcomes. A consistent export cut must survive concurrent submission and retention. |
| Continuation has one execution owner | A fork is a new identity. A move transfers permission to continue an existing conversation. Cancellation, restart, retries, and restore must not make both sides runnable or replay completed tool work. |
| Destination admission is real admission | Validate all effective models, tools, workspace mappings, and incoming pending work before import commits. Unsupported state fails explicitly. Configuration validation cannot promise provider acceptance. |

These contracts are requirements. Later sections record candidate mechanisms
and the evidence needed to select them. They do not freeze every storage
column, error shape, or migration algorithm. Agent has no compatibility
requirement for its own earlier protocols; update current callers together
instead of adding legacy modes. Preserving durable history is still required.

## Phase 1: bounded observation in one daemon

### Current paths

The protocol already exposes most of the data an observer needs. Routing is
in [`Service::dispatch`](../src/server/mod.rs); storage execution is in
[`Store::op` and `Store::read`](../src/store.rs).

| Read | Current execution path |
| --- | --- |
| `bots`, `turns`, `result`, `resume` inspection, `events`, `artifact`, store counts for `stats` | Storage worker shared with writes |
| `history_nodes`, `history_items`, `item` | Reader shared with model-context construction |
| `follow` | Bot inspection and replay on the worker; live delivery through the hub |
| `wait` | Handle registration and settlement against durable outcomes |

[`Hub::replay`](../src/server/hub.rs) performs its final replay-to-live
transition on the worker to avoid missing a commit between those modes.
`Hub::fan_out` visits subscriptions and clones events into their output
queues. Slow socket followers close on a failed `try_send`; the stdio owner
is backpressured and is not the observer transport.

[`Output`](../src/output.rs) bounds each session's queue at 2 MiB and holds
its byte permit until writing finishes. That does not bound the aggregate
cost of many observers. Dispatch also awaits storage reads, so a read can
delay the service loop even when it writes nothing.

Compaction summaries and carry-forward notes already exist in the store,
versioned with history. There is no protocol read for their text. A local
fork can inspect a completed checkpoint, but inherits its source's tools
and consumes model allowance if asked to summarize. Those are later client
features, not prerequisites for measuring the observation path.

### Implementation boundary

Move the observation reads above to a separate read-only connection, leaving
the context reader for model requests. Route `resume`'s inspection and
`follow`'s initial bot lookup through it too. Keep decisions coupled to
writes, including handle settlement for `wait`, on the worker. This is a
change to the current protocol path, not an optional alternate mode.

The implementation must satisfy the whole path from admission to delivery:

1. **Admit before allocating.** Dispatch uses bounded, non-blocking
   admission and returns immediately; a full observer queue answers
   `observer_busy`. Bound observer sessions, subscriptions, outstanding
   jobs, and response bytes separately. One connection has at most one
   observation page in flight, including while its socket is blocked.
   Refusals must not create an unbounded queue of error responses.
2. **Read bounded pages.** Apply encoded-byte and item limits to every
   observation operation, including listings and batched history reads.
   A single oversized item or result uses an explicit range/continuation
   protocol instead of materializing the whole value. Size large blobs
   before loading them; reserve encoding overhead as well as payload
   bytes. Artifact reads require a named stream and a bounded page.
   `resume` returns a bounded bot view: `instructions` and
   `compaction_instructions` (64 KiB each today) are read through the
   range protocol rather than inlined. Bound the SQLite work behind a
   page, not only its output: oldest-first `history_nodes` orders the
   whole ancestry before its `LIMIT`, and `history_items` walks from the
   head to the oldest requested node. Bound the work in each scheduled
   chunk and make progress resumable, so one request cannot hold the
   reader for the length of a history. That bounds each chunk, not the
   total cost of reaching a deep page. Choose an index (such as a stored
   depth) only after measuring its write and storage cost. Update the
   CLI (including `follow` and `interrupt`), shared client, and app to
   consume the new pages.
3. **Hold the charge through delivery.** Queued, materialized, serialized,
   and socket-held data all remain charged until written or discarded.
   Cancelling a request, disconnecting, or closing an overloaded follower
   releases its outstanding work and permits. The reader itself must not
   wait for a slow socket to drain.
4. **Bound live delivery too.** Serialize once and share event bytes where
   possible. Charge follower output to the aggregate budget and cap
   subscriptions before accepting them, including idle `follow *`
   subscriptions. Byte sharing does not remove per-subscriber CPU work.
   Close overloaded followers promptly; they reconnect from their last
   received durable cursor.
5. **Preserve replay ordering.** Replay pages use the observer reader.
   The final transition remains ordered with commits, but handoffs are
   admitted in bounded batches with at most one batch queued on the
   worker. Validate each follower against its requested stream, including
   a quiet bot whose last event is older than the fleet's last cursor.
   The hub keys subscriptions and replay by name today. Capture the bot
   id at the initial lookup and enforce it on every replay page and live
   event, not only at the handoff. A bot deleted and recreated under
   that name ends the follow with an explicit replacement notice before
   any of the new identity's events are delivered.
   A worker-ordered watermark or equivalent barrier must establish that
   replay and live delivery leave no gap. Do not substitute a comparison
   of unrelated per-bot and fleet cursors. Keep both the queued work and
   the execution time of a handoff batch bounded.

An observer connection is not a new authorization boundary. Today socket
permissions grant full access to one principal. When a second principal
needs observation alone, add a separately protected socket that serves only
the observe set; a client-selected flag cannot enforce read-only access.

### Acceptance before adding observer features

Behavior tests must cover the distinct contracts below. These are planned
tests, not claims about tests already written or passed.

| Path | Required observation |
| --- | --- |
| Every observe operation, including an oversized item, result, and batched history request | No unbounded materialization; ranges reassemble exactly; overload refuses promptly while unrelated bots progress. |
| A stopped reader, cancellation, and disconnect | Byte charges persist through socket delivery, then release; queued work and session/subscription counts return to baseline. |
| Idle and stopped followers, both one-bot and fleet-wide | Admission and output stay bounded; closure is explicit; reconnect replays the retained interval. |
| Commits during replay and a burst of tail attachments | Durable stream equals replay without gaps or duplicates, including quiet bots, retention notices, and restart. |
| Deepest pages of a long history, and `resume` on a bot with maximal instructions | Each chunk's work stays bounded and resumable regardless of history depth; the bot view fits the page ceiling and its long fields reassemble exactly. |
| Delete and recreate a followed bot's name during replay | The follow ends with a replacement notice and never delivers the new bot's events. |
| Existing CLI and app consumers | Their history, artifact, result, and follow flows still work through the bounded protocol. |

Use the socket transport, synthetic provider, and fixtures from the
[lifecycle screen](BENCHMARKS.md#rust-lifecycle-and-feature-costs) and
[mixed-workload soak](BENCHMARKS.md#mixed-workload-soak). Extend the harness
for this matrix; existing soak results do not establish these contracts.

Freeze the host, revisions, workload seed, achieved concurrency, store
snapshot, provider timing, and durability before comparing. Use five
alternating baseline/candidate pairs with identical completed model and tool
work. Verify normalized provider requests and tool outcomes as well as turn
counts; the existing soak provider does not validate conversation content.
Separate the required controller connection from additional observers.
Exercise no extra observers, long-log replay, stopped readers, idle live
followers, and bursts attaching at the tail. Include individual and fleet
subscriptions, long histories, compaction, retained outputs, and retention.
Use a steady offered turn rate and enough headroom that provider saturation
does not hide runtime delay; record completed work and refusals in each arm.
Overload arms may complete different amounts of observer work because the
candidate refuses excess demand. Report that difference as a feature cost;
do not turn it into an equal-work efficiency ranking.

For this screen, fix the candidate's observer output budget at 16 MiB,
read-page ceiling at 64 KiB encoded, job limit at 128, and session and
subscription limits at 64 each. These are fixture settings, not selected
production defaults. Include the controller within the limits and reserve
room for it. Ramp extra observers through 1, 8, 32, and the remaining
admissible slots, then offer four times the limit to test refusal. Existing
live-event size limits remain distinct from the read-page ceiling.

Report daemon CPU, RSS, threads, file descriptors, store/WAL growth,
completed turns, observer throughput, refusals, and p95/p99 commit,
model-context queue, and turn latency. Attribute worker, context-reader,
and observer-reader work separately. Report controller/provider/child costs
outside daemon totals. Histograms give percentile intervals; if their
buckets cannot resolve a gate, add focused timing before drawing a verdict.

Proposed gates, to be recorded before running the candidate:

- With no extra observers, median daemon CPU per completed turn grows by
  at most 5%, and peak RSS by at most 2 MiB. Each latency p99 grows by no
  more than the larger of 5% or 1 ms against the matched baseline.
- Under each observer load, candidate commit, context-queue, and turn p99
  remain within the larger of 5% or 1 ms of the candidate without extra
  observers. Completed work at the fixed offered rate is unchanged and
  no additional bot failures occur. Also report the old implementation
  under the same load; do not hide its cost in the no-observer comparison.
- Charged response bytes, jobs, sessions, and subscriptions never exceed
  their bounds. After warm-up, repeated four-times-limit overload cycles
  show no continuing RSS, descriptor, task, or WAL growth attributable to
  observers. Report SQLite/cache/allocator memory separately from the
  response-byte budget; that budget is not a bound on total RSS.
- Stream and transcript checks pass in every run. Publish all five pairs
  and their spread. A noisy result that cannot distinguish the gate is
  inconclusive, not a pass; change a gate only with an explicit rationale
  recorded before another candidate run.

These limits make "without disturbing" a testable, bounded claim rather
than a promise of zero shared-resource cost. Follow
[COMPARISON_CONTRACT.md](COMPARISON_CONTRACT.md); performance evidence does
not replace the behavior tests.

## Later phases and their entry gates

| Phase | Deliverable | Gate before implementation |
| --- | --- | --- |
| 2. Observer features | Versioned `summary` read, observe capability, CLI digest, then app presentation | Phase 1 behavior and performance evidence |
| 3. Several endpoints | Store identity, opt-in store discovery, named endpoints, then a merged client view | Identity lifecycle and reconnect contract; matched one-versus-N daemon screen before changing defaults |
| 4. Cross-store fork | Export a restricted, completed root bot and import a new identity | Bundle/reference contract, consistent export, destination validation, and round-trip acceptance cases |
| 5. Move a bot | Drain/resume, transfer ownership, durable destination routing | Tested single-store drain plus a crash/retry/cancel/restore state machine |
| 6. Move dependent bots | Transfer a parent and the children it awaits together | Phase 5 evidence and an atomic group contract |

Only phase 1 is the next implementation. Later phases are provisional and
may change after measurements or a concrete workload. No restore utility,
cross-store alias layer, or group-move subsystem is required to begin it.

### Observer features

A `summary` read returns the compaction summary, covered turns, version,
and carry-forward note selected for that bot's history. It spends no model
tokens and can be absent or stale. A client digest combines turn state,
events, recent output, and usage, returning JSON by default.

An explicitly requested model summary can use a fork with tools disabled.
Add and validate fork tool selection before offering that recipe. Preserve
provider-required schemas for historical tool blocks while forbidding new
calls; test both provider families. The model call consumes shared provider
allowance and may affect fleet pacing, so it is never automatic polling and
does not inherit the no-model observer performance claim. Cache reuse is
unmeasured. A protected observe-only socket waits for a second principal.

### Several endpoints

Per-workspace daemons already work with an explicit store path. Separate
daemons isolate daemon-local failures; they share host resources and user
permissions. They also multiply provider pools, learned pacing, process
bounds, and baseline memory. Opt-in discovery can search for a workspace
store, but must document ignoring database/WAL files and excluding live
stores from ordinary workspace copies. Keep the default store unchanged.

Before a client aggregates endpoints, define identity across normal
restart, file copy, and backup restore. A candidate uses a lineage id plus
an instance id: lineage survives copying; instance distinguishes live
stores and cursor histories. File device/inode and an external sidecar
are possible detection inputs, not proof against every restore or clone.
Replacing a database with an old backup must invalidate the old cursor
identity. State unsupported cases, including full-machine/block clones,
before claiming unique move destinations. Restore must leave either the
complete old store or the complete restored store after a crash, including
committed WAL frames; deleting sidecars before replacing the database is
not a sufficient protocol. Select and crash-test that protocol in this
phase rather than treating an inode recipe as an established guarantee.

Announce identity in `ready`. Key client state by endpoint and instance,
detect duplicate instances, qualify displayed bot identities and handles,
and keep a separate replay cursor per endpoint. Reconnect one endpoint
without clearing another's state. A lost `wait` request must be reissued
using its handle; replay cursors do not resume requests.

Keep remote access on SSH-forwarded Unix sockets in a private directory.
The operator starts remote daemons. A forwarded full-access socket grants
the daemon user's tool authority. Cross-daemon creators/parents remain an
explicitly unsupported relationship until their identity and routing
contract exists; never resolve them by a same-named local bot.

Measure one daemon with N workspaces against N daemons on identical work:
total CPU/RSS, connections, achieved concurrency, p95/p99 latency, and
shared-key pacing/refusals. Synthetic checks come first; a real-provider
check needs a stated spend cap. Add jitter or shared pacing only if evidence
requires it. Per-workspace daemons remain opt-in meanwhile.

## Mobility: constraints to settle before implementation

This section preserves the durability requirements behind the reviews.
It is not an approved import format or a complete move algorithm. Resolve
each contract once for the whole carried state, then test it across the
relevant record types instead of adding a separate exception per field.

### Bundle and reference contract

Use a versioned bundle carried by the caller; daemons do not connect to one
another. The bundle represents one durable cut and includes the records
needed for the promised history, context, result, fork, and retention
operations. An implementation inventory must cover:

- Bot configuration; lineage nodes and checkpoint rows; turns and request
  ids; tool intents and known/unknown outcomes; completed process results;
  retained artifacts; note/compaction versions; outcome and authorization
  events; and retained-turn ownership.
- Every structural id in columns and event payloads, including
  `nodes.turn`, turn ownership in associated tables, prompt-node links,
  checkpoint/cut/parent links, and completion/steer/tool-completion ids.
  Reassign store-local ids consistently. Keep lineage ordinals unchanged.
- Every carried model-visible or retrievable value: transcript nodes,
  notes, summaries and retained prompts, bot and compaction instructions,
  artifact streams, and completed process-result text. Preserve their
  bytes. References in any of these need an origin-aware resolution rule
  or explicit refusal, including references whose objects did not travel.

The reference contract also covers values saved by clients outside the
bundle, including checkpoints and history-node references. Keep their
origin identity and provide qualified resolution or a documented refresh
to target-local references. For a completed turn's saved checkpoint, a
refresh can read `result` for its remapped turn; other saved nodes need a
mapping or an explicit unsupported answer. Never reinterpret a source
integer as a target-local id, even if it happens to name a valid node there.
The receipt need not inline the entire node map, but the selected lookup
or refresh path must work before clients use imported history.

Use the storage codecs instead of copying encoded columns blindly:
[`artifact::read` and `artifact::put`](../src/store/artifact.rs) handle
artifact encoding, and large turn prompts are resolved through
`prompt_node`. A round trip must retain idempotent request comparison.

Outcome events are required even when most replay history is omitted:
[`turn_outcome` and `authorize_artifact`](../src/store/db.rs) use them for
results and fork-authorized artifact reads. Rebuild retained-turn ownership
so prune/delete work. Remap payload ids as well as relational columns.
Import creates fresh event cursors and an `imported` creation event carrying
the target bot's list fields, origin, and any omitted-history notice.
Clients must understand that event. Do not copy the source retention
watermark or mark newly retained events as pruned; the target per-bot
watermark starts at zero and unrelated fleet retention is unchanged.

The first cross-store fork is deliberately restricted: a root bot with only
finished turns, no running processes, no fork ancestry, no retained
artifacts, and no handles or artifact references anywhere in the carried
values above. Refuse each unsupported case explicitly. Source work remains
untouched, so carrying ready/queued turns would duplicate execution.
The new identity starts with zero usage and an optional new budget, like a
local fork. Drop the source creator, report that fact, and validate any
new target-local creator rather than exporting a stale parent identity.

Before lifting those restrictions, choose an origin-qualified handle and
artifact-reference design. Imported handles need a mapping to the carried
local records, not just a foreign-store tag. A reference to a child that
stayed behind must fail explicitly. Preserve resolution through later
local forks and subsequent imports; ambiguous unqualified references cannot
silently select a target object. Fork ancestry also needs an ownership
representation for inherited turns and outputs before forked bots travel.

### Consistency and destination admission

Capture the bot state, history head, and turn/event cut in one writer
transaction. Export in bounded pieces that all read the same cut. Fence
prune/deletion for the captured records until export finishes or aborts;
release the fence on disconnect. New work after the cut must not leak into
it. Avoid holding a long read transaction that pins the WAL fleet-wide.
Bundles contain transcripts and tool output and belong in ignored storage.

A consistent cut is not proof of a complete export. The source must bind
an expected complete record set to that cut and, for a move, its nonce.
Before publishing imported state, the destination verifies completion,
record identities/counts, and contents through a manifest with digests or
an equivalent validated format. Missing, unexpected, duplicated, or corrupt
records refuse the whole import. This check includes every carried record
class, not only transcript nodes or rows with foreign keys. Transport page
order need not matter if the format verifies the same complete contents.

Before committing an import, validate provider name/family, compaction
provider, selected tools, name availability, and all effective per-turn
models/workspaces. Map the bot's and unfinished turns' workspace paths;
refuse unmapped or absent paths. Admit the entire incoming pending queue
against the destination's turn and byte bounds atomically, or refuse the
whole import with `pending_limit`. Do not partially import a queue.

These are local checks. Provider acceptance of stored reasoning, especially
across credentials or organizations, remains a per-family validation task.
A first-call error is reported normally and never replaced with a fresh
conversation. Stored absolute paths can still direct tools to source paths;
workspace placement and the client instruction explaining a move need an
explicit contract before claiming transparent continuation. Cache transfer
or reuse is unmeasured.

### Drain and ownership transfer

First implement and test drain within one store. A bot drains only at a
durable round boundary after its in-flight model/tool work commits. Running
background processes are checked separately from bot idleness and must
finish or be durably resolved. Unknown tool outcomes stay unknown; moving
is never permission to replay their side effects.

A durable `drained` turn remains parked across restart. Releasing it resumes
from its existing head without appending its prompt again. It must not go
through the `ready`/`start_locked` path used for fresh submissions. Queued
work behind it stays queued and subject to destination admission.

The candidate move protocol has these ownership boundaries:

| Boundary | Required behavior |
| --- | --- |
| Prepare | Name one destination instance and a nonce; mark the source `moving`; fence submissions, deletion, and conflicting transitions. |
| Export cut | Atomically bind the cut and exported state to that nonce. A cancel ordered before it retires the nonce and prevents later export pages. |
| Destination import | Accept only the named instance, atomically and idempotently by nonce. The source stays non-runnable if the response is lost. |
| Receipt | Tombstone the source with destination lineage/instance, bot name/id, and mappings for every carried turn, finished or unfinished. |
| Cancel after the cut | Require the destination's durable refusal of that nonce, issued only if it has not imported it. An unreachable destination leaves the source fenced; any operator override must state the duplicate-execution risk. |

Tombstones route supported bot queries and controls with `bot_moved`:
submissions, `resume`, `follow`, old/new `wait`s, `result`, `interrupt`, and
fork/history access. The route identifies the destination store and bot;
operations naming a turn or node use the corresponding target reference
or the refresh path above. Unsupported operations fail explicitly instead
of presenting a stale local bot. Resolve already registered waiters too,
and emit a durable `bot_moved` event for existing followers and reconnects.
Clients follow that route: `follow` and `interrupt` currently inspect via
`resume`, so both must handle a moved response there as well as on the
later operation. Start replay with a target cursor, not a saved source
cursor, and send cancellation to the remapped target turn. The caller
connects to the destination; this adds no daemon-to-daemon forwarding.
Ordinary delete cannot remove a moving bot or its tombstone; explicit
expiry must state the routing guarantees it removes.
After a destination restore, use the origin and move nonce to verify the
imported bot before rebinding to a new instance and replaying from scratch.
A backup predating import answers `moved_bot_missing`, not a same-named bot.

A source restore can undo the fence: a backup from before prepare carries
neither the nonce nor the tombstone, so both copies could run. A restored
store keeps execution fenced until ownership is explicitly reconciled.
The durable ownership record that makes reconciliation safe is a phase 5
design gate, not specified here. An operator override is outside the
single-owner guarantee and must say so.

Move runtime-local clocks and limits by meaning, not raw values. A paced
turn re-enters the destination's provider gate, with a fresh per-call retry
budget and preserved cumulative retry/pacing history. Charge source pacing
through the cut, start target pacing at import, and exclude transfer time
and clock skew. Preserve the state discriminator used to resume a model
call. A wait timeout carries its remaining duration under an explicitly
defined transfer-time policy.

Refuse a move with outbound dependencies whose records do not travel, or
with source turns waiting on the moved bot. Client waiters can be redirected
as above. Group moves are a separate phase: one nonce, source transaction,
destination transaction, and receipt for all participants. Remap internal
creator/parent links along with handle references; drop external creators
explicitly. Do not emulate this by moving mutually dependent bots one at
a time.

### Evidence required to advance mobility

| Gate | Acceptance cases |
| --- | --- |
| Restricted fork | Compare next request context against a local fork at the same checkpoint, including summary/note; preserve results, history ordinals, idempotency, older checkpoints, prune/delete, and imported-event visibility. Race submission and retention against paged export. Exercise every stated refusal. |
| Bundle completeness | Drop a complete page from each carried record class; duplicate, truncate, or corrupt records; interrupt export before finalization. Each invalid bundle leaves no visible import. Exercise page reordering according to the chosen format. |
| Saved client references | Save a completed turn's checkpoint, move the bot into a store with colliding node ids, then fork from that checkpoint through qualified resolution or refresh. Exercise saved history-node references and explicitly refused unsupported cases. |
| References and ancestry | Use colliding source/target ids across every carried record and event kind. Read pre-import outputs from the imported bot and its later local fork. Resolve handles embedded in instructions, notes, summaries, artifacts, and process results, including after another import. Reject missing or ambiguous origins. |
| Drain | Drain a multi-round turn, restart, release, and prove its prompt and every completed round appear exactly once with no tool replay. |
| Move | Crash/retry at each ownership boundary; race cancel, delete, waits, and followers; rename at import; exceed target pending bounds; use skewed clocks and restores before/after import; restore the source from a pre-prepare backup and submit to both sides. Reconnect through the source and exercise the CLI resume/follow/interrupt flow, including cancellation of a moved queued or drained turn. Prove at most one side can continue and old handles route correctly. |
| Group | Move a waiting parent with its child atomically; the child completes, the parent resumes, and subsequent parent/child communication uses the remapped identities. |

Each phase needs bounded resource measurements on long histories as well
as behavior evidence. These gates are requirements for future implementation,
not prerequisites for merging this roadmap or starting phase 1.

## Source map

The observations above refer to the pinned revision at the top. Recheck
these symbols when implementing; this table is not a claim about later main.

| Contract | Starting points |
| --- | --- |
| Storage ownership, queues, and metrics | [`Store`, `Counters`](../src/store.rs); [`socket`](../src/server/socket.rs) |
| Observe routing and service-loop dispatch | [`Command`, `Service::dispatch`](../src/server/mod.rs) |
| Replay ordering and live fan-out | [`Hub::fan_out`, `replay`](../src/server/hub.rs); [`Output`](../src/output.rs) |
| History, compaction, results, artifact authorization, retention | [`Database`](../src/store/db.rs); [`artifact`](../src/store/artifact.rs) |
| Fresh start versus continuation | [`start_locked`, `Database::resume`](../src/store/db.rs); [`rounds`](../src/server/turn.rs) |
| Handles and waiter settlement | [`Handle`, `Handles`](../src/server/handles.rs) |
| Per-store connections and client reconnect | [`socket`](../client/src/socket.rs); [`Client`](../client/src/lib.rs); [`ensure_daemon`](../src/client.rs) |
| Fleet creation events and app connections | [`app.js`](../app/ui/app.js); [`main.rs`](../app/src-tauri/src/main.rs) |
| Existing measurement fixtures | [`bench.lifecycle`](../bench/lifecycle.py); [`bench.soak`](../bench/soak.py); [`soak_followers`](../bench/soak_followers.py) |
