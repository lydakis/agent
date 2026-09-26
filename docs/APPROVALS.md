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
   and no timeout of its own.
3. **Manual and automatic are clients.** The mode names live in the CLI and
   the app; the daemon sees a list of gated tools and an opaque tag naming
   which approver answers.
4. **Auto is hands-off.** Deterministic rules in the client answer the
   obvious cases in microseconds, and Jev answers a handful of narrow
   questions about the rest in about 0.4 s. What is dangerous or unclear is
   denied, never silently allowed and never sent to a person. The model
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
  stores, reports in `resume`, `bots`, and `approval_requested`, and copies
  with the list, but never interprets. Clients use it to decide who answers
  (see the modes above).
- **A fork copies it and can only add to it,** the mirror of the allowed
  list, which a fork can only narrow. Tool definitions never change, so the
  prompt cache is untouched.
- **Bots created from inside a gated bot inherit its list.** When `create`
  names a creator that has an `approve` list, the new bot's list is the
  union of both, intersected with the new bot's own tools, so it stays a
  subset of them. A read-only child of a bot gated on `shell,write,edit`
  gets nothing to approve, because it has none of those tools. A fork keeps
  its source's tools, so its list stays a subset even when the fork's
  allowed list is narrower; an entry for a tool the fork may not call never
  comes up. This is a guard against accidents, not a boundary: the
  creator is declared by the shell's environment, and a command that clears
  it creates an ungated bot. The approver sees that command first.
- **The request rides the plan commit.** When `append` records a model
  response whose calls include gated tools, the same transaction marks
  those `tools` rows as needing a verdict and writes one
  `approval_requested` event for the round:
  `{"calls":[{"call_id","name","node","arguments","arguments_truncated"}]}`.
  Each call names its own node: an Anthropic round keeps all its calls in
  one assistant item, but a Responses round stores each `function_call` as
  an item of its own. Arguments are previewed to 2,048 characters, as
  `tool_started` already does; an approver reads a longer one (a large
  `write`) with `item` on that call's node.
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
- **A verdict is for the round as planned.** When a call ends denied, with
  an error, or as a shell command that exits nonzero, every later gated
  call in the round whose verdict arrived before that happened loses it and
  is announced again with the failed call named. The approver judged
  `sh run.sh` expecting the planned `write`; if the write was refused, the
  file it would run is not the one it saw. The new request rides the failed
  call's own finishing commit, and a rules-only approver answers it again
  in microseconds. The success path pays nothing.
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
- **Verdicts are durable no later than the call starts, and the storage
  worker holds them until then.** `answer` is a job on the storage worker,
  the thread that already runs every commit. The worker keeps unwritten
  verdicts in a map of its own, not in the turn's task. For a live turn,
  the job records the verdict there and wakes the task, with no commit. The
  next commit the turn makes anyway takes it out and writes it: that
  call's start, its denial, or a park. For a parked turn, the job commits
  the verdict and resumes the turn. Because `answer`, `suspend`, and
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

The daemon never judges a call, never writes text for the model beyond the
error code, and never times out a verdict. A client that wants a deadline
denies on its own clock.

### The three modes

| Mode | Gated tools | Who answers | Default |
| --- | --- | --- | --- |
| `full` | none | nobody; every allowed call runs | yes |
| `auto` | `shell,write,edit,read` | the automatic approver | |
| `manual` | `shell,write,edit,read` | a person, through the app or `agent answer`, or a program | |

- **Chosen per bot at creation.** `agent run --new --approval auto`, or
  `AGENT_APPROVAL=auto` in the environment, as `AGENT_MODEL` sets a model.
  `--approve LIST` changes which tools are gated. A child or fork gets its
  creator's mode, as the daemon section says.
- **The daemon sees no modes.** The CLI turns a mode into two `create`
  fields: the `approve` list, and an `approver` tag the daemon stores and
  reports but never reads (`auto` or `manual`). The automatic approver
  answers only bots tagged `auto`; the app offers Allow and Deny only on
  bots tagged `manual`. So one daemon can run bots in all three modes at
  once, and a program can invent its own tag and answer those bots itself.
- **`auto` needs its approver running.** The CLI already starts the daemon
  when it is not running; with `auto` it starts `agent approver` the same
  way. A gated call with no approver waits, parked and visible in
  `approvals`, rather than running or failing silently.
- **Software callers keep `full`.** A program that drives bots gains nothing
  from a gate it has to answer itself, and `full` pays nothing.

### Manual mode

