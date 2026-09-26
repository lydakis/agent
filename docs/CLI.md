# CLI contract

Agent uses flat verbs: `run`, `follow`, `fork`, `interrupt`, `wait`, `ls`,
`turns`, `result`, `rm`, `prune`, `approvals`, `answer`, `stats`, `shutdown`,
and `serve`. All named
bots use the same commands. There is no parent/child command hierarchy.

Use `agent --help`, `agent COMMAND --help`, or `agent help COMMAND` for help;
`-h` also works. Help writes to stdout and exits successfully without opening
a store or connecting to a daemon.

Inside a bot's shell tool, `AGENT_BOT` and `AGENT_BOT_ID` identify that bot.
The client sends both as `created_by` and `created_by_id` when creating or
forking a child. The store rejects missing or stale creator identities before
creating the child, including after a daemon restart or name reuse;
`AGENT_PARENT` and `AGENT_PARENT_ID` identify the running bot's recorded creator
when its identity was known at creation. Address that creator with
`run --detach --bot "$AGENT_PARENT" --bot-id "$AGENT_PARENT_ID" -- TASK`.
The ID stays pinned after deletion, so a replacement with the same name rejects
the call instead of receiving the work.
`run` without `--bot` creates a fresh identity. `run --bot NAME` continues an
existing bot; add `--new` to create that name. A prompt of `-` reads stdin.
`follow --bot NAME` replays and follows the selected current turn to its end;
an idle bot returns after replay. `follow --all` stays connected for future work.

## Arguments and flags

- Put the command first. Options may appear before or after its operands.
- Long value flags accept both `--flag VALUE` and `--flag=VALUE`.
- `--` ends option parsing. For example, `agent run -- --help` sends the
  literal prompt `--help`. Use `--instructions=--example` for a flag-like value.
- Prompts and wait handles are positional operands. Every bot selector is
  `--bot NAME`; a fork uses `--source NAME` and `--bot DESTINATION`.
- `--all` selects all bots for `follow`; `--any` selects first-completion
  behavior for `wait`. `follow --all --bot NAME` is an error.
- `run --delivery MODE` is `reject`, `queue`, or `steer`: what the
  submission does when the bot is busy. Without the flag, `AGENT_DELIVERY`
  applies, then `reject`. `--turn N` with `steer` makes it strict: for that
  running turn or `stale_turn`. The daemon has no default of its own: the client
  always sends the mode it resolved, so one person's preference never
  changes what a program's submission means. A blocking `run` with `queue`
  follows the turn from its queued state to its end; with `steer` it ends
  with exit 0 when the message is absorbed (`steered`, naming the turn it
  joined) or when the turn it started completes.
  A steer with an explicit workspace or model that differs from the running
  turn stays queued and runs separately with those choices.
- Unknown flags, flags belonging to another command, unexpected operands,
  and repeated singleton flags are usage errors. `--provider` is repeatable.
- `--instructions` and `--instructions-file` are mutually exclusive.
  `--agents` composes the shared client policy instead: the harness preamble,
  every AGENTS.md from the workspace up to the root plus `~/.agent/AGENTS.md`,
  and an index of `.agent/skills/*.md` files ([CLIENT.md](CLIENT.md)). It is
  opt-in on the CLI, the default in the app, and exclusive with
  `--instructions`. `fork` takes none of the three: a fork is an exact copy
  of its source, instructions included.
  With `run`, instructions, reasoning, and token budget apply to new identities;
  passing them while continuing an existing named bot is an error.
- `run --new --approval MODE` and `fork --approval MODE` choose whether a
  new bot's tool calls wait for a verdict: `full` runs every allowed call (no
  gate), `manual` waits for an answer from any client, and `auto` is refused
  with `approval_mode_unsupported` until an automatic approver exists.
  Without the flag, `AGENT_APPROVAL` applies, then `full`. `--approve LIST`
  picks the gated tools and must name at least one; the default is every
  tool but `history`, `wait`, `note`, and `echo`. A fork keeps its source's gates and a created bot its
  creator's ([APPROVALS.md](APPROVALS.md)).
