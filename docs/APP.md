# Desktop client

`agent-app` is the Thread design as a window: the prototype's page, rendered by
the system webview, with a Rust core that speaks the daemon's socket protocol.
It exists because the design as drawn needs pixels, and a terminal's cell grid
cannot give it rounded cards, sub-cell spacing, or shadows. The daemon is
unchanged and never knows which client is attached.

Started 2026-09-19, after the terminal client reached the limit of its grid.

## Shape

```
app/
  src-tauri/     Rust core: three commands, one event
  ui/            the page: index.html, app.css, app.js, daemon.js
client/          agent-client: the socket protocol, shared with the TUI
```

- **Rust core** ([app/src-tauri/src/main.rs](../app/src-tauri/src/main.rs)) is a
  transport. `setup` returns the socket, model and workspace defaults;
  `attach` connects, lists every bot, follows `*` from the page's cursor, and
  forwards every notification to the window as a `daemon` event; `request`
  relays any protocol op. State and protocol logic live in the page, exactly
  as they did in the prototype, so the design and the mechanics iterate in one
  place. The core links `agent-client`, the same crate the TUI uses.
- **The page** is the prototype's HTML and CSS with the fake daemon swapped
  for events. Its state model is a port of the terminal client's: bots and
  transcripts built from events, peers inferred from the shell call that
  spawned them, cards for peers and background commands, thoughts folded to
  their duration, lazy item loads for whatever is on screen. Events and loads
  are processed one at a time, in arrival order, like the TUI's loop.
- **Demo mode.** In a plain browser there is no Rust core, so `daemon.js`
  becomes a simulated daemon that emits the same protocol shapes and answers
  `item`, `submit`, `create`, `interrupt`. The scenario plays on load: main
  thinks, starts a release build in the background, spawns plan, build and
  test, build spawns review, and main waits on all of it. Serve `app/ui` with
  any static server to work on the design without a daemon.

## Running it

```sh
cargo build --release -p agent-app
AGENT_MODEL=anthropic/claude-sonnet-4-5 .local/target/release/agent-app \
  --socket ~/.agent/state.sqlite.sock --workspace "$PWD"
```

Arguments and environment are the TUI's: `--socket`, `--store`, `--model`,
`--workspace`, `AGENT_SOCKET`, `AGENT_STORE`, `AGENT_MODEL`. Closing the
window is detaching; the daemon and its bots continue. The page remembers the
bot on screen, the open peek, the rail and the folds per socket and workspace
in the webview's local storage, and restores them on the next start. If the
daemon is unreachable or closes the session, the page shows why and retries
every two seconds.

Keys are the concept's: `^k` switch, `^b` rail, `^p` peek, `^t` thoughts,
`^o` output, `Esc` close then interrupt, `↑` `↓` on an empty prompt to move
between bots, `^d` close the window, `/new NAME [PROVIDER/MODEL]` to create a
bot, `?` on an empty prompt for the list. `⌘` works where `^` does.

The playground drives it like the TUI: `python3 tui/playground.py --no-tui`,
then the app with the printed socket.

## Verified

2026-09-19. Demo mode in a browser: the scenario renders as the concept
(cards with elapsed at the right edge, thought fold, wait line), `^p` opens a
peek with the peer's tool calls and final text, `^b` shows the tree main →
plan, build → review, test, and `^k` filters to "review ↳ build". Live mode:
the built app attached to a playground daemon: asked over its socket, the
daemon reported the app's session while the window was up and none after it
closed, and the page forwarded no errors. That needed
`src-tauri/capabilities/default.json` granting `core:default`; without it the
page cannot subscribe to window events and never attaches. Page errors are
forwarded to the app's stderr through a `log` command.

Not verified: the live window's rendering by eye (the debug binary is not a
bundle, so it could not be screenshotted here), a real provider, and macOS
packaging, which needs `bundle.active` and real icons.

The tree uses the daemon's `created_by` when the record has one (bots created
from a shell tool since schema 20) and falls back to inferring the creator
from the spawning shell call for older bots.

`/new` gives a bot the shared client policy ([CLIENT.md](CLIENT.md)); the
create notice says what went in.

## Next

1. Run it against a real daemon and model by eye; fix what the screenshot
   shows.
2. Packaging: a real icon set, `bundle.active`, a signed build.
