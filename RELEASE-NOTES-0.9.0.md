A feature release. Protocol 48 -> 55 and schema 78 -> 84. The first `sonny` or `agent` command after the upgrade replaces the running daemon at its next turn boundary. The store is copied and checked before it migrates.

Speed against 0.7.0 and 0.8.1, measured in the same run: the daemon uses less memory and CPU everywhere, and turn latency is unchanged. With 2,000 agents, 200 of them live, 0.9.0 has 0.8.1's speed (an 8 s burst, against 27 s on 0.7.0) at 0.7.0's memory (31 MiB, against 42 on 0.8.1).

## Sonny

**Leads and tasks**
- A task opens a tab only when asked. That happens through `start_task`'s `tab`, when an agent shows it, when you go to it from `/status` or by name, or once it needs you. `task_tabs = "every"` keeps the old behaviour (#773).
- `/status` is the hub. It has one line per agent and four states: working, needs you, idle and done. Helpers fold under their task. `sonny status` gives an agent's shell the same reading (#784).
- A reply that ends by asking you something in words needs you too. A copy of the agent with no tools says whether the reply waits on you, and for what (#784).
- A task idle for 8 hours (`task_done_check`) is asked by its lead's window whether it is done. A yes marks it done, and a no keeps the reason on its row (#790). When a lead's window opens, it checks the turns that ended while no window was open (#792).
- Each lead gets a writable state folder, `$SONNY_STATE` (~/.local/state/sonny/agents/LEAD), which it shares with its tasks (#771).
- In the clean view, everything between two things said is one row. It counts what came in and who it came from, then says what the agent did. It opens to each item's own row (#798).
- Quiet outside working hours, off unless `[sonny] hours` is set. Quiet starts 30 minutes after you step away outside your hours, or when you type `/quiet`. Nothing rings, footers say "quiet since …", and your next words end it (#778). Leads load the away skill, keep working without writing to you, and say what they held when you're back. `/config` takes your hours as a typed row (#788).

**Asks and approvals**
- An ask is named by its turn and call, and its answer is sent once (#707).
- An ask says what a yes covers, on its card and in its answer (#753). The reviewer reads the asks put to you and what a yes covers. A yes approves only what it covers, and an open ask approves nothing (#754).
- A question's card says where it stands: open, set aside, answered or withdrawn. Words you type while a question is in focus answer it. An agent closes its own question with `close_ask` (#776). `sonny ask list` shows the questions a lead's agents asked lately, and where each one stands (#787).
- A task's question shows in its lead's window, and a pick there answers it (#760).
- A turn's reply can follow a JSON Schema (`output_schema`), and the judge's verdict follows a fixed one that its provider enforces (#722). Each `judged` line says where the check's time went (#725).
- A `/permissions` pick says how far it reaches: the lead, its agents and new ones, and which busy agents switch when their turn ends (#769).
- `agent run` steers a busy agent by default, as a person's message does. Sonny never sends reject, and a call's row says what happened: started a turn, steering, queued or busy (#770).
- Beside a task's worktree is the judge's to weigh, not yours (#738). Command rules see through `timeout`, `nice`, `nohup`, `time` and `env` (#697).

**Lookout**
- A note steers once its news settles. News that waits for a busy agent to rest cuts in after its window (its digest, otherwise `lookout_rest_minutes`, 15 by default) (#768).
- A task's turn that wrote to its lead and completed is not told again. Adding or removing a subscription answers with what changed, not the whole listing (#795).
- Sources:
  - A source restarts when a file it watches changes (`watch = [...]`), says when it is down, and lists its topics (#779).
  - A source with `wants` reads the values its subscriptions filter on from `$SONNY_SOURCE_WANTS` (#791).
- Subscriptions and automations end by one rule: until, runs, or owner deleted (#781). `test` tries a subscription on its sources' last day, and `lately` counts what each one told (#789).
- If a wait never returned a task's turn, the turn is news again (#777).
- Goals are removed: the goal tool, `/goal`, and the lookout's check runner. A lead's reaction in a task is kept where the call lands (#782).

**Budgets**
- Budgets say what they count: tokens by default, or work with `budget_counts = "work"`. Work is what a request sends that is new, measured per history item so elision can't hide new material, not the history it resends.
- A spent budget ends the turn on a reply without tools: done at its budget, not failed.
- Commons wakes run uncapped, and the invite's allowance gates the next wake (#729).

**Screen**
- An idle window opens no files (#775). It:
  - keeps the terminal size from resize events;
  - builds no frame when nothing changed;
  - re-reads automations only when their folder changes;
  - removes one-off automations whose `--if` check said no, such as an answered ask's deadline.

  An idle window went from about 71 file opens a second to none; 0.8.1 had raised it.
- Every frame is shown whole, with the cursor hidden while it lands (#733), and a frame that changes nothing is not written (#758). A lead's agents and a streamed reply each cost their length (#735).
- Scrolled up, the view keeps its top row while a reply streams (#744). A light/dark switch reaches your messages at once (#703). A quote keeps its bar over blank lines (#694).
- A long message's fold row opens and folds it again (#706). `/status` folds done tasks to their heading (#723). Footer counts are marks and numbers only (#727). Zen's focus uses the row's whole width (#728).
- A new question beats once, and a working star breathes wider (#740). A `/usage` row's share stays on its row (#736). A finished turn ends on its words alone (#721).
- A link is written again only when it changed (#739). A `file://` link opens through the same path checks (#695).

**Composer**
- Ctrl+R opens over the composer and leaves the draft in it (#702). Ctrl+X boosts the next message, as Alt+T does (#714). A steer waiting to go in says what it waits for (#713). A dropped screenshot attaches as a picture, not its path (#696).
- `/side` opens a fork with its agent's tools by default (#748).

**Sky and margin**
- The haiku reads as text, the log keeps its margin, and models colour names (#710). There is no daily haiku limit by default, and haikus are kept as long as you say (#701).
- The sky opens whole (#731), lays out every star in one pass (#730), and names the lookout's notes for what they bring (#712).
- `margin_notes` sets the margin's cap, and the right gutter matches the left (#742). A margin holds 6 notes of 12 lines. An agent past either limit is refused with the limit and what to do (#761).
- Diagrams write their style variables only as long as the style (#737).

**Commons**
- A picked title is reversed on every row it wraps to (#745). `/commons` counts the week's golden tickets (#746).

**Automations and memory**
- `--start --fresh` gives each run its own agent. The nightly memory cleanup runs fresh (#715) and is on by default (#724). A caller that carries on an open cleanup joins its owners (#763). A refused cleanup that was cut short is carried on, keeping others' saves (#777).
- An agent looks at its own automations without a review. `list` and `show` hold only the automations it has a part in, while `add`, `remove` and `run` go to its gate one step at a time (#734).
- An automation `if` that an agent wrote runs in that agent's sandbox. Yours and Sonny's own run as you (#783).
- `/background` tells a quiet check from one that could not check (#700). The scheduler job's label names no one (#698). The lookout says when it reads a changed subscription file (#743).
- A memory save missing its type or name says what the fact needs (#693). A save past the index head an agent starts with answers `past_head`: it is saved, and search finds it. `memory schedule` keeps the automation's time (#794).

**Companions**
- Companions do less while idle (#793):
  - The tool host saves its stream cursor at most once a second.
  - The lookout re-reads a subscription file or config.toml only when it changed.
  - An idle home backup with nothing changed reads no file and runs no git, and it is whole once a day.

**Setup and browser**
- `sonny setup` says which first-run defaults it wrote (#705). It offers to start agents' browsers signed in, and the docs say where the profile lives (#747).
- Sonny's daemon and companions run under their own names: `sonny-daemon`, `-lookout`, `-tool-host`, `-approver` and `-scheduler` (#765).
- `unknown_tool` names the tools Sonny answers now (#720).

## Daemon
- Models and providers (#772):
  - The daemon rewrites the discovered model catalog, `~/.agent/models.discovered`, every time it starts. `~/.agent/models` holds your own additions, and `-PROVIDER/MODEL` lines hide models.
  - Providers follow `~/.agent/config.toml` while the daemon runs: an edit is checked and swapped in whole, and running turns finish on the providers they started with.
  - `bedrock-openai-runtime` binds Bedrock runtime's OpenAI-compatible route.
- Spawn depth belongs to the daemon. `[daemon] depth` is 2 by default, every create and fork is checked against it, and a refusal is a typed `depth_limit` (#774).
- A client gives a bot's commands their variables on create and configure (#771). `lend` returns a bot's sandbox policy and command environment, for `agent sandbox --bot` (#783).
- Removed: the prompt-cache keep-warm (`keep_warm`, `--keep-warm`), fork-from-turn, and the per-turn task snapshots (#780).
- Restart: a client takes a call before running it, so no call runs twice. A second take is refused and answered unknown (#756).
- A rule change reaches an agent at work at its next round, descendants included (#759).
- Bounded context:
  - `[context_windows]` in config.toml sets a model's context window by provider/model pattern, and an exact path beats every wildcard. Restart the daemon to apply it (#764).
  - Compaction events say what triggered them and how much room they left. A summary that comes back too long is asked for again, smaller (#718).
- Store:
  - Readers open as reads wait for them and share one cache budget (#726).
  - A follow of some kinds reads only those (#741).
  - Each prompt keeps its opening, so listings parse no prompt item (#709).
- The connection shards share one TLS setup (#752). `stats.running_processes` counts foreground commands (#716).
- Links: a call the machine never took ends unknown in seconds, not refused after 45 s (#699). `write_stdin` says when a handle is a background command (#711). `tool_search` puts a tool named exactly first (#708).

## CLI
- Every command replaces an older daemon and resends. One that can't says why (#717). A non-zero exit says why on stderr (#704).

## Also
- A nightly Homebrew channel for Sonny.
- Tests: timing checks are taken again before they fail (#749). A broken-pipe race in the link tests is fixed (#762). Two runtime tests no longer race the daemon (#767).

## Upgrade
- The first command after upgrading replaces the running daemon. Turns park at their next boundary and resume on the new one (#717). Calls the old daemon left unanswered are marked taken, so none runs twice (#756).
- Before the schema migration the store is copied and checked, and the two newest copies are kept. If the copy fails, the store is not migrated and the daemon says `store_backup_failed` (#757).
- The first open after upgrading builds the prompt openings table and the event kinds index. That takes well under 2 s per 20,000 prompts or 300,000 events (#709, #741).
- `~/.agent/models` is now yours alone. An old one still holds Sonny's first list, so every model in it stays offered until you trim it to your own lines or delete it.
- `~/.agent/env` no longer sets the provider: an `AGENT_PROVIDER` there is passed over, and Sonny says to move it to `provider` in `~/.agent/config.toml` (#772).
- Every automation `if` that an agent added before 0.9.0 stops running until it is added again, because gates now run in the writer's sandbox. Sonny's memory cleanup and ask deadlines keep running (upgrade step `agent-gates`) (#783).
- `task_depth` leaves `[sonny]`, and spawn depth is `[daemon] depth` (#774).
- `keep_warm` is dropped from stored bots, and the per-turn task snapshot refs are removed (migration `no-snapshots`, upgrade step `snapshot-refs`) (#780).
- Goals are removed, along with the lookout's goals file (#782).
- The judge's base bot is remade on its own (#722).
- Upgrade step: a reviewer model left as `AGENT_JUDGE` in `~/.agent/env` (`judge-env`) (#719).
- Migrations that run once on first start: the scheduler job's new label (#698), haiku history and daily limit (#701), and Sonny's nightly memory cleanup scheduled again as fresh (`cleanup-fresh`) (#794).
- Upgrade steps and migrations named here retire at 0.11.0.
- Setup offers to name a signed-in browser profile where none is set (#747).
