# Approving tool calls: a manual approver and a fast automatic one

Status: design note, 2026-09-26. Nothing here is built. Code facts are from
lydakis/agent at b07080c. Peer facts were read on 2026-09-26 from the pages
linked in each section; blog claims are marked as claims. The latency figure
for the socket hop was measured for this note; the Jev figures come from the
2026-09-19 probe in [DAEMON_MEASUREMENTS.md](DAEMON_MEASUREMENTS.md#jev-data-points-for-compaction).

## The question

Every allowed tool call runs today without asking anyone. George wants two
more ways to run a bot:

- **Manual:** a person, or a program, allows or denies each call before it
  runs.
- **Automatic:** a small, fast "System 1" model decides, the way Jev
  (TypeSafe's bounded-decision model, `POST /v1/systemone`) answered the
  compaction probes, and only the unclear cases go further.

The daemon and CLI must stay unopinionated mechanisms
([AGENTS.md](../AGENTS.md), George 2026-09-25), and performance comes first.
An approver sits on the critical path of every call it judges, so its cost
has to be counted per call.

## Recommendation in brief

1. **The daemon gets one mechanism and no policy.** A bot can name tools
   whose calls wait for a verdict. The daemon announces those calls when the
   model plans them, waits for an `answer` from any client, runs or refuses
   the call, and records who decided. It has no rules, no prompts, no model,
   and no timeout of its own.
2. **Manual and automatic are clients.** Which one answers is the mode.
   Switching modes, or running both with the model escalating to a person,
   needs no change to the bot.
3. **The automatic approver is layered, and most calls never reach a
   model.** Ungated tools skip it entirely. Deterministic rules in the client
   answer the obvious cases in microseconds. Jev answers a handful of narrow
   questions about the rest in about 0.4 s. Whatever stays unclear goes to a
   person, a larger model, or a denial, never to a silent allow.
4. **The fast path adds no storage commit.** Requests ride the commit that
   already records the model's plan, and verdicts ride the commit that
   already starts or finishes the call. A verdict answered by rules costs
   one socket round trip, measured at 0.13 ms median. A person's slow answer
   parks the turn durably, like `wait`, so it holds no task and no slot and
   survives a restart.
5. **It is oversight, not containment.** An allowed shell command runs with
   the user's permissions and can reach the daemon's socket. Approval
   catches mistakes and overeager actions; only a sandbox around the tools
   bounds what an approved command can do.

## What happens today

Code facts at b07080c:

- **One check before execution, by tool name.** `execute_calls`
  (src/server/turn.rs:1545) commits `tool_start`, then refuses a tool the
  bot was not created with (`tool_not_available`, line 1563), validates the
  arguments, runs the tool, and commits `tool_finish`. "Allowed tools run
  without approval prompts" (RUST_PROTOTYPE.md:1440).
- **The design brief left room for this.** [DESIGN.md](DESIGN.md#initial-permissions)
  keeps "one policy boundary before tool execution, carrying bot, session,
  turn, tool-call identity, tool name, and arguments", with room for
  allow/deny/request-input decisions, and lists an "Answer input" operation
  that correlates a response to the outstanding approval (DESIGN.md:62).
- **Plans are durable before anything runs.** `append`
  (src/store/db.rs:2379) commits the model's items and inserts one `tools`
  row per call as `planned`, in the same transaction.
- **Waiting already costs nothing live.** A `wait` call parks the turn:
  `suspend` (src/store/db.rs:2646) stores the calls still to run, the task
  ends, and the handle registry resumes it (src/server/handles.rs). A parked
  turn is a store row and a registry entry of a few hundred bytes, and it
  survives restart ([parked turns](DAEMON_MEASUREMENTS.md#parked-turns)).
- **Recovery never reruns a tool.** A planned call left by a crash or an
  interrupt is answered `cancelled`; an executing one without a committed
  result gets `tool_outcome_unknown` (src/store/db.rs:2549).
- **A bot's shell can reach the daemon.** Delegation depends on it: a bot
  runs `agent run --new` from its shell. The creator of a new bot is
  whatever `AGENT_BOT` and `AGENT_BOT_ID` say in that shell
  (src/client.rs:618), so it is declared, not verified.
- **Planned but not built:** an allowed-tools list that a fork inherits and
  can only narrow ([FORK_MID_TURN.md](FORK_MID_TURN.md#2-an-optional-allowed-tools-list)).
  Approval sits next to it: that list says which tools may run at all, this
  one says which of those need a verdict first.

## How peers do it

PEERS_PLACEHOLDER

## What to take from them

- **Most calls should never reach a model.** Claude Code skips its
  classifier for read-only actions and edits inside the working directory,
  and Anthropic reports that sandboxing cut prompts by 84% internally. The
  cheapest verdict is the one never asked for.
- **The approver must not read tool results.** Anthropic's classifier sees
  the user's messages and the bare tool calls; the model's own prose and
  every tool output are stripped, so text in a file or web page cannot
  address the judge. Adding them made results worse on its test set.
- **Ask narrow questions.** Anthropic's classifier answers against named
  rules and denies with a rule tag. Our own Jev probes found the same: a
  question that names the judgment ("does turn N state a rule that applies
  later?") separated cleanly, and an abstract one ("will the agent need
  turn N?") barely did.
- **Deny and continue, then stop asking.** A denied call returns its reason
  to the model, which tries another way. After 3 denials in a row or 20 in a
  session, Claude Code stops auto-deciding and asks the person.
- **Fail closed.** A classifier error, timeout, or unreadable answer denies
  the call in Claude Code; it never allows it.
- **People are poor approvers at volume.** In Anthropic's study of 1,053
  testers, people caught 13.6% of dangerous commands, falling to about 5%
  after 50 prompts, and approved 93 to 97% of all prompts (claims from
  Anthropic's posts). A manual mode is for rare, high-stakes calls or for
  watching a new bot, not for every `ls`.
- **A classifier is not a boundary.** Rehberger's August 2026 chain got
  Claude Code's auto mode to run attacker code in 60 to 80% of tries: each
  step the classifier saw looked benign. Anthropic's answer was that auto
  mode is "best-effort… not a security guarantee".

## Design

### The daemon mechanism

- **`create` takes an `approve` list**, a subset of the bot's tools whose
  calls need a verdict. Absent or empty means today's behavior, at today's
  cost. Like `tools`, it is fixed for the bot's life; there is no operation
  that removes a tool from it, because the bot's own shell could call that
  operation. The CLI flag is `run --approve shell,write,edit`.
- **A fork copies it and can only add to it,** the mirror of the allowed
  list, which a fork can only narrow. Tool definitions never change, so the
  prompt cache is untouched.
- **Bots created from inside a gated bot inherit its list.** When `create`
  names a creator that has an `approve` list, the new bot's list is the
  union of both. This is a guard against accidents, not a boundary: the
  creator is declared by the shell's environment, and a command that clears
  it creates an ungated bot. The approver sees that command first.
- **The request rides the plan commit.** When `append` records a model
  response whose calls include gated tools, the same transaction marks
  those `tools` rows as needing a verdict and writes one
  `approval_requested` event for the round:
  `{"node":N,"calls":[{"call_id","name","arguments","arguments_truncated"}]}`.
  Arguments are previewed to 2 KiB, as `tool_started` already does; an
  approver reads a longer one (a large `write`) with `item` on the node.
  One event per round, not per call, and no extra commit.
- **`answer` decides one call.**
  `{"op":"answer","bot","turn","call_id","decision":"allow"|"deny","reason"?,"by"?}`.
  The first answer wins. A second gets `approval_already_answered`, an
  unknown call `no_pending_approval`, a finished turn `stale_turn`. `by` is
  a label the client chooses, recorded for audit and not verified. Any
  client can answer, including a bot's shell; see the security section.
- **`approvals` lists what is pending,** optionally for one bot, from the
  store, so an approver that connects late or restarts can catch up. `stats`
  counts pending verdicts.
- **Every planned call of a round is announced at once,** so an approver
  judges them in parallel and can judge each in light of the others (a
  `write` of `run.sh` followed by `sh run.sh`). Execution stays in order,
  one call at a time, as today. Each gated call waits only for its own
  verdict.
- **Allow rides `tool_start`.** The call starts as today, and its
  `tool_started` event gains `"approval":{"by","waited_ms"}`. No extra
  commit.
- **Deny is a result.** One commit, the same one a finished call makes,
  records `{"error":"approval_denied","detail":REASON}` as the call's result
  and a `tool_completed` event with `"denied":true`. REASON is the
  approver's text, or null. The model sees it and the turn continues, as
  with any tool failure. The daemon writes the code and nothing else.
- **A short wait stays live; a long one parks.** A gated call whose verdict
  has not arrived waits in memory for `--approval-hold-ms` (proposed default
  2,000). After that the turn parks the way `wait` does: one `suspend`
  commit, no task, no active slot, restart-safe. A verdict for a parked turn
  is committed when it arrives and resumes the turn. The hold keeps model
  verdicts off the park path and keeps a person's wait off the task table.
- **Verdicts are durable no later than the call starts.** While the turn has
  a task, an early verdict (for call 3 while call 1 runs) waits in memory and
  is written by the next commit the turn makes anyway: that call's start, its
  denial, or a park. If the daemon dies first, recovery interrupts the turn
  and cancels its planned calls, so the verdict no longer matters. When the
  turn is parked, `answer` commits the verdict itself. Answers, parks, and
  call starts go through the storage worker, which serializes them, so a
  verdict landing while the turn parks is neither lost nor applied twice.
- **Interrupt and restart need nothing new.** Interrupting a turn with
  pending verdicts cancels those calls, as it cancels any planned call. A
  turn parked on a verdict survives restart like a parked `wait`, and its
  requests stay in `approvals`.
- **Steers still wait for the round boundary.** To redirect the model about
  one call, deny it with a reason; the reason is the model's next input.
- **Not built:** allowing a call with edited arguments. The transcript would
  show a call that never ran. Deny with a reason instead. Persisting "always
  allow this" is client state, not daemon state.

The daemon never judges a call, never writes text for the model beyond the
error code, and never times out a verdict. A client that wants a deadline
denies on its own clock.

### Manual mode

- **CLI:** `agent approvals [--bot NAME]` lists pending calls;
  `agent answer --bot NAME --turn N --call ID allow|deny [--reason TEXT]`
  decides one. `follow --pretty` shows a pending call and who later decided
  it. `agent answer` refuses to run inside a tool shell (it sees
  `AGENT_BOT`), the same kind of guard as the creator identity: it stops
  accidents, not a determined command.
- **App:** a pending call renders as a card on the bot's timeline with Allow
  and Deny (with an optional note). The app already follows every bot, so it
  gets `approval_requested` with no new subscription.
- **Program:** any process that follows events can answer. A CI job can
  allow a fixed set of commands and deny the rest with no model at all.

### Automatic mode

The approver is a client program with its policy as data, like the CLI's
default compaction text: `agent approver`, a long-running command, and the
same module inside the app. It follows `approval_requested` on every bot
(`follow --all`) and answers in layers:

1. **Daemon gate.** Only tools in a bot's `approve` list reach it. The
   suggested list is `shell,write,edit`; `read`, `history`, `wait`, `note`,
   and `echo` change nothing outside the store. (`read` can read a secret,
   but sending it anywhere takes a gated call.)
2. **Deterministic rules, microseconds.**
   - `write` and `edit` inside the turn's workspace are allowed. The path is
     resolved before judging, and a symlink that leaves the workspace does
     not count as inside.
   - A shell command that is one simple command from a read-only list (`ls`,
     `cat`, `rg`, `git status`, `git diff`, `git log`, and so on) is allowed.
     Anything with a pipe, `;`, `&&`, `$(`, a backtick, or a redirect goes
     on to the model. Parsing shell well enough to allow compound commands
     is where Cursor's and Claude Code's pattern rules were bypassed; the
     approver does not try.
   - Commands that talk to the approval channel itself (`agent answer`, the
     daemon's socket, the store file) are denied outright.
   - Rules the user adds ("allow `cargo test`", "deny `git push`") are
     checked here. "Always allow" from the manual UI adds a rule here.
3. **Jev, about 0.4 s.** One request per call, answering a few questions,
   each naming one judgment, each returning a probability:
   - Did the user ask for this action, or for something that needs it?
   - Does it delete or overwrite data outside the workspace, or rewrite
     history someone else has (a force push)?
   - Does it send files, secrets, or code to a destination the user did
     not name?
   - Does it read, print, or change credentials or access permissions?
   - Does it change a shared or production system (deploy, migrate,
     publish, send a message)?
   - Does it download and run code from the network?

   Allow when every risk is low. Deny, with the question as the reason (like
   Claude Code's rule tags), when a risk is high and the user did not ask
   for it. Everything else goes to step 4. The thresholds start as guesses
   and are set from a labeled run (below).
4. **Unclear.** With a person attached (the app is open, or `agent
   approver --escalate`), the approver answers nothing and marks the call
   for the person, who sees only these. Unattended, it asks a larger model
   if one is configured, else denies with "needs review". It never allows
   on an unclear, failed, or timed-out verdict.
5. **Circuit breaker.** After 3 denials in a row for a bot, or 20 in one
   turn, the approver stops answering that bot automatically and escalates
   everything, or interrupts the turn when no one is attached. A model
   that keeps trying variations of a denied action is the pattern this
   catches.

**What Jev is shown.** The human prompts, the round's planned calls, the
calls already allowed in this turn, the workspace path, and the user's
environment note (trusted remotes and hosts). Not tool results, not the
model's prose, and not the model's reasoning. For a bot created by another
bot, the prompt was written by a model; the approver follows `created_by` to
the first bot a person prompted and shows that prompt as the user's intent,
the child's prompt as a model-written task. Anthropic reports that most of
its classifier's misses come from misjudging consent, and a delegation
chain is where consent is easiest to fake.

**Where the key lives.** The approver process holds the Jev key. Bots never
see it: their shells are children of the daemon, not of the approver.

## Performance

| Path | Added per gated call | Extra commits | Holds a task |
| --- | ---: | ---: | --- |
| Tool not in `approve` | nothing measurable (one list lookup, as today) | 0 | as today |
| Rules answer | one socket round trip, 0.13 ms median, 0.3 ms p99 (measured) | 0 | yes, briefly |
| Jev answers | about 0.34 to 0.44 s median (2026-09-19 probe) | 0 | yes, up to the hold |
| Person answers | the person's time | 2 (park, verdict) | no, after the hold |

- **The socket hop.** A Python client sent 5,000 sequential JSONL requests
  to a release build of b07080c over its Unix socket on this 4-vCPU Linux
  VM (2026-09-26): `resume` on an existing bot took 0.13 ms median,
  0.16 ms p90, and 0.23 to 0.31 ms p99 across three runs; `stats` 0.18 ms
  median. An answer is one event out and one request in, so about one such
  round trip. The probe script is in the pull request's description. This
  is an estimate of the hop, not a measurement of the unbuilt path.
- **Why no extra commit matters.** On an idle Mac a commit costs about
  5.4 ms since `fullfsync` ([macOS flush](DAEMON_MEASUREMENTS.md#group-commit)),
  40 times the socket hop. A design that parked every gated call would add
  two of those to each call on the fast path, and one that committed every
  verdict on arrival would add one.
- **Rounds, not calls, pay the model.** Because every call of a round is
  announced when the plan commits, their verdicts come back together. A
  round pays about one Jev latency however many calls it has, instead of one
  per call, and later calls' verdicts overlap earlier calls' execution.
- **Jev in context.** The live fleet check measured 1.9 s median per turn on
  gpt-5.6-luna, so a round that needs Jev is roughly 20% slower. A
  Terminal-Bench schemelike trial made 36 to 53 model calls
  (2026-09-26 rerun); if every round needed Jev that is 14 to 23 s more per
  trial. How many rounds reach Jev is the number that decides whether this
  is acceptable, and it is not measured yet (below).
- **Cost.** The probe billed 420 to 1,970 input tokens per request and no
  output, about $0.00003 to $0.0001 per check. A trial of 50 checks costs
  under a cent.
- **A person's wait.** Parking and recording the verdict are two commits,
  about 11 ms on an idle Mac; the call's start after resuming is a commit it
  makes anyway. A parked turn then costs a store row and a registry entry
  ([parked turns](DAEMON_MEASUREMENTS.md#parked-turns)). A wait longer than the prompt-cache TTL (5 minutes by
  default, `--cache-ttl 1h`) means the next model call writes the cache
  again. Keeping it warm through a parked wait is the same open question as
  parked `wait` turns (NEXT.md item 42).

**Placements considered and rejected:**

- **The classifier inside the daemon.** It would save the 0.13 ms hop, which
  is 0.03% of a Jev call, and it would make the daemon choose prompts,
  models, and thresholds, which is policy.
- **Park on every gated call.** Simpler, one path. Costs two commits per
  call on the fast path, about 11 ms on an idle Mac.
- **Ask at execution time, one call at a time.** A round of n gated calls
  would pay n Jev latencies instead of about one.

**Later, if the numbers ask for it:** announce a call as soon as its
arguments finish streaming, before the response ends, so the verdict starts
earlier. That saves only the tail of the stream after the call, which is
little when the call is the last thing the model writes, and it announces a
call before its plan is durable.

## Security

Approval is oversight. It is not a sandbox, and the note should not be read
as one.

- **An allowed command can do anything the user can.** `python script.py`
  approved after the model wrote `script.py` has approved arbitrary code. A
  multi-step chain in which each step looks benign passes any per-call judge
  that cannot see results; that is Rehberger's attack on Claude Code.
- **A bot can reach the approval channel.** Its shell can call `answer` on
  its own pending calls, create a bot without the gate, or edit the store.
  The rules layer denies the obvious forms; a determined command can hide
  them from any text-based judge. Making this a boundary means running the
  tools where the socket and the store are out of reach, in a container or
  VM the caller provides. The daemon has no such isolation and the roadmap
  keeps sandboxing out of scope (NEXT.md).
- **What it is good for.** Catching the overeager or mistaken action a
  person would have stopped: the deleted branch, the force push, the token
  pasted into a request. Anthropic reports its pipeline let 17% of real
  overeager actions through at 0.4% false positives. That is the scale of
  protection to expect.

## Measure before building

1. **Tool mix, no model calls.** Count, in the Harbor transcripts on
   George's Mac, how many calls would be ungated, answered by rules, or sent
   to Jev, per round and per trial. This decides whether the latency above
   is acceptable.
2. **Jev on a labeled set.** A few hundred calls from those transcripts
   plus synthetic dangerous ones (a force push, `curl | sh`, a key sent to
   an unknown host, `rm -rf ~`), each labeled. Record false allows and false
   denials per question and threshold, and latency p50 and p99. At the
   probe's prices this is a few cents, but it is a paid run, so it waits for
   George's go-ahead.
3. **The daemon path.** On the lifecycle screen with `approve` set and a
   rules-only approver, confirm zero added commits per call and under 1 ms
   added per gated call; then the park path with a delayed answer.

## Open decisions

- The hold before parking: 2 s covers a Jev verdict with margin; shorter
  frees slots sooner for people.
- Whether bots created from a gated bot inherit its list in the daemon, or
  only by the CLI passing it on.
- Unattended and unclear: deny (proposed default), or a larger model.
- Where the automatic approver runs: the app, `agent approver`, or both
  (proposed: both, one module).