- **CLI:** `agent approvals [--bot NAME]` lists pending calls;
  `agent answer --bot NAME --turn N --call ID allow|deny [--reason TEXT]`
  decides one. `follow --pretty` shows a pending call and who later decided
  it. `agent answer` refuses to run inside a tool shell (it sees
  `AGENT_BOT`), the same kind of guard as the creator identity: it stops
  accidents, not a determined command.
- **App:** on a bot tagged `manual`, a pending call renders as a card on
  the bot's timeline with Allow and Deny (with an optional note). The app already follows every bot, so it
  gets `approval_requested` with no new subscription.
- **Program:** any process that follows events can answer. A CI job can
  allow a fixed set of commands and deny the rest with no model at all.

### Automatic mode

The approver is a client program with its policy as data, like the CLI's
default compaction text: `agent approver`, a long-running command, and the
same module inside the app. It follows `approval_requested` on every bot
(`follow --all`) and answers in layers:

1. **Daemon gate.** Only tools in a bot's `approve` list reach it. The
   suggested list is `shell,write,edit,read`. `read` is gated because a
   result is sent to the model provider in the next request: reading
   `~/.ssh/id_ed25519` sends the key off the machine with no further call.
   `history`, `wait`, `note`, and `echo` touch only the bot's own store
   records and stay ungated.
2. **Deterministic rules, microseconds.**
   - `write` and `edit` inside the turn's workspace are allowed. The path is
     resolved before judging, and a symlink that leaves the workspace does
     not count as inside. Files a later command will execute are the
     exception and go to the model: `.git/`, `.agent/`, `AGENTS.md`, build
     and hook files (`Makefile`, `package.json`, `.envrc`, CI workflows).
     An independent test of Claude Code's auto mode found 36.8% of
     state-changing actions went around its classifier as in-project file
     edits (Ji et al., 2026). The approver also remembers what each bot
     wrote this turn, so a command that runs one of those files is judged
     with the file's content in view.
   - `read` inside the workspace is allowed, except files that look like
     secrets (`.env*`, `*.pem`, `*.key`, `id_*`, anything under `.ssh/`,
     `.aws/`, `.gnupg/`, or named `credentials`). Those, and reads outside
     the workspace, go to the model.
   - A shell command is split the way Codex splits it: only plain words
     joined by `&&`, `||`, `;`, or `|`. It is allowed only if every part is
     a command on the read-only list, every flag it uses is on that
     command's own list of allowed flags, and every path it names is a
     readable path by the rule above. The lists name what is allowed, not
     what is not: `find` may take `-name` and `-type` but not `-exec`,
     `-delete`, `-fprint`, `-fprintf`, or `-fls`, simply because they are
     not listed; the same goes for `git diff --output`, `rg --pre`, and
     `sort -o`. An unlisted flag, a redirect, `$(`, a backtick, or a
     variable makes the command opaque, and it goes on to the model. This
     is where Cursor's allowlist was bypassed; the approver does not try to
     parse more.
   - Commands that reach the approval channel itself (`agent answer`, the
     daemon's socket other than through `agent`, the store file) are denied
     outright.
   - Delegation goes to the model with the task as the action. `agent run
     --bot NAME` hands work to an existing bot that may have no gate, so a
     task that asks for something this bot could not do unreviewed is
     judged as if this bot did it.
   - Rules the user adds ("allow `cargo test`", "deny `git push`") are
     checked here. "Always allow" from the manual UI adds a rule here.
3. **Jev, about 0.4 s.** One request per round: the state is the context
   below plus the round's calls, and each call gets a few `noul` (true or
   false) questions, each naming one judgment and returning a probability:
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
   for it. Everything else goes to step 4. The starting thresholds are
   TypeSafe's guardrail cookbook's: below 0.35 is low, 0.70 or more is
   high. TypeSafe publishes no calibration figures; a third-party benchmark
   (jev-bench, 12 tasks, 2026-09-20) found answers reliable at 0.9 and
   task-dependent below it. So the thresholds are set from our own labeled
   run (below), per question.
4. **Unclear or dangerous: deny, and let consent come through messages.**
   The approver denies with a reason and moves on; it never pages a
   person. The reason tells the model what was refused and that it can ask
   its caller for an explicit go-ahead. If the caller's next message (a new
   turn or a steer) names the action, the intent question now finds it in
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
   turn, the approver denies every call of that turn that the rules do not
   allow outright, with a reason asking the model to stop and report what it
   needs. The turn ends with that report instead of more attempts. Claude
   Code uses 3 and 20 per session and then asks the person; Codex uses 3 in
   a row or 10 of the last 50 and interrupts. A model that keeps trying
   variations of a denied action is the pattern this catches.

