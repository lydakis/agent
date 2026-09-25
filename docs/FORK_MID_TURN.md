# Forking a running bot, and fork versus a fresh bot

Status: design note, 2026-09-25. Nothing here is built. Code facts are from
lydakis/agent at 02e79eb. Anthropic cache behavior is from its
[prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
page, whose cache invalidation table was read live on 2026-09-25.

## The question

A bot that wants help, or an app user who wants a side chat with a busy bot,
has two ways to get a second agent:

- A **fork**, which starts from the source's history.
- A **fresh bot**, which starts from a short brief.

George wants both available, with the model choosing between them. The fork
has to work while the source's turn is still running.

## What happens today

A live test on George's Mac (Sonnet 5, 2026-09-25) failed in both ways a
model tries:

- **Forking itself mid-turn is refused.** `fork` without `--checkpoint` on a
  bot with a running turn returns a bare `bot_busy` (src/store/db.rs:2904).
  A parked `wait` turn is refused the same way.
- **The live head can't be forked either.** While a tool runs, the head is
  the assistant item holding the open call, so forking there fails with
  `fork_point_has_open_tool_calls`.
- **The model can't find a good checkpoint.** Node ids never appear in the
  model's context, and the preamble says `--checkpoint N` without saying how
  to find N. The model tried `--checkpoint 1`, which is the store's first node:
  here, its own task prompt. The fork's history was that prompt alone, so its
  first turn re-ran the whole task (13 calls, 62k tokens, 90% cached).
- **The `bot_busy` hint's fork is taken before the running turn.** It offers
  the node before the turn's prompt, so the fork knows nothing about the
  current task. On a bot's first turn it offers no fork at all.

What already works:

- **History is durable round by round.** Each model call's items commit
  before any tool runs, and each tool result commits as it finishes. So the
  newest fully answered round of a running turn is durable, and a fork there
  passes validation (tests/store_contract.rs:1428).
- **A fork keeps its source's cache key and tools.** It inherits the key
  when its instructions match (db.rs:2943), and a fork cannot change its
  tools. Its window start is not kept yet (see section 1), so its first
  request can still begin somewhere other than the source's.

## Fork or fresh: the rule

Add one line to the preamble, and let the model pick:

> Fork when the helper needs what you have learned, or should try another
> way from where you are. Start a new agent when the task fits a short brief
> or needs fresh eyes, such as a review.

Costs behind the rule:

- **A fork starts warm and large.** Its first call carries the source's
  context window, mostly from cache. That window is bounded by
  `--context-bytes` and `--context-items`, and compaction, not by the whole
  lineage. It is cheap per token, but every later call carries a window that
  size too. Warm holds for the forks this note designs: the source's
  instructions, the default fork point, and the model that warmed the
  cache. A fork given its own `--instructions` gets its own cache key
  (db.rs:2919-2949), and one at an older `--checkpoint` may start from a
  different window, so both can start cold. A fork also copies the bot's
  stored model, not a per-turn `--model` the running turn uses
  (`turns.model`). A caller that ran the source's turn on another model
  passes the same `--model` to the fork's first turn, or starts cold. The
  cache run measures these cases separately.
- **A fresh bot starts cold and small.** It pays for the brief only, and it
  doesn't share the source's blind spots. That is why reviews should start
  fresh.
- **Racing is the costly failure.** Only two of 15 Terminal-Bench trials
  used a helper, both on GPT-6 Sol in the run at 9ed0f00 (2026-09-24),
  whose transcripts were read on 2026-09-25. A read-only scout helped and
  used 13% of its trial's tokens. A duplicate "racer" that redid the
  parent's work was discarded after using 42% of its trial's. These are
  exploratory observations from two trials, not a matched measurement. A
  fork makes racing easier, which is one reason for the allowed-tools list
  below.

## Design

The daemon and CLI stay unopinionated (George, 2026-09-25). They provide
mechanisms, and the caller decides what a fork is told and what it may touch.
The harness adds no text at fork time. A fork is not told it is a fork unless
its caller says so in the message it sends.

### 1. Fork at the newest finished round

When `fork` gets no `--checkpoint` and the source's turn is running or
parked, fork at the newest closed node instead of refusing. That is the
newest node in the head's lineage with no unanswered call behind it and no
reasoning item left without its response: the last result of the newest
fully answered round, a steer message absorbed at a round boundary, or a
final answer not yet finished. When no round has finished yet, it is the
turn's prompt.

- **Cost: one column read, no transcript read.** Today `validate_fork_point`
  (db.rs:2827) walks back until it reaches a `checkpoints` row, and only a
  finished turn writes one. On a running turn it would parse every item
  since the turn began: up to `MAX_ROUNDS` (200) rounds, each with as many
  results as the model asked for, each as large as its tool allows, all on
  the store's serialized worker. Stopping at the round's start would still
  parse one whole round. So the store keeps the newest closed node of each
  running turn in a nullable `bots.closed` column, and a default fork reads
  it instead of scanning. Each write rides a transaction that already
  exists:
  - A turn sets it to its prompt, and `open_calls` (below) to zero, when it
    starts. The prompt is closed because `finish` answers every open call
    (db.rs:2488), so a turn always starts on a closed head.
  - Appending a model response with calls sets it to the head before that
    response, since the request that produced it had to answer every
    earlier call. In a turn's normal flow it is already there. The same
    write sets a new nullable `turns.open_calls` count to the response's
    number of calls, for the same reason.
  - Appending a model response with no calls moves it to the new head,
    unless the response ends on a reasoning item, which stays unforkable
    (`fork_point_splits_reasoning`).
  - Committing a result (`tool_finish`) decrements `open_calls` on the
    turn's row. When it reaches zero, the boundary moves to that result. No
    index and no scan of `tools` rows is needed.
  - Absorbing a batch of steers (`Database::absorb`, db.rs:2188) moves it to
    the batch's last item and sets `open_calls` to zero. Absorbing happens
    only at a round boundary, and steers hold no calls.
  - Finishing the turn clears it.

  Because every write keeps the column closed, the default path needs no
  validation walk. The column only ever names a node that today's
  validator accepts as an explicit checkpoint. That validator checks the
  node itself and the calls since the last checkpoint. It accepts, for
  example, a prompt that follows a reasoning-only completion, which is
  history the source's own turn already sends. Explicit `--checkpoint`
  forks keep today's validation.
- **Turns running across the upgrade are refused, not guessed.** The
  migration only adds the two nullable columns. It builds no index and
  reads or writes no row, so opening an upgraded store costs the same
  however much history it holds. A waiting or paced turn restored at open
  has no boundary and no count until its next model response or steer
  batch writes them. Until then a default fork of it fails with
  `fork_point_unknown`, which names `--checkpoint`. Guessing the prompt
  would repeat the failure seen live, a fork that redoes the task. Finding
  the real node would take the transcript scan this design avoids. An
  upgrade test opens a store with thousands of parked turns and many
  finished ones. It checks that open reads no transcript or `tools` rows,
  and that the refusal lifts at the turn's next model response.
- **The fork keeps its source's window.** Today a fork's `context_start` is
  the carried compaction's cut, or NULL (db.rs:2978). The fork's first call
  then picks a new start at three quarters of the budget, which differs from
  the source's whenever the source's window has grown past that. The
  fork's request then begins elsewhere and misses the cache after the tools
  and system. The fork should copy the source's `context_start` when it
  carries the source's current compaction and that start is in the fork
  point's lineage. For the newest closed node the start is always in that
  lineage, because a start is a turn's first node and the fork point is at
  or after the running turn's prompt. Otherwise the fork keeps today's
  rule. The fork's own first message still counts against the budget. When
  the source's window sits so close to the budget that this message pushes
  it over, the fork's first call picks a new start, and that call misses
  the cache after the tools and system. That is the same reset the source
  takes on its own next item, so the fork only brings it forward. We accept
  it rather than keep headroom, which would move every bot's window earlier
  and cost each bot the misses it avoids today.
- **Consistency:** nodes are immutable, and the fork reads the head in one
  store operation, which the store serializes with the source's appends. The
  source can keep appending, and the fork is a clean snapshot.
- **The `forked` event** reports the node chosen.
- **`--checkpoint N` keeps its meaning,** for callers that want a specific
  point.
- **An idle source keeps today's rule:** fork at its head, validated as
  today. A head that is a reasoning-only completion is still refused with
  `fork_point_splits_reasoning` (tests/store_contract.rs:1584), and the
  caller picks an earlier point with `--checkpoint`. `finish` clears the
  boundary, so the idle path gains no state to keep correct.
- **A bot forking itself** (`fork --source "$AGENT_BOT"`) gets the round
  before the one that is running its `fork` command. Whatever the fork should
  do goes in the message its caller sends next.
- **Inherited process handles are unavailable.** A finished round can hold a
  background shell's `proc:N` result while that process still runs. Today
  `proc:N` resolves by process id store-wide, so a fork that waits on it gets
  its source's result (NEXT.md item 39 names the same gap). A bot's `wait`
  tool must resolve a `proc:N` only when the calling bot started that
  process, and answer an inherited one as unavailable, before this default
  ships. The tool path knows its caller, and each `processes` row records
  its turn, whose bot is the owner, so no new handle format is needed. The
  `wait` protocol op and `agent wait` keep resolving any `proc:N`, as
  RUST_PROTOTYPE.md promises: their caller is a program, not a bot, and the
  handle it was given is its capability. This is not a sandbox. A bot's
  shell can reach the socket, as it can today, and `agent wait` inside a
  shell tool is already refused. The scoping only stops a fork from
  inheriting its source's results by accident. Tests cover both paths.
  Peer `turn:BOT/N` handles name another bot's turn and stay valid.
- **So is the output a process stores when it finishes.** `process_finish`
  (db.rs:2772) stores a background process's large streams under the call
  that started it. `authorize_artifact` (db.rs:3580) lets any branch whose
  history holds that call's result read them, and the call's first result
  is only `proc:N`. So a fork could read its source's later output even
  with the handle unavailable. A process's artifacts should be readable by
  the bot that started it, and by a fork only when the fork's history holds
  the `wait` result that delivered them. The store records that result's
  node when it commits, and authorization checks it against the reader's
  lineage, as it checks a tool result today.

### 2. An optional allowed-tools list

The app's side chats offer three choices: answer only, read files, and all
tools in a new worktree. Callers need a way to express them that keeps the
fork on its source's cache.

- **Tools stay shown.** Removing a tool changes the tool definitions, which
  rebuilds the whole cache on every model.
- **Answer only can't use `tool_choice: none` on Anthropic.** Per the
  prompt caching page's invalidation table, changing `tool_choice`
  invalidates the messages cache, which is the whole conversation; only the
  tools and system caches survive. The mechanism
  `allow_tool_calls: false` sends exactly that, so it doesn't fit here.
  Whether `tool_choice` breaks OpenAI's prefix cache is unknown, and should
  be measured before we rely on it.
- **So `fork` takes an optional `allow` list, checked at dispatch.** It is
  stored in a new nullable `bots.allowed` column, where NULL means the bot's
  whole `tools` set. A bot's effective list is `allowed` when set, else
  `tools`. The migration only adds the column, so every existing bot keeps
  its current access, and a migration test checks that. The existing
  dispatch check (src/server/turn.rs:1446) already refuses unlisted tools
  with `tool_not_available`; this narrows the list it checks.
- **A fork never widens.** An omitted `allow` copies the source's effective
  list, so a plain fork behaves like its source, restricted or not. A given
  `allow` must be a subset of the source's effective list, or the fork is
  refused. Widening a bot's list is a separate operation on that bot, not
  part of `fork`, and is left open below.
- **For an unrestricted source, a plain fork gets all its tools.** George
  chose this for forks a model starts, in the parent's folder unless the
  caller gives `--workspace`.
- **The wire shape keeps "inherit" and "none" apart.** In the protocol,
  an absent `allow` inherits, `[]` allows no tool, and a list of names
  allows those. `null` is refused as invalid. The CLI's `--allow LIST`
  uses the same comma syntax as `--tools`: no flag inherits, and
  `--allow ''` sends `[]`.
- **There are no named modes** in the daemon or the CLI. The app maps its
  three choices to lists, and makes the worktree itself; per
  [MULTI_DAEMON.md](MULTI_DAEMON.md), placement and file copying stay with
  callers.

### 3. Hints and preamble

- **`bot_busy` from `submit`** offers `fork --source NAME --bot NEW` with no
  checkpoint. That now means the newest finished round, including on a first
  turn.
- **The preamble** keeps an executable fork command, `"$AGENT_BIN" fork
  --source NAME --bot NEW`, without the checkpoint, and adds the
  fork-or-fresh sentence above. This is the one opinion kept. It lives in
  the replaceable policy layer (`client/src/policy.rs`), which already
  teaches delegation.

Dropped after George's review on 2026-09-25: framing text in the fork's first
message, and a CLI `fork --ask` that would have added it. The failure seen
live came from the wrong fork point, not from missing framing.

## What changes, in order

1. **Store:** keep `bots.closed` at each running turn's newest closed
   node, with `turns.open_calls`. Fork at it when no checkpoint is given,
   and report it in `forked`. Copy the source's window
   start as section 1 describes. Scope process handles, and the artifacts
   processes store, to the bot that started them. Add store contract tests
   for a running turn, a parked turn, a turn with no finished round, a fork
   while the next model call is in flight, a fork after several batches of
   steers, a fork while a reasoning-only response waits to finish, a turn
   that starts after a reasoning-only completion (the default fork picks
   what an explicit checkpoint would accept), a fork of oneself, a fork
   that waits on an inherited `proc:N`, and a fork that tries to read a
   large-output process's streams after it finishes. Test
   that a fork during a round with many large results, in a turn near
   `MAX_ROUNDS`, reads no transcript item. Add the upgrade test above.
2. **Store and daemon:** add `allow` and the nullable `bots.allowed`, with
   refusal at dispatch. Test that the fork's first request repeats the
   source's last request byte for byte up to the source's newest item:
   instructions, tools, and the window's items. Also test that a plain fork
   of a restricted bot keeps its list, that a wider `allow` is refused,
   that an absent `allow`, `[]`, and `null` inherit, allow nothing, and
   are refused, in the protocol and through `--allow`, and that bots from
   before the migration keep their tools.
3. **Client and contract:** the `bot_busy` hint and the preamble
   sentence. Update every place that says the default is the current head:
   the `--checkpoint` help (src/cli.rs:135), the `fork` usage error
   (src/client.rs:851), the protocol's `Fork` docs (src/server/mod.rs:67),
   and the fork paragraph in RUST_PROTOTYPE.md (lines 880-887). Each should
   say it is the head for an idle source and the newest closed node for a
   running or parked one. Protocol tests assert both defaults.
4. **App:** side chat forks with its chosen list, in a worktree it makes for
   all tools.

## Measure before keeping

- **Cache.** A fork's first call should read nearly all of the source's
  last request, which is its bounded context window, on Sonnet 5 and on
  ChatGPT. Include a source whose window has grown past three quarters of
  its budget, where today's reset would move the start, and one within a
  message of the budget, where the fork's own first message forces a reset.
  Count how often the second happens. Measure separately a fork with its
  own `--instructions`, one at an older `--checkpoint`, and a fork of a
  turn run with `--model`, with and without the same `--model` on the
  fork's first turn. The runs need a
  nonce per arm, because Anthropic shares its cache across an
  organization.
- **Answer only: dispatch refusal versus `tool_choice: none`.** Compare
  cached tokens and extra rounds on both providers. Keep refusal unless
  `tool_choice` turns out to be cache-safe on a provider.
- **Plain asks.** Check whether a fork sent only a plain ask keeps to it or
  carries on with its source's task. If it drifts, callers learn to say more;
  the harness still adds nothing.
- **The rule.** Run a prompt A/B on tasks with parallel parts, since
  Terminal-Bench is mostly single-threaded. Count helpers, tokens spent on
  discarded work, and wall time.

## Open

- How is a bot's allowed list widened, and by whom? Until that exists, a
  side chat kept as a task keeps its list.