- `approvals [--bot NAME] [--tag TAG]` lists the calls waiting on a gate.
  With `--pretty`, each call shows what it would do (every line of its
  command, terminal controls escaped) and, for each gate still
  open, two whole commands, one that allows and one that denies, so a
  pasted line never carries a verdict its reader did not pick. The call id
  is written `--call=ID`, so an id that starts with `--` stays the value. `answer --bot NAME --turn TURN --call ID --request N
  [--tag TAG] [--reason TEXT] allow|deny` records one gate's verdict; `--tag`
  may be left out when the call has one gate, and a denial's `--reason` is
  what the model sees (an allow takes none). `answer` refuses to run inside a bot's own tool shell
  (`answer_in_tool_shell`). `run --pretty` and `follow --pretty` print the
  same commands when a call waits. A printed command carries `--store` or
  `--socket`, as absolute paths, whenever the daemon it came from is not
  the default one, so it answers that daemon from any shell.
- Time units are explicit: `--timeout-ms` is milliseconds; `--idle-exit`,
  `--stall-timeout`, and `--keep-warm` are seconds. `--after` is an exclusive event cursor for `follow` and an exclusive
  turn ID for `turns`. `--checkpoint` is a history node ID.
  `--approval-hold-ms` is milliseconds: how long a gated call waits live for
  its verdict before its turn parks (default 2,000; 0 parks at once).

The lightweight command registry in `src/cli.rs` supplies help and option scope
for both client commands and `serve`. Keep that registry, the implementation,
and behavior tests aligned when adding an option. No CLI framework or runtime
dependency is required.

## Output and status

Default output is machine-readable JSON. `run` and `follow` stream one JSON
object per line. Snapshot commands return compact JSON objects; `ls` and `turns`
return arrays. `interrupt` and `shutdown` return no stdout on success.
`shutdown` returns once the daemon process has exited and its store is closed.
`shutdown --grace SECONDS` first lets running turns finish for up to that long
while starting none; turns still running then end `interrupted` with
`daemon_shutdown`.

`--pretty` is an explicit human view: rendered streams, tables for lists, and
indented JSON for other results, including detached submission handles. It is
rejected on commands with no output. Diagnostics go to stderr. Rendered model
text and tool output keep their line breaks and tabs; any other character a
terminal would act on is printed escaped, so a stream cannot hide or restyle a
call waiting for approval.

| Exit | Meaning |
| --- | --- |
| 0 | Requested operation succeeded, including help/version |
| 1 | Operation failed, requested result is incomplete, or wait timed out |
| 2 | Invalid command-line usage |
| 75 | `serve` could not acquire store/socket ownership |

`wait` normally requires all handles to resolve without errors. With `--any`,
one resolved successful handle suffices and the remaining handles stay valid.
An errored first result or a timeout with no resolved result exits 1.
`--timeout-ms 0` polls once and prints the same JSON result with unresolved
handles marked pending. Completed successful handles can still return exit 0.

## Connection and startup

Every client command accepts `--store` and `--socket`. Client store selection is explicit
`--store`, then `AGENT_STORE`, then `~/.agent/state.sqlite`. Explicit `--socket`
wins; otherwise `AGENT_SOCKET` applies unless `--store` was explicit. Without a
socket override, the socket is derived from the selected store.

`run` may start the daemon. `stats`, `turns`, `result`, `rm`, `prune`,
`approvals`, and `answer` may restart it only for an existing store. These commands accept the startup
provider/limit flags shown in help and `--no-spawn` to require an
already running daemon. Startup flags configure a newly started daemon. A
running daemon is never reconfigured: a stated startup flag it does not match
fails the command with `daemon_configuration_mismatch` naming the difference.
Comparison uses effective limits: `--idle-exit 0` means disabled, and positive
context limits are clamped to at least 1,024 bytes and two items.
With no `--provider`, a started daemon takes `AGENT_PROVIDER` (the same specs,
separated by whitespace), else the providers whose key variable is set; a
running daemon is not checked against either.
`run --model` names a new bot's model or an existing bot's turn model; the
daemon has no model of its own, so a new bot needs `--model` or `AGENT_MODEL`.
`AGENT_MODEL` is only a creation default. An existing bot uses its stored
model unless `--model` explicitly overrides it for this turn.
`run --instructions` and `--instructions-file` set a new bot's instructions,
with the CLI's built-in text as the default; the daemon has none. `run
--tools LIST` chooses a new bot's tools from those the daemon registers,
default `shell,read,write,edit,wait,history`; an existing bot keeps its own,
so `--tools` while continuing one is a usage error.
Other client commands require an already running daemon.

`serve` requires explicit `--store` and `--provider` options. It runs the daemon
directly, with Unix-socket service when `--socket` is
provided and the stdio protocol otherwise. Credentials and provider configuration
follow the [runtime documentation](RUST_PROTOTYPE.md).