**What Jev is shown.** The human prompts, the round's planned calls, the
calls already allowed in this turn, the workspace path, and the user's
environment note (trusted remotes and hosts). Not tool results, not the
model's prose, and not the model's reasoning. Jev's own limits page says
content written to steer it "can move the answer", and a planned call is
model-written text too: a command can carry a comment claiming the user
approved it. So the intent question is asked of the human prompts, and a
high risk that the prompts do not name is denied whatever the command
says about itself. For a bot created by another bot, the prompt was
written by a model; the approver follows `created_by` to the first bot a
person prompted and shows that prompt as the user's intent, the child's
prompt as a model-written task.

**The intent view is bounded.** Jev takes at most 32k tokens of state, and
a long-lived bot's prompts outgrow that. The approver sends, in this
order: the environment note, the root person's prompt for this turn, the
current turn's prompt and its steers, the planned and already allowed
calls, and then earlier prompts newest first until a 16k-token budget is
spent. If the first four alone do not fit, it does not call Jev; the call
is unclear (step 4), with the reason "not reviewed: intent too long".
Standing limits ("never push to main") belong in the environment note,
which is always sent, not in an old prompt that may fall out of the view.
Claude Code has the same gap: its docs say a boundary stated in the
conversation can be lost to compaction. Anthropic reports that most of
its classifier's misses come from misjudging consent, and a delegation
chain is where consent is easiest to fake.

**Where the key lives.** The approver process holds the Jev key, so it is
not in the environment the bots' shells inherit. That is all it gets. A
tool running as the same OS user can still read it: on Linux through
`/proc/PID/environ` of the approver, or from wherever the key is stored.
The daemon's provider keys are exposed the same way today. Protecting
either from tools needs a separate OS user or a credential broker, which
this design does not provide.

**Jev's limits bound a fleet.** The published limits for `jev-1.13.0` are
1,200 requests a minute and 250k tokens a second, "adjusting dynamically",
with 64k tokens per request of which 32k can be state. The sustained live
fleet ran 60 model calls a second through one daemon; if every round there
needed Jev, that is three times the request limit. Hence one request per
round rather than per call, the rules layer in front, and a pace in the
approver that backs off on 429 and 529 as TypeSafe's docs ask. A call
waiting on a paced Jev request stays pending and, past the hold, parks
like any other.

## Performance

| Path | Added per gated call | Extra commits | Holds a task |
| --- | ---: | ---: | --- |
| Tool not in `approve` | nothing measurable (one list lookup, as today) | 0 | as today |
| Rules answer | one socket round trip, 0.13 ms median, 0.3 ms p99 (measured), plus one storage-worker job with no commit | 0 | yes, briefly |
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
- **Jev in context.** The live fleet check measured 1.9 s median for a
  short turn on gpt-5.6-luna, so a model round that also needs Jev is
  roughly 20% slower. A
  Terminal-Bench schemelike trial made 36 to 53 model calls
  (2026-09-26 rerun); if every round needed Jev that is 14 to 23 s more per
  trial. How many rounds reach Jev is the number that decides whether this
  is acceptable, and it is not measured yet (below).
- **Cost.** Jev bills input only, at $0.042 per million tokens (TypeSafe's
  models page, read 2026-09-26). The probe's requests were 420 to 1,970
  input tokens, so $0.00002 to $0.00008 each. A trial of 50 checks costs
  well under a cent. For comparison, the same 2,000 tokens cost $0.002 on
  Haiku 4.5 before any output.
- **A person's wait.** Parking and recording the verdict are two commits,
  about 11 ms on an idle Mac; the call's start after resuming is a commit it
  makes anyway. A parked turn then costs a store row and a registry entry
  ([parked turns](DAEMON_MEASUREMENTS.md#parked-turns)). A wait longer
  than the prompt-cache TTL (5 minutes by default, `--cache-ttl 1h`) means
  the next model call writes the cache again. Keeping it warm through a
  parked wait is the same open question as parked `wait` turns (NEXT.md
  item 42).

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
  an existing bot that has none, or edit the store.
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

Settled by George on 2026-09-26: three modes, `full` stays the default,
`AGENT_APPROVAL` picks the mode, and `auto` denies what is dangerous or
unclear without asking anyone.

- The hold before parking: 2 s covers a Jev verdict with margin; shorter
  frees slots sooner for people.
- Whether bots created from a gated bot inherit its list in the daemon, or
  only by the CLI passing it on.
- Whether auto sends the unclear band to a larger model before denying, or
  denies straight away (proposed: straight away, and measure how often the
  band is hit).
- Where the automatic approver runs: the app, `agent approver`, or both
  (proposed: both, one module).
