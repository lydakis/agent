# Terminal client

`agent-tui` is a terminal client for one running daemon. Open it and you are
talking to a bot; the rest of the fleet is one key away. It is a consumer of
the socket protocol, not a new surface on the daemon: the crate in `tui/` is a
workspace member that does not link the runtime, so the core's dependency set
and build are unchanged.

First cut 2026-09-19; rebuilt the same day around the "Thread" design after
prototyping three layouts as an interactive artifact.

## What the daemon speaks, and why the TUI speaks it directly

The daemon's contract is JSONL over a Unix socket: requests with an `id`,
responses with the same `id`, and notifications without one, replayed and
then live under a store-wide cursor ([protocol](RUST_PROTOTYPE.md#software-protocol)).
The TUI, the `agent` CLI, and a fleet controller are the same kind of client.
Two alternatives were considered for the client path and rejected for now:

- **ACP between a client and the daemon.** ACP models one editor talking to one
  agent process over stdio. It has no vocabulary for named durable bots shared
  across clients, a store-wide event cursor, forks from history nodes, wait
  handles, delivery modes, or daemon stats; all of that would ride in extension
  fields. ACP remains the right shape for an editor adapter, as a bridge
  process that is a consumer like this TUI. It is not built.
- **A binary framing.** Per-event cost on the client path is the storage commit
  and the provider stream, not JSON encoding, and the socket carries no TLS or
  HTTP. A different framing would be a drop-in change behind the same op set
  if a measurement ever shows encoding on the follower path as a cost. None
  does; the JSON line protocol stays.

Remote use needs no transport work either: the protocol assumes nothing about
the stream, so `ssh -L` forwarding of the daemon's socket to a local one runs
the TUI locally at full speed against a daemon elsewhere. The client is built
to tolerate that latency anyway: one `follow *`, item loads pipelined per bot,
nothing polled.

## The design

One conversation fills the screen and the prompt has focus. Everything else
is summoned and slides in.

```
 ◐ main  waiting
   › ship the login fix; split the work and wait for it
   thought 6s
   Splitting this into three peers and waiting on them.
   ▸ shell "$AGENT_BIN" run --new --bot plan --detach '…'
   │ ● plan  12s
   │   ▸ write PLAN.md
   │ ● build  12s
   │   Patched refresh_session to reissue the cookie…
   │ ○ $ cargo build --release
   │   Finished release profile in 41.2s
   ▸ wait plan/1, build/1, proc:1  12s
 ────────────────────────────────────────────────────────────
  main ›
  ● main                     ^k switch  ^p peek  ^b bots  ^d detach  ? keys
```

Rules, each with its reason:

- **One shape for anything a bot starts and can wait on.** A peer created
  through the shell tool and a background command are both cards: glyph,
  name, elapsed, last line. That is what they are to the daemon too, a
  `turn:BOT/N` or `proc:N` handle. A foreground tool call is a plain line
  because it holds the turn and has nothing to wait on. The handle JSON those
  calls return, and a wait's result, are never shown: the cards are that data.
- **No progress bars.** The daemon has no percentage for model work. Elapsed
  time is shown while something runs, and a tool call's duration only when it
  took more than a moment. Replayed history shows no times at all, because
  events carry no clock; the `turns` op does, and can supply them later.
- **Thinking folds.** Live thinking is one dim line with the latest sentence;
  when the message lands it folds to `thought 6s`. `^t` unfolds every thought
  in the transcript.
- **Output folds.** Tool output shows two lines plus `+N lines`; `^o` unfolds.
  Shell results render like a terminal: stdout, then stderr, then a nonzero
  exit code.
- **Nothing else marks a turn.** A blank row separates turns. No turn
  numbers, cursors, token counts, delivery modes, or model names on screen.
- **The bottom line is the only chrome.** A dot that is green when the daemon
  session is live and amber while the bot on screen works, the bot's name, a
  transient notice, and only the keys that apply right now.
- **Colors are the terminal's sixteen.** Green, yellow, magenta, red, dark gray
  and cyan by name, plus dim, italic and reverse. The terminal's theme is the
  theme; there is no theme file and no true color. The mapping lives in one
  `Palette` so that could change without touching widgets.
- **Weight and depth from the terminal's own means.** Names are bold, secondary
  text is dim, cards and the selected row carry an accent bar, and overlays
  sit on a one-cell shadow. No fills behind panes.
- **Shading follows the terminal's answer.** At startup one OSC query asks the
  terminal whether its ground is light or dark; the answer picks a
  near-background gray from the 256-color palette (255 or 236), and no answer
  means no shading. `AGENT_TUI_THEME=light|dark|none` overrides. Shaded:
  fenced code in replies, inline code, tool output, and the composer. Code
  blocks and the composer are pills: half-block and quadrant characters in
  the shade color give half-cell side padding and softened corners, since a
  background fill cannot be rounded. Tool output is a flat shaded strip.
- **Markdown, lightly.** Fenced code, inline code, bold and headings are
  rendered; nothing else is interpreted. No syntax highlighting: it would
  add a syntax bundle and per-block parsing for short snippets, and the
  shade already says "verbatim".
- **Motion is cheap and optional.** The bot rail and the peek pane slide over
  160 ms; the redraw loop ticks at 60 fps only while a slide runs, at 2 Hz
  while a bot works (glyph pulse, elapsed counters), and otherwise sleeps.
  `--no-motion` snaps instead. Idle cost measured at 0.1% CPU.

Bots are shown as a tree by creator in the rail and the switcher, from the
record's `created_by` (schema 22: the CLI declares it from `AGENT_BOT` when a
bot's shell tool creates a peer). Bots without one, created before that,
fall back to inference from the shell call that ran `agent run --new --bot
NAME` at the moment the `created` event arrives, live or on replay.

## Keys

| Key | Does |
| --- | --- |
| type, `Enter` | prompt the bot on screen; queued if it is busy |
| `^k` | switch bot: type to filter, `↑↓`, `Enter` |
| `↑` `↓` on an empty prompt | previous / next bot in tree order |
| `^b` | slide the bot rail in or out |
| `^p` | peek the next peer beside the thread; `Esc` closes |
| `^t` / `^o` | unfold thoughts / tool output |
| `Esc` | close what is open; with nothing open, interrupt the running turn |
| `^d` or `^c` | detach: the TUI exits, the daemon keeps running |
| wheel, `PgUp` `PgDn` `End` | scroll the thread, or the peek under the pointer; text selection is shift-drag while the mouse is captured |
| `/new NAME [PROVIDER/MODEL]` | create a bot |
| `?` on an empty prompt | all keys |

Detach is exiting, as in tmux: the daemon and its bots keep going. On exit
the TUI writes a session record keyed by socket and workspace to
`~/.agent/tui-sessions.json`: the bot on screen, the open peek, whether the
rail was out, and the fold settings. The next start in that folder restores
all of it after replay. A first start with no record selects the first root
of the tree. If the follower ever lags and is dropped, the TUI attaches again
from its cursor by itself; if the daemon closes the session, the TUI exits
and says so.

## Running it

```sh
cargo build --release
.local/target/release/agent serve --store ~/.agent/state.sqlite \
  --socket ~/.agent/state.sqlite.sock --provider anthropic &
AGENT_MODEL=anthropic/claude-sonnet-4-5 .local/target/release/agent-tui \
  --socket ~/.agent/state.sqlite.sock --workspace "$PWD"
```

Socket selection is `--socket`, then `AGENT_SOCKET`, then the socket adjacent
to `--store`, `AGENT_STORE`, or `~/.agent/state.sqlite`. Deep store paths that
fall back to the daemon's hashed socket name need an explicit `--socket`. New
bots take their model from `--model` or `AGENT_MODEL`, their workspace from
`--workspace` or the current directory, the client's built-in instructions
(the same text the CLI uses), and the default tool set.

### Offline playground

`tui/playground.py` starts a synthetic streaming Responses model and a daemon
bound to it, then opens the TUI; `^d` in the TUI stops the daemon too, and
`--no-tui` keeps the daemon in the foreground for another terminal instead.
Prompt prefixes drive the model: `shell: CMD`, `bg: CMD` (background, then a
wait on its proc handle), `delegate: NAME` (spawn a peer through the shell
tool and wait on it), `fanout: A,B` (several peers, wait with `any`),
`slow: TEXT` (ten seconds of streaming to interrupt), `limited:` (429 until
interrupted), `hold:` (open until `kill -USR1` or thirty seconds). The store
persists under `.local/playground`, so a second attach replays history. The
synthetic model does not stream reasoning, so the thought fold is exercised
only against a real Responses or Anthropic model.

## Verified

Against the offline playground on 2026-09-19, driven through tmux: create
with `/new`, streamed replies, a fan-out whose three peers appear as cards
with live elapsed while the parent shows ⏳ and the wait line counts up, the
rail as a two-level tree, a peek pane with the peer's tool calls, the switcher
filtering to `test ↳ main`, a background command as a card with its output
and the wait JSON hidden, interrupt of a streaming turn, `^d` exiting with the
session written and a rerun resuming the same bot with the rail out, a fresh
start replaying the whole store with the root bot selected, mid-slide frames
of the rail at two widths, and 0.1% CPU after five quiet seconds. Unit tests cover item decoding
for both provider families and shell-result rendering.

Not verified: a real provider through this TUI, the thought fold (needs
reasoning deltas), terminals narrower than about 90 columns, and stores with
thousands of bots.

New bots get the shared client policy ([CLIENT.md](CLIENT.md)): the preamble,
AGENTS.md files from the workspace up, and a skills index. The create notice
says what went in.

## Next

1. A run against a real provider to exercise the thought fold and real
   delegation, then `turns` timestamps for elapsed times in replayed history.
