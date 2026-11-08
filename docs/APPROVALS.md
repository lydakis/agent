# Approving tool calls: gates and approvers

Status: design note, 2026-09-26, revised 2026-09-27; split 2026-11-08, when
the automatic approver's own design, now Sonny's reviewer, moved to
[sonny/REVIEWER.md](../sonny/REVIEWER.md) and this note kept the daemon's
mechanism in the daemon's words ([src/AGENTS.md](../src/AGENTS.md#words)).
The daemon mechanism, manual answering, serving a tag to one approver, and
the automatic approver (`agent approver` then, Sonny's `sonny reviewer`
now) are built;
[Built so far](#built-so-far) lists where the build differs from the design
below. On 2026-09-27 George
decided that `auto` means a model decides every gated call, with no rules
in front of it: an approver that answers by rules alone is manual mode with
a program in the person's place, and rules for manual mode are a later
option (NEXT.md item 45). The rules layer this note first proposed, and the
daemon checks that only it needed, are gone from `auto`. On 2026-11-01
the person's own command rules came in, as Codex's execpolicy writes them
([command rules](RUST_PROTOTYPE.md#command-rules)): they sit in front of
every approver, so a `forbidden` or `allow` command never reaches one,
and a `prompt` command waits on the gate even inside the sandbox.
Code facts in the sections below are from lydakis/agent at b07080c, before
any of it was built. Peer facts were read on 2026-09-26 from the pages
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
An approver sits on the critical path of every call it decides, so its cost
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
4. **Auto is hands-off, and a model decides every call.** Every model
   round with a gated call sends one request to Jev, which answers ten
   narrow questions about the round in about 0.3 s; no rules answer
   first (George, 2026-09-27). Rules would have settled only 21 to 29% of
   calls in the stored Harbor trials anyway, and 64 to 76% of rounds would
   have waited on Jev with them, about 1 to 2% of median trial time.
   Without them every round with a gated call waits about 0.26 s at the
   median and costs about $0.0001. What is dangerous or unclear is
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
   already starts or finishes the call. Beyond the approver's own time, a
   verdict costs one socket round trip, measured at 0.13 ms median, and
   one job on the storage worker that commits nothing. A person's slow answer
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
  (src/client.rs:618), so it is declared, not verified. That holds for an
  unsandboxed bot and for a call allowed to escalate; a sandboxed bot's
  commands reach only its own socket, which answers as that bot and
  refuses what would undo its confinement
  ([sandbox](RUST_PROTOTYPE.md#sandbox)).
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

| Harness | Modes | Automatic approver | What it sees | When it fails | On denial |
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
  cannot settle. George chose otherwise for `auto` (2026-09-27): the model
  decides every gated call, and rules are a later option for manual mode.
- **A model approver should not read tool results here.** Anthropic strips them,
  and the model's own prose, so text in a file or web page cannot address
  the approver; adding them made its results worse. OpenAI shows them, marked
  untrusted, but behind a sandbox that still bounds a wrong allow. Agent
  had no sandbox behind the approver, so it took Anthropic's side until
  2026-10-28, when it took OpenAI's ([Sonny's reviewer](../sonny/REVIEWER.md#how-it-reviews)).
- **Ask narrow questions.** Anthropic's classifier answers against named
  rules and denies with a rule tag; Codex's reviewer returns a risk level
  and the user's authorization separately. Our own Jev probes found the
  same: a question that names the decision ("does turn N state a rule that
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
- **A model grading its own call is not an approver.** OpenHands' default
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
  stores, reports in `inspect`, `bots`, and `approval_requested`, but never
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
  a fork whose calls need both, and nothing a bot asks for when it makes
  one can replace a gate it inherited. Configuring a bot replaces its
  gates, inherited ones too: a person overriding a bot's mode decides. Mixed gates are rare; the common bot has one. A
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
  `denials: {"in_row","recent"}` for that tag, counted in the call's own
  turn (`recent` replaced `in_turn` on 2026-10-27, below). The daemon keeps these two counts on the bot's row, updated in
  the commits that already record a denial or start an allowed call, so an approver that restarts or
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
  large `write`) with `history_items` on that call's node. Bot followers get the
  compact event. One event per round, not per call, and no extra commit.
- **`answer` decides one request.**
  `{"op":"answer","bot","turn","call_id","request","tag"?,"decision":"allow"|"deny","reason"?,"by"?,"lease"?}`.
  `request` is the number the call was announced with, and it changes each
  time the call is announced again. `tag` names the gate answered and may
  be left out when the call has one. The first answer to the current
  request wins, per gate. A second gets `approval_already_answered`, and
  so does any answer after a deny, which decided the call already; an
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
  with `approvals_served`, including duplicate registration on the same
  session. The holder keeps the tag by sending an answer or
  a `renew_approvals` at least every `lease_ms`, a period it chooses
  (`sonny reviewer` renews every second on a 5 s lease). A holder that goes quiet
  while connected, like a suspended app or a wedged loop, loses the tag:
  the daemon sends it `approvals_lost`, and the next `serve_approvals`
  takes over. Until one does, its calls wait. The approver never reads the
  fleet's other events, so its cost grows with gated calls, not with
  everything the bots stream, and two automatic approvers (the app and
  `sonny reviewer`) do not both pay Jev for the same call in normal
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
  decides them in parallel and can weigh each in light of the others (a
  `write` of `run.sh` followed by `sh run.sh`). Execution stays in order,
  one call at a time, as today. Each gated call waits only for its own
  verdict.
- **A verdict is for the round as planned.** When a call ends denied, with
  an error, or as a shell command that exits nonzero, every later gated
  call in the round is announced again, with a new `request` number and
  the failed call named, whether its verdict had arrived or not. A verdict
  it already had is dropped, and one still being computed is refused on
  arrival as `approval_superseded`. The approver decided
  `sh run.sh` expecting the planned `write`; if the write was refused, the
  file it would run is not the one it saw. The new request rides the failed
  call's own finishing commit, and the approver decides the rest of the
  round again, one more model request. The success path pays nothing.
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
  has not arrived waits in memory for its bot's `approval_hold_ms` (default
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

The daemon never decides a call and never writes text for the model beyond
the error code and the fixed "not reviewed: no verdict" reason. Its only
clocks are ones a client chose: a gate's `expire_ms`, kept like a parked
`wait`'s deadline (stored with the park, so it survives a restart), and
the serving lease. A client that wants a shorter deadline denies on its
own clock.

### The three modes

| Mode | Gated tools | Who answers | Default |
| --- | --- | --- | --- |
| `full` | none | nobody; every allowed call runs | yes |
| `auto` | every tool except `history`, `wait`, `note`, `echo`, `read` | the automatic approver | |
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
- **`auto` needs its approver running.** The approver is a client of its
  own, Sonny's reviewer (`sonny reviewer`), which Sonny keeps as the
  daemon's companion, so it comes back with the daemon. The CLI decides
  nothing: when it makes,
  continues, or forks a bot that an `auto` gate answers and no session
  serves `auto`, it refuses with `approver_not_serving`. A gated call with no approver waits, parked and visible in
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

`auto` is a tag like any other: the daemon serves its calls to whichever
session holds its lease (`serve_approvals`) and takes that session's
answers, and the CLI refuses an `auto` bot when no session serves it
(`approver_not_serving`). What decides is the approver's own business.
Sonny's approver for `auto` is a model, its reviewer: its design, its
measurements and what was built are in
[sonny/REVIEWER.md](../sonny/REVIEWER.md), moved there on 2026-11-08 with
measurements 2, 4 and 5 below and its part of what was built.

## Performance

| Path | Added per gated call | Extra commits | Holds a task |
| --- | ---: | ---: | --- |
| Tool not in `approve` | nothing measurable (one list lookup, as today) | 0 | as today |
| Jev answers within the hold (every round with a gated call) | 0.26 s median, 0.52 to 0.57 s p99 (labeled run, ten questions), plus one socket round trip, 0.13 ms median, and one storage-worker job with no commit | 0 | yes, up to the hold |
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

**Tool mix in the Harbor trials (measured).** This measured the earlier
draft, with rules in front of Jev; it shows what rules would have saved.
The suggested gate and rules were applied, without model calls, to every tool call in the stored
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
  decide.
- Across every stored trial (89 ChatGPT-plan and 34 Sonnet trials, the
  earlier runs included) the shares were 62% and 75%.

So even with rules Jev would have reviewed most rounds of coding work;
without them (2026-09-27) it reviews every round with a gated call, which
in these trials is every round with a call but those whose only calls were
the 7 ungated ChatGPT ones. That is affordable in time because a round's
model call and its tools take several seconds (a median trial ran 92 s for
ChatGPT and 354 s for Sonnet): at 0.26 s a round, about 1 to 3% of a
median trial by the rounds above. It is what sets the fleet cap above.

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
  multi-step chain in which each step looks benign passes any per-call approver
  that cannot see results; that is Rehberger's attack on Claude Code.
- **An unsandboxed bot can reach the approval channel.** Its shell can
  call `answer` on its own pending calls, create a bot without the gate,
  hand its task to an existing bot that has none, submit a message that
  looks like its caller's consent, or edit the store. So can a call
  allowed to escalate out of a sandbox, which runs unconfined.
  The model may catch the obvious forms; a determined command can hide
  them from any text-based approver. Making this a boundary means running the
  tools where the socket and the store are out of reach. A sandboxed bot
  ([sandbox](RUST_PROTOTYPE.md#sandbox)) has that: its commands reach the
  daemon only through its own socket, which refuses `answer`, `configure`
  and the other requests that would undo its confinement, makes bots that
  inherit its sandbox and its gates, submits only as itself, and cannot
  write the store. The roadmap still leaves process isolation beyond
  that, a container or VM, to the host (NEXT.md).
- **The agent's home is the person's call, never the reviewer's** (2026-11-01).
  `--protect-path ~/.agent` keeps it read-only inside every sandbox, but an
  escalated command runs unconfined, and the Monday rehearsal had Sonny's reviewer
  allow one that wrote `~/.agent/triggers/jobs/x`, a job the scheduler runs
  as the person: the policy called triggers under `~/.agent` ordinary work,
  from before agents had the `triggers` tool. The policy now rates the
  scheduler's jobs, the command rules and Sonny's scripts there as the
  approval channel (`critical`), and changing `config.toml` or `env`
  directly as persistent weakening. And Sonny's reviewer never allows a
  call that reaches `~/.agent` outside the folders the agent was given
  there (a task's worktree, the zen folder) on the reviewer model's word: a write,
  edit or patch by the path it resolves to, a command that would run
  outside the sandbox by any path word holding `.agent` (`~`, `$HOME` and
  `${HOME}` read as the home). What the reviewer would allow waits for the
  person, as a call too long to show does; a denial stays one. What sits
  in `~/.agent/worktrees` beside the agent's own worktree (a note, another
  task's worktree) is agents' work, not the person's settings: the reviewer
  weighs it as it weighs a folder elsewhere, so a note beside the
  worktree no longer waits for the person. The folder itself and Sonny's
  records in it (`.folders`) still do. Codex's
  guardian may approve writes to `~/.codex`; Claude Code routes writes to
  its own paths to its classifier, except under `--restricted`, where only
  the person may allow them, which is what this follows. The command check
  reads words, so one that spells the name another way is left to the
  reviewer, and a command an allow rule lets out is not reviewed at all, as in
  Codex.
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
2. Moved to [sonny/REVIEWER.md](../sonny/REVIEWER.md#measured): a
   model approver on a labeled set.
3. **The daemon path.** On the lifecycle screen with `approve` set and a
   rules-only approver, confirm zero added commits per call and under 1 ms
   added per gated call, path resolution included, and record the bytes
   each gated call adds to the store and WAL and to the approver's
   socket, against the same screen without `approve`; then the park path
   with a delayed answer, counting its commits, and a verdict that
   arrives while the turn is parked on an earlier `wait`.

   Done in part 2026-09-26 with
   [`bench.approval_overhead`](../bench/approval_overhead.py), before the
   rules approver exists, so with no path resolution: the approver is the
   screen itself, answering `allow` as soon as a call is announced
   (`held`) or, with an `approval_hold_ms` of 0, after the turn parks
   (`parked`). Each turn is one synthetic `shell` call (`true`) and a
   reply, on a fresh store, against the same turn ungated and against
   main at `ddf3f8b`, with within-turn compaction; the build measured is
   `973be14`, and the runs record both binaries' SHA-256. Medians of three
   rotated runs of 200 sequential turns on one bot, on a 4-CPU Linux
   x86_64 container (fsyncs counted in a separate pass under strace,
   setup included):

   | | main | ungated | held | parked |
   |---|---:|---:|---:|---:|
   | Turn latency p50 / p99, ms | 13.4 / 21.0 | 13.3 / 22.0 | 14.0 / 25.6 | 16.0 / 26.4 |
   | Daemon CPU per turn, ms | 8.6 | 8.7 | 9.4 | 10.9 |
   | Storage worker time per turn, ms | 7.7 | 7.7 | 8.3 | 10.3 |
   | Store commits per turn | 11 | 11 | 12 | 17 |
   | fsyncs per turn | 6.24 | 6.24 | 6.27 | 9.32 |
   | Event bytes per turn | 1,071 | 1,071 | 1,331 | 1,539 |
   | Store bytes per turn | 2,990 | 3,072 | 3,359 | 3,543 |
   | Answer request and reply bytes per call | | | 240 | 240 |

   - Ungated bots pay nothing: the same jobs, commits, and fsyncs as main,
     and CPU, storage time, and latency within run-to-run spread (p50
     12.6 to 13.6 ms in both). Six rotated runs of 32 bots at 100 turns
     each agree: 266 turns/s against main's 260 (runs 255 to 287 against
     257 to 274), 4.9 ms CPU per turn in both. The store's 82 bytes per
     turn are the four empty b-trees of `approvals`, `approval_tags`, and
     their indexes, 16 KiB whatever the turn count.
   - A held call adds no durable commit. Its one added commit is the
     answer's group, which writes nothing; the 0.03 fsyncs per turn are
     WAL checkpoints that come sooner with the added event bytes. It adds
     about 0.7 ms of daemon CPU and 0.5 ms of storage worker time per call
     (the announcement, `approval_start` in place of `tool_start`, and
     the answer), and about 0.7 ms at the median, the screen's own
     answering round trip included: the daemon's announce-to-start
     `waited_ms` is 1 to 2 ms.
   - A parked call adds three durable commits (the park, the verdict, the
     resume) and about 2.6 ms, the screen's reaction to `turn_waiting`
     included.
   - With 32 bots at 25 turns each, where commits group, the one
     single-threaded screen answering every bot becomes the wait
     (`waited_ms` median 37 ms held, 64 parked): 391 turns/s ungated
     against main's 381, 292 held, 192 parked. The answers arrive spread
     out, so fewer jobs share a group: 1.1 more commits and 0.5 more
     fsyncs per turn held, 2.3 and 2.0 parked, and 0.9 ms more daemon CPU
     per turn held.
   - An earlier run at `7238c6c`, against main at `7e46c5f`, before the
     review fixes and the merges with admission batching and within-turn
     compaction, found the same: ungated equal to main, a held call no
     durable commit, a parked one three.

   Serving a tag adds nothing measurable (2026-09-27). The plan commit and
   a re-announcement now also build each call's served entry and hand it
   to the publisher, whether or not a session serves the tag. Same method,
   `ea82f7a` against main at `0b295d2`, one bot, 200 turns, three rotated
   runs, `ungated` and `held` only, same container: held p50 15.9 against
   15.7 ms (runs 15.3 to 16.4 against 15.4 to 19.0), daemon CPU 10.7
   against 10.5 ms a turn, the `append` job 0.61 against 0.57 ms, and the
   same 12 commits, 1,331 event bytes, and 3,379 store bytes a turn;
   ungated p50 14.0 against 14.4 ms. The differences are inside the
   run-to-run spread.

   A macOS arm64 check on 2026-09-27 compared main `17e6fb6` with
   the direct-main consolidation of `3d725b4`: one bot, 200 synthetic
   shell/reply turns per arm, three rotated runs, identical release
   profiles and fresh stores. Median run p50 was 45.73 to 45.57 ms
   ungated and 45.44 to 45.93 ms held; p99 was 61.89 to 61.60 ms and
   67.48 to 64.91 ms respectively. Daemon CPU was 7.679 to 7.804 ms
   per ungated turn and 7.911 to 8.013 ms per held turn, with overlapping
   run ranges. This small screen shows no clear regression or speedup;
   it does not establish fleet capacity or served-socket overhead. Raw
   synthetic records and binary hashes: ignored
   `.local/validation/lease-main/result.json`.

   Still to measure: the same with the automatic approver answering over
   a socket rather than stdio, and a verdict that arrives while the turn
   is parked on an earlier `wait` (covered by a behavior test, not
   timed).
4. and 5. Moved to [sonny/REVIEWER.md](../sonny/REVIEWER.md#measured):
   approver models compared.

## Built so far

Built on 2026-09-26, documented in [RUST_PROTOTYPE.md](RUST_PROTOTYPE.md#tool-approval):

- The daemon mechanism: gates on `create` and `fork` (`approve`, `approver`,
  and `approve_expire_ms` for a gate's expiry), their inheritance, the
  `approval_requested` event in the plan commit, `answer`, `approvals`,
  verdicts on `tool_started` and as a denial result, the hold and the park,
  expiry, and a new request for the rest of a round after a failure.
  Ungated bots skip all of it.
- Manual mode in the CLI: `--approval manual` or `AGENT_APPROVAL`,
  `--approve`, `agent approvals`, `agent answer`, and the pending call
  with its answering command in `run --pretty`. The CLI answers with
  `by: "cli"`.
- The daemon's cost, measured without an approver of its own
  ([Measure](#measure-before-building), item 3).

Built on 2026-09-27, for the automatic approver:

- `serve_approvals`, `renew_approvals`, and `lease` on `answer`: one
  session holds a tag under a lease, gets the calls waiting on it with
  the first page and each call announced for it after, and loses the tag
  with `approvals_lost` when another session takes over a lease that ran
  out. `stats` lists the served tags in `approvers`.
- The `denials` counts, per bot and tag, kept on the bot's row and sent
  with every listed and served call.

Where it differs from the design above:

- A verdict that lands as the hold runs out and decides the call is taken
  at once: the turn does not park. One that leaves a gate open is written
  with the park.
- `stats` reports `approval_requests`, the calls announced and not yet
  started or denied, rather than counting pending gates.
- A verdict committed for a parked turn just before a crash is picked up
  at the next start, which checks every turn parked on a verdict once.
- An answer that arrives after its gate's expiry is refused with
  `approval_expired`, even when the turn has not yet denied the call, so a
  late allow cannot outrun the expiry.
- A bot carries at most 8 gates; a `create` or `fork` past that fails with
  `gate_limit`.
- `approvals` previews each top-level argument field on its own (2,048
  characters, at most 8 fields), taken when the call is planned, rather
  than the first 2,048 characters of the node's arguments; an Anthropic
  `write` puts `content` before `path`, which a prefix would hide.
- A `reason` goes only with a deny; an allow with one is refused with
  `invalid_reason`.
- Each `approvals` entry carries `allow`, and a denied or expired call's
  `tool_completed` lists every verdict it got, so replay keeps an allow that
  came before the deciding deny.
- A round's announcement is split into several `approval_requested`
  events when its calls take more than 256 KiB, so every event pages.
- A turn parked on a `wait` or on a verdict ahead of another gated call
  wakes and ends when that call lapses, rather than when the wait returns
  or the verdict comes, and no call starts once a later one lapsed.

- A served call is the whole listing entry (arguments, open gates,
  expiry, and counts). Each tag's holder gets only the calls waiting on its
  tag, in messages of up to 256 KiB of calls numbered `part` of `parts`,
  so it decides a whole round in one request.
- A holder whose lease ran out keeps its tag until any session next serves
  a tag or the holder renews or answers; the daemon keeps no timer for
  leases. Either way the holder is sent `approvals_lost`. The lease period
  starts when the serve reply is queued, and pushes follow the reply. An
  accepted leased answer holds the lease through storage and restarts its
  period when its reply is queued, including a failed answer. Calls waiting
  for output room are discarded when their lease ends; already queued output may arrive.
- A lapsed gate's denial is not counted in `denials`: nobody decided the
  call.

The automatic approver, built on 2026-09-27 as `agent approver` and now
Sonny's reviewer, is in [sonny/REVIEWER.md](../sonny/REVIEWER.md#built-so-far).

## Open decisions

Settled by George on 2026-10-26, after a day on Sonny: `auto` allows
almost everything and stops only what is likely dangerous and plainly
unasked; otherwise he runs at full access. A person's `/approvals`
override replaces any bot's gates, including one another bot made; in
Sonny it reaches every agent the lead made, and those they made.

Settled by George on 2026-09-27: `auto` is a model deciding every gated
call, with no rules; a rules-only approver is manual mode, and rules for
manual mode are a later option, not a priority.

Settled by George on 2026-09-26: three modes, `full` stays the default,
`AGENT_APPROVAL` picks the mode, and `auto` denies what is dangerous or
unclear without asking anyone. Installing a dependency the task needs
but does not name counts as unconsented, so `auto` denies it (2% of real
Harbor calls); what that costs in pass rate is measured once `auto`
runs. Settled by this design: the daemon enforces gate inheritance, so a
bot cannot drop a gate by going around the CLI.

- The hold before parking: 2 s covers a Jev verdict with margin; shorter
  frees slots sooner for people.
