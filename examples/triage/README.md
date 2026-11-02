# Issue triage: an app on the CLI and the socket

`triage.py` is a small service built on Agent the way an outside program
would build on it: the `agent` CLI, whose commands print JSON, and the
daemon's Unix socket, which speaks JSON lines. It uses no SDK, no Rust
crate, and nothing from Sonny, and it needs only Python's standard library.

It triages a batch of issues. One template bot holds the instructions and two
[declared tools](../../docs/RUST_PROTOTYPE.md#declared-tools) that this
program answers: `search_issues`, which searches the other issues, and
`set_triage`, which records labels, a priority and a duplicate. The bot has no
other tools, so it cannot touch files or run commands. Each issue goes to its
own fork of the template (`run --fork`), so every triage is its own named
conversation that `agent follow`, `agent inspect` or `agent turns` can read
later. Each decision is printed as one JSON line.

```sh
cargo build --release
PATH="$PWD/.local/target/release:$PATH" examples/triage/offline.sh
```

`offline.sh` runs it against `scripted_model.py`, a local Responses endpoint
whose words are scripted per issue, in a throwaway store, so it needs no API
key and gives the same output each time. Everything else is real: the daemon,
the store, the forks, the declared tools and their answers.
`tests/test_examples.py` runs it in CI, so a protocol change that breaks this
app fails there. With a real model, set the model and its provider's key and
run the app itself:

```sh
AGENT_MODEL=anthropic/claude-sonnet-5 ANTHROPIC_API_KEY=... \
  python3 examples/triage/triage.py examples/triage/issues.jsonl
```

The issues in `issues.jsonl` are made up.

## What it took

Measured on 2026-11-02 at 7ea7b7a (protocol 48, schema 78).

**Lines.** `triage.py` is 148 lines, 102 of them code (no blank lines,
comments or docstrings). 28 of those are the app itself: its instructions,
its two tool schemas and what the tools do. The other 74 connect it to Agent:

| Part | Code lines | Uses |
| --- | --- | --- |
| Imports and names | 10 | |
| Run a command, parse its JSON | 5 | CLI |
| A socket session: `ready`, then requests matched by `id` | 17 | socket |
| Make the template bot, or update its tools | 8 | socket `create`, `configure` |
| Answer calls to the declared tools | 19 | socket `follow`, `tool_calls`, `tool_result` |
| Start the daemon, fork per issue, wait, report | 15 | CLI `start`, `run --fork --detach`, `wait` |

The offline run of five issues takes about 0.45 s of wall time, daemon start
included.

**What the author had to know.** Eleven facts, seven of them about answering
declared tools:

1. `agent start` starts the daemon if none runs and prints its `ready` line
   with `socket`, the path to connect to.
2. `agent run --fork TEMPLATE --bot NAME --detach --request-id ID` makes a
   fork and starts its turn in one call and prints a handle. The fork keeps
   the template's instructions and declared tools, and a resent request
   gets the same turn.
3. `agent wait HANDLE...` prints every result as JSON, and exits 1 when any
   turn failed, with the results still on stdout.
4. The socket speaks one JSON object per line. A session starts with a
   `ready` line. A reply carries its request's `id`, and events, which carry
   none, can come between, so a client matches replies by `id` or uses one
   session for requests and another for a follow.
5. `create` takes no defaults from the CLI: it needs the model as
   `provider/model`, a `workspace` (any turn needs a folder, even a bot with
   no file tools), instructions and `tools` (`[]` for declared tools only).
6. A declared tool is `{name, description, parameters, answered_by}`, and
   `expire_ms` is how long a call waits for its answer: 60 s unless set, at
   most an hour. A waiting call parks as a row in the store, so a long wait
   costs nothing and outlasts a restart of the app.
7. A bot that exists answers `create` with `bot_exists`, and `configure`
   replaces its declared tools.
8. `follow` with `bot: "*"`, `since_ms` and `kinds: ["tool_started"]` gets
   every bot's calls as they start, and `follow_live` marks the end of the
   replay.
9. `tool_started` previews the arguments, cutting long fields, so the whole
   arguments come from `tool_calls` with the tool's tag, a page at a time
   (`next_after`).
10. A listing shows every call still waiting. Nothing documented says that a
    listing sent after an answer leaves that call out, so the app keeps its
    own record of the calls it answered rather than risk recording a
    decision twice. Five runs without that record never answered a call
    twice, so this may be caution the protocol does not need.
11. `tool_result` takes `bot`, `turn`, `call_id` and `output`, and refuses a
    second answer with `call_already_answered`.

Facts 8 to 10 together make the app stateless. On `follow_live` it lists the
calls that started while it was not running and answers them, so it keeps no
cursor across restarts. A test of that path, where all five turns parked on
their first call before the app started listening, completed all five.

## Gaps it hit, with proposed fixes

**1. Declaring tools needs the socket.** A bot the CLI makes cannot declare
tools, so the app writes its own `create`, and with it things the CLI would
have resolved: the model (the app reads `AGENT_MODEL` and misses `model` in
`~/.agent/config.toml`), the folder and the tool list. That costs the
socket session (17 lines) and the template setup (8).
*Fix:* `run --new --declare FILE`, where FILE holds the protocol's own
`declare` list. This is a CLI change only. With it, each issue can be
`run --new --bot triage.N --declare tools.json --tools '' --instructions-file
triage.md`, with no template and no socket request but the answers.

**2. Answering needs the socket and a loop that is easy to get wrong.** The
answer loop is 19 lines, and seven of the eleven facts above are in it:
follow with `kinds` from now, treat `follow_live` and each `tool_started` as
the cue to list, page through `tool_calls`, and skip calls already answered.
The first draft got three of those wrong: it read replies on the follow
session, which drops the events that come between, it kept no record of the
calls it answered, and it looked for a cursor in `ready`, which has none.
*Fix:* two CLI commands over the existing ops, as `approvals` and `answer`
are for gates. One prints each call waiting for a tag once, as a JSON line
with its arguments whole: the backlog first, then live with `--follow`. The
other answers one call with output from its operand or stdin, or with
`--error CODE`. The loop then lives in the CLI once, and any language,
including a shell script reading lines, can serve a tool. These are client
changes, with no daemon change. Names are open: `tool-calls` and
`tool-result`, after the ops, or single words like the other verbs.

**3. A bot without a folder fails late, and with no detail.** The first run
made the template without a `workspace`. `create` took it, and every
`run --fork` then failed with `{"error":"workspace_required","detail":null}`.
That breaks the CLI's own contract that `detail` states the rule, and the
CLI gave no hint.
*Fix:* `workspace_required` names the rule ("the bot has no folder and the
submission names none"), and the CLI's hint names `--workspace DIR`. This is
a daemon change, to the text of one error.

**Not gaps.** These worked as an adopter would want, and need no change:
`start` returning the socket path, `run --fork` with `--request-id` as one
idempotent call per issue, forks keeping declared tools, parked calls waiting
for an answerer that has not started yet, and structured tool arguments,
which carried everything the app needed, so it never wanted an app-only part
of a tool result. The one-hour cap on `expire_ms`, already listed as later
work, did not come up: an hour covers a restart of this app.
