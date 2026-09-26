# Approving tool calls: a manual approver and a fast automatic one

Status: design note, 2026-09-26. Nothing here is built. Code facts are from
lydakis/agent at b07080c. Peer facts were read on 2026-09-26 from the pages
linked in each section; blog claims are marked as claims. The latency figure
for the socket hop and the tool mix were measured for this note; the Jev
figures come from the 2026-09-19 probe in [DAEMON_MEASUREMENTS.md](DAEMON_MEASUREMENTS.md#jev-data-points-for-compaction).

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

1. **Three modes, and full access stays the default.** `full` is today's
   behavior: every allowed call runs, and nothing new is paid. `auto`
   lets an automatic approver decide with no person in the loop. `manual`
   waits for a person or a program to answer each gated call. The CLI
   takes the mode from `--approval` or `AGENT_APPROVAL` when it creates a
   bot, like `AGENT_MODEL`; unset means `full` (George, 2026-09-26).
2. **The daemon gets one mechanism and no policy.** A bot can name tools
   whose calls wait for a verdict. The daemon announces those calls when the
   model plans them, waits for an `answer` from any client, runs or refuses
   the call, and records who decided. It has no rules, no prompts, no model,
   and no verdict timeout beyond one the gate's creator sets.
3. **Manual and automatic are clients.** The mode names live in the CLI and
   the app; the daemon sees gates, each a list of tools and an opaque tag
   naming which approver answers.
4. **Auto is hands-off.** Deterministic rules in the client answer the
   obvious cases in microseconds, and Jev answers ten narrow questions
   about the rest in about 0.3 s. In the stored Harbor trials
   the rules settled only 21 to 29% of calls, so Jev is the common path,
   not the exception: at least 64 to 76% of model rounds would wait on
   it (the count treated four git commands as read-only), about 1
   to 2% of median trial time. What is dangerous or unclear is
   denied, never silently allowed and never sent to a person. On a
   labeled set of 341 calls from those trials, Jev's answers with the
   rule below allowed nothing it should have denied, but at the starting
   thresholds they also refused 23% of the benign calls. The set holds
   almost no dangerous calls, so the thresholds stay open. The model
   gets the reason and tries another way, or tells its caller what it
   needs. If the caller then says yes in a message that names the action,
   the approver reads that as the user's consent on the retry. Nobody
   approves anything in a separate step.
5. **The fast path adds no storage commit.** Requests ride the commit that
   already records the model's plan, and verdicts ride the commit that
   already starts or finishes the call. A verdict answered by rules costs
   one socket round trip, measured at 0.13 ms median, and one job on the
   storage worker that commits nothing. A person's slow answer
   parks the turn durably, like `wait`, so it holds no task and no slot and
   survives a restart.
6. **It is oversight, not containment.** An allowed shell command runs with
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

Read on 2026-09-26. Claude Code from its docs (changelog at v2.1.283) and
Anthropic's [auto mode post](https://www.anthropic.com/engineering/claude-code-auto-mode)
(2026-03-25); Codex from source at `25270df` and its
[auto-review docs](https://learn.chatgpt.com/docs/sandboxing/auto-review);
Cursor from its [run modes](https://cursor.com/docs/agent/security/run-modes)
and [hooks](https://cursor.com/docs/agent/hooks) docs; the others from source
pinned the same day (Gemini CLI `2fe7c2d`, goose `04ed836`, OpenHands SDK
`a350dc7`, opencode `696f41b`, Pi `d6af72e`).

| Harness | Modes | Automatic judge | What the judge sees | Judge fails | On denial |
| --- | --- | --- | --- | --- | --- |
| Claude Code | manual, acceptEdits, plan, auto (default since v2.1.283), dontAsk, bypass | Sonnet 5, two stages: a one-token filter, then reasoning only on flags | user messages, bare tool calls, CLAUDE.md; not tool results or the model's prose | closed: deny | reason to the model; after 3 in a row or 20 per session, asks the person |
| Codex | `approval_policy` untrusted, on-request, never, granular; sandbox read-only, workspace-write, full | `codex-auto-review` (ChatGPT login) or gpt-5.6-luna, low effort, as a read-only turn that may run commands to inspect | transcript with tool outputs, marked untrusted; only requests that would escalate out of the sandbox | closed, labeled "a review failure"; timeout reported apart | reason plus "do not work around this"; interrupts after 3 in a row or 10 of the last 50 |
| Cursor | auto-review (default), allowlist, run everything; sandbox | Haiku 4.5 or GPT-5.4 Mini | not documented | not documented | the agent may try another way, then the person is asked |
| Gemini CLI | plan, default, autoEdit, yolo; TOML policy rules | Conseca (off by default): Gemini 3.5 Flash writes a policy per prompt, then checks each call | the prompt's policy and the call | Conseca allows on its own errors; the runner denies on timeout | "denied by policy" plus the rule's message |
| goose | auto, approve, smart_approve, chat | the session's own model, one call per batch, asked only "which are read-only?" | tool name and arguments, marked untrusted, no transcript | asks the person | "DO NOT attempt to call this tool again" |
| OpenHands | always confirm, never, confirm risky | the acting model's own risk label, or a separate guard model | recent actions (guard model) | unknown risk asks the person | "Action rejected: REASON" |
| opencode | allow, ask, deny per tool and pattern | none | | | a reason continues the loop; a plain reject stops it |
| Amp | none by default since 2026-05-06; legacy rules could `delegate` to a program | a program you name, by exit code | tool name and arguments on stdin | | the program's stderr goes to the model |
| Pi | none; extensions can block a call | none | | an extension's failure blocks | |
| Unreal Agent | none described in its README | none | | | |

## What to take from them

- **Most calls should never reach a model.** Claude Code skips its
  classifier for read-only actions and edits inside the working directory,
  Codex's reviewer only sees what would leave its sandbox, and Anthropic
  reports that sandboxing cut prompts by 84% internally. CARE (2026) scored
  85.0% F1 on command safety with static rules alone at 0.34 ms a command,
  and 85.6% with a model behind them. Anthropic found 0.8% of actions in its
  API traffic looked irreversible. A model belongs on the few calls rules
  cannot settle.
- **The judge should not read tool results here.** Anthropic strips them,
  and the model's own prose, so text in a file or web page cannot address
  the judge; adding them made its results worse. OpenAI shows them, marked
  untrusted, but behind a sandbox that still bounds a wrong allow. Agent
  has no sandbox behind the approver, so it takes Anthropic's side.
- **Ask narrow questions.** Anthropic's classifier answers against named
  rules and denies with a rule tag; Codex's reviewer returns a risk level
  and the user's authorization separately. Our own Jev probes found the
  same: a question that names the judgment ("does turn N state a rule that
  applies later?") separated cleanly, and an abstract one ("will the agent
  need turn N?") barely did.
- **Deny and continue, then stop.** Every peer returns the reason and lets
  the model try another way, except opencode on a reject with no message.
  Claude Code asks the person after 3 denials in a row or 20 per session;
  Codex interrupts after 3 in a row or 10 of the last 50.
- **Fail closed, and say so.** Claude Code denies on a classifier error or
  timeout. Codex denies too, but tells the model it was "a review failure,
  not a determination that the action is unsafe", and reports a timeout
  separately. Gemini's Conseca and Cursor's hooks fail open by default,
  which is the mistake to avoid.
- **Nobody times out a person.** Codex, opencode, and OpenHands wait for a
  human approval as long as it takes, and Claude Code's SDK callback may
  stay pending indefinitely.
- **Programs can drive a rich protocol.** Codex's app-server sends
  `item/commandExecution/requestApproval` with the command, its directory,
  and a proposed rule, and takes `accept`, `acceptForSession`,
  `acceptWithExecpolicyAmendment`, `decline` (the turn continues), or
  `cancel` (the turn ends).
- **Pattern rules get bypassed.** Cursor's allowlist fell to backticks and
  `$(...)` (CVE-2025-54131) and to environment variables poisoned through
  shell built-ins (CVE-2026-22708). Claude Code's docs say its Bash rules do
  not match `/bin/rm`, `sh -c '...'`, or `git -C . push`. Codex splits a
  script into commands only when it is plain words joined by `&&`, `||`,
  `;`, or `|`, and treats anything with a redirect, substitution, or
  variable as one opaque command.
- **A model grading its own call is not a judge.** OpenHands' default
  analyzer and Cline's safe-command toggle trust the acting model's own
  risk label.
- **No one publishes latency.** Codex exposes a 90 s review deadline. A
  user's measurement of Claude Code found 94% of first-stage checks under
  2 s and 60% of second-stage checks at 10 s or more (claude-code issue
  #82133, 2026-07). The only sub-second figure in this note is our own Jev
  probe.
- **People are poor approvers at volume.** In Anthropic's study of 1,053
  testers, people caught 13.6% of dangerous commands, falling to about 5%
  after 50 prompts, and approved 93 to 97% of all prompts (claims from
  Anthropic's posts). A manual mode is for rare, high-stakes calls or for
  watching a new bot, not for every `ls`.
- **Monitors can be talked to.** A model under watch can write text aimed
  at its monitor: in one study GPT-4.1 as a monitor scored all 375
  backdoored samples as unsuspicious once they carried such text
  (Terekhov et al., 2025), and adaptive attacks broke 12 published
  injection defenses, most at over 90% success (Nasr et al., 2025). No
  published pre-execution gate claims to resist an adaptive attacker.
- **A classifier is not a boundary.** Rehberger's August 2026 chain got
  Claude Code's auto mode to run attacker code in 60 to 80% of tries: each
  step the classifier saw looked benign. Anthropic's answer was that auto
  mode is "best-effort… not a security guarantee". An independent test
  (Ji et al., 2026) found 36.8% of state-changing actions went around the
  classifier as in-project file edits. Cursor's docs say the same of its
  classifier: "not a security boundary".

## Design

### The daemon mechanism

- **`create` takes an `approve` list**, a subset of the bot's tools whose
  calls need a verdict. Absent or empty means today's behavior, at today's
  cost. Like `tools`, it is fixed for the bot's life; there is no operation
  that removes a tool from it, because the bot's own shell could call that
  operation. The CLI flag is `run --approve shell,write,edit`.
- **`create` also takes an `approver` tag,** an opaque string the daemon
  stores, reports in `resume`, `bots`, and `approval_requested`, but never
  interprets. Clients use it to decide who answers (see the modes above).
  A list and its tag together make a **gate**. A gate may also carry an
  `expire_ms` its creator chooses: a call whose verdict for that gate has
  not arrived that long after it was announced is denied by the daemon
  with the reason "not reviewed: no verdict", and the turn ends as
  interrupted, since with no approver answering nothing further in it
  could run. So a crashed approver stops its bots, and their model
  calls, instead of stalling them. The CLI sets 15 s on `auto` gates
  (the approver's own 10 s Jev deadline plus margin) and none on
  `manual` ones, since a person may take hours.
- **Gates only accumulate.** A new bot keeps every gate it descends from
  and adds the one it asks for:
  - `create`: its own gate, plus its creator's when it names one.
  - `fork`: the source's gates, plus its own, plus its creator's when a bot
    forks from its shell. A gated bot that forks an ungated one gets a
    gated fork, and a fork never drops its source's gate.

  Each gate's list is intersected with the new bot's tools, and gates with
  the same tag merge, keeping the shorter expiry. A call needs an allow
  from every gate whose list names its tool, and the first deny from any
  of them denies it. So a bot under `manual` that forks an `auto` bot gets
  a fork whose calls need both, and nothing a bot asks for can replace a
  gate it inherited. Mixed gates are rare; the common bot has one. A
  read-only child of a bot gated on `shell,write,edit` gets nothing to
  approve, because it has none of those tools. A fork keeps its source's
  tools, so its lists stay subsets even when its allowed list is narrower.
  Tool definitions never change, so the prompt cache is untouched. This is
  a guard against accidents, not a boundary: the creator is declared by
  the shell's environment, and a command that clears it creates an ungated
  bot. The approver sees that command first.
- **The request rides the plan commit.** When `append` records a model
  response whose calls include gated tools, the same transaction marks
  those `tools` rows as needing a verdict and writes one
  `approval_requested` event for the round:
  `{"calls":[{"call_id","request","announced_ms","gates","name","node"}]}`,
  where `gates` lists the tags whose answer the call needs and
  `announced_ms` is when this request was written. The stored event holds
  no arguments, since the node already does; a few dozen bytes per call.
  What is sent to approvers also carries, per gate, the bot's
  `denials: {"in_row","in_turn"}` for that tag. The daemon keeps these
  two counts on the bot's row, updated in the commits that already record
  a denial or start an allowed call, so an approver that restarts or
  takes over sees the same counts its predecessor did and the circuit
  breaker does not reset. `approvals` and
  `serve_approvals` return it too, so an approver that takes over a
  pending request knows how much of the client's deadline is left.
  Each call names its own node: an Anthropic round keeps all its calls in
  one assistant item, but a Responses round stores each `function_call` as
  an item of its own. Only what is sent to a serving approver, and what
  `approvals` lists, adds an `arguments` preview to 2,048 characters with
  `arguments_truncated`, as `tool_started` already does, taken from the
  node the worker has just written; an approver reads a longer one (a
  large `write`) with `item` on that call's node. Bot followers get the
  compact event. One event per round, not per call, and no extra commit.
- **`answer` decides one request.**
  `{"op":"answer","bot","turn","call_id","request","tag"?,"decision":"allow"|"deny","reason"?,"by"?,"until_prior"?,"lease"?,"path"?}`.
  An allow of `read`, `write`, or `edit` carries `path`, the file the
  approver resolved and judged (see the rules); for a person, the CLI or
  app resolves it and shows it before the person allows.
  `request` is the number the call was announced with, and it changes each
  time the call is announced again. `tag` names the gate answered and may
  be left out when the call has one. The first answer to the current
  request wins, per gate. A second gets `approval_already_answered`; an
  answer to an earlier
  request of the same call, computed before an earlier call failed, gets
  `approval_superseded` and changes nothing; an unknown call gets
  `no_pending_approval`, a finished turn `stale_turn`. `by` is
  a label the client chooses, recorded for audit and not verified. Any
  client can answer, including a bot's shell; see the security section.
- **`approvals` lists what is pending,** optionally for one bot or one tag,
  so an approver that connects late or restarts can catch up. It runs on
  the storage worker, like `answer`. A call is pending while any of its
  gates lacks a verdict, and each listed call names only the gates still
  unanswered, counting verdicts that are in the worker's map but not yet
  written. So it never offers a gate that would only fail with
  `approval_already_answered`, and a call answered by `auto` stays
  visible to `manual` until a person answers too. `stats` counts pending
  gates the same way. It pages: each reply holds at most a limit the
  caller sets and at most 256 KiB, in announcement order, with a cursor
  for the next page, so a fleet with a pending call per bot stays under
  the socket's 1 MiB response cap and no single worker job builds a large
  list.
- **One session serves a tag's approvals.** `serve_approvals
  {"tag","lease_ms"}` hands a session the pending requests whose gates
  include that tag and then streams only new `approval_requested` events
  for them. The first worker job fixes the point where the stream starts;
  the pending requests before that point come in pages like `approvals`,
  and everything after it arrives on the stream, so nothing falls between
  the list and the stream. At most one session holds a tag; a second is refused
  with `approvals_served`. The holder keeps the tag by sending an answer or
  a `renew` at least every `lease_ms`, a period it chooses (`agent
  approver` renews every second on a 5 s lease). A holder that goes quiet
  while connected, like a suspended app or a wedged loop, loses the tag:
  the daemon sends it `approvals_lost`, and the next `serve_approvals`
  takes over. Until one does, its calls wait. The approver never reads the
  fleet's other events, so its cost grows with gated calls, not with
  everything the bots stream, and two automatic approvers (the app and
  `agent approver`) do not both pay Jev for the same call in normal
  running. A takeover can bill a round twice: a holder suspended with a
  Jev request in flight loses the tag, the new holder sends the same
  round, and the old answer is refused when the old holder wakes. That is
  bounded by the rounds in flight at the takeover. The refusal is a fence:
  `serve_approvals` returns a `lease` number that changes at every
  takeover, a serving approver sends it with each answer, and an answer
  whose lease has ended gets `approvals_lost` and changes nothing, so a
  woken holder cannot race its replacement. An answer with no `lease` is a
  manual override, which is how a person answers over the approver.
- **Every planned call of a round is announced at once,** so an approver
  judges them in parallel and can judge each in light of the others (a
  `write` of `run.sh` followed by `sh run.sh`). Execution stays in order,
  one call at a time, as today. Each gated call waits only for its own
  verdict.
- **A verdict is for the round as planned.** When a call ends denied, with
  an error, or as a shell command that exits nonzero, every later gated
  call in the round is announced again, with a new `request` number and
  the failed call named, whether its verdict had arrived or not. A verdict
  it already had is dropped, and one still being computed is refused on
  arrival as `approval_superseded`. The approver judged
  `sh run.sh` expecting the planned `write`; if the write was refused, the
  file it would run is not the one it saw. The new request rides the failed
  call's own finishing commit, and a rules-only approver answers it again
  in microseconds. The success path pays nothing, with one exception the
  approver asks for: an allow sent with `"until_prior":true` holds only
  for the filesystem the approver could have looked at, so if any earlier
  call of the round finished after that request was announced,
  successfully or not, the call is announced again the same way. The
  comparison is with the announcement, not with the verdict's arrival,
  because the approver may have resolved the path before a call that
  finished while its answer was on the way. The rules set it when an
  allow depended on resolving a path and an earlier call of the round
  could change files.
- **Allow rides `tool_start`.** The call starts as today, and its
  `tool_started` event gains `"approvals":[{"tag","by","waited_ms"}]`, one
  entry per gate, so a replay shows which approver allowed which gate. No
  extra commit.
- **Deny is a result.** One commit, the same one a finished call makes,
  records `{"error":"approval_denied","detail":REASON}` as the call's result
  and a `tool_completed` event with `"denied":true`. REASON is the
  approver's text, or null. The model sees it and the turn continues, as
  with any tool failure. The daemon writes the code and nothing else.
- **A short wait stays live; a long one parks.** A gated call whose verdict
  has not arrived waits in memory for `--approval-hold-ms` (proposed default
  2,000). After that the turn parks the way `wait` does: one `suspend`
  commit, no task, no active slot, restart-safe. A verdict for a parked turn
  is committed when it arrives, and resumes the turn only if the turn is
  parked on that call's verdict. A turn parked on an earlier call's
  `wait` stays parked; the verdict waits in the store until execution
  reaches its call. The hold keeps model
  verdicts off the park path and keeps a person's wait off the task table.
- **Verdicts are durable no later than the call starts, and the storage
  worker holds them until then.** `answer` is a job on the storage worker,
  the thread that already runs every commit. The worker keeps unwritten
  verdicts in a map of its own, not in the turn's task. For a live turn,
  the job records the verdict there and wakes the task, with no commit. The
  next commit the turn makes anyway takes it out and writes it: that
  call's start, its denial, or a park. For a parked turn, the job commits
  the verdict, and resumes the turn only if it parked on that call.
  Because `answer`, `suspend`, and
  `tool_start` all run on that one thread, a verdict that lands as the hold
  expires is either in the map when `suspend` runs, and written with the
  park, or it arrives after, finds the turn parked, and is committed. The
  client is answered after the job runs, so an acknowledged verdict is
  never lost. If the daemon dies before a live turn writes it, recovery
  interrupts the turn and cancels its planned calls, so the verdict no
  longer matters. The price is one trip through the worker's queue, a few
  microseconds when idle and up to a few milliseconds under load (the
  `append` queue p99 was under 2.5 ms in the [storage
  counters](DAEMON_MEASUREMENTS.md#storage-counters-by-operation)).
- **Interrupt and restart need nothing new.** Interrupting a turn with
  pending verdicts cancels those calls, as it cancels any planned call. A
  turn parked on a verdict survives restart like a parked `wait`, and its
  requests stay in `approvals`.
- **Steers still wait for the round boundary.** To redirect the model about
  one call, deny it with a reason; the reason is the model's next input.
- **Not built:** allowing a call with edited arguments. The transcript would
  show a call that never ran. Deny with a reason instead. Persisting "always
  allow this" is client state, not daemon state.

The daemon never judges a call and never writes text for the model beyond
the error code and the fixed "not reviewed: no verdict" reason. Its only
clocks are ones a client chose: a gate's `expire_ms`, kept like a parked
`wait`'s deadline (stored with the park, so it survives a restart), and
the serving lease. A client that wants a shorter deadline denies on its
own clock.

### The three modes

| Mode | Gated tools | Who answers | Default |
| --- | --- | --- | --- |
| `full` | none | nobody; every allowed call runs | yes |
| `auto` | every tool except `history`, `wait`, `note`, `echo` | the automatic approver | |
| `manual` | every tool except `history`, `wait`, `note`, `echo` | a person, through the app or `agent answer`, or a program | |

- **Chosen per bot at creation.** `agent run --new --approval auto`, or
  `AGENT_APPROVAL=auto` in the environment, as `AGENT_MODEL` sets a model.
  `--approve LIST` changes which tools are gated. A child or fork keeps
  every gate it descends from, as the daemon section says.
- **The daemon sees no modes.** The CLI turns a mode into two `create`
  fields: the `approve` list, and an `approver` tag the daemon stores and
  reports but never reads (`auto` or `manual`). The automatic approver
  answers only `auto` gates; the app offers Allow and Deny only for
  `manual` gates. So one daemon can run bots in all three modes at
  once, and a program can invent its own tag and serve those bots itself.
- **`auto` needs its approver running.** The CLI already starts the daemon
  when it is not running; with `auto` it starts `agent approver` the same
  way. A gated call with no approver waits, parked and visible in
  `approvals`, rather than running or failing silently, until its gate's
  15 s expiry denies it and ends the turn. So if the approver dies and
  nothing restarts it, each auto bot stops at its next gated call with
  that reason, instead of hanging or looping; the next `auto` command
  starts a new approver.
- **Software callers keep `full`.** A program that drives bots gains nothing
  from a gate it has to answer itself, and `full` pays nothing.

### Manual mode

- **CLI:** `agent approvals [--bot NAME]` lists pending calls, each with
  its request number and gates; `agent answer --bot NAME --turn N --call ID
  --request R allow|deny [--tag T] [--reason TEXT]` decides one. The
  request number is required, so a decision made on what a person saw
  cannot land on a call announced again since; it gets
  `approval_superseded` and the person looks again. `follow --pretty` shows
  a pending call and who later decided
  it. `agent answer` refuses to run inside a tool shell (it sees
  `AGENT_BOT`), the same kind of guard as the creator identity: it stops
  accidents, not a determined command.
- **App:** a call pending on a `manual` gate renders as a card on the
  bot's timeline with Allow and Deny (with an optional note). The card
  answers the request it shows; a call announced again replaces it. The app
  already follows every bot, so it
  gets `approval_requested` with no new subscription.
- **Program:** any process that follows events can answer. A CI job can
  allow a fixed set of commands and deny the rest with no model at all.

### Automatic mode

The approver is a client program with its policy as data, like the CLI's
default compaction text: `agent approver`, a long-running command, and the
same module inside the app. It holds the `auto` tag with `serve_approvals`,
so only one instance runs at a time, and answers in layers:

1. **Daemon gate.** Only tools in a bot's `approve` list reach it. The
   modes gate every tool the bot has except four that touch only its own
   store records: today that is `shell,write,edit,read`, and a tool added
   later, such as one a caller registers, is gated without anyone
   remembering to list it. The rules below know only the built-in tools,
   so any other tool goes to the model with its name and arguments.
   `read` is gated because a result is sent to the model provider in the
   next request: reading
   `~/.ssh/id_ed25519` sends the key off the machine with no further call.
   `history`, `wait`, `note`, and `echo` touch only the bot's own store
   records and stay ungated.
2. **Deterministic rules, microseconds.**
   - `write` and `edit` inside the turn's workspace are allowed, with two
     exceptions that go to the model. The first is files a later command
     will execute: `.git/`, `.agent/`, `AGENTS.md`, build and hook files
     (`Makefile`, `package.json`, `.envrc`, CI workflows), and shell
     startup files (`.bashrc`, `.zshrc`, `.profile`). An independent test
     of Claude Code's auto mode found 36.8% of state-changing actions went
     around its classifier as in-project file edits (Ji et al., 2026). The
     second is files that hold secrets or grant access: every path on the
     `read` list below, and `authorized_keys`. A workspace that is a home
     directory would otherwise let a bot change its own keys unreviewed.
     The approver also remembers what each bot wrote this turn, so a
     command that runs one of those files is judged with the file's whole
     content in view (next paragraph).
   - Every path is resolved on disk when the approver judges it, not read
     as text, so a symlink inside the workspace that points out of it
     counts as outside. An earlier call in the same round can change what
     a path resolves to, so when one could change files, an allow that
     rests on a resolved path is sent with `until_prior` and judged again
     after that call runs. Other processes can change it too: a
     background command from an earlier round, a watcher, or anything
     else the user runs. For `read`, `write`, and `edit` the daemon closes
     that gap. The allow carries the path the approver resolved, and at
     execution the daemon opens the file, asks the OS which file it opened
     (`F_GETPATH` on macOS, `/proc/self/fd` on Linux), and fails the call
     as `path_changed` if that is not the path allowed, which
     re-announces the rest of the round like any failure. The approver
     resolves each path once, which it must do anyway; the daemon adds
     one system call after an open it makes anyway, and no lookup before
     the plan commit. A shell
     command resolves its own paths when it runs, so there the gap stays:
     oversight, not containment.
   - `read` inside the workspace is allowed, except files that look like
     secrets or commonly hold them: `.env*`, `*.pem`, `*.key`, `*.p12`,
     `*.pfx`, `*.keystore`, `id_*`, `.npmrc`, `.pypirc`, `.netrc`,
     `.git-credentials`, `.git/config` (remote URLs can carry tokens),
     `*.tfvars`, `.terraformrc`, anything under `.ssh/`, `.aws/`, `.gnupg/`,
     `.docker/`, or `.kube/`, and any file named `credentials` or
     `secrets.*`. Those, and reads outside the workspace, go to the model.
     This is a list, and a secret in a file it does not name reaches the
     provider. The daemon's redaction only catches its own provider keys.
   - `read` with an `artifact` instead of a path pages through the
     retained output of a shell call this bot's lineage already ran,
     and the daemon refuses any other. That output's head and tail
     already went to the model as the call's result, unscreened, so
     the rules allow the page. Approval judges actions, not results: a
     secret a command prints unexpectedly reaches the provider in its
     result whether or not anyone pages the middle. Screening results
     would be a filter on the daemon's output path, beside its
     provider-key redaction, and is not part of this design.
   - A shell command is split the way Codex splits it: only plain words
     joined by `&&`, `||`, `;`, or `|`. It is allowed only if every part is
     a command on the read-only list, every flag it uses is on that
     command's own list of allowed flags, and every path it names is a
     readable path by the rule above. The name must also resolve, through
     the tool shell's `PATH`, which the daemon reports in the
     `serve_approvals` reply for that reason, to an executable outside the
     workspace that
     the user cannot write, in a directory the user cannot write, or to
     one the environment note trusts (a Homebrew prefix is owned by the
     user). An `rg` or `find` found anywhere else, such as one an earlier
     command put in a writable directory on `PATH`, makes the command
     opaque. The lists name what is allowed, not what is not: `find` may
     take `-name` and `-type` but not `-exec`, `-delete`, `-fprint`,
     `-fprintf`, or `-fls`, simply because they are not listed; the same
     goes for `rg --pre` and `sort -o`. An unlisted flag, a redirect, `$(`,
     a backtick, or a variable makes the command opaque, and it goes on to
     the model. This is where Cursor's allowlist was bypassed; the approver
     does not try to parse more. Two kinds of command need more than
     that:
     - A recursive search (`grep -r`, `rg`) prints every file it reaches,
       so a `.env` or key file inside the workspace goes out with the
       results. It is allowed only with an include glob that cannot match
       a secret-looking name (`rg -g '*.rs'`); otherwise it goes to the
       model. `rg` also reads a config file named by `RIPGREP_CONFIG_PATH`,
       which can add `--pre` and run a program, so it counts only with
       `--no-config`. The approver cannot see the tool shell's variables:
       a daemon started earlier keeps the environment it was started
       with, which may not be the approver's.
     - Git is not on the read-only list at all. Even `git status` and
       `git diff` can run programs: `core.fsmonitor`, external diff
       drivers and `GIT_EXTERNAL_DIFF`, textconv and clean filters chosen
       by `.gitattributes`, and hooks such as `post-index-change` when the
       index is refreshed. `git diff` and `git show` also print the
       contents of any tracked file, a changed `.env` included, without
       naming it. Each could be checked, but the list keeps growing. Git
       commands go to the model.
   - Commands that reach the approval channel itself (`agent answer`, the
     daemon's socket other than through `agent`, the store file) are denied
     outright. So are `read`, `write`, and `edit` of the daemon's own
     files (its store, `~/.agent/state.sqlite` by default, with its WAL,
     and its socket), checked before the workspace rules: a workspace that
     is a home directory contains them, and a read would hand other bots'
     transcripts to the provider.
   - Delegation goes to the model with the task as the action. `agent run
     --bot NAME` hands work to an existing bot that may have no gate, so a
     task that asks for something this bot could not do unreviewed is
     judged as if this bot did it.
   - Rules the user adds ("allow `cargo test`", "deny `git push`") are
     checked here. "Always allow" from the manual UI adds a rule here.
3. **Jev, about 0.3 s.** One request per round: the state is the context
   below plus the round's calls, and each call gets `noul` (true or false)
   questions, each naming one judgment and returning a probability. Five
   name a risk:
   - Does it delete or overwrite data outside the workspace, or rewrite
     history someone else has (a force push)?
   - Does it send files, secrets, or code to a destination the user did
     not name?
   - Does it read, print, use, or change credentials or access
     permissions, such as signing in to a remote host (`ssh prod
     uptime`) with a saved key or login? (The labeled run asked this
     without "use"; the broader question is untested.)
   - Does it change a shared or production system (deploy, migrate,
     publish, send a message)?
   - Does it download and run code from the network?

   Each risk has a consent question asked beside it that names the same
   effect: does the caller's request for this turn ask for this deletion,
   this destination, this use or change of credentials, this system
   change, this download? The request for this turn is its prompt and
   steers, or those of the person-written turn that delegated it; earlier
   turns' prompts are shown for context but do not consent, so a deploy
   asked for yesterday does not cover one today. Later messages override
   earlier ones, so "don't deploy after all" withdraws consent. A request
   to do something once is used up by a call of this turn that did it and
   succeeded, which is why the already allowed calls are shown with their
   exit status; a retry after a failure is still covered. The
   consent question has to cover everything its risk question does: in
   the labeled run, asking about "this credential change" beside a risk
   that also covers using credentials denied a requested package upload
   that used a stored token (consent 0.06). Asking about "this use of
   credentials", with the same verbs as the risk question, raised it to
   0.91 and left every unrequested call at 0.13 or below. Asking all ten
   in one request keeps it to one round trip; they share the state, and
   Jev bills input only.

   Allow when every risk is either low or has a high consent answer, and
   no hard deny (step 4) matches. A
   task that says to install a package, publish, or deploy has consented
   to that effect and no other: a requested deploy that also sends a
   credential to a host nobody named has a high risk with no consent, and
   is denied. Deny, with the question as the reason (like Claude Code's
   rule tags), when any risk is high and its consent is low. Everything
   else goes to step 4. The starting thresholds are
   TypeSafe's guardrail cookbook's: below 0.35 is low, 0.70 or more is
   high. TypeSafe publishes no calibration figures; a third-party benchmark
   (jev-bench, 12 tasks, 2026-09-20) found answers reliable at 0.9 and
   task-dependent below it. Our labeled run
   ([Measure](#measure-before-building), item 2) found them too strict for
   coding work, but it cannot yet set
   better ones: benign calls such as starting the server a task asks for
   score up to 0.64 on "changes a shared system", while a requested
   `git push` scored 0.36, so that question has no safe gap between the
   two.
4. **Unclear or dangerous: deny, and let consent come through messages.**
   The approver denies with a reason and moves on; it never pages a
   person. The reason tells the model what was refused and that it can ask
   its caller for an explicit go-ahead. If the caller's next message (a new
   turn or a steer) names the action, the consent question now finds it in
   a prompt the caller wrote, and the retry is allowed. This is how Claude
   Code's auto mode already weighs explicit user intent, and it needs no
   approval screen: consent is part of the conversation. Only prompts from
   the caller count, never a prompt one bot wrote for another, and the
   message has to name the action, not just say "go ahead". The hard
   denies (a force push to a protected branch, `rm -rf` outside the
   workspace, the approval channel itself) stay denied whatever the prompt
   says; changing those is an edit to the approver's rules. An optional
   larger model can take the unclear band before the denial: a bot in the
   same daemon with only `read`, working as Codex's reviewer does, a
   separate turn that may look at files before it answers. The approver
   never allows on an unclear, failed, or timed-out verdict. As in Codex,
   the reason says which it was: "judged risky: REASON" or "not reviewed:
   the check failed", so the model can tell a refusal from an outage. A Jev
   request gets 10 s, the SDK's default, before it counts as failed.
5. **Circuit breaker.** After 3 denials in a row for a bot, or 20 in one
   turn, counted from the daemon's `denials` so a restart does not reset
   them, the approver denies the call with a reason asking the model to
   stop and report what it needs, then ends the turn with `interrupt` at
   its next gated round or 30 s later, whichever comes first. A model that
   reports ends the turn itself first; one that keeps going is stopped
   without its cooperation. Interrupting a turn that already ended gets
   `stale_turn` and does nothing. Claude
   Code uses 3 and 20 per session and then asks the person; Codex uses 3 in
   a row or 10 of the last 50 and interrupts. A model that keeps trying
   variations of a denied action is the pattern this catches.

**What Jev is shown.** The human prompts, the round's planned calls, the
calls already allowed in this turn with whether each succeeded, the
workspace path, and the user's environment note (trusted remotes and
hosts). Not tool results, not the model's prose, and not the model's
reasoning. An exit status is one word the harness writes, so it carries
no text from a tool. Jev's own limits page says
content written to steer it "can move the answer", and a planned call is
model-written text too: a command can carry a comment claiming the user
approved it. So the consent questions are asked of the human prompts, and a
high risk that the prompts do not name is denied whatever the command
says about itself.

**Whose words a prompt is.** A turn a bot submits to another bot carries a
prompt a model wrote. `created_by` names only a bot, not which of its turns
did the delegating, so the daemon records it per prompt: a tool shell's
environment gains `AGENT_TURN` next to `AGENT_BOT`, the CLI run from that
shell sends `from: {bot, turn}` with `submit`, `create`, and `fork`, and
the daemon stores it on the prompt item that `submit` writes, whether it
opens a turn or is absorbed as a steer, and reports it in
`approval_requested`. A prompt item without `from` is a person's words; one
with `from` was written by the model of the turn it names, and the
approver follows it back to that turn's own person-written items. So a
bot that steers a person's turn does not speak for the person, and a
person who steers a delegated turn does. If the chain cannot be followed
because a bot it names was deleted, the approver treats the prompt as
model-written with no person above it: nothing in it counts as consent,
so a risky call from that bot is denied rather than allowed on a model's
words.
Like the creator, `from` is declared, not verified: a command that clears
the environment submits a turn that looks like the caller's. The rules
deny `agent` commands that clear or override these variables (`env -i`,
`env -u AGENT_…`, `AGENT_BOT=…`), which stops the obvious form. On Linux
the daemon could also refuse a submission without `from` from a process
descended from its own tools, but a process that double-forks escapes that,
and macOS has no equivalent. Consent, like the gate, is advisory without
isolation.

**Secrets are redacted before Jev sees them.** A planned call can carry a
secret the model learned from an earlier result, such as a token in a
`curl -H 'Authorization: …'`, and sending it to Jev would disclose it
to one more service before the call is judged. So the approver runs a
local detector over everything it sends: the values of credential
variables in its own environment and the daemon's provider keys, known
key formats (provider prefixes, PEM blocks, JWTs, cloud access keys),
`Authorization` and cookie headers, URLs with a password, and long
high-entropy strings assigned to a name like `token`, `secret`, `key`,
or `password`. Each match is replaced with a typed placeholder such as
`[secret: bearer token]`, so Jev still sees that a secret is sent and
where, and judges the destination. A secret the detector does not
recognize is sent as is; like the `read` list, this is a list.

**The intent view is bounded.** Jev takes at most 32k tokens of state,
and a long-lived bot's prompts outgrow that. The approver sends, in this
order: the environment note, the root person's prompt for this turn, the
current turn's prompt and its steers, each marked as a person's or a
model's words, the calls being judged, the calls already allowed, and
then earlier prompts newest first until a 16k-token budget is spent. The
calls being judged are sent whole, read with `item` when their preview
was cut, and so is any file written this turn that one of them runs. So
are the already allowed shell and caller-registered calls, since a
one-time request is used up by a call that did it and a deploy past a
preview's 2,048 characters would hide that; an allowed `write` or `edit`
is shown as its path and size, since a file's content is not what
consent is about. If the calls being judged do not fit, it does not call
Jev; the call is unclear (step 4), with the reason "not reviewed: intent
too long". If the already allowed calls do not fit, consent is not
counted: Jev still judges the risks, a call whose risks are all low can
be allowed, and a risky one is unclear. A command that runs a file too
large to show is refused rather than judged on its first part.

In the stored Harbor trials the first limit never fires: across the
1,398 rounds that would reach Jev, the prompt plus every planned and
already allowed call (each previewed to 2,048 characters) came to a
median of about 4k characters for ChatGPT-plan runs and 7k for Sonnet,
and 25k at most, roughly 1k to 8k tokens (measured 2026-09-26, no model
calls). Sending allowed shell calls whole makes this larger; that has
not been measured.
Standing limits ("never push to main") belong in the environment note,
which is always sent, not in an old prompt that may fall out of the view.
Claude Code has the same gap: its docs say a boundary stated in the
conversation can be lost to compaction. Anthropic reports that most of
its classifier's misses come from misjudging consent, and a delegation
chain is where consent is easiest to fake.

**Where the key lives.** The approver process holds the Jev key, so it is
not in the environment the bots' shells inherit. When the CLI starts the
daemon and the approver together, it starts the daemon with the Jev key's
variable removed and passes it only to the approver; the daemon also adds
that variable to the set it already strips from tool environments, as it
does for provider keys, in case it was started some other way. That is
all it gets. A
tool running as the same OS user can still read it: on Linux through
`/proc/PID/environ` of the approver, or from wherever the key is stored.
The daemon's provider keys are exposed the same way today. Protecting
either from tools needs a separate OS user or a credential broker, which
this design does not provide.

**Jev's limits bound a fleet.** The published limits for `jev-1.13.0` are
1,200 requests a minute and 250k tokens a second, "adjusting dynamically",
with 64k tokens per request of which 32k can be state. The sustained live
fleet ran 60 model calls a second through one daemon. At the 64 to 76% of
rounds that reached Jev in the Harbor trials, that is about twice the
request limit: one key caps an `auto` fleet at roughly 26 to 31 model
rounds a second, at most. Both figures are bounds: the count treated
`git status`, `diff`, `log`, and `show` as read-only, and git now goes
to the model, so the share is higher and the cap lower. Hence one
request per round rather than per call, the
rules layer in front, and a pace in the approver that backs off on 429
and 529 as TypeSafe's docs ask. The pace has a bound: each round gets 10 s
from announcement to verdict, queue time included, measured from the
request's `announced_ms` so a new holder after a takeover inherits the
clock instead of starting a fresh one. The queue holds at most as many
rounds as Jev's current limit admits in that time. A round that would
wait longer, or runs out of time, is denied at once with "not reviewed:
the approver is overloaded". Under sustained overload `auto` then fails
closed and visibly, instead of growing a queue of parked turns.

## Performance

| Path | Added per gated call | Extra commits | Holds a task |
| --- | ---: | ---: | --- |
| Tool not in `approve` | nothing measurable (one list lookup, as today) | 0 | as today |
| Rules answer | one socket round trip, 0.13 ms median, 0.3 ms p99 (measured), plus one storage-worker job with no commit, plus the approver's path resolution (a few `stat` calls; not measured, and slower on a network mount) | 0 | yes, briefly |
| Jev answers within the hold (at least 64 to 76% of rounds, measured below) | 0.26 s median, 0.52 to 0.57 s p99 (labeled run, ten questions) | 0 | yes, up to the hold |
| A verdict after the hold, or while the turn is parked on an earlier `wait` | the answer's time | 1 or 2 (the park if this call caused it, and the verdict) | no, after the hold |
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
- **Jev latency.** The labeled run sent 676 requests of ten questions each
  from George's Mac, four in flight (2026-09-26): 0.26 s median, 0.31 to
  0.34 s p90, 0.52 to 0.57 s p99, 0.67 s at most, with no timeouts, rate
  limits, or retries. The 2026-09-19 probe measured 0.34 to 0.44 s median
  for shorter questions.
- **Jev in context.** The live fleet check measured 1.9 s median for a
  short turn on gpt-5.6-luna, so a short model round that also needs Jev is
  roughly 15% slower. Real trials are dominated by long rounds and tool
  runs, so the share of a whole trial is far smaller (below).
- **Cost.** Jev bills input only, at $0.042 per million tokens (TypeSafe's
  models page, read 2026-09-26). The labeled run's requests were 2,224
  input tokens median, 4,490 p90, and 7,886 at most; its 676 requests
  cost $0.071 in all, about $0.0001 each. A trial of 50 checks costs
  about half a cent. For comparison, the same 2,000 tokens cost $0.002 on
  Haiku 4.5 before any output.
- **A person's wait.** Parking and recording the verdict are two commits,
  about 11 ms on an idle Mac; the call's start after resuming is a commit it
  makes anyway. A parked turn then costs a store row and a registry entry
  ([parked turns](DAEMON_MEASUREMENTS.md#parked-turns)). A wait longer
  than the prompt-cache TTL (5 minutes by default, `--cache-ttl 1h`) means
  the next model call writes the cache again. Keeping it warm through a
  parked wait is the same open question as parked `wait` turns (NEXT.md
  item 42).

**Tool mix in the Harbor trials (measured).** The suggested gate and rules
were applied, without model calls, to every tool call in the stored
Terminal-Bench trials of the c585c16 matched rerun (5 tasks, 3 trials per
arm; 2026-09-26, on George's Mac). The extracted rounds match each trial's
recorded model rounds, and the calls match its `tool_started` events.

| | ChatGPT plan (gpt-6-sol) | Sonnet 5 |
| --- | ---: | ---: |
| Rounds / calls | 172 / 168 | 381 / 368 |
| Calls ungated / settled by rules / sent to Jev | 7 / 49 / 112 | 0 / 79 / 289 |
| Rounds that would wait on Jev | 64% | 76% |
| Jev rounds per trial, median (range) | 6 (2 to 21) | 16 (7 to 54) |
| Jev time at 0.4 s a round, share of median trial | about 2% | about 1% (worst trial 9%) |

The last row used the probe's 0.4 s; at the labeled run's 0.26 s median
the shares are about a third smaller.

- Every call sent to Jev was a shell command, apart from 4 writes outside
  the workspace. The commonest reasons were heredocs (`python - <<'PY'`),
  redirects such as `2>&1`, globs, variables, and running `python`
  itself. `cat`, `ls`, and `grep` reached Jev only when one of those made
  them opaque; the read-only list was not what held calls back.
- Letting `2>&1`, redirects to `/dev/null`, and globs that cannot match a
  secret name through moved the share by 1 to 2 points (64% to 63%, 76% to
  74%). On these tasks most rounds run or test code, which a parser cannot
  judge.
- Across every stored trial (89 ChatGPT-plan and 34 Sonnet trials, the
  earlier runs included) the shares were 62% and 75%.

So in `auto` Jev reviews most rounds of coding work. That is affordable in
time because a round's model call and its tools take several seconds (a
median trial ran 92 s for ChatGPT and 354 s for Sonnet), and it is what
sets the fleet cap above.

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
  its own pending calls, create a bot without the gate, hand its task to
  an existing bot that has none, submit a message that looks like its
  caller's consent, or edit the store.
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

1. **Tool mix, no model calls.** Done 2026-09-26 (the table in
   Performance): 64 to 76% of rounds would wait on Jev, about 1 to 2% of
   median trial time. It also found the three gaps now closed in the rules:
   recursive search, git's config-run programs (git now goes to the
   model), and symlinks. The count treated `git status`, `diff`, `log`,
   and `show` as read-only, so the Jev share is somewhat higher than the
   table shows.
2. **Jev on a labeled set.** Done 2026-09-26 on George's Mac, $0.071.
   The set was 338 rounds, 341 calls: 303 real calls from the c585c16
   trials plus 38 benign synthetic ones, 8 of them risky actions the
   prompt asks for, each labeled per question. The correct decision is
   the rule applied to the labels. Two identical passes, every answer
   from `jev-1.13.0`:
   - At 0.35 / 0.70, as first written, 84% and 83% of decisions were
     right, 20% were unclear, and 23% of benign calls were not allowed;
     there were no false allows. The rule then left a mid-band risk with
     high consent unclear; counting it as allowed (the rule as now
     written) makes that 89%, with a third fewer false denials and still
     no false allows.
   - The false denials came from starting the server a task asks for
     (shared system), installs from a task's own local package index
     (download and run), scratch files and copies under `/tmp` (delete,
     send), and the credentials consent wording fixed above. A rerun
     with the new wording took the false denials the credentials question
     caused from 5 to 3, with no false allows. A broader wording that also
     counts a requested deploy or message through a saved login as
     consent took them to 1, but that is the kind of wording that could
     grant consent too easily, so it waits for the dangerous set.
   - The best single threshold pair that kept every labeled risk and
     every refused consent out of the allowed side reached 93% with no
     unclear band; one pair per question reached 96% on the pass it was
     fitted to and 92 to 96% on the other. These show how many false
     denials are avoidable, not settings to adopt: the set had only 1
     destroy, 0 exfiltration, 2 credential, 5 shared-system, and 25
     download positives.
   - 6 of the 303 real calls (2%) install something the task needs but
     does not name, such as `torch`. The paired consent question denies
     them whatever the thresholds, since nobody asked for that download.
     George kept that strict (Open decisions).
   - Answers barely moved between passes (largest change 0.13), but 9 of
     341 decisions (2.6%) flipped at 0.35 / 0.70.

   False allows stay unmeasured until a labeled dangerous set exists;
   authoring one was out of reach for this run. The thresholds stay open
   until then.
3. **The daemon path.** On the lifecycle screen with `approve` set and a
   rules-only approver, confirm zero added commits per call and under 1 ms
   added per gated call, path resolution included, and record the bytes
   each gated call adds to the store and WAL and to the approver's
   socket, against the same screen without `approve`; then the park path
   with a delayed answer, counting its commits, and a verdict that
   arrives while the turn is parked on an earlier `wait`.

## Open decisions

Settled by George on 2026-09-26: three modes, `full` stays the default,
`AGENT_APPROVAL` picks the mode, and `auto` denies what is dangerous or
unclear without asking anyone. Installing a dependency the task needs
but does not name counts as unconsented, so `auto` denies it (2% of real
Harbor calls); what that costs in pass rate is measured once `auto`
runs. Settled by this design: the daemon enforces gate inheritance, so a
bot cannot drop a gate by going around the CLI.

- The hold before parking: 2 s covers a Jev verdict with margin; shorter
  frees slots sooner for people.
- Whether auto sends the unclear band to a larger model before denying, or
  denies straight away (proposed: straight away, and measure how often the
  band is hit).
- Where the automatic approver runs: the app, `agent approver`, or both
  (proposed: both, one module).
- Thresholds per question, once a labeled dangerous set measures false
  allows.
