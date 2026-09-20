# Desktop client

`agent-app` is the Thread design as a window: the prototype's page, rendered by
the system webview, with a Rust core that speaks the daemon's socket protocol.
It exists because the design as drawn needs pixels, and a terminal's cell grid
cannot give it rounded cards, sub-cell spacing, or shadows. The daemon is
unchanged and never knows which client is attached.

Started 2026-09-19. A terminal client (`agent-tui`, ratatui) came first the
same day and reached the limit of its grid; it was removed once the app
covered it, so one state model exists, not two.

## What the daemon speaks, and why the client speaks it directly

The daemon's contract is JSONL over a Unix socket: requests with an `id`,
responses with the same `id`, and notifications without one, replayed and
then live under a store-wide cursor ([protocol](RUST_PROTOTYPE.md#software-protocol)).
The app, the `agent` CLI, and a fleet controller are the same kind of client.
Two alternatives were considered for the client path and rejected for now:

- **ACP between a client and the daemon.** ACP models one editor talking to one
  agent process over stdio. It has no vocabulary for named durable bots shared
  across clients, a store-wide event cursor, forks from history nodes, wait
  handles, delivery modes, or daemon stats; all of that would ride in extension
  fields. ACP remains the right shape for an editor adapter, as a bridge
  process that is a consumer like this app. It is not built.
- **A binary framing.** Per-event cost on the client path is the storage commit
  and the provider stream, not JSON encoding, and the socket carries no TLS or
  HTTP. A different framing would be a drop-in change behind the same op set
  if a measurement ever shows encoding on the follower path as a cost. None
  does; the JSON line protocol stays.

Remote use needs no transport work either: the protocol assumes nothing about
the stream, so `ssh -L` forwarding of the daemon's socket to a local one runs
the app locally at full speed against a daemon elsewhere. The client is built
to tolerate that latency anyway: one `follow *`, item loads pipelined per bot,
nothing polled.

## Shape

```
app/
  src-tauri/     Rust core: six commands, no events
  ui/            the page: index.html, app.css, app.js, daemon.js
  playground.py  an offline daemon with a synthetic model, for mechanics
client/          agent-client: the socket protocol and the client policy
```

- **Rust core** ([app/src-tauri/src/main.rs](../app/src-tauri/src/main.rs)) is a
  transport. `setup` returns the socket, model and workspace defaults;
  `policy` composes the client policy for the workspace; `attach` connects
  and follows `*` from the page's cursor; `pull` hands the page the next
  batch of that session's notifications, at most 256, when it asks;
  `request` relays any protocol op. State and protocol logic live in the
  page, exactly as they did in the prototype, so the design and the
  mechanics iterate in one place.
- **The page** is the prototype's HTML and CSS with the fake daemon swapped
  for events: bots and transcripts built from events, lineage from the
  daemon's `created_by`, cards for peers and background commands, thoughts
  folded to their duration, lazy item loads for whatever is on screen. Events
  and loads are processed one at a time, in arrival order.
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

Arguments and environment are the CLI's: `--socket`, `--store`, `--model`,
`--workspace`, `AGENT_SOCKET`, `AGENT_STORE`, `AGENT_MODEL`, and a store's
socket is resolved the way the CLI and the daemon resolve it (the shared
client crate's rendezvous), so a deep store path meets the same short socket.
The page draws with the machine's own monospace face and fetches nothing.
Closing the window is detaching; the daemon and its bots continue. The page
remembers the bot on screen, the open peek, the rail and the folds per socket
and workspace in the webview's local storage, and restores them on the next
start. If the daemon is unreachable or closes the session, the page shows why
and retries every two seconds.

Keys are the concept's: `^k` switch, `^b` rail, `^p` peek, `^t` thoughts,
`^o` output, `Esc` close then interrupt, `↑` `↓` on an empty prompt to move
between bots, `^d` close the window, `/new NAME [PROVIDER/MODEL]` to create a
bot, `?` on an empty prompt for the list. `⌘` works where `^` does.

`python3 app/playground.py` starts a daemon on a synthetic streaming model
and opens the app on it; prompt prefixes (`shell:`, `bg:`, `delegate:`,
`fanout:`, `slow:`, `hold:`, `limited:`, `md:`) drive tool calls, delegation,
waits and pacing with no provider. `--no-app` prints the attach command
instead.

## What it costs, and where the bounds are

Every path that runs per event or per draw is bounded by what is on screen,
not by the history or the fleet:

- **Attach** replays events from the page's cursor, which on a first start is
  the beginning of the daemon's retained log. That log is bounded by the
  daemon's retention (`--retain-turns`, `prune`), and a `pruned` notice marks
  the gap. Nothing is staged on the way: the page pulls the replay a batch
  at a time and applies each before the next, so the transport's 4,096-event
  queue is the only buffer between the daemon and the screen, and a page
  slower than the fleet is told it lagged and attaches again from its
  cursor. The snapshot is paged (256 bots a request) while the replay flows;
  a bot the replay already spoke of keeps the state those events built and
  takes only the record's static fields, a bot an event names before its
  record arrives gets a seat at once, and a listed bot nothing mentioned is
  seated from its record.
- **Transcripts** keep a window of 1,200 decoded items around whichever end
  the reader is at; bodies outside it fold back into their history nodes, and
  a scroll toward them loads the next batch of 400. A run of unloaded nodes
  renders as one placeholder row, so unloaded history costs one element per
  gap. Counters (thoughts, long outputs, peers) are kept in step with the
  items, so the key bar reads them.
- **Rendering** rebuilds the window's HTML only on a structural change (a
  load, a fold, another bot). Items appended since the last render are added
  on their own; a tool finishing or a process ending replaces its own line; a
  streamed delta touches only the tail; the once-a-second clock refreshes the
  elapsed spans and peer cards in place. The rail shows a window of 300 rows
  around the selection, extended by scrolling to an edge; its tree is rebuilt
  when the fleet's shape changes (a bot created, forked or deleted) and a
  status change replaces that bot's own row. The picker shows at most 200
  rows, and the activity check behind the clock is cached per fleet change.
- **Creation** costs no request: the `created` and `forked` events carry the
  record's list fields, so a burst of ten thousand bots is ten thousand
  events, not ten thousand `resume` round trips. Lineage is the store's
  `created_by_id`: a bot links under its creator only while the bot holding
  that name is the identity that created it.
- **Sessions** count up; an event from an older session is dropped, a
  submission carries the bot id on screen, and a bot that reappears under a
  known name with a new id starts from nothing. A lagged stream (the core's
  4,096-event queue filled) closes the transport, fails every request made
  after that at once, and the page attaches again from its cursor, from
  outside the event chain so the attach cannot wait on itself. A new attach
  lets the previous session's socket go first, so a pull still waiting on it
  comes back closed rather than holding the new session up.

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

The tree uses the daemon's `created_by` (bots created from a shell tool since
schema 22, with the creator's identity since 23) or the `created` event; a bot
without a creator, or whose creator's name has since changed hands, is a root.

`/new` gives a bot the shared client policy ([CLIENT.md](CLIENT.md)); the
create notice says what went in.

## Next

1. Run it against a real daemon and model by eye; fix what the screenshot
   shows.
2. Packaging: a real icon set, `bundle.active`, a signed build.
