# CLI contract

`agent` is a flat-verb CLI built for programs first: every command outputs JSON by default, uses stable exit codes, and reports structured errors with actionable hints. Add `--pretty` to any client command for a readable terminal view.

[Commands](#commands-at-a-glance) · [Output & exit codes](#output-and-status) · [Running & delivery](#running-and-delivery) · [Creating bots](#creating-bots) · [Inspecting & approvals](#inspecting-waiting-and-approvals) · [Companions](#companions) · [Daemon & environment](#connection-and-startup)

## Commands at a glance

All named bots use the same flat verbs. There is no parent/child command hierarchy.

| Group | Commands |
| --- | --- |
| **Run & converse** | `run`, `fork`, `follow`, `wait`, `interrupt` |
| **Inspect & audit** | `ls`, `inspect`, `turns`, `usage`, `audit`, `ps`, `stop`, `held`, `retry`, `models`, `stats` |
| **Approvals** | `approvals`, `answer` |
| **Lifecycle & daemon** | `rm`, `prune`, `companion`, `start`, `shutdown`, `serve`, `sandbox`, `login` |

Run `agent --help`, `agent COMMAND --help`, or `agent help COMMAND` (`-h` works too). `agent --help` gives each command in one line; a command's own help lists its flags, and `run --help` lists a new bot's settings apart, after the everyday flags. Help writes to stdout and exits `0` without opening a store or connecting to a daemon.

### General syntax rules

- Put the command first; options may appear before or after positional operands.
- Long flags accept `--flag VALUE` or `--flag=VALUE`.
- `--` ends option parsing (`agent run -- --help` sends the literal prompt `--help`). Use `--instructions=--example` for a flag-like value.
- Bot selection uses `--bot NAME` (`--source NAME --bot DESTINATION` for `fork`). `--all` selects all bots for `follow`; `--any` selects first-completion behavior for `wait`. `follow --all --bot NAME` is an error.
- Prompts and wait handles are positional operands; a prompt of `-` reads stdin.
- **Duration flags require units** (`500ms`, `30s`, `5m`, `1h`, `7d`, or bare `0`; a bare number is refused, so no flag guesses a unit): `--timeout`, `--approval-hold`, `--grace`, `--stall-timeout`, `--idle-exit`, `--attach-wait`, `--since`, `--bucket`, `--older-than`, `--hold`. `--stall-timeout` and `--idle-exit` count whole seconds. The protocol keeps its own units (`timeout_ms`, `grace_ms`, `approval_hold_ms`).
- `--after` is an exclusive event cursor for `follow` and an exclusive turn ID for `turns`. `--checkpoint` is a history node ID.
- Unknown flags, flags belonging to another command, unexpected operands, and repeated singleton flags are usage errors (exit `2`). `--provider`, `--image`, `--writable`, `--unix-socket`, `--quote`, `--shell-exclude`, and `--env` are repeatable.
- Listings read only what they print: `--limit N` stops `ls`, `turns`, and `approvals` after N entries, and every filter runs in the daemon.

## Output and status

| Command type | Default output | `--pretty` output |
| --- | --- | --- |
| `run`, `follow` | JSON Lines (one event per line) | Rendered stream (text, tool rows, diffs) |
| `ls`, `turns`, `audit` | Compact JSON array | Aligned table / one-line rows |
| Other client commands | Compact JSON object | Indented JSON (detached submission handles included) |
| `serve` | Long-running daemon (no reply) | Rejected |

- `run` follows its bot live before it submits, so a turn that answers before the submit is answered still prints its text whole, and its output is that turn's events (#613). A resent `--request-id` follows the turn it made from the turn's start.
- Every command prints its reply: `interrupt` the turn view, `answer` what it answered, and `shutdown` `{"stopped":true,"pid":N}` once the daemon is gone. `ps` prints `{processes: [...]}`: the commands still running for `--bot` (a name or a GLOB pattern) or every bot, each by its `proc:N` handle with its kind, the start of its command, when it started, and a detached command's pid. `stop --bot NAME proc:N` stops one with everything it started.
- `--pretty` is rejected on commands with no output. Rendered model text and tool output keep their line breaks and tabs; any other character a terminal would act on is printed escaped, so a stream cannot hide or restyle a call waiting for approval.
- Diagnostics and failures always go to stderr.

### Terminal program status (OSC 7501)

On a terminal, `run --pretty` and `follow --pretty` also report program status ([OSC 7501](https://www.superlogical.com/rex/docs/build/program-status)) so a terminal that supports it can mark the tab: `working` while the turn runs, `blocked` (`kind=permission`) while a call waits for a person's approval (a `manual` gate; a program's gate, like `auto`, is still `working`), then `done`, `error`, or `idle` when the turn completes, fails, or is interrupted.

- `follow --all --pretty` reports one record per bot, with the bot's name as its id, and clears a deleted bot's record. Only the bot's running turn moves it, not one queued behind.
- History replayed before the stream goes live reports only turns still running.
- Each report has `app=agent` and the bot's name as its title, and is written once per change. Nothing is written when stdout is not a terminal.

![A bot waits for approval and its tab says so](demo/program-status.gif)

*(Recorded with `run --pretty` against the offline playground, replayed in xterm.js with a small OSC 7501 handler that draws the tab's mark.)*

### Errors and exit codes

A failure prints one JSON object on stderr, the shape the daemon answers with (with `--pretty`: one line, `agent: CODE: DETAIL`, followed by `; HINT` when there is one):

```json
{"error":"bot_busy","detail":"turn 4 is running","running_turn":4,"fork_point":9,
 "hint":"resend with --delivery steer --turn 4 to add this to it, or --delivery queue to run it afterwards; ..."}
```

- `error` is the code (`usage` for command-line errors).
- `detail` states the rule that was broken. A detail that echoes what was sent is cut in its middle to about 4 KiB, so a refusal always fits in one response.
- Any other field is a fact about the current state, named as a request field (such as `running_turn`, `bot_id`, `efforts`, `deliveries`, or `field`).
- `hint` is added when the CLI can say how to get past the refusal with its own flags.
- When `run` or `start` starts a daemon that fails, the command fails with the daemon's own error, such as `invalid_provider_url`. `serve` adds its `pid` to its error line, so a launcher reports its own daemon's failure from the store's shared log even when several start at once; a daemon that exits without one is `daemon_start_failed`, naming the log.

| Exit | Meaning |
| --- | --- |
| `0` | Requested operation succeeded (including help/version) |
| `1` | Operation failed, requested result is incomplete, or `wait` timed out |
| `2` | Invalid command-line usage |
| `75` | `serve` could not acquire store/socket ownership |
| `125` | `agent sandbox` itself failed (a usage error included); otherwise it exits with its command's own status |

## Running and delivery

- `agent run -- PROMPT` creates a fresh identity.
- `agent run --bot NAME -- PROMPT` continues an existing bot; add `--new` to create `NAME`.
- `agent run --detach` prints the submission handle immediately without waiting.

### Delivery modes (`--delivery`)

Controls what `run` does when the target bot is already running a turn:

| Mode | Behavior when bot is busy |
| --- | --- |
| `steer` *(default)* | Taken into the running turn (or starts a turn if idle), as a person's message to a busy bot is. A blocking `run` exits `0` once the message is taken (`taken`, naming the turn it joined) or when the turn it started completes. Add `--turn N` to require turn `N` or fail with `stale_turn`. |
| `queue` | Queued durably behind the running turn. A blocking `run` follows the turn from its queued state to its end. Default when passing `--workspace` or `--turn-budget-tokens`, which only a turn of its own takes. |
| `attach` | Adds a message that needs no reply to the bot's next model call, mid-turn or in its next turn, without starting a turn; `run` prints the submission and returns at once. If no call has taken it in after `--attach-wait` (`30m`, `1h`, `7d`, up to `365d`; else `attach_wait` in `config.toml`, else `1d`; `0` waits as long as it takes), it goes as a steer, so a bot that stays idle still hears it. |
| `reject` | Refuses a busy bot with `bot_busy`, for a program that must start now or hear why. |

Without the flag, `AGENT_DELIVERY` applies, then `steer` (`queue` with `--workspace` or `--turn-budget-tokens`). The daemon has no default of its own: the CLI always sends the mode and attach wait it resolved, so one person's preference never changes what a program's submission means.

**Steering vs. running work:** A steer never waits behind a wait on someone and never cancels a call. A turn parked on `wait` resumes at once with its unresolved handles pending (still valid) and `steered: true`. A call waiting for its approval, or a client-answered call one of whose steps waits for approval, moves to the background and returns its `proc:` handle, waits on past the turn, and reports back as a background command does. A running call is never moved: a `shell` command or a client-answered call is answered first, or yields on its own time (a command at its `timeout_ms`, a client's call when its client yields it), as Codex lets a running tool return, and the steer joins right after. `queue` deliveries leave all of these alone. A steer with an explicit workspace or model that differs from the running turn stays queued and runs separately with those choices.

### Everyday `run` options

- **Idempotency (`--request-id ID`):** A command that lost its reply is resent unchanged. The key covers both creation and submission, so `run --new --bot NAME --request-id ID` resent gets the bot and the turn it made, with `duplicate: true`, rather than `bot_exists`. `fork --request-id ID` does the same for a fork, wherever its source has moved since, even once the source is deleted.
  - Once the bot a keyed command made is deleted, any command with that name and key is `bot_deleted` rather than a new bot.
  - The same key with any other field changed is `idempotency_conflict`, naming the field in `detail` and `field`. A name taken by any other request is still `bot_exists`.
  - `rm --bot NAME --bot-id N` deletes that identity: resent while it runs or once it is gone, it succeeds as a duplicate and leaves a later bot of the same name alone. An identity that name never held is `bot_not_found`.
- **Workspace (`--workspace DIR`):** New bots start in `DIR` (default: the directory `run` was invoked from) and keep that folder: a later `run` without the flag runs there wherever it is invoked. Passing `--workspace DIR` on a later turn runs in `DIR` and moves the bot there, a steer's too, whose running turn goes on in its own folder. A `fork` starts in its source's folder unless it names one.
- **Model (`--model`):** The daemon has no model of its own, so a new bot needs `--model`, `AGENT_MODEL`, or `model` in `~/.agent/config.toml`, in that order. Those are only creation defaults: an existing bot uses its stored model unless `--model` overrides it for this turn.
- **Effort (`--effort LEVEL`):** `low`, `medium`, `high`, or `xhigh` on either family, and `max` on Anthropic's; any other is `invalid_effort`. Without it the request carries no level and the model uses its own default. On an existing bot it sets this turn's effort, and the bot's own is unchanged. A turn with a level sees it in its shell as `AGENT_EFFORT`, and a new bot that takes its model from `AGENT_MODEL` takes its level from `AGENT_EFFORT`, so a peer started on its creator's model thinks as hard as its creator; a bot given `--model` or a profile's model gets only its own `--effort`.
- **Speed (`--speed`):** Reaches only the models that serve that tier; calls on any other model run at standard. See [context windows and speeds](#context-windows-and-speeds).
- **Budgets (`--turn-budget-tokens N`, `--budget-tokens`, `--budget-counts`):** `--turn-budget-tokens` caps this turn's input plus output tokens, beside the bot's lifetime `--budget-tokens`. Once a call reaches N, the turn's next call goes without tool calls and the turn completes on its reply with `limit: "turn_budget"`; the cap is the turn's own, so the next turn starts from zero. A steer joins a turn that already has its cap, so `steer` with the flag is `invalid_delivery`. `--budget-counts work` makes the budgets given with it count work instead: each call's output and the input it adds, not the history it sends again, so a bot with a long history can take a small allowance (`tokens`, the default, counts every call's whole input).
- **Attachments (`--image PATH`):** Sends a PNG, JPEG, GIF, or WebP with the prompt, after its words; repeat it for more (up to 20, 5 MiB together). It goes with a steer or a turn of its own alike.
- **Message replacement & quoting:**
  - `--replaces BOT/N` names a message this same sender sent to `--bot` that this prompt replaces: still in line, it is withdrawn and this prompt sent in its place in one step; taken in already, it stays and this prompt goes after it. The reply's `replaced` says which (`withdrawn` or `taken`, with the turn that took it in), and `--pretty` adds it to the first line.
  - `--quote BOT/N` copies that one message's words after this prompt, with a line saying who wrote it, even when other messages were steered or attached beside it; `BOT/N:reply` copies the reply the finished turn it opened ended with. `N` is the number the model reads after each message and Sonny shows beside its time. It repeats, up to 64, the quotes a quoted prompt brings counted. The daemon copies the words, so the model reads what was written and by whom, never the sender's retelling; a turn without those words is refused (`quote_not_found`, `quote_unfinished`, `quote_too_large`). Sonny's reviewer takes a quote of the person's words as consent only when the person sent the quoting prompt: an agent could otherwise quote an old "yes" for a new ask.
- **Creator identity in tool shells:** Inside a bot's shell, `AGENT_BOT` and `AGENT_BOT_ID` identify that bot, and the client sends both as `created_by` and `created_by_id` when creating or forking a child. The store rejects missing or stale creator identities before creating the child, including after a daemon restart or name reuse. `AGENT_PARENT` and `AGENT_PARENT_ID` identify the running bot's recorded creator when its identity was known at creation; address it with `agent run --detach --bot "$AGENT_PARENT" --bot-id "$AGENT_PARENT_ID" -- TASK`. The ID stays pinned after deletion, so a replacement with the same name rejects the call instead of receiving the work.
- **Creating for another bot (`run --new --created-by BOT`):** Makes the new bot BOT's, as a client that starts a bot for one of its agents says (Sonny's `/new`), while the prompt stays the person's. In an agent's shell, a bot is made only by that agent.

## Creating bots

The flags below configure a new bot (`run --new`) and are rejected when continuing an existing bot (instructions, tools, and token budget apply to new identities only). A `fork` takes none of them: it is an exact copy of its source, instructions, tools, and gates included.

### Instructions, policy, and profiles

- `--instructions TEXT` or `--instructions-file PATH`: Explicit system instructions (mutually exclusive). The default is the CLI's built-in text; the daemon has none.
- `--agents`: Composes the shared client policy instead: the harness preamble, every `AGENTS.md` and `.agents/AGENTS.md` from the workspace up to the root plus `~/.agents/AGENTS.md`, indexes of the skills in `.agents/skills/NAME/SKILL.md` and the profiles in `.agents/agents/ROLE.md`, and the memory indexes in `~/.agents/memory` ([client policy](CLIENT.md)). It is opt-in on the CLI, what Sonny composes for its agents, and exclusive with `--instructions`.
- `--profile ROLE`: Composes the same text with that role last, and takes the role's `model`, `tools`, and `sandbox` unless `--model`, `--tools`, or `--sandbox` names them; a missing role is `profile_not_found`. The bot keeps ROLE as its `profile`, which `inspect` and `ls` show.
- `--profile-name ROLE`: Keeps ROLE as `profile` for instructions a client composed itself (with `--instructions`), reading nothing.

### Tools and fallbacks

- `--tools LIST`: Chooses a new bot's tools from those the daemon registers. Default: `shell,write_stdin,read,write,wait,history,send_message,list_agents,pause_agents,spawn_agent,close_agent`, plus the file editor its model family is trained on (`apply_patch` for a Responses model, else `edit`), plus `web_search` where the provider offers it and `web_fetch` for any. `web_search = "disabled"` in `~/.agent/config.toml` drops both web tools; naming the model in `web_search_off` (`PROVIDER/MODEL` patterns with `*` and `?`) leaves search out and keeps `web_fetch`. A bot whose sandbox keeps it off the network (`read-only`, or `workspace-write` without `network`) is offered neither web tool and runs neither, from its next call, while they stay among its tools for when its network is set. An existing bot keeps its own, so `--tools` while continuing one is a usage error.
- `--fallback PROVIDER/MODEL`: A model in the bot's family that answers a request a safety classifier declined, that request alone; the next goes to the bot's own model. Without one of its own, a bot takes the entry for its turn's model in `[fallbacks]` in `~/.agent/config.toml`, which the CLI starts the daemon with (`serve --fallbacks`), so an entry reaches every bot, old or new, once the daemon starts again. Keys are `PROVIDER/MODEL` patterns with `*` and `?`, the most specific winning; the protocol's `configure` with `fallback` gives a bot its own.

```toml
[fallbacks]
"anthropic/claude-opus-5*" = "anthropic/claude-opus-4-8"
```

A refused turn says how in `wait` and `turns`: the refusal's `category` and the provider's `explanation`, `provider_request_id` for a support case, `mid_stream` and `written_tokens` for how far the answer got, and `fallback`, the model it was sent to again or null. A fallback it could not go to (a provider the daemon does not serve, or one of another API family) is named in `unused_fallback`, `{model, error}`.

### Approvals and gates

- `--approval MODE` chooses whether a new bot's tool calls wait for a verdict (the daemon's `approval_policy`; without the flag, `AGENT_APPROVAL` applies, then `full`):
  - `full`: No gate; every allowed call runs.
  - `manual`: Gated calls wait for an answer from any client.
  - `auto`: The approver serving the `auto` tag decides each call, such as Sonny's reviewer model (`sonny reviewer`), which Sonny keeps as the daemon's companion. The CLI decides nothing itself. For `auto`, and whenever `run` continues or `fork` copies a bot an `auto` gate answers, the CLI refuses with `approver_not_serving` when none serves the tag, before any bot is made, and gives a call 105 s before its gate lapses (the approver's 90 s review budget, plus margin).
  - `never`: Refuses every call a gate would hold without asking anyone.
- `--approve LIST`: Picks the gated tools and must name at least one. Default: every tool but `history`, `wait`, `echo`, and `web_search`, and in `auto` `read` too.
- A bot made by a gated one keeps its creator's gates whatever the flag, so there `full` is refused (`invalid_gate`) and `never` refuses what those gates would ask about. A fork keeps its source's gates; `fork` takes no approval flags, and a program that wants a fork gated further passes `approve` on the protocol's `fork`. See [APPROVALS.md](APPROVALS.md).

### Sandbox and bot settings

New bots accept per-bot runtime settings; they are not daemon options, and an existing bot keeps its own (see [bot settings](RUST_PROTOTYPE.md#bot-settings) for defaults):

- **Sandbox:** `--sandbox read-only|workspace-write|danger-full-access`, `--writable DIR` (repeatable, made absolute from the current folder), `--network`, `--unix-socket PATH` (repeatable: an absolute socket, or folder of them, its commands reach).
- **Context & retention:** `--context-bytes`, `--context-items`, `--note-turns`, `--compact-at`, `--compact-keep`, `--compact-tokens`, `--keep-turns`, `--max-output-tokens`, `--cache-ttl`. `--keep-turns N` means the same on `prune` and `run`: keep the newest N turns' records. `prune` applies it once; on `run --new` it is the bot's setting, applied after each of its turns.
- **Execution:**
  - `--approval-hold DURATION`: How long gated calls wait live for a verdict, and terminal reads for output, before the turn parks (default `2s`; `0` parks at once).
  - `--hold DURATION`: How long [held turns](#inspecting-waiting-and-approvals) wait on a fixable cause before failing (default `5d`, `hold` in `~/.agent/config.toml` for the bots the CLI makes; `0` fails at once).
  - `--max-restarts N`: The bot's own crash-continuation limit, in place of the daemon's `restarts`.
  - `--trailer`: `sender`, `time`, both comma-separated, or `""` for no line.
  - `--quiet-background`, `--namespace NAME` (the group its agent tools list and name the bots it makes in), `--outputs DIR` (made absolute; without it a new bot gets `outputs` beside the store).

### Context windows and speeds

A model's context window, which sets where its bots compact unless they set `--compact-tokens`, is what its provider lists, else the window Anthropic publishes for a Claude model, else 200K.

- A bot's `context_windows` setting sets it instead for every model whose `PROVIDER/MODEL` path matches a pattern, with `*` and `?` wildcards: `bedrock/*` for every Bedrock model, `bedrock/*openai.*` for OpenAI's there, or one exact model. The most specific pattern wins (an exact path over any wildcard, then the most characters that are not wildcards, then the first by name), and it wins over the provider's listing, so a window can be set lower on purpose.
- The daemon's own speed list names exact model ids (each provider's in `ready.providers.NAME.speeds`). A bot's `speeds` setting gives the tiers past standard (`fast`, `ultrafast`, or `[]` for none) for every model whose path matches, ranked as `context_windows` patterns are and over the daemon's list: for a model the list does not name, or to take a tier from one it does.
- `agent run --new` gives a bot the person's `[context_windows]` and `[speeds]` from `~/.agent/config.toml`, and each `agent run` from outside a bot's turn sends them again when they changed, so an edit reaches a bot's next turn with no daemon restart. A bot made by another takes its creator's.

## Inspecting, waiting, and approvals

- **`ls`** `[--limit N] [--name GLOB] [--active] [--created-by BOT]`: `--name` keeps the bots whose name matches (`'project.*'`, `'*.lead'`; SQLite GLOB, case-sensitive), `--active` those with a turn running, and `--created-by` those that bot made (by its id, so a later bot of the same name lists none of an earlier one's). The daemon filters, so none of them pages through every bot.
- **`inspect`** `--bot NAME [--bot-id N]`: Prints one bot's whole record: the fields of its `ls` row plus its instructions, settings, and compaction. It changes nothing, so it is safe to repeat; with `--bot-id N`, a bot that no longer holds that identity is `bot_not_found`.
- **`turns`** `--bot NAME [--newest] [--after TURN] [--limit N]`: `--newest` lists newest first, read from the end, so `turns --bot NAME --newest --limit 3` is a bot's last three turns.
- **`follow`** `--bot NAME | --all [--after CURSOR | --since DURATION]`: `--bot` replays and follows the selected current turn to its end; an idle bot returns after replay. `--all` stays connected for future work. `--since 1h` replays from the first event written an hour ago or later instead of from a cursor; it conflicts with `--after`.
- **`wait`** `HANDLE... | --bot NAME [--bot-id N] [--any] [--timeout DURATION]`:
  - Normally requires all handles to resolve without errors. With `--any`, one resolved successful handle suffices and the remaining handles stay valid. An errored first result, or a timeout with no resolved result, exits `1`; completed successful handles can still exit `0`.
  - `--timeout 0` polls once and prints the same JSON result with unresolved handles marked pending.
  - `--bot NAME` waits on that bot's running turn, else its newest, so a caller can wait on work it did not submit; the result is keyed by the handle it chose, as if that handle had been given. A turn queued behind the running one is not it. A bot with no turns is `no_turn`, and `--bot-id` pins the identity as on `inspect`. `--bot` and handles do not mix.
- **`ps`** `[--bot NAME|GLOB]` and **`stop`** `--bot NAME proc:N`: List the commands still running and stop one with everything it started (see [output](#output-and-status)).
- **`approvals`** `[--bot NAME] [--tag TAG] [--full]`: Lists the calls waiting on a gate, each previewing its arguments; `--full` reads the whole arguments of a call whose preview was cut from the node that planned it. With `--pretty`, each call shows what it would do (every line of its command, or of what a `write` or `edit` puts in its file, terminal controls escaped) and, for each gate still open, two whole commands, one that allows and one that denies, so a pasted line never carries a verdict its reader did not pick. The call id is written `--call=ID`, so an id that starts with `--` stays the value. `run --pretty` and `follow --pretty` print the same commands when a call waits. A printed command carries `--store` or `--socket`, as absolute paths, whenever the daemon it came from is not the default one, so it answers that daemon from any shell.
- **`answer`** `--bot NAME --turn TURN --call ID --request N [--tag TAG] [--reason TEXT] allow|deny`: Records one gate's verdict. `--tag` may be left out when the call has one gate, and a denial's `--reason` is what the model sees (an allow takes none). `answer` refuses to run inside a bot's own tool shell (`answer_in_tool_shell`).
- **`held`** `[--bot NAME] [--limit N] [--after TURN]`: Lists the [held turns](RUST_PROTOTYPE.md#held-turns): turns a cause a person can fix (a sign-in, a usage limit, a refused key, a provider that is not set up or not answering) holds instead of failing. Each shows its bot, its cause as `blocked` (`code`, `detail`, `since_ms`), and when it next tries on its own as `resume_at_ms`, waiting longer each try up to an hour, for the bot's `--hold`; then it fails with its cause. It prints 64 a page (`--limit N`, up to 256), the next page after `--after TURN`, with `total` and each cause's count in `causes`.
- **`retry`** `[--bot NAME]`: Tries every held turn (or one bot's) again now and prints `{"retried": N}`; once the cause is fixed, that is the way not to wait. `agent login chatgpt` does it on its own once it signs in, and a daemon that starts tries its held turns at once.
- **`usage`** `[--name GLOB] [--since DURATION] [--bucket DURATION] [--limit N]`: Prints what the bots' turns spent as one JSON object: tokens in all, by bot (the `--limit` that spent most, 64 by default, the rest summed), and by the model that spent them, with each model's cache writes, so a summary or fallback on another model counts under that model. `--name` keeps the bots `ls --name` would, `--since 7d` counts only turns started that long ago or since, and `--bucket 6h` adds a `series` of what was spent every six hours from then. Durations take `ms`, `s`, `m`, `h`, or `d`.
- **`audit`** `[--bot NAME|GLOB] [--turn N] [--kind K] [--place P] [--approved A] [--access A] [--outcome O] [--tool T] [--since D] [--before ID] [--limit N]`: Prints the [audit trail](RUST_PROTOTYPE.md#audit-trail), newest first, as one JSON array: a row for every tool call and client step of every agent, with its kind, where it ran, who approved it, whether it read or wrote, and how it ended. `--limit` defaults to 100; `--pretty` prints a line each. `audit trim --before ID` or `audit trim --older-than 90d` deletes the older rows, which nothing else ever does; inside an agent's shell it is refused (`trim_in_tool_shell`), and a sandboxed agent's socket refuses the request whatever sends it.
- **`models`** `[--discover]`: Prints the models clients offer as JSON `[{id, note?}]` (`--pretty`: one line each). See [models](#models).
- **`stats`**: Reports active sessions and turns, pacing pools, storage worker timings, `companions`, and:
  - `files`: how many files the daemon holds open against its limit, so a leak (a count that grows while idle) is told from a busy peak.
  - `temp`: the room left in each temporary folder every agent's commands share (the daemon's `TMPDIR`, then `/tmp`, each folder once): `bytes_free` and `bytes_total`, `inodes_free` and `inodes_total`, one `statvfs` each off the run loop. A file system can run out of inodes with space to spare; one that keeps no inode count reports none, or as many free as in all.
  - `provider_reload`: what the last config change came to (see [hot reload](#providers-credentials-and-hot-reload)).

### Models

`models` reads two files with one owner each:

- `~/.agent/models.discovered` is discovery's catalog: what the providers list, rewritten whole, so an edit there is replaced.
- `~/.agent/models` is the person's, and nothing writes it: a `PROVIDER/MODEL` line adds a model, or puts a listed one first with its note, and a `-PROVIDER/MODEL` line hides one.

In both, anything after `#` is a note. The person's models come first in their order, then the catalog's they did not name, less what they hide. No files is an empty list, and a malformed line fails with `models_invalid` naming it. Reading needs no daemon, and the daemon never reads either file: any model runs. Model ids are compared as strings, never read for versions or families. Bots see the same list by running `models`.

The daemon a client starts on the store beside them (`~/.agent/state.sqlite`) rewrites the catalog (`serve --models`): each time it starts, once it is ready, it asks every provider for its listing, so a start never waits on a provider. A daemon on another store, such as a test's or a side daemon's, leaves it alone. `models --discover` does the same on demand through the daemon (starting one like `run`) and says on stderr what changed (`+ id`, `- id`) and which providers did not answer.

- Each model's name and context size are its note, with a comment for a provider that listed nothing.
- A provider that does not answer keeps its lines from the catalog; one the daemon no longer runs loses them.
- When no usable model survives rendering within the file limit, nothing is written and it fails with `models_none_listed`, naming each refusal, so it can run again after a login.
- The catalog is written beside itself and renamed into place, so a reader sees the old one or the new one, and a link put there is replaced, never written or read through.

## Companions

A companion is a command the daemon keeps running for as long as it runs: a client's helper that agents wait on, such as Sonny's lookout, tool host, or reviewer, so it lives as long as the daemon whoever started either. The daemon knows nothing of what one is.

```sh
agent companion add NAME [--cwd DIR] [--env KEY=VALUE]... -- COMMAND [ARGS]...
agent companion rm NAME
agent companion ls
```

- `add` keeps the companion in the store under `NAME` (1 to 64 letters, digits, `.`, `_`, or `-`, not starting with `.`) and starts it now; every later daemon on that store starts it once it listens. The same spec again changes nothing (`changed: false`); another spec under the name replaces the old one, which is stopped first.
- `rm` stops and forgets one (`removed: false` when there was none).
- `ls` lists them, as `stats` does under `companions`, each with its spec, `running`, `pid`, `started_ms`, `restarts`, and `last_exit` (`code` and `signal`, or `spawn_failed` with why, and `at_ms`).
- Each runs in its own process group with the daemon's environment, its `--env` over that, and `AGENT_STORE`, `AGENT_SOCKET`, `AGENT_BIN`, and `AGENT_COMPANION` (its name), never a bot shell's marks. Its stdin is empty and its output goes to `STORE.companions/NAME.log`.
- One that exits starts again after 1 s, the wait doubling up to 60 s while it keeps exiting within a minute of starting. The daemon's shutdown stops each with `SIGTERM` to its group, then `SIGKILL` after 5 s.
- A companion's own sessions keep no daemon from `--idle-exit`, and a daemon on stdio, with no socket to reach it on, runs none.
- The protocol request is `companions` with `add` (`name`, `command`, `args`, `env`, `cwd`), `remove` (a name), or neither. A sandboxed bot's commands may not make one, since a companion runs as the daemon does.

## Connection and startup

### Store and socket selection

Every client command accepts `--store` and `--socket`:

1. **Store:** `--store` → `AGENT_STORE` → `~/.agent/state.sqlite`.
2. **Socket:** `--socket` → `AGENT_SOCKET` (unless `--store` was explicit) → derived from the selected store. A socket path that is not UTF-8, which the ready line cannot carry, is refused before anything starts (`socket_path_unsupported`).

### Starting, upgrading, and stopping the daemon

- **`start`** `[--replace]` starts the daemon if none is running, with the same startup as `run`, and prints the running daemon's ready line with `socket`, the absolute path it answered on, added; a caller that ran `start` over SSH forwards that path. The ready line names the daemon's release as `version` beside its `protocol`, so after an upgrade `start` shows which build serves.
- **Auto-spawn:** `run`, `start`, and `stats` start the daemon (so `stats` answers on a fresh install); `turns`, `rm`, `prune`, `approvals`, and `answer` may restart it only for an existing store (`store_not_found` otherwise). Other client commands require an already running daemon. These commands accept the startup provider/limit flags shown in help, and `--no-spawn` to require an already running daemon.
- **Inside a bot's turn:** An `agent` command run from inside a bot's turn never starts a daemon on the store that turn runs on, which its shell inherits as `AGENT_STORE`, and never replaces the daemon there: with that daemon down, it is `daemon_unavailable`, so a build of the bot's own never takes the person's store. On a `--store` of the bot's own it starts one as anyone would. An agent's shell runs the `agent` of the daemon serving it (`AGENT_BIN`), so an upgrade's handover is started from outside any agent's turn.
- **Startup flags:** Startup flags configure a newly started daemon. A running daemon is never reconfigured: a stated startup flag it does not match fails the command with `daemon_configuration_mismatch` naming the difference. Comparison uses effective limits: `--idle-exit 0` means disabled.
- **Upgrades:** When the daemon holding the socket speaks another protocol, `start` still prints its ready line and then fails with `daemon_protocol_mismatch`, so a caller can tell an older daemon from a newer one. An older one that names its pid is replaced instead, by every command that may start the daemon (not with `--no-spawn`, and not from inside a bot's turn, where `AGENT_BOT_ID` is set, since that turn is one it would wait on):
  1. It is asked to shut down at once, as `shutdown` without `--grace` asks, so its running turns park where they are and the new daemon continues them, or sent `SIGTERM` when it refuses that request, as daemons before 0.6.0 do.
  2. The new daemon starts at once (`serve --await-owner`) and waits on the store and socket the older one holds: the kernel hands them over the moment it exits, so a daemon anything else starts meanwhile (a window of the older build that lost its connection) is refused `store_already_owned`, its detail naming the daemon that holds the store (`an older daemon (protocol N, pid P) holds this store`).
  3. The new daemon upgrades the store, and the command goes to it.

  The commands on running bots that never start a daemon where none runs (`ls`, `wait`, `follow`, `fork`, `interrupt`) replace an older one the same way, with the provider their environment names, when it listens on their store's own socket (a socket named apart from its store may serve another store). One that keeps the older daemon says why in its `daemon_protocol_mismatch` detail, and that nothing was sent: from inside a bot's turn, with `--no-spawn`, with no provider to start a new one, or on a socket that is not the store's own. Sonny starts the daemon this way, so opening it after an upgrade replaces the old daemon, and it says so first.
- **`start --replace`** replaces the daemon running now the same way whatever its version, so an upgrade that kept the protocol still runs the new build (`sonny upgrade` runs it). It fails with `daemon_kept`, saying why, from inside a bot's turn, with `--no-spawn`, or with no provider to start a new one; a newer daemon is left running and refused with `daemon_protocol_mismatch`; with none running, it starts one as `start` does.
- **`shutdown`** `[--grace DURATION]` returns once the daemon process has exited and its store is closed. `--grace` first drains for up to that long: nothing starts, and each running turn parks at its next boundary, once its model's answer and the calls already running have committed, or finishes if that answer ends it. Turns still running at exit, and every running turn without `--grace`, park with their unanswered calls answered `daemon_shutdown`; the next start continues them. A daemon whose ready line announces an older protocol than this `agent`'s cannot be asked in this protocol, so `shutdown` sends `SIGTERM` to the process that ready line names (a process already gone counts as stopped) and waits for it the same way: the older daemon's running turns end interrupted (from protocol 36 they park) and the store keeps every chat. That is how an upgrade on a machine replaces the daemon an older `agent` started. A newer daemon is left running and `shutdown` fails with `daemon_protocol_mismatch`.
- **`serve`** requires explicit `--store` and `--provider` options. It runs the daemon directly, serving the Unix socket when `--socket` is given and the stdio protocol otherwise. Credentials and provider configuration follow the [runtime documentation](RUST_PROTOTYPE.md).

### Providers, credentials, and hot reload

- **Provider resolution:** With no `--provider`, a started daemon takes `AGENT_PROVIDER` (the same specs, separated by whitespace), else `provider` in `~/.agent/config.toml` (the same), else the providers whose key variable is set; a running daemon is not checked against either. With none of them, starting fails with `provider_required`.
- **Keys file (`~/.agent/keys/env`):** A daemon `agent` starts gets the `KEY=VALUE` lines of this file over `agent`'s own environment (an empty value unsets the name), so it has the same keys whoever starts it: Sonny, an automation's check, a script, or cron. The file is read only then, never for a command a running daemon answers. As Codex keeps `~/.codex/.env` from setting `CODEX_` names, names starting `AGENT_` (any case) are passed over: they are Agent's own settings. A key there names its provider as one in the environment would.
- **Unresolved keys:** A provider whose key variable is unset or blank when the daemon starts (or when a config change binds it) does not stop the daemon, as Codex starts without one: its `ready.providers` entry carries `unresolved` with `provider_key_unavailable`, whose detail names the variable first, then the provider and the keys file, and each of its calls fails with that error before anything is sent. Its other providers serve as usual. A turn that meets it is held, as on a refused key, until the daemon restarts with the key set.
- **Small `/tmp`:** On a machine whose `/tmp` is small (a RAM disk, say), make a folder and put `TMPDIR=/that/folder` in the keys file: the daemon and every tool shell, which inherit its environment, then write temporary files there, and a sandboxed command may write it as it may `/tmp`.
- **Hot reload:** Providers taken from the config file (no `--provider`, no `AGENT_PROVIDER`) follow it while the daemon runs (`serve --provider-config`). The kernel tells the daemon when the file changes (inotify, kqueue; nothing runs in between), including the file a linked `config.toml` names, and the new set is built and checked beside the old, then swapped in whole.
  - Turns that start after the swap use it; running turns finish on the providers they started with, and a turn parked meanwhile (on a tool wait or an approval) resumes on the set in use when it resumes, as it would after a restart, so a rotated key or a moved endpoint reaches it.
  - A provider whose spec is unchanged is kept as it was, with its pacing, connections, and keys.
  - A file that does not parse, a spec that does not bind, and the removal of a provider that an agent's model, fallback, or compaction model, an unfinished turn's own model, or the daemon's `[fallbacks]` still names, or that binds such a provider to another API family (`provider_in_use`, naming them), are refused whole, and the old set stays.
  - `stats.provider_reload` says what the last change came to (`applied` with `added`, `removed`, and `changed`, or `refused` with its error). New sessions' `ready.providers` show the set in use, and `provider_models` names each provider's family and efforts for sessions that connected before (a provider's speeds still come with `ready`, so `/fast` offers a newly added provider's faster tiers after Sonny reconnects). Held turns are tried again after a change is applied.

### Daemon limits

The daemon's [limits](RUST_PROTOTYPE.md#limits) come from `[daemon]` in `~/.agent/config.toml` where the command line sets none, each a number or a formula of the machine, such as `processes = "cpus * 64"`. A key it does not know, or a formula it cannot work out for this machine (one below 0 here included), is `config_invalid` when the file is read.

| Key (flag) | Default | Meaning |
| --- | --- | --- |
| `load` (`--max-load`) | `"cpus * 2"` | While the machine's 1-minute load average is over it, new commands wait for one of the daemon's to end, as `make -l` does, a turn held long parking until then. |
| `stall_timeout` (`--stall-timeout`) | `"10m"` | How long a provider stream may send no content before it is retried. |
| `durability` (`--durability`) | `"full"` | `"full"` syncs the store's log before answering each commit. `"normal"` syncs only at checkpoints: faster on a busy disk, and a crash of the machine or its power can lose the last commits, never a crash of the daemon alone. |
| `depth` (`--max-depth`) | `2` | How many generations below the bot that starts its line a new bot may be, as Codex's `agents.max_depth`: a bot run with no `AGENT_BOT` starts one, its children are at 1 and theirs at 2, and a `run --new` or `fork` deeper is refused as `depth_limit`. `0` is no end. |
| `restarts` (`--restarts`) | `5` | How many times in a row the daemon may stop unplanned (a crash, a kill, a fatal error) while a turn runs, with nothing recorded for it between, before the next start ends that turn `process_interrupted` rather than continuing it, as Pi Durable's `restartLimit`. `0` continues it always. A bot's own `max_restarts` (`--max-restarts` on `run --new`) takes its place for that bot. |

The daemon raises its soft limit on open files to the hard one as it starts (on macOS, to the most a process may open), as Go's runtime does, and the commands it runs inherit the raised limit.

### Environment boundaries

- **Shell environment filtering:** A bot's shell gets the daemon's environment without the keys its providers use, whose values are also replaced with `[redacted by agent]` in what tools return. Nothing else in tool output is redacted, as in Codex: what a program prints of its own secrets reaches the model as it printed it.
  - `serve --shell-exclude PATTERN`, repeatable, leaves out more: every variable whose name matches, with `*` for any run and `?` for any one character, in any case. A client that starts a daemon passes it Codex's `[shell_environment_policy]` from `~/.agent/config.toml`: each name in `exclude`, and `*KEY*`, `*SECRET*`, and `*TOKEN*` too with `ignore_default_excludes = false`. As in Codex, those three are kept by default.
  - It also leaves out the variables that name the terminal it was started in (`TMUX`, `TMUX_PANE`, Rex's, Herdr's, and cmux's places, `agent_client::TERMINAL_PLACES`), so a bot's command never lands in that terminal.
  - A running daemon keeps the excludes it started with, and a variable a bot or caller sets with `--env` is still set.
- **Protected paths:**
  - `serve --protect NAME` keeps `NAME` at the top of a sandboxed bot's writable folders read-only and from being made, and `--protect-existing NAME` keeps it read-only where it exists; a client passes `.git`, `.agents`, and `.codex`, and `.aws` where it exists.
  - `serve --protect-path PATH` keeps `PATH`, by its absolute path, read-only in every sandbox and from being made there; the CLI passes `~/.agent/rules`, where Sonny reads the [command rules](RUST_PROTOTYPE.md#command-rules) it gives its bots, so no bot writes its own.
  - `serve --unreadable FOLDER` keeps `FOLDER` and what is in it from every sandboxed command's reads, writes, links, and renames, as Codex's `deny_read`, and the `read` tool refuses it for a sandboxed bot (`read_denied`); it is made private (mode 700) if missing. The CLI passes `~/.agent/keys`, which holds `env`, the keys it starts the daemon with, and each folder `~/.agent/config.toml` lists as `[sandbox] unreadable`, from `/` or `~/` (`unreadable = ["~/.aws"]`). That list is empty by default, so an agent may use what the person keeps at home, a browser's session cookies or a cloud CLI's sign-in, until they list it; a running daemon keeps the folders it started with.
- **Socket wall:** `serve --wall FOLDER` keeps every sandboxed command, the network on or not, from connecting to the Unix sockets in `FOLDER`, which it makes private (mode 700) if missing; the CLI passes `~/.agent/run`, where clients such as Sonny keep the sockets of programs they run for bots outside the sandbox. It is a wall for sockets, not a way to hide files. On Linux the folder shows empty to a sandboxed command, since bubblewrap mounts an empty read-only folder over it (made again first if something removed it), but on macOS Seatbelt only refuses connections to its sockets and hard links into it, so its files stay readable; and the `read` tool, which the daemon answers itself outside any sandbox, reads them on both. See [what the sandbox closes](../sonny/REFERENCE.md#what-the-sandbox-closes).
- **Multi-daemon isolation:** Every daemon lists its store and socket in `~/.agent/run/daemons` as it opens its store, and keeps every other listed daemon's as it keeps its own: the store and the files SQLite keeps beside it read-only in every sandbox, and the socket and its bots' own sockets (in `SOCKET.d` or, for a long socket address, a short private directory) out of every sandboxed command's reach, the network on or not. So a second daemon on the machine is no way out of the first one's sandbox, nor the first out of the second's. An entry stays while its store does, so an idle store is kept too. A daemon that cannot write the list, as one started inside a sandbox cannot, refuses to start with `daemon_not_listed`, naming the folder and the error. A command that was already running when a new daemon listed itself keeps the sandbox it started with.
- **One daemon per person:** That is the shape everything else assumes. A second one, on its own `--store`, is a separate world for chats, usage, stats, the pacer, Sonny's windows and Sky, the lookout, approvals, and upgrades (each client replaces only the daemon on the socket it reaches), while it shares everything under `~/.agent` and `~/.agents`: settings, prices, logins, memory, the commons, automations, and task folders. Run one for a build under test or an eval, on a store in a temporary folder, and stop it when done.

> The lightweight command registry in `src/cli.rs` supplies help and option scope for both client commands and `serve`. Keep that registry, the implementation, and behavior tests aligned when adding an option. No CLI framework or runtime dependency is required.
