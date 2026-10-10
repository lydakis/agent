# Desktop client

`agent-app` is the Thread design as a window: the prototype's page, rendered by
the system webview, with a Rust core that speaks the daemon's socket protocol.
It exists because the design as drawn needs pixels, and a terminal's cell grid
cannot give it rounded cards, sub-cell spacing, or shadows. The daemon exposes
bounded history reads and creator identities; it does not
need to know which client is attached.

Started 2026-09-19. A terminal client (`agent-tui`, ratatui) came first the
same day and reached the limit of its grid; it was removed once the app
covered it, so one state model exists, not two.

## Screenshots

Demo mode in headless Chromium, 1280×780. The data is the synthetic `demo`
project.

### Navigation

Captured 2026-10-09. Home: the list on the right is one level below what is
open, here the projects.

![Home with its projects listed](app/nav-home.png)

A click on a row looks in beside, with its own composer; ← goes back to the
list and ⤢ Full screen opens it.

![The demo coordinator open beside Home](app/nav-peek.png)

The project full screen, in a tab of its own: the crumbs go back up and the
list holds its threads.

![The demo project in a tab, its threads in the list](app/nav-project.png)

A double-click on a thread opens it as another tab; its list holds what it
made.

![build in a second tab, its reviewer in the list](app/nav-thread.png)

⌘K finds any agent and opens it as a tab.

![The finder](app/nav-find.png)
![A third tab from the finder](app/nav-tabs.png)

### Earlier

Captured 2026-09-28, before the navigation above: the list was on the left
and held the whole tree.

The lead splits the work, build opens beside it, then build's ⋯ menu.

![The lead runs, a task opens beside it, and its ⋯ menu opens](app/shell.gif)

The lead waits on its tasks; each task is a card.

![Projects and tasks in the sidebar, the lead mid-run](app/shell.png)

A task opened beside the lead, with its own composer.

![build open beside the lead](app/side-pane.png)

One menu per agent: side chat, stop, fork, delete, show all.

![The agent menu](app/menu.png)

The model chip: the agent's effort on top, then models under their provider;
other families need a new agent. The chip names the effort its next turn runs
at (here `xhigh`, picked in this menu for an agent made at `high`). A level or
model picked here applies to the agent's next turns and leaves the one it was
made with unchanged; picking that one again goes back to it. "default"
appears only for an agent made without a level. A steer names neither, so it
joins the running turn at that turn's model and effort.

![The model chip's menu](app/model-chip.png)

Send on a busy agent: queue after this turn, steer into it, or ask a side
chat.

![Send's choices](app/send.png)

build works in its own worktree: its branch follows its name in the head.

![build on its own branch, beside the lead](app/worktree.png)

A message another agent sent shows who sent it where yours has `›`: the
coordinator starting or steering a task, a task's answer, a swarm post. The
name opens that agent beside the chat. The daemon records a prompt's author
when a bot's turn sent it (`from`, which the CLI fills from `AGENT_BOT` and
`AGENT_TURN`), and `history_items` returns it with each prompt, so a chat
read back after a restart keeps the names. Messages the app sends on its own
name their `origin` and are tagged with it: `tasks` for a coordinator's task
updates and `trigger` for a triggered message. The daemon keeps the sender
with the message, so a fork keeps it after its source is deleted; the name
links to the agent only while that name still holds the identity (`from.id`)
that sent it.

![A task's chat: the coordinator's messages tagged](app/agent-message.png)

A side chat asked while the lead works: a fork beside it, the lead untouched.

![A side chat beside the running lead](app/side-chat.png)

A turn you or the app asked for (a triggered message, a coordinator's task
update) that finishes while its agent is off screen shows ✔ instead of ○
until you open that agent; a swarm's row shows it for its agents until you
open the swarm. Settings, help, the finder and the swarm sheet count as off
screen. A turn another agent asked for is that agent's news, so the tasks stay
○ unless you steered into the turn; a failure already shows ✘ until the next turn. The mark lives in the
window and does not survive a restart.

![demo finished while notes was open](app/done-unseen.png)

A reply drawn as Markdown, captured 2026-10-09: a heading, a list, a table and
highlighted Rust, each drawn as soon as its block ends while the rest streams.
See [Messages](#messages).

![A reply streaming: finished blocks draw as Markdown, the open block stays text](app/rich-stream.gif)

![A finished reply: heading, list, table and a highlighted code block](app/rich.png)

Further down the same reply: a ```` ```vega-lite ```` block drawn as a chart
in the window's colors and an ```` ```html ```` block's preview, each shown
with a click.

![A Vega-Lite chart and an HTML preview](app/rich-preview.png)

A file the reply links, open beside it: `PLAN.md` drawn as Markdown with its
diagram. Its own link to `report/latency.vl.json` opens that chart file in its
place.

![PLAN.md open beside the reply that links it](app/file-beside.png)

New project takes a folder, the model its lead starts on, and that model's
effort. Effort is how hard the model thinks: every provider offers low,
medium, high and xhigh, Claude also max, and "default effort" sends no level,
so the model uses its own default. It is picked with the model, the model chip
changes it later, and it is kept in `.agents/project.toml` as `reasoning`
beside `model`; `/new NAME PROVIDER/MODEL [EFFORT]` takes one too. A task the
coordinator starts on its own model takes its effort as well
(`AGENT_REASONING`, see [CLI.md](CLI.md)).

![New project](app/new-project.png)

A first run opens setup: connect providers, then open a first project on a
model from any of them (demo `?first`).

![Setup's provider choices](app/setup-providers.png)

Each provider asks for what it needs to sign in; Bedrock takes a region and an
AWS profile or a Bedrock API key, and serves Claude and its other models as
one provider.

![Connecting Amazon Bedrock](app/setup-bedrock.png)

AWS login takes AWS CLI version 2 (2.9 or later), whose `aws configure
export-credentials` hands the profile's keys to the daemon. An older CLI cannot,
and Setup says so with the version it found.

![Bedrock with an AWS CLI that is too old](app/setup-bedrock-old-cli.png)

Once a provider answers, the first project takes a folder and a model, listed
under its provider; nothing is chosen for you.

![The first project and its model](app/setup-project.png)

Settings (⚙ in the sidebar, or ⌘,) is the same screen. A provider that fails
says why beside those that answered.

![Settings with one provider failing](app/settings.png)

Once a project exists, Settings also lists the app's two roles: the
coordinator's and a swarm agent's. **Edit** opens your own copy in
`~/.agents/agents/` in your text editor, made from the app's text the first
time, and from then on that file is the role in every project (a project's
own `.agents/agents` file of the same name still comes first). An agent keeps
the text it started with, so an edit reaches coordinators and swarm agents
made after it. Captured 2026-09-28.

![Settings with the roles](app/settings-roles.png)

A project's ⋯ menu starts a swarm: a goal, how many agents, where they work,
the tokens they share, what they are made of, and how they organize: one
board, or a council of three that approves streams of work. What they are
made of is a mix: rows of an identity (a plain agent, or a profile the
folder offers, such as a reviewer), a model from any connected provider, its
effort, and a share, each shown as the agents it makes at the size picked. The
first row starts on the lead's model and effort. Captured 2026-09-29.

![The new swarm sheet](app/new-swarm.png)

The swarm's board: your goal first, then its agents' posts, the ones they
name marked, each under what its agent is when the swarm mixes kinds (here
three plain agents and a reviewer on another model). Its row in the sidebar
is working while any agent works.

![A swarm's board while three of four agents work](app/swarm-board.png)

Its agents as cards, each with what it is and its newest line.

![A swarm's agents](app/swarm-agents.png)

Your post naming `@latency-3` woke that agent alone; clicking a name opens
the agent beside, with the board's posts it heard.

![An agent beside the board](app/swarm-beside.png)

With a council, the board also carries roles, proposals, the seats' votes and
their decisions; a post in a stream wears its tag, which filters the board.

![A council swarm's board](app/swarm-council-board.png)

The Council tab: each open proposal with its votes so far, which you can
approve or deny yourself, then the ones decided.

![The council](app/swarm-council.png)

Streams: the approved proposals, each with its lead and the agents in it.

![Streams](app/swarm-streams.png)

When a stream's lead leaves, the first agent left in it leads it, and the
board says so.

![A stream handed on when its lead left](app/swarm-lead.png)

You can also ask a project's coordinator for one ("start a swarm of six, two
of them reviewers, to halve the p99"). Its role tells it to run
`~/.agent/swarms/start`, a script the app writes, which starts the swarm the
way the sheet does; the swarm shows under the project once its agents take
their briefs.

A coordinator hears when you work in a task it started: once it rests, one
message lists the turns its tasks ended, and it passes on what another task
needs.

![A coordinator reads a task update and passes build's change on to test](app/coordinator-wake.png)

Agents can be woken at set times, when a file is written or a repository
gets a commit, or by name. Settings lists the triggers, what each one last
did, and the message it sends, with Run now and Remove.

![Triggers in Settings](app/settings-triggers.png)

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
the stream, so the daemon's socket forwarded over SSH to a local one serves a
window exactly as a local socket does, with the link's latency added to each
request. The app does that forwarding itself for a window on a host
([Hosts over SSH](#hosts-over-ssh)). The client is built to tolerate that
latency: one `follow *`, item loads pipelined per bot, nothing polled. Nothing
has measured the app over a real network link yet.

## Shape

```
app/
  src-tauri/     Rust core: a transport, plus the project file
  ui/            the page: index.html, app.css, app.js, daemon.js
  playground.py  an offline daemon with a synthetic model, for mechanics
client/          agent-client: the socket protocol and the client policy
```

- **Rust core** ([app/src-tauri/src/main.rs](../app/src-tauri/src/main.rs)) is a
  transport, with one attachment per window. `setup` returns the socket or
  host, model and workspace defaults;
  `policy` composes the client policy for the workspace; `attach` connects
  and follows `*` from the page's cursor; `pull` hands the page the next
  batch of that session's notifications, at most 256, when it asks;
  `request` relays any protocol op; `models` reads `~/.agent/models`;
  `settings`, `save_settings`, `restart_daemon` and `discover_models` back
  [Setup](#setup-and-settings); and
  `project` and `write_project` read and write a folder's
  `.agents/project.toml` ([project.rs](../app/src-tauri/src/project.rs));
  `hosts` lists `~/.ssh/config`'s hosts and `open_host` opens a window on one
  ([remote.rs](../app/src-tauri/src/remote.rs));
  `policy` composes a folder's client policy, in a profile when named, and
  falls back to the profiles the app ships for `coordinator`;
  `triggers`, `trigger_fire` and `trigger_remove` list, run and remove
  [triggers](#triggers) ([trigger.rs](../app/src-tauri/src/trigger.rs)).
  When nothing listens on a store's socket, `attach` starts a daemon first
  ([daemon.rs](../app/src-tauri/src/daemon.rs)); see
  [Installing](#installing).
  State and protocol logic live in the
  page, exactly as they did in the prototype, so the design and the
  mechanics iterate in one place.
- **The page** is the prototype's HTML and CSS with the fake daemon swapped
  for events: bots and transcripts built from events, lineage from the
  daemon's `created_by`, cards for peers and background commands, thoughts
  folded to their duration, lazy item loads for whatever is on screen. Events,
  snapshot reconciliation, creation replies, and history loads share one
  mutation queue, so a pending read cannot splice over a newer snapshot.
- **Demo mode.** In a plain browser there is no Rust core, so `daemon.js`
  becomes a simulated daemon that emits the same protocol shapes and answers
  `history_items`, `submit`, `create`, `fork`, `delete`, `interrupt`. A steer is
  queued as its own turn and joins the running turn at its next round
  boundary as a user message (`steered`), and the scripted reply
  acknowledges it. The coordinator's tasks and swarm posts name the agent
  that sent them, as the daemon's history does. The scenario
  plays on load in two projects: `demo.lead` thinks, starts a release build
  in the background, spawns its tasks plan, build and test, build spawns
  review, and the coordinator waits on all of it. A message to one of its
  tasks brings the coordinator a task update it answers. Serve `app/ui` with any
  static server to work on the design without a daemon.

## Installing

The app is published as a Homebrew cask for macOS (Apple silicon and Intel):

```sh
brew install --cask lydakis/agent/agent
```

The bundle carries the `agent` runtime as `Agent.app/Contents/MacOS/agent`,
and the cask links it onto `PATH` as `agent`, so the terminal's CLI is always
the app's own version. When the app finds no daemon on its
store's socket, it runs that binary as `agent start --store STORE`, which
starts the daemon exactly as a CLI command would: providers from
`AGENT_PROVIDER` or the keys that are set, the log beside the store, a
process that outlives the window. A window opened from the Dock inherits
launchd's environment rather than a terminal's, so `agent start` runs with
the environment of the user's login shell (`$SHELL -l -i`), plus
`~/.agent/env` for keys kept out of shell profiles: `KEY=VALUE` lines
(`export` and quotes allowed, `#` comments), refused unless it is a regular
file of at most 64 KiB that only its owner can read. That file is the app's; the CLI and the daemon never read it.
[Settings](#setup-and-settings) writes it.
The login shell is read once per app run. A value the file sets empty is
unset for the start, so Settings can clear what the shell exports. There is
no default model: a project or agent is given its own when it is made. The
store and socket themselves are resolved from the app's own arguments and
environment. A failed start shows the CLI's
reason on the page and is not retried for 30 seconds. An explicit `--socket`
or `AGENT_SOCKET` never starts anything. Uninstalling or upgrading the cask
quits the app and runs the bundled `agent shutdown --store
~/.agent/state.sqlite --grace 30`, so the default store's daemon, whoever
started it, lets running turns finish and exits before its binary is replaced.
The store is named so the uninstalling shell's `AGENT_STORE` or `AGENT_SOCKET`
cannot point the shutdown elsewhere.
A daemon that survived an upgrade anyway, and speaks an older protocol than
the app, leaves the window detached with **Restart the daemon**: the app
sends SIGTERM to the process that daemon named in its `ready` line (running
turns end as interrupted; the store keeps every chat), waits up to 30 seconds
for it to exit (an exited process nobody has reaped yet counts as gone), and
attaches, which starts the bundled daemon. Only a daemon whose `ready` greeting names
a protocol strictly older than the app's is signalled. Reattaching pauses
meanwhile, so nothing starts a daemon while the old one closes its store. A
daemon newer than the app is left alone: the page says to update the app and
stops trying to attach. A window given `--socket` or `AGENT_SOCKET` did not
start that daemon, so it offers no restart and says to stop the daemon with
its own agent and start one from this update's agent; it attaches when that
one answers. An exited daemon its parent has not reaped yet counts as gone,
on Linux from `/proc` and on macOS from `proc_pidinfo`.

![An older daemon still owns the socket after an upgrade](app/daemon-replace.png)

Releases follow Errand's: pushing a `vX.Y.Z` tag on `main` whose version both
`Cargo.toml` and `app/src-tauri/Cargo.toml` carry runs
[release.yml](../.github/workflows/release.yml) on a macOS 26 runner. The app
must link the macOS 26 SDK: one built with an older SDK keeps the separate,
pre-26 title bar on macOS 26. The tag
itself only starts [release-request.yml](../.github/workflows/release-request.yml),
which holds no secrets; release.yml and publish-homebrew.yml run after it from
`main`'s own definitions, so code at a tag never sees the signing secrets or
the tap token. A failed run is re-run from its own page, but a re-run keeps
the workflow file it first ran; to pick up a fix merged to `main`, re-run the
release's Release request run instead. It runs
the tests, installs the Tauri CLI from
[app/release/package-lock.json](../app/release/package-lock.json) before any
signing material exists, builds a universal `agent` and app, and has Tauri sign both with
the Developer ID and hardened runtime, notarize and staple the bundle
([tauri.release.conf.json](../app/src-tauri/tauri.release.conf.json)); it
then verifies the signature, Gatekeeper assessment, staple, architectures and
versions, and leaves `Agent_X.Y.Z_universal.zip`, the generated cask,
`source-commit.txt` (the commit it built) and `checksums.txt` on a draft
release. Publishing the draft (not a prerelease) runs
[publish-homebrew.yml](../.github/workflows/publish-homebrew.yml): it
resolves the tag to a commit on `main` before running any of its code,
refuses assets built from any other commit (a draft's tag can move) or
lacking release.yml's build attestation (a draft's assets can be replaced),
checks them against their checksums and the tag's cask generator,
installs and audits the cask, and writes `Casks/agent.rb`, rendered by
`main`'s generator, to
[lydakis/homebrew-agent](https://github.com/lydakis/homebrew-agent). It never
downgrades the tap or replaces a different cask of the same version. The
helpers and their tests are in [app/release](../app/release)
(`python3 -m unittest discover -s app/release`).

Secrets: the six Apple signing and notary secrets Errand uses
(`APPLE_DEVELOPER_ID_CERTIFICATE_P12_BASE64`,
`APPLE_DEVELOPER_ID_CERTIFICATE_PASSWORD`, `APPLE_DEVELOPER_ID_APPLICATION`,
`APP_STORE_CONNECT_API_KEY_P8`, `APP_STORE_CONNECT_KEY_ID`,
`APP_STORE_CONNECT_ISSUER_ID`), and `HOMEBREW_TAP_GITHUB_TOKEN` with Contents
write access to the tap.

## Running it

```sh
cargo build --release -p agent-app
.local/target/release/agent-app \
  --socket ~/.agent/state.sqlite.sock --workspace "$PWD"
```

A source build is not a bundle, so it links no skills: it uses whatever
`~/.agents/skills` holds. To try the current `app/skills/NAME`, link it there
yourself (a link of yours stays), or build a bundle as the release does
([Installing](#installing)).

Without `--workspace` the workspace is the launching directory, or home when
that is `/`, as for a window opened from the Dock. `--host ALIAS` opens the
window on that SSH host instead ([Hosts over SSH](#hosts-over-ssh)), with
`--workspace` then a path on the host. Arguments and environment
are the CLI's: `--socket`, `--store`, `--workspace`, `AGENT_SOCKET`,
`AGENT_STORE`, and a store's
socket is resolved the way the CLI and the daemon resolve it (the shared
client crate's rendezvous), so a deep store path meets the same short socket.
The page draws with the machine's own monospace face and fetches nothing.
Closing the window is detaching; the daemon and its bots continue. The page
remembers the open tabs and the one on screen, the agent beside, the
sidebar, model picks and the steps fold per store and workspace in the
webview's local storage, and restores them on the next start. Each is kept
with its bot's id and the id of what is a level above it, so a tab whose agent
was deleted while the window was closed comes back as that level above, as it
would have moved live, and one whose name now holds another bot does not come
back. The store is
the identity the daemon announces when the window attaches, so two hosts, or
a host and this machine, never share what a window remembers, whatever socket
reaches them. When the socket a window reattaches to answers with another
store than before (the host's daemon replaced by one on another store), the
window drops the last store's bots, threads and drafts and follows the new
one from its start. Send's queue or
steer pick is remembered for every window. If the daemon is unreachable or closes the session, the page shows why
and retries every two seconds. Only one attachment runs at a time, including
the snapshot pages. A connected peer must send its ready line within five seconds.

Keys: `^k` find an agent and open it as a tab, `^b` sidebar, `^,` settings, `^p` next task beside, `^o` every
run's thoughts and output, `Esc` close the side pane then stop, `↑` `↓` on an
empty message to move between bots (from Home, `↓` opens the first and `↑` the
last), Enter or Space on a focused tab to choose it, `^d` close the window, Enter to send and
Shift-Enter for a new line, `/new NAME PROVIDER/MODEL` to create a bot, `?`
on an empty message for the list and the models in `~/.agent/models`, read
each time. `⌘` works where `^` does.

## Setup and settings

What a daemon needs before anything runs is the app's to ask for, not the
daemon's: it runs whatever providers it is started with and whatever model a
turn names. There is no default model (George, 2026-09-28): a project's lead,
and every agent, is given its model when it is made, from any provider
connected, and forks and side chats keep their source's. One screen covers a
first run and later changes:

1. **Providers.** Anthropic, OpenAI and OpenRouter take an API key; a ChatGPT
   plan uses the sign-in Codex saved; Amazon Bedrock takes a region and signs
   in one of two ways, chosen on the form: the AWS CLI's credentials for an
   optional profile (which drops a saved key), or a Bedrock API key. Bedrock serves Claude over Anthropic's API and
   its other models over OpenAI's, so the daemon runs it as two providers,
   `bedrock` and `bedrock-openai`; the app connects, lists and removes them
   as one. Connecting writes `AGENT_PROVIDER` (the providers already running
   kept, specs as `--provider` takes them) and the provider's fields to
   `~/.agent/env`, restarts the daemon with `agent shutdown`, which stops
   running turns and returns once the process is gone, attaches again, which
   starts a daemon with the new settings, and asks every provider for its
   models. Each row then shows its model count, or its refusal with a Retry.
   Connect and Remove read the file again first, so a change another window
   saved is kept.
   A key is never read back into the page: the core reports only which keys
   are set, and a key field left empty keeps the saved one; any other field
   left empty is saved empty, which clears the shell's value too (an AWS
   profile, say). Removing a provider drops its key unless another provider
   uses it; removing the last one also empties each key the shell exports,
   since a start with no provider named would otherwise detect one from it,
   and the window then waits for a provider instead of retrying. Removing
   rewrites no list; the pickers leave that provider's models out. A provider
   set up by hand under a known name (a gateway named `openai`, say) has no
   Edit, since the form would replace it with the provider's defaults.
   Refresh models asks again and writes the answer to `~/.agent/models`, replacing
   it: a provider that answers replaces its lines, one that fails keeps the
   lines it had, one no longer running loses them, and an answer with no
   usable model leaves the file as it was. A file that no longer reads is
   replaced whole. Pickers offer only the models of connected providers. A
   change that would make `~/.agent/env` larger than a start accepts
   (64 KiB) is refused.
2. **First project**, shown until one exists: a folder and a model, the
   models listed under their providers, then New project's path.

It opens on its own when a daemon cannot start for lack of a provider, and
once when a window first attaches to a store with no agents. An
`agent` the app did not bundle, or an explicit `--socket`, cannot be
restarted, so Settings saves no provider change there and says why; it
shows that daemon's providers instead of the ones this machine would start.

## Hosts over SSH

A window can attach to the daemon on a machine you reach over SSH, so long
work runs there while the app runs here. Settings lists **Hosts**: the
concrete `Host` aliases in `~/.ssh/config` (not patterns, negations, or
`user@host`, which ssh reads as a user and a host), at most 1024 of them
from at most 4 MiB of config across includes,
following `Include` with a relative path from `~/.ssh`, a `~/` path, or
wildcards in the last component only (an `Include` inside a `Host` or
`Match` block is followed only under `Host *` or `Match all`; one that
applies to some hosts is skipped), and beside each what `ssh -G` says it
connects to. Words are split as OpenSSH splits them (either quote, and `\`
escapes), and a line with an open quote, which OpenSSH rejects, names no
host. The `ssh -G` answers run eight at a time, for the first 64 aliases.
**Open window** opens a window on that host; `agent-app --host
box` opens the first one there. The window's title names the host. Nothing
about SSH is reimplemented: every connection is `ssh box` with your own
config, keys and agent.

Per host, the app owns one `ssh` process, a ControlMaster in the foreground
with keepalives and no session. Its control socket and the forwarded daemon
socket are in `~/.agent/hosts/`, made owner-only and named by a hash of the
exact alias (so `Box` and `box` never share them on a case-insensitive
disk), beside a lock only one app process may hold per host. Over that
master the app runs `agent start` in the remote user's login shell
(`exec "$SHELL" -l -i -c ...`, so the `agent` and provider keys a terminal
there would have), which prints the daemon's ready line and the socket it
answered on, then asks the master to forward that socket (`-O forward -L
LOCAL:REMOTE`). The page's attach, cursor and pulls then run unchanged
against the local socket. An attach tries the forward first; when the daemon
or the link has gone, it runs `agent start` again, starting a new master
first if the old one exited. A failure is not tried again for a backoff that
doubles from one second to thirty, so a window retrying every two seconds
does not open a connection each time. The daemon on the host keeps running
when the app lets go of it.

### Connection lifecycle

Every process the app starts for a host is an `ssh` that leads a process
group of its own. It has exactly one owner, which ends it, group and all, on
every path. What OpenSSH starts under it (`Match exec`, `ProxyCommand`,
`KnownHostsCommand`) is in its group. When the `ssh` exits, the rest of its
group is killed before the `ssh` is reaped, so a group is signalled only
while its id is still the app's.

| Process | Owner | Ends when |
| --- | --- | --- |
| `ssh -G ALIAS` (Hosts list) | the list request | it answers, or after 5 s or 64 KiB of output |
| `ssh -F none -G -o OPTION` (once per launch) | the first attach | it answers, or after 5 s |
| The master (`-N`, `ControlMaster=yes`) | the host's link | the last window closes, the app quits, it prints over 1 MiB of errors in one second, or it exits (the next attach starts another) |
| `agent start`, `agent shutdown` on the host | the attach or restart that ran it | it exits, or after 60 s or 1 MiB on stdout or stderr |
| `-O forward`, `cancel`, `exit` to the master | the attach, close or cleanup that ran it | as above |

The last window on a host closing sends SIGTERM to the master's group (ssh
closes the connection and removes its control socket), then SIGKILL after
two seconds, and removes the host's files, its lock last and while still
held. Quitting ends every group still running, then closes every host
concurrently under one three-second deadline, then kills what is left. An
attach that finishes after its window closed starts nothing
(`host_closed`). A crash leaves its groups running. On the next launch, the
app sweeps `~/.agent/hosts/`. For each host whose lock no process holds, it
asks a master still answering on the control socket to exit, waits until
that master has removed its control socket itself, and removes the files.
It never trusts or reuses any of them. The same retirement runs before any
master starts, so an old master cannot remove a new one's socket on its way
out. A lock file is taken only when it is still the file at its path, so two
processes never hold locks on different files of one name.

Every option the app depends on is forced on the command line, in one table
in `remote.rs` (`FORCED`). OpenSSH keeps an option's first value, and the
command line is read before any config, so `~/.ssh/config` cannot change
them:

| Option | Master | Exec | Why |
| --- | --- | --- | --- |
| `BatchMode=yes`, `ConnectTimeout=10` | yes | yes | no terminal to answer a prompt |
| `LogLevel=ERROR` | yes | yes | reasons come from ssh's error lines; `QUIET` hides them |
| `RequestTTY=no`, `RemoteCommand=none` | yes | yes | the command is the app's, with no terminal |
| `ForkAfterAuthentication=no`, `ControlPersist=no` | yes | yes | the supervised ssh is the one doing the work; no master outlives it |
| `ControlMaster=yes` / `no` | yes | no | one master per host; a command only uses it |
| `SessionType=default` | no (`-N`) | yes | runs on a host kept for forwarding (`SessionType none`) |
| `ClearAllForwardings=yes`, `Tunnel=no`, `ForwardAgent=no`, `ForwardX11=no` | yes | yes | a config's forwards cannot stop the master; the daemon inherits no agent or display |
| `PermitLocalCommand=no`, `AddKeysToAgent=no` | yes | yes | no `LocalCommand` and no `ssh-askpass` here |
| `ServerAliveInterval=15`, `ServerAliveCountMax=3` | yes | no | a dead link is noticed within a minute |
| `ChannelTimeout=global=0 *=0` | yes | no | an idle forwarded connection is not closed |
| `ExitOnForwardFailure=yes`, `StreamLocalBindUnlink=yes`, `StreamLocalBindMask=0177` | yes | no | the daemon's forward binds or fails, replaces a stale socket, and is owner-only |

The control socket is always `-S`, which the command line also decides.
`-O` requests read no config at all (`-F none`): a config's
`ClearAllForwardings yes` would otherwise make `-O forward -L` succeed while
creating no listener. Two caveats:

- `StreamLocalBindMask` is last-value-wins in OpenSSH, so a config can loosen
  it. The owner-only directory is what keeps other users out.
- `ForkAfterAuthentication`, `SessionType` and `ChannelTimeout` are unknown to
  OpenSSH before 8.7 (macOS 12 ships 8.6), which rejects them as bad options.
  The app asks its `ssh` once which it knows and leaves out the ones it
  doesn't; a config for that ssh cannot set them either.

Not forced, as the user's own: how the host is reached (`ProxyCommand`,
`ProxyJump`, `Match exec`, `KnownHostsCommand`), keys and agents, and
environment (`SetEnv`, `SendEnv`). `StdinNull` and `EscapeChar` need
nothing: stdin is `/dev/null` and there is no terminal. A `ProxyJump` host
whose own config says `ControlPersist` backgrounds a master of the user's
own with `setsid`. That master leaves the app's group and is not the app's
to end.

A path containing `%` or `$` is refused (`host_path_unusable`), because ssh
expands both in control and forward paths. So is a remote socket with `:`
or `%`.

A window on a host never starts or replaces a daemon on this machine, and
never signals a process on the host itself. What stops it says why:

- No `agent` on the host's login PATH: `agent_missing`, which says to install
  the Linux `agent` there (the app bundles a macOS one and copies nothing).
- A refused login, an unknown host key, or an unreachable host:
  `host_auth_failed` (load a key into ssh-agent so `ssh box` needs no
  password), `host_key_unverified` (run `ssh box` once in a terminal), or
  `host_unreachable`, each with ssh's own last line.
- No provider there: `host_no_provider`; the host's daemon runs the providers
  its login shell exports, and Settings shows them without offering a change.
- Versions: the host's `agent start` prints the daemon's ready line even when
  that `agent` refuses it. A daemon older than the app that the host's
  `agent` refused is `daemon_older`, and **Restart the daemon** runs the
  host's own `agent shutdown` (which stops an older daemon by the pid it
  announced) and then `agent start`. A daemon the host's `agent` accepted but
  that is older than the app means that `agent` is old: `host_agent_older`
  says to install this version there. A newer one is `daemon_newer`.

The app does not read a host's files yet, and never reads this machine's in
their place. On a host window these are refused by name
(`remote_unsupported`) rather than answered from this machine: composing
instructions from AGENTS.md, skills and profiles (so `/new` and New project),
a project's `.agents/project.toml`, the model list in `~/.agent/models`, the
roles in `~/.agents/agents`, provider settings in `~/.agent/env`, and a
folder's branch, which is not shown. Swarms stay on this machine: their board
is the app's files and their agents run the app's scripts, so New swarm is
disabled on a host. Chat, follow, steer, stop, fork, side chats and delete
work on the host's bots, and a bot made there (`agent run --new --agents` in
a shell on the host) shows in the window.

Not built: reading a host's files (step 2, in [NEXT item
41](NEXT.md)); an app-level heartbeat beyond SSH keepalives; `Include` with
wildcards in a directory; a conditional `Include` (under a `Host` or
`Match` that is not every host); a login shell that takes `-l` only alone
(tcsh); ending what a crash left before the next launch (macOS has no
parent-death signal for the master to follow).

## Projects and panes

The shell follows the "Agent App Concepts" prototype (NEXT item 47). The
daemon learns nothing about projects; everything here is client work.

- **Projects.** A project is a folder, its coordinator bot `<project>.lead`,
  and `.agents/project.toml` (name, coordinator, model; mechanics only). The
  sidebar lists every coordinator in the store as a project, with its tasks
  under it: the coordinator's `created_by` lineage, plus any root bot named
  `<project>.<task>`. Bots in no project follow. **＋ New project**, under
  Home's list, takes a
  folder and a model from `~/.agent/models` under its provider's name (the
  last one picked comes first), reads its `project.toml` (unknown keys are refused) or names the
  project after the folder, creates the coordinator there with the folder's
  own client policy, and then writes the file if there was none, so a model
  the daemon refuses is never saved. The file goes in through a temporary
  and a link, so it is never partial and never replaces one. An existing
  coordinator is opened if it works in that folder (and the file it lacks is
  written with its model); one in another folder is a name collision,
  reported and not opened.
- **Navigation.** Home, then a tab for each agent opened full screen, on a
  bar of floating tabs. The list on the right holds one level below what is
  open: at Home the projects and the bots in none, in a project its threads
  and swarms, in a thread what it made, in a swarm its agents, and in a swarm
  agent what it made; each row
  counts the rows one further down. A click on a row looks in beside, in
  place of the list, with its own composer (a beat later, so a double-click
  can claim it); a double-click, or "Open as tab" in its ⋯, opens a tab. The
  double-click is the click the system counts as second, so it holds when the
  look has already replaced the row, and it opens the tab from where the
  window was before the first click. Every move (rows, Home, tabs, crumbs,
  the finder, keys) goes through one function that first cancels a row's
  pending look, so any later click or key wins over it. A
  task card in a chat opens beside the same way. ⤢ Full screen takes the tab
  on screen a level down; from Home it opens a tab. The crumbs in the head
  (Home › project › thread) go back up, and the window's title says the same.
  An agent already in a tab is that tab. A closed tab hands the window to the
  one before it, and the first to Home; a deleted agent's tab goes up to what made it. Tabs, and the
  agent beside, are saved with the bot's id and come back only for that
  identity, never for a new bot under the old name. ← or `Esc`
  closes what is beside. One index of the fleet, every bot in tree order with
  a swarm's agents and what they made under its row, is rebuilt when the
  fleet's shape changes; the list's levels, the finder (⌘K) and the arrow
  keys read it. The list draws a window of rows cut from it in one pass over
  the open agent's subtree, so a large level costs a screenful of rows.
- **Composer.** The model chip lists `~/.agent/models`, read on each open,
  each provider under its own heading.
  Models of any provider in the bot's family (known from the fleet's bot
  records) switch the next turns (sent as `submit`'s `model`); other
  families, and providers no record places, show disabled as "new agent",
  since a bot keeps its family. Send starts a turn on a bot at rest; on a
  working bot it queues, steers or asks a side chat, as picked last from
  its ▾. A steer names
  no model or workspace, so it joins the running turn, and it names that
  turn (`expected_turn`): if the turn ended meanwhile, the daemon refuses it
  as `stale_turn` and the message stays in the composer. A model pick is
  remembered for the bot's identity, not its name. Unsent text belongs to
  the bot it was typed for: a pane that shows another bot puts it away and
  brings back that bot's own, a closed side pane keeps it, and Enter sends
  it to the bot it was typed for even if the pane is already switching.
  Drafts live for the window's life and go with a deleted bot.
- **One menu per agent**, from the head's ⋯, a sidebar row's or card's ⋯ on
  hover, or a right-click: side chat, stop, fork (an exact copy of a bot at rest in its
  folder, next to it in the tree, opened beside), delete (confirmed), and every run's
  thoughts and output. An open menu follows its bot's status. A sidebar row
  is one line; its glyph and the pane head say what it waits on.
- **Runs.** Thinking, tool calls and their output between two messages fold
  to one line: the call in progress with its clock, or the tools used, and
  any failure. A click opens a run or unfolds one long output.
- **Side chats.** A side chat forks a bot, running or not, with no
  checkpoint, so the daemon copies it at its newest finished round; the
  source is untouched. The copy is named `NAME-side`, nests under its
  source, and opens beside. From the ⋯ menu it opens empty; from Send's
  side pick, the message is its first. It has its source's tools and works
  in its source's folder, so it can edit there while the source runs: it
  is the same agent asked something else at the same time (George,
  2026-09-27). The fork names no tool list and no folder.
- **Tasks in worktrees.** This is the app's opinion, not the CLI's or the
  daemon's. A coordinator the app creates is started in the `coordinator`
  profile: the folder's `.agents/agents/coordinator.md`, the user's, or the
  one the app ships ([coordinator.md](../app/agents/coordinator.md)), whose
  model and tools apply when the project names none. The shipped text
  says: the coordinator makes a small, quick change itself (a few lines it
  can check at a glance) and sends work in an area an existing task owns to
  that task with `--delivery queue`; a new task that changes files, named with the
  project's prefix so projects do not collide, gets
  `git worktree add -b agent/NAME ~/.agent/worktrees/NAME HEAD`, the
  folder's `.agents/setup` run inside it, and `agent run --new --agents
  --workspace` (or `--profile ROLE` when a listed role fits) that worktree,
  in the project's subfolder of it; the task
  keeps that folder for later messages. The text also says the worktree
  starts at the last commit, that a failed setup, or a start that left no
  bot, removes the worktree and branch,
  and that without git every task works in the project folder. The daemon only runs a
  turn where the bot is, or where a message moves it. A bot in a
  linked worktree shows its branch after its name in the head, read once
  from the worktree's files when the head is first drawn.
- **Swarms.** Also the app's opinion; the daemon learns nothing new. A
  project's ⋯ menu has **New swarm**: a goal, any number of agents up to 64 (typed), a mix, where
  they work (one worktree they share, `~/.agent/worktrees/PROJECT.NAME` on
  `agent/PROJECT.NAME` with the folder's `.agents/setup` run in it, or the
  project folder), and a token budget typed in millions (0.1 to 1,000),
  split evenly among them, each agent's share shown beside it. The default is
  **10 million per agent** (40 million for four), scaling with the agent count
  until the user edits the total. Input is counted again on every call,
  including cached input. Smaller explicit totals remain allowed. A goal is
  at most 16 KiB. The swarm is named after the goal's longest telling word
  among its first six; a name a bot holds (or its agents' names), or whose
  folder, worktree or branch another swarm or store holds, is skipped for
  the next, `-2`, `-3` and so on. Setup gets ten minutes and the login
  shell's ordinary variables (PATH, HOME, USER, LOGNAME, SHELL, LANG, LC_*,
  TMPDIR, TERM) and no others, so no provider or cloud keys (as do the
  swarm's git commands, whose hooks run too; this process's own variables
  stand in only when there is no login shell); past its time
  its whole process group is killed, and its output is kept only to its
  last 64 KiB. A failed start removes a worktree and branch only when it
  made them, never the project folder. The mix is up to eight rows of an identity, a model and a
  share, the shares adding up to 100%. An identity is a profile in the
  folder's or the user's `.agents/agents` other than the app's own
  `coordinator` and `swarm`; picking one picks the model its profile names,
  when that model is connected. The agents are dealt one at a time, each
  to the row furthest below its share of the agents so far, so any prefix
  of them keeps the shares as well as whole agents can (the council's seats
  mix too), and the sheet shows each row's count, or that a row makes none
  at that size. Start, Add and Stop are one call each to the app's Rust
  side, which names the swarm, deals its agents, makes or stops everything
  and undoes a failed start, so the
  page only shows the outcome: a swarm whose files or instructions cannot
  be made, or none of whose agents can be created, is removed with its
  worktree and branch; an agent that could not be made or briefed beside
  others that were is reported by name.
  The swarm is a folder, `~/.agent/swarms/STORE/PROJECT.NAME/`, where
  STORE is the store identity the daemon announces when a window attaches,
  so a window sees only its store's swarms, whatever socket reaches it:
  `swarm.toml` (its project, goal, folder, budget, mix, members with each
  one's bot id and row of the mix, whether you stopped it, the highest
  number an agent of it was made under, and the bot ids of members that
  left; at most
  1 MiB, never written past it (an Add that would is refused and its agent
  deleted), read only when every member has an id and a row, changed only
  under the board's lock, synced, and replaced whole; written last when
  a swarm is made, so a folder without it is not listed), `board.jsonl` (one
  line a post or act, appended under a lock and synced, your goal first;
  each line says how many agents it was `sent` to, so what a
  swarm's posts cost in deliveries can be read off its board; the line is
  written before the sends, and a send that failed is in the poster's answer), `state.json` (what the board adds up to: roles, open and approved
  proposals with their votes (at most 16 open at once, and an approved
  stream closes when nobody is in it any more), the last 16 denied ones, a
  count that numbers the next, who is in which stream; a decided proposal's
  vote reasons stay on the board only). A change to it commits with its lines: the new state
  is written beside it as `state.pending.json` with the board's length
  before and after the lines, the lines are appended and synced, and the
  file then replaces `state.json`; the next act under the lock, or the
  next read of the board, finishes a change whose lines are all on the board and otherwise cuts the board back
  to where it was and drops the change. Then scripts that run the app's own
  executable with `--swarm-post`: `post`, `role`, `assign`, `claim`,
  `submit`, `review`, `finish`, `leave`, `status`, and with a council
  `propose`, `vote` and `join` (replaced whole when the app moves). Its agents
  are ordinary bots named `PROJECT.NAME-N`, each created with its row's
  model and its share of the budget, and started in the mode's profile:
  [swarm-flat.md](../app/agents/swarm-flat.md) or
  [swarm-council.md](../app/agents/swarm-council.md). Each uses the folder's
  override, then the user's, then the shipped file. An agent with an identity
  starts in that profile, with the selected mode's text after its own and the
  identity's tools, which must include `shell` since the board's scripts
  run in it. Each joins the swarm once created, and then gets a first
  message naming it, the goal, the others with their identities, the board
  and its scripts (and, with a council, the seats). A card and a post show
  an agent's identity, and its model when the swarm has more than one. A post is written to the board, then
  steered into the agents it reaches over one daemon connection: an
  ordinary publication does not interrupt anyone. `@NAME` delivers only to
  named members, waking them if idle. `post --all` explicitly wakes all other
  members. Your post wakes everyone, or only the agents it names. A post holds
  the board lock until delivery completes, and Stop holds the same lock, so a
  post either precedes Stop or is refused afterwards; your post resumes a
  stopped swarm. A post is at most 16 KiB and comes only from a member, named
  by its shell's `AGENT_BOT`, `AGENT_BOT_ID` and `AGENT_TURN`; the daemon records
  it as each steer's author. Every steer pins the member's bot id, so a bot
  deleted and recreated under a
  member's name is not a member: a post misses it, and the app keeps it in
  the sidebar and out of the swarm's cards and counts; a deleted agent
  leaves its swarm when the app sees it go, taking its share of the budget,
  its role, its stream, its votes on open proposals and the open proposals
  it made (the board keeps them all); its helpers still count in the
  swarm's tokens and Stop still ends them (`swarm.toml` keeps its bot id
  in `left` until a look at the daemon's list finds nothing it made). A
  stream it led goes to the first agent left in it (a `lead` line on the
  board, and that agent is told, never one leaving with it); an agent that
  moves up into its council seat is named on the board (a `seat` line) and,
  when proposals are open, told it holds one, with them. A stopped swarm
  tells neither, and a send that fails is shown. A new agent always takes a
  number no agent of the swarm had, so a name on the board is only ever
  one agent's. A swarm is one sidebar row
  under its project (⁂, working while any agent works); its agents are not
  in the sidebar. Its view has two tabs: **Board**, read from where the
  last read ended whenever one of its agents does something durable, and
  only while it is on screen (more than 256 KiB behind, it reads the
  board's last 256 KiB instead; lines and state are read together under a
  shared lock, so they always agree, and a budget check that adds a line
  reads it too); and **Agents**, their cards, which open
  beside. The head counts working agents and tokens used against the
  budget, its helpers' tokens included. Its composer posts to the board.
  Its ⋯ menu stops every agent and helper
  (agents first, then the helpers they made, looked for again until a look
  finds none it has not stopped and no agent given a turn meanwhile; every unfinished turn, queued ones first, of each member that is still
  the bot that joined; a name now held by another bot leaves instead; and
  the swarm refuses its agents' posts until your next one the board takes; that post
  resumes the swarm before it goes on the board, so a failure between leaves it
  running with nothing new rather than your post on a stopped board)
  or, unless it is stopped, adds one from the row furthest below its share, told to read the board first,
  whose share of tokens the swarm's budget grows by. New agents join and
  get their briefs under the board's lock, so a Stop from another window
  comes first and they are refused (and deleted), or waits and ends the
  turns their briefs started. An agent whose brief did not arrive is
  deleted with its share, the agents briefed with it hear that it left,
  and a start none of whose briefs arrived starts nothing. A swarm every
  agent left takes no new one (`swarm_empty`); start another. A swarm's budget is what the agents it made
  were given: a start where some could not be made has the budget of those
  that were. Any agent says what it is doing with
  `role ROLE`, shown on the board and on its card. A helper is a bot an
  agent made with the CLI (a fork of a peer to ask it something, or of
  itself for a subtask), named after its maker (`PROJECT.NAME-N.WHAT`) so
  it sorts in the swarm's range of the daemon's list; one made by a member
  or by another helper counts in the swarm's tokens and stops with it, and
  cannot post. The daemon forgets a deleted bot's tokens, so each act that
  reads its list keeps each helper's tokens in `state.json` (`helpers`), and
  a helper deleted since counts on (`gone`) with what that look saw it use;
  what it used after that look is not counted. It keeps where each helper
  comes from too (`roots`), so a helper whose maker was deleted still
  counts and still stops, as long as a look saw its maker first; Stop keeps
  what each of its looks saw, so a Stop tried again after one that failed
  still finds such a helper. Each act that reads the daemon's list, and a check the page
  asks for on **usage events during model/tool work**, tell the working agents
  at 50%, 65% and 80% of their allowances. A check lists the swarm's bots and
  takes its board lock, so the page asks for one only once the tokens its
  usage events report (input, cached included, plus output) since the last
  check reach a twentieth of a member's allowance, or when a turn finishes;
  each warning lands within five points of its mark. Checks are coalesced
  over 250 ms with one in flight per swarm, including offscreen swarms and
  descendants, and a check with nothing to say writes nothing.
  Both total and individual allowances are checked: idle peers cannot hide
  a worker running out. Individual notices name exact used/remaining tokens
  and target the observed running turn; a stale notice cannot wake a finished
  agent. The role starts reporting at 65% and keeps the final 20% for review
  and synthesis. This reserve is a work instruction, not a second allowance
  or a relaxation of the runtime cap. Checks require an attached app or a
  board operation; `status` also exposes remaining allowances. Checks return
  `board_changed` when they append an entry, so the open board refreshes for
  budget and stall notices without extra reads after unchanged checks. A call can
  cross a threshold before delivery, and the runtime's existing admission
  check can allow one call to overshoot its token limit.
- **Work and results.** The board is the shared place for deliverable decisions,
  changes of approach, progress and results, all visible in the app. There is
  no separate planning file. The separately editable `swarm-flat` and
  `swarm-council` Markdown profiles define how agents derive the deliverable, completion evidence, work split and handoff
  owner from the request. It asks for a concise board agreement before work,
  revised through board posts as agents learn. No task-type enum or output
  template is prescribed. Under the default profiles, the first member posts
  the agreement with every piece's owner and reviewer and registers those
  assignments itself, so peers wake to work already theirs; other members
  explore briefly and wait for it. Council votes on those assignments assess
  the agreement with its pieces. These work
  habits live in the profile, not in generated launch or budget messages.
  `assign TASK OWNER REVIEWER BRIEF` is available to every member and records
  distinct work with an independent reviewer. Each owner has at most one
  unfinished task. `claim TASK` checks ownership atomically under the board
  lock. In council mode each assignment becomes a proposal and the owner
  cannot claim it until approved. `submit TASK RESULT` preserves evidence,
  leaves the owner's stream and wakes its reviewer. `review TASK
  supported|conditional|rejected EVIDENCE` is accepted only from that reviewer
  and closes the stream. The profile requires checking the actual output
  against the board agreement, including integration when relevant.
  The swarm chooses its handoff owner on the board; any member can publish
  `finish achieved|partial|failed SUMMARY`. It records one current final result with
  `outcome`, `summary`, `by` and `at`. The summary can contain outputs or point
  to them, with evidence and remaining gaps. The profile asks agents to review
  the work before claiming `achieved`; `finish` records their judgment without
  enforcing that workflow. It accepts a handoff with unfinished work or no
  assignments. A rejected hypothesis might still satisfy an investigation,
  so verdicts do not mechanically determine task success.
  The swarm judges completion against the request; the harness records that
  assessment, not automatic certification. An updated `finish` replaces the
  current result when its outcome or summary changes; earlier handoffs remain
  on the board. An identical repeat is refused without another notification
  or write. Continued work can therefore turn a partial handoff into an
  achieved one without creating another assignment. Finish does not cancel running
  turns. The profile asks the handoff owner to coordinate before finalizing.
  These checks establish authorship and lifecycle, not truth or sufficiency
  of evidence. `leave` releases actual membership; a working task becomes
  assigned again. Members can revise released assignments or recover a
  departed member's task; council revisions require fresh approval. A
  submitted task whose reviewer departed keeps its result: assigning a new
  reviewer hands it over for review without new work or approval, even if
  its original owner also left. Keep that original owner in the assignment;
  authorship and the submitted result are preserved. A new
  assignment clears the current final result, retaining its board history.
  A swarm with one member needs another for independent review.
  The Work/Streams view shows partial results, verdicts and the final outcome.
  The `status` script is callable by the coordinator or a terminal without
  impersonating a member. Its JSON includes member states,
  remaining budgets, tasks, partial results, `result`, last board activity and
  the top-level `outcome`: `running`, `partial`, `blocked`, `failed`, `stopped`,
  `budget_exhausted` or `completed`. `budget_exhausted` means the swarm's
  total is spent or every member is at its cap; a helper at its own cap, or
  one member out while others work, is not, and `exhausted_members` names
  members at their caps. Here `completed` means a final handoff was
  published, regardless of whether agents still run; `result.outcome` states
  whether the goal was achieved, partial or failed. Without a handoff, `result`
  is null and task outputs remain available. Finished bot turns alone never
  establish task success. Task constraints remain instructions; no
  write-prevention policy is introduced.
- **Swarms a coordinator starts.** The app writes `~/.agent/swarms/start`
  each time it opens, a script that runs the app's executable with
  `--swarm-start` and no window, as the board's scripts do. It takes
  `--agents N` (4), `--budget MILLIONS` (an explicit total; otherwise 10 per agent), `--council 3`, `--in-project`
  (else one shared worktree) and any number of `--row MODEL,SHARE[,IDENTITY]`,
  then `-- GOAL`; without rows every agent is a plain agent on the
  coordinator's own model. It runs only in a coordinator's shell (its bot is
  `PROJECT.lead`, the id its shell names still that bot's), reaches the
  daemon that shell belongs to, starts the swarm in the coordinator's
  project and folder through the same Rust start as the sheet, and prints
  the swarm, its agents' names and its board's path. Without rows its
  agents get the model of the coordinator's current turn (`AGENT_MODEL`).
  The start runs in a process group of its own, so a shell that gives up
  on it (its timeout, or its turn ending) cannot cut it off between making
  a worktree and undoing it; the role asks for a ten-minute shell timeout.
  When the goal's name and eight more are all taken it makes nothing. The app's
  [coordinator.md](../app/agents/coordinator.md) says when and how to run
  it. A window learns of a swarm it did not start when an agent it does
  not know, named like an agent (`-N`), takes a turn: it reads the swarms
  again once for a burst of those, and at most once for each such name.
  `swarm.toml` keeps the starting coordinator's name and bot id, and the
  coordinator hears from its swarm by a queued message starting
  `[swarm NAME]`, which never interrupts its running turn and names the
  status script: `finish` sends the final result, and a check that finds
  nothing running (helpers included) and no final result says so once,
  again only after something has run since. Only the page's budget checks
  can find a swarm quiet (an agent acting is running), so a quiet swarm is
  reported only while the app is attached. A swarm you start from
  the sheet has no coordinator and tells nobody.
- **The roles as files.** Settings lists the app's `coordinator`, `swarm-flat` and
  `swarm-council` roles and whether you have your own file for each. Edit writes
  `~/.agents/agents/NAME.md` from the app's text only when it is missing
  (whole beside it, then linked into place),
  then opens it with `open -t` (`xdg-open` elsewhere). The file is the
  user's profile of that name, read in every folder that has none of its
  own. A bot's instructions are fixed when it is made, so an edit applies
  to coordinators and swarm agents made afterwards, including members added
  to an existing swarm. Flat and council roles are independent files; neither
  appends the other mode's instructions.
- **Swarm councils.** The sheet's "Organized as" picks one board (peer-owned
  pieces with independent review) or a council of 3, which needs at least three
  agents; a start that made fewer than three deletes them (naming any it
  could not) and starts nothing. With a council, the seats are the swarm's
  first three agents; when a seat is deleted the next agent takes it, and
  only the votes of the seats as they are now count, still 2 of the
  council's 3 however many seats are filled. An agent proposes a stream of work with
  `propose STREAM WHY`, which wakes the other seats; a seat votes with
  `vote ID yes|no REASON`, once. A majority of the seats (2 of 3) approves or
  denies; an approved assignment puts its owner in the stream and wakes that
  owner. A standalone proposal uses its proposer as lead. A denied proposal
  wakes only the proposer; a denied assignment is withdrawn, freeing its
  owner for other work, and wakes both its owner and its proposer. You decide any open proposal alone
  from the **Council** tab. `join STREAM` puts an agent in an approved
  stream (one at a time). An agent in a stream posts to that stream: the
  post carries its tag but is silent unless it names a recipient;
  `post --all` explicitly notifies all peers. Reviewers do not join the task
  they review. Your untargeted posts reach everyone. The
  head gains **Council**, with the open count, and **Streams**, the approved
  ones with their lead and agents and their roles; a tag filters the board.
- **Not built yet.** Keep, which turns a side chat into a task, removing a
  deleted task's worktree, deleting a swarm, and approvals are later steps
  of item 47.

`python3 app/playground.py` starts a daemon on a synthetic streaming model
and opens the app on it; prompt prefixes (`shell:`, `bg:`, `delegate:`,
`fanout:`, `slow:`, `hold:`, `limited:`, `md:`) drive tool calls, delegation,
waits and pacing with no provider. `--no-app` prints the attach command
instead.

## Coordinators hear from their tasks

Work goes on in the tasks a coordinator started, by its own asks and by you
working in them directly, and the coordinator should hear about it. The page already
follows every bot, so it tells it; the daemon has no part in this beyond
recording who asked for each turn (`from` on `accepted` and `queued`).
A turn counts when it ends in a bot the project's `PROJECT.lead` created,
unless the coordinator is waiting on that very turn (its `turn_waiting`
names the handle, so its `wait` reads the reply) or the bot is the
coordinator's own fork or side chat (`PROJECT.lead-…`). A turn of such a
task that starts waiting for an approval (`turn_waiting` with `approval`)
counts too, since only you can give it; the coordinator's role raises it
to you. A
steer's turn is part of the turn it joined. Turns replayed on attach are
history, not news; one that ends live while the window is still reading the
list of bots is held until the list says who made its bot, and dropped if the bot is
deleted meanwhile or the window detaches first. A queued turn's
`accepted` names its author again, so a window attached after the `queued`
event was pruned still knows the coordinator asked for it. The coordinator is told only while it rests (nothing is armed while it
works), at most
once every ten minutes, in one message queued to it: `Task updates:`, then
one line per task with its latest ended turn's handle and status (or
`waiting for approval`), who asked for it (`you` for the coordinator's own
ask, a bot's name, the app's `origin` such as `schedule`, or `the person`), and how many turns ended before it
since which handle. Turns you asked for in a task yourself are listed
last, under a line saying they are yours; the role tells the coordinator to
leave them to you rather than check or correct them. Turns anyone else asked
for, and approvals, are listed first; a turn's end is listed with its
pending approval, so an approval you already answered is not raised. Each list keeps its own first and
latest handle and count per task, so a task in both is named in both, each
time by the handle that list needs. The handles are what its
`wait` tool reads a final reply by, so the message stays small however much
was said. The page keeps only that per task (first and latest turn, a
count), so a long coordinator turn or a failing daemon cannot grow it. One
message names at most 32 tasks; the rest wait for the next, and it says how
many. The
[coordinator role](../app/agents/coordinator.md) says what to do with it:
send another task only what it needs, with `agent run --detach --delivery
queue`, which it reads at its next turn without being interrupted, and
otherwise answer in one line. A message that fails is kept for the next
one, and one due while the window was detached goes out when it attaches
again; a coordinator deleted, or gone when the window reattaches, has its
dropped. The window must be open for it. The message's `request_id` is made
from the coordinator's id and a hash of each task's newest turn in each
list, its status and approval call ID. Separate approvals and completion in one turn are distinct,
while two windows with the same news make one turn: the daemon answers
the second with the first, or with `idempotency_conflict` when that window
counted from an earlier turn, which it takes as told. A task deleted before
its news goes out is dropped from it.

## Triggers

A trigger wakes an agent with a message when something happens: a time
comes, a file is written, a repository's HEAD moves, or someone fires it by
name. The message is a new turn in the agent's own conversation; a trigger
may also start the agent it names on its first fire. launchd watches, so a
trigger fires with the app closed, and a time the Mac slept through fires
once when it wakes (`StartCalendarInterval` coalesces missed times;
`StartInterval` and cron skip them). The daemon has no clock or watcher of
its own, and no process of a trigger runs between its fires: while nothing
happens a trigger costs its plist and launchd's watch, and no model call.

The app writes `~/.agent/trigger` each time it opens, a script that runs its
executable with `--trigger`:

```sh
~/.agent/trigger add [--name NAME] [WHEN] [--bot NAME | --start NAME --model PROVIDER/MODEL [--effort LEVEL]] [--reply-to BOT] [--if CMD] [--runs N] -- MESSAGE
  WHEN: --every 30m | --in 45m | --at 'YYYY-MM-DD HH:MM' | --cron 'MIN HOUR DAY MONTH WEEKDAY' | --file PATH | --commit REPO
~/.agent/trigger ls [--after NAME]
~/.agent/trigger fire NAME
~/.agent/trigger rm NAME
```

Every reply is one JSON value on stdout; a failure is one
`{"error": CODE, "detail": ...}` on stderr with exit 1.

**When.** `--every` counts from the next whole minute in minutes that
divide an hour, hours that divide a day, or `1d`, and waits at least one
full interval before its first message; calendar ticks before that earliest
time do nothing. `--in` and `--at` are one-offs within a year, which remove
themselves once fired (`--in` rounds up to the next whole minute, since
launchd keeps minutes; `--at` refuses a part out of range or extra parts);
`--cron` is read as cron reads it, a day or a weekday when both are given,
up to 1,024 calendar entries. `--file PATH` fires when the file is written,
or a file is added to or removed from it when it is a folder (launchd's
`WatchPaths`); it need not exist yet, and it may not be in
`~/.agent/triggers` or be its daemon's store (or its `-wal` and `-shm`, by
any name, a hard link included), which every fire writes: `add` refuses one, and a fire that finds its path
became one (a link moved) ends the trigger, sending nothing and keeping a
`failed` row that says why. A shell that
names its daemon's socket and not its store cannot add one, since its store
is not known to check. `--commit REPO` sends when a commit was made: launchd wakes it on any write
to the repository's own HEAD log, which git writes on every move of HEAD,
and the fire asks git one question, `git rev-list -n1 --since=LOOKED NEW
--not OLD`: whether HEAD now reaches a commit the HEAD it last saw did not,
committed since it last looked. A commit, an amend, a merge, a cherry-pick,
a rebase that rewrote commits, or a pull of work committed since sends; a
checkout, a reset, or a fast-forward to commits that were already there
sends nothing, and the fire keeps where HEAD went. A HEAD the repository no
longer has (it was made again) counts as unknown. `add` records where HEAD
is, a repository with no commit yet included, before launchd watches, and
when a commit came in between asks for a fire that looks as launchd would
(a `wake.` ask), so the commit is sent once. A repository where git keeps
no HEAD log (`core.logAllRefUpdates` false, or a bare one by default) is
refused, and adding the same trigger again watches the git folder the
repository has now, recording where its HEAD is in place of the old one's.
git is the one on the `PATH` `add` ran with, which the plist keeps. With no WHEN, only `fire` runs it; `fire` while a fire still runs is
sent by that fire once it is done.

**Whom.** `add` defaults to the agent whose shell runs it (`AGENT_BOT`,
which must still match `AGENT_BOT_ID`); `--bot` names another. Either is
pinned by its id: a bot deleted since, or a new bot under its name, is not
reached (`bot_not_found`), and the trigger ends. `--start NAME --model
PROVIDER/MODEL [--effort LEVEL]` names an agent that does not exist yet:
the first fire makes it as the app makes an agent (the daemon's `create`,
in the folder `add` ran in, with that folder's composed policy and the
default tools), made by the agent that added the trigger when one did, so
it shows under that agent, and gives it the fire's message. Its id is kept with the trigger's
state when the fire settles, and later fires message it. A name an agent already has is refused at
`add` (`bot_exists`), and an agent of that name made before the first fire
makes that fire fail, naming it. The agent that added it is pinned by its
id too: once it is deleted, `create` fails with `creator_not_found`, and the
trigger ends, keeping its row, as when its `--reply-to` agent is gone. The `create` carries the trigger's own
request id, so a fire cut short after the daemon made the agent asks again
and gets that same agent, and no other.

**What else.** `--reply-to BOT` keeps the fire's process until the turn it
sent ends (the daemon's `wait`, up to a day), then queues that turn's
answer to BOT, pinned by id; launchd starts no second fire of the trigger
meanwhile, so a repeating one skips the times that turn spans. The fire's
request id then ends `-to-ID`, BOT's id, so the app does not also tell
that agent of the turn as a task update, unless that answer has not
reached it 15 s after the turn ended. A fire cut short while it waits
(the Mac restarted) passes no answer on: nothing resumes that wait. An
answer the daemon cut short is
marked so in its first line; a turn that ended saying nothing passes on an
empty answer. One that does not get through keeps an ended trigger listed,
saying so, and one whose BOT is gone (`bot_not_found`: pinned by id, it
never comes back) ends the trigger. `--if CMD` runs `sh -c CMD` in the folder
`add` ran in, with the `PATH` `add` ran with, before anything else, for up
to 60 s, in its own process group, which ends with it; any exit but 0
skips that fire and records nothing but, for `--commit`, that its commit
was seen, so a heartbeat whose check finds
nothing to do costs one process and no model call. A one-off whose check
says no ends, listed as not sent.
`--runs N` ends the trigger once N messages went out; a fire after that
(its end could not unload it) only tries to end it again. Each message the
agent gets starts with one line, `[trigger NAME · YYYY-MM-DD HH:MM · why]`,
the local fire time and what fired it (its time, `file PATH`, `commit REPO
at SHA`, or `fired`), so a catch-up fire after sleep reads as late; an
answer passed on by `--reply-to` starts with the agent and turn it is from.
Submissions carry `origin: "trigger"`, and a passed-on answer carries
`from: {bot, turn}`, the turn it is; coordinator updates carry `origin:
"tasks"`. All are automated input, not human consent for the approver.

**Adding again.** A trigger is named after its agent unless `--name` says
otherwise. `add` with a name in use and the same definition changes nothing
and returns that trigger with `"duplicate": true`, so a retried `add` is
safe; with another definition it is refused as `trigger_exists`, whose
`field` names the first that differs (`when`, `bot`, `start`, `message`,
`reply_to`, `if`, `runs`, `commit`, `dir`, `daemon`, `store_id`): `rm` it
first. A name differing from another only in case is refused
(`name_taken`), since macOS folders would give both one file, and `rm` finds
a trigger only by the name as stored; `rm` of a name not there is
`trigger_not_found`. `ls` returns 64 triggers per page, with `next_after`
for `--after NAME` or the next page in Settings; only the current page's
messages are retained. `fire NAME` asks for a fire and returns
`{"name", "fired": true}`; what the fire did shows in `ls`. An ask is a file
in the trigger's queue folder, `~/.agent/triggers/NAME.asks`, which its plist
names as launchd's `QueueDirectories`: launchd runs the job while an ask is
there, one run at a time, and runs it again when a run ends with one still
there. Each ask is one fire: the fire moves the oldest out of the queue into
`NAME.taking`, which launchd also watches, sends whatever its time or
watched path, queues behind work rather than skipping it, and removes the
ask once done; one a fire was cut short on is the next fire's, and its
message has the same `request_id`, named by the ask, so the daemon takes it
once. A queue that cannot be read, or an ask that cannot be moved out of it
or removed once done, would have launchd run the job for ever, so the
trigger ends, saying why (`asks_stuck`). A trigger that goes sets its asks aside with its files until launchd unloads its
job, and puts them back when it will not. Nothing else starts a fire:
launchd is the only thing that runs one.

Each is one LaunchAgent, `~/Library/LaunchAgents/me.lydakis.agent.trigger.NAME.plist`,
and that file is its definition: its program arguments carry the agent and
its id (or the agent to start, its model and effort, and who added it), the
store and the socket of the shell it was made from (an agent's shell has
both), the store identity its daemon announced, the one-off's time, the
repository, the folder, the reply agent and its id, the gate, the run limit,
and the message. A fire whose daemon announces another store (a reused
socket) sends nothing and records `store_mismatch`. When it fires, the app's
executable runs with `--trigger-fire` and those arguments. It connects to
the daemon, and when none answers and the store is known, starts one for it
on that socket as the app does, with the login shell's environment and
`~/.agent/env`. A repeating trigger submits with `delivery: reject`: a bot
that is working, or has work waiting, skips that fire rather than having it
cut in or pile up. A one-off, and a fire `fire` asked for, submits with
`delivery: queue`, so a working bot gets it after its turn. A one-off's
calendar entry has no year, so a fire more than two days before its time
does nothing (the slack keeps a one-off whose Mac changed time zone since,
since launchd follows the new zone's clock), and one more than half a year
after it is that entry's next year: it sends nothing and ends as `missed`.
What the fire did (`sent` with the turn and any `reply`, `skipped`, `gone`,
`missed` or `failed` with why), with the messages sent so far, the agent it
started and the commit it saw, is kept in `~/.agent/triggers/NAME.json`,
which Settings shows beside each trigger with its message, a Run now button
and a Remove button. Triggers are local to this machine. Remote windows
neither list, run nor remove local triggers.

A trigger's state is three things: its plist, launchd's loaded copy, and
that state file. Every change keeps them either whole or as they were, and
anything else a failure can leave is listed and removable:

- **Adding** writes the plist and loads it. A load launchd refuses removes
  the plist again. A plist there already, readable or not, is never
  replaced: `rm` first. Each installation has a generation ID, and listing
  and fires only use a state file of the current generation, so a trigger
  made again under an ended one's name starts afresh.
- **Firing** records its result and ends a trigger that is over (a one-off,
  one whose agent, `--reply-to` agent or, for `--start`, adding agent is
  gone, one that reached `--runs`), both under the lock
  and only while the plist is still the one it fired for: a trigger
  removed while its message went out is left as it now is. A one-off that
  delivered leaves nothing. One that ends without delivering (its agent
  gone, the daemon unreachable, `missed`) loses its plist but keeps its
  last result, so Settings and `ls` list it as not delivered, and why,
  until it is removed. When that result cannot be written, the plist stays,
  listed, rather than ending with no trace. The plist goes before the
  unload, since the unload ends the fire's own process. It and the result
  are set aside by rename (`.NAME.retiring`), whatever their size or
  contents, and deleted once launchd lets the job go; a file that cannot be
  set aside keeps its job loaded, and an unload launchd refuses renames
  them back, so either stays listed with what its fire did. A fire whose
  unload ends it before it deletes them leaves them aside; the app's next
  start, or the next `add` or `rm` of that name, finishes that end. The
  app's start also removes the temporary of a write that died before its
  rename: every write takes the lock, so none is under way. A job
  left loaded after its plist went (an end cut short) is unloaded by its
  next fire.
- **Removing** unloads the job by its label whether or not its plist is
  there, then deletes the plist and the state file, so it reaches an ended
  row, a plist launchd no longer has, and a job loaded without its plist
  alike. An unload launchd refuses keeps everything, to be tried again.
- **Listing** shows a one-off still there two days after its time as
  `missed: true` (launchd did not run it, as when the Mac was off, or its
  end was cut short), and a plist that cannot be read as a row with its
  `problem`, which `rm` removes.

The files are written, synced, renamed and their folder synced; deletions
and set-asides sync their folder too. `add`, `rm`, `fire`, a fire's result and end, and
the app's refresh take a lock (`~/.agent/triggers/.lock`) around their
changes, and the refresh reads each plist again under it.
`~/.agent/trigger` is written with its executable mode from the start. When
the app starts from a new place, as after an update, it writes its path into
every trigger and loads it again, on a thread of its own so the window does
not wait; one launchd refuses keeps its old path and is tried again at the
next start. Settings lists triggers also when no project exists. Only macOS
has launchd; elsewhere `add` refuses with `triggers_unsupported`.

Earlier apps called these schedules (`me.lydakis.agent.schedule.NAME`).
They are not converted: remove them by hand (`launchctl bootout
gui/$UID/me.lydakis.agent.schedule.NAME`, then delete the plist and
`~/.agent/schedules`) and add them again as triggers.

The app ships an `automation` skill
([SKILL.md](../app/skills/automation/SKILL.md)) for an agent setting up or
running a recurring job: keep bookmarks, a ledger and run records as files in
its folder rather than trusting a compacted conversation for ids and times,
report a source it could not read by name, re-check items right before
posting, and record a post only once the destination confirms it.

## Skills the app ships

Agents read only skills in a folder's `.agents/skills` or in
`~/.agents/skills` ([client policy](CLIENT.md)). The app bundle carries its
skills, from `app/skills/NAME/`, in `Contents/Resources/skills/NAME`, and on
every start the app links each one from `~/.agents/skills/NAME`
([skills.rs](../app/src-tauri/src/skills.rs)). Updating the app updates what
the link points at, so nothing is copied or recorded; a moved app re-points
its links at its next start, and a skill it stops shipping loses its link.
The Homebrew cask runs `agent-app --setup` after an install or upgrade, which
writes what a start writes (scripts, `~/.agent/trigger`, these links) before
the first window, and `agent-app --unlink-skills` before an uninstall, which
removes this bundle's links and nothing else, so none outlives the app. The app's links are those into a copy of it (a bundle with
`Contents/MacOS/agent-app`) or into an app since removed; a folder, file or
link of yours at that name, another app's skills folder included, is left
alone, and a folder's own skill of the same name wins over it. To change a shipped skill, replace the
link with a folder of your own: the link leads into the signed app, which is
not yours to edit. Agents already running keep the index they were created
with.

## Messages

What a model writes is drawn the way a page would draw it. A message is
Markdown (GitHub's flavour, with a line break wherever the model wrote one),
parsed by [markdown-it](https://github.com/markdown-it/markdown-it) once and kept with the item, so a
pane drawn again reuses it; when highlighting loads, only messages whose code
waited for it are drawn again (a pane with none keeps what it drew), and a file beside only when it shows as code
(a page, diagram or chart there keeps running). A table past 256 columns or 10,000 cells shows
as its source, as a short row is padded to the header's width and a few bytes
a row could ask for millions of cells. A message past 50,000 lines, or one
that would draw past 100,000 tags, shows as its text, as a `- x` line makes
an element from four bytes. One with more than 100,000 marks that open an
inline element (`*`, `_`, a backtick, `[`, `!`, `<`, `~`, `|`, `@`, `\`,
`&`, `www.`, `://`) is not parsed either: the parser's tokens for a single
line of `*x*` cost far more than the HTML they become. The parser was chosen
for how its time grows: marked, used first, took 217 ms on a line of 10,000
`!` and 3.2 s on one of 40,000, while markdown-it 14.1.0 grows with its
input on every run of markers tried (`!`, `![`, `[`, `*x`, `_a`, backticks,
references), its slowest being about 0.6 s for 100,000 marks of `![` in
Node on this container, and a test holds it to that. A link's target and title are
copied into the page once per use, and a reference defined once can be used
thousands of times, so a message's links and images carry at most 1 Mi
characters of targets and titles in all, counted as written into the page
(escaped, and an image's twice); past that a link is its text. A
streamed reply's blocks share those bounds (marks parsed included) and the highlighting budget below,
as do the text blocks of one stored message on either side of its tool calls,
and a later block is drawn anew when an earlier one's share changes (as when
highlighting arrives); past them the rest shows as text. Fenced blocks are drawn by their language:

- **Code** is highlighted with [highlight.js](https://highlightjs.org) (its
  common languages) in the window's own colors, with a copy button. A block
  names its language or is left plain; nothing guesses. Blocks over 64 KiB
  stay plain, and a message or file highlights at most 256 KiB of code in
  all.
- **`mermaid`** draws as a diagram with [Mermaid](https://mermaid.js.org),
  themed to the window. In a message it opens as its source and draws with a
  click on **diagram**: Mermaid lays out on the window's thread, and a few
  characters (`block-beta` with `space:500000`) can hold it for minutes. A
  diagram that does not parse stays as its source with the parser's message
  in the block's head.
- **`vega-lite`** (or `vl`, or `vega` for a full Vega spec) draws as a chart
  with [Vega](https://vega.github.io), as static SVG in the window's colors:
  an eight-hue categorical order validated for color-vision deficiency
  against the panel, a single-hue ramp, recessive axes. A single view without
  a width fills the block. The spec is its only data: the loader refuses every
  URL (data and images alike), and expressions run in Vega's interpreter, not
  as generated code. A chart in a message opens as its spec and draws with a
  click on **chart**: Vega draws on the window's thread, and a few characters
  of spec (a `sequence` transform to a billion, a billion ticks) can ask it
  for more than it can draw. While a reply streams, its diagrams and charts
  (and pages and SVG) show as code; each can be drawn once the reply is in.
  Nothing remembers a click: a block drawn again (its pane redrawn, or its
  message drawn anew for highlighting) is code until clicked again, which
  draws it from the cache at once. One that fails to draw shows its error and
  asks again before it is tried again. One in a file opened beside is
  measured once the pane has finished opening.
- **`html`** opens as code, and runs as a preview in a sandboxed frame only
  when asked: a click on **preview** runs it, a click on **code** stops it.
  A preview's scripts share the window's thread (a frame is not a process),
  so a page a model wrote never runs merely by being in a reply. Running is
  asked of one block, once: a preview is code again when its pane is drawn
  anew. **`svg`** likewise opens as code and draws as an image (no script, no
  network) with a click, as its filters and animations also take the
  window's thread; a `data:` SVG image in Markdown shows as its text.

Charts are a block of their own because a preview cannot load a charting
library: models reach for one from a CDN, and a preview fetches nothing.
Vega-Lite is a spec, not a program, so it needs no network and runs no
model-written script in the window. Mermaid's own `pie` and `xychart-beta`
also draw, and an HTML preview can still draw a chart with its own inline
SVG or canvas.

Highlighting, Mermaid and Vega load the first time something needs them;
markdown-it loads with the page. Text, lists, tables and code draw as a message
arrives; diagrams, charts, previews and images draw only when asked, so
opening a long chat runs none of them, and a reader below a block that
draws keeps their place. A file beside is not
scanned by the once-a-second clock of running turns. Mermaid's own limits (50,000 characters, 500 edges) do
not bound its layout work, which is why a diagram waits for a click. Drawn
diagrams and charts are kept by source (a chart also by its width), so a
second click draws at once: at most 64 and 8 MiB. A
message's drawn HTML counts toward the chat's 8 MiB of decoded bodies, and
so does what it puts on the page and what parsing it cost, 40 bytes for each
tag it draws and 16 for each mark parsed: a window holds about 200,000 drawn
tags and 500,000 parsed marks however its messages split them, so many
replies each within their own bounds never add up to more. Drawing past that
folds the oldest bodies as a load would. A chat off screen keeps its drawn
HTML only while the most recently shown ones hold 16 MiB of it in all; past
that the oldest let theirs go and parse again when shown, so visiting many
agents never adds up. All are vendored
under `app/ui/vendor` (versions
and licenses in `LICENSES.txt`), so drawing a message fetches nothing.

A file opens beside the chat, in the pane a task opens in, from a path a step
read, wrote or edited (the path in its line) or a message's link to a path
(`[plan](PLAN.md)`, `src/a.rs:12` or `a.rs:12`, `src/a.rs#L4`, `README.md#install`; the
line or section is dropped). A path is the agent's folder's, and a link
inside an open file is relative to that file. The core reads the first 4 MiB of a regular file (`read_file`; a
FIFO or device is refused, as reading one need not end, and a window on a
host is refused by name, as its files are the host's). The file draws by its
kind: Markdown, a diagram (`.mmd`, `.mermaid`) or a chart (`.vl.json`,
`.vg.json`), drawn at once, a page (`.html`, opened as its preview), an SVG
(its root after any declaration, comments and doctype) or image (opening the
file is the asking), a CSV or TSV as a table of its
first 1,000 rows and 256 columns, ending with the row that reaches 10,000
cells (quoted fields kept whole, and the view says when rows were left out),
a binary file as its size, anything else as code highlighted by its
extension. Esc or ✕ closes it (Esc too from inside a page it shows, which
hands the key to the window) and brings back the task that was beside, if
any. A write or edit to the open file reads it again, and what the agent
wrote is new: a page, diagram, chart or image in it waits for a click, as in
a message. A file drawn again (rewritten, or highlighted once highlighting
loads) keeps the reader's place. One that failed or was refused changed nothing, so the file stays
as it is shown. A click in the file puts the keyboard in the chat's composer,
as the pane beside has none while a file is open. Deleting the agent it came from, or attaching to another store,
closes it. Searching a project's
files (from ^k or elsewhere) is not built.

What a model writes never becomes the app's markup unparsed. Raw HTML inside
Markdown shows as text. Links open in the default browser and only for `http`,
`https` and `mailto` (the core's `open_link` refuses anything else); other
links show as their text. A link is drawn inert, its target data that
only a click reads, and offers no context menu, so nothing the web view does
natively (Open Link, a middle click, a drag) follows it in the window. A link inside a drawn diagram (a Mermaid `click`
link) goes the same way and never navigates the window. A chart's `href` drew
no link in Chromium, as Vega's string renderer passes it through the loader,
which refuses every URL; one that did draw would go the same way. Images draw
only from raster `data:` URLs the message carries, and only on a click, as a
small image can decode to far more than its bytes; a remote image is a link
and a local one opens beside, so drawing a message makes no request a model
chose. An HTML preview runs scripts in a frame sandboxed without same-origin
access: it cannot read the app or its storage, cannot navigate the window, and
the app's script globals are injected into the main frame only. Its page
carries a policy that loads nothing from the network (no fetch, scripts,
styles, images or fonts but its own inline ones and `data:`), and the window's
policy (`frame-src about:`) stops a preview from navigating its own frame to a
website. A preview runs only once asked, so a long transcript holds no idle
pages. The window's policy also takes images, fonts, media and stylesheets
only from the app itself, `data:` and `blob:`, so a library drawing a message
cannot fetch one either: a Mermaid node's `img:` URL or a `url()` in its theme
CSS is refused, and the diagram names the failure in its head.

Measured 2026-10-10 at 023fd3e with `node app/bench/render.cjs`, in headless
Chromium 141.0.7390.37 on a 4-core cloud container: three runs, each the
median of nine (synthetic messages: prose, lists, a table, and Rust in every
third one). Drawing 400 messages (292 KiB) costs 50 to 52 ms of parsing the
first time (14 to 16 ms of it markdown-it's, 22 to 23 ms highlighting),
against 5.5 to 5.6 ms for the line renderer this replaced; drawn again, a
message costs no parsing, as its HTML is kept on its item. Putting those 400
into the page and laying them out takes 261 to 392 ms, against 121 to 164 ms
for the old renderer's markup under its own stylesheet (read from b7bdfe4),
2.0 to 2.4 times as long, as its HTML is larger (653 KiB against 425 KiB)
and its code highlighted. This container's layout times vary widely: at
a038530, with marked, seven runs had the new page at 0.55 to 3.6 times the
old, 1.3 times at the median. Parsing a whole 9 KiB reply again on each of
its 1,121 deltas would cost 0.83 to 0.87 s.

Streaming is measured with each delta paying what the app's render does
around it: reading whether the reader is at the bottom, keeping them there
(a layout per delta), and for the new tail hydrating the blocks that delta
finished. That 9 KiB reply in 8-character deltas (44 finished blocks) costs
58 to 61 ms in all, against 318 to 358 ms for the old tail, one text node
that grows and is laid out whole on every delta.

While a reply streams, each block that has ended (a paragraph after its blank
line, a fence once it closes) is drawn once and appended; only the block still
being written is plain text. Each delta reads only its own characters for
block ends, so a long reply costs its deltas, never its length on each one.
The durable message replaces the streamed tail with one parse of the whole.

Verified 2026-10-09 in headless Chromium 141 with the app's page: an
` ```html ` block whose script tried `parent.document`, `top.location`, a
`fetch`, a remote image and navigating its own frame got none of them, and
`window.__TAURI__` was undefined inside it. Three charts, one loading its
data from a URL, one drawing a remote image and one whose expression reached
for `constructor`, made no request; the third did not draw and named the
interpreter's refusal in its head. That the Tauri core injects its
scripts into the main frame only was read from tauri 2.11.5's source
(`for_main_frame_only: true`), not observed in the macOS webview. A page
whose script loops forever left the window responsive while it was not
asked to run, and so did a `block-beta` diagram with `space:500000` until
its **diagram** was clicked (drawn, it held Chromium past 110 s even at
`space:1000`). A Mermaid image node and a theme `url()` pointing at a
website were refused by the window's policy and made no request. A Vega
spec with a `sequence` transform to a billion left the window responsive in
view, as it draws only when asked. A diagram fenced inside an opened
Markdown file still waits for its click.

## What it costs, and where the bounds are

The UI bounds payload buffering, history decoding, and rendered fleet rows:

- **Attach** replays events from the page's cursor, which on a first start is
  the beginning of the daemon's retained log. That log is bounded by the
  daemon's retention (each bot's `retain_turns`, `prune`), and a `pruned` notice marks
  the gap. Nothing is staged on the way: the page pulls the replay a batch
  at a time and applies each before the next, so the transport's 4,096-event / 8 MiB encoded
  queue is the buffer between the daemon and the screen, and pulls also stop at 1 MiB (plus one event). A page
  slower than the fleet is told it lagged and attaches again from its
  cursor. The snapshot is paged (256 bots a request) while the replay flows;
  a bot the replay already spoke of keeps the state those events built and
  takes only the record's static fields, a bot an event names before its
  record arrives gets a seat at once, and a listed bot nothing mentioned is
  seated from its record. Session tombstones keep late snapshot pages from
  restoring a deleted identity; touched identities take precedence over stale records.
- **Transcripts** keep at most 1,200 decoded entries and an 8 MiB serialized JSON budget
  (measured as UTF-16 strings) around the reader's window (up to 400 entries of count hysteresis).
  Eviction folds whole durable nodes, tool rows and process cards into ranges
  with only endpoint IDs, rather than retaining an object for every old node.
  Scrolling loads at most 400 references in either direction, including a fork's
  inherited history after its source is deleted. Snapshot heads seed older
  lineage even when activity events were pruned. Overlapping ranges are unioned,
  so one node cannot be decoded twice around interleaved peer cards. Bodies are
  fetched with `history_items`: one ancestry validation per requested batch,
  a 768 KiB reply target, and at most one larger item within the frame limit.
  An item exceeding the transport limit returns an item-specific error, while
  the rest of the batch stays readable. Turn identities survive pagination.
  Completed process results keep their
  ordinary output row, including stdout and stderr; cards show only a summary.
  Event-only activity notes outside the window collapse to an explicit count.
  Thinking yields to answer text as soon as answer deltas arrive, and partial
  streams are discarded at turn end unless a durable message was committed. Counters for
  thoughts, long outputs and peers are updated with the items. Peer cards compact
  from 601 to the newest 300 with a count of earlier peers; older bots remain
  reachable through the switcher. Both creation and fork events insert creator
  peer cards, and deletion removes them.
- **Rendering** rebuilds the window's HTML only on a structural change (a
  load, a fold, another bot). Items appended since the last render are added
  on their own; a tool finishing or a process ending replaces its own line; a
  streamed delta appends plain text to the tail, and a block it finishes is
  parsed once and appended; a durable message is parsed once and its HTML kept
  with the item (counted in the window's decoded bytes); the once-a-second clock refreshes the
  elapsed spans and peer cards in place. The rail shows a window of 300 rows
  around the selection, extended by scrolling to an edge; its tree is rebuilt
  when the fleet's shape changes (a bot created, forked or deleted) and a
  status change replaces that bot's own row. The picker shows at most 200
  rows, and the activity check behind the clock is cached per fleet change.
- **Creation** costs no request: the `created` and `forked` events carry the
  record's list fields, so a burst of ten thousand bots is ten thousand
  events, not ten thousand `resume` round trips. An event missing its provider
  field is a protocol error, with no legacy `resume` fallback. Lineage is the store's
  `created_by_id`: a bot links under its creator only while the bot holding
  that name is the identity that created it.
- **Sessions** count up; an event from an older session is dropped, a
  submission waits for a known bot id and carries that identity. A bot that
  reappears under a known name with a new id starts from nothing. A lagged stream (the core's
  event count or 8 MiB byte budget filled) closes the transport, fails every request made
  after that at once, and the page attaches again from its cursor, from
  outside the event chain so the attach cannot wait on itself. A new attach
  lets the previous session's socket go first, so a pull still waiting on it
  comes back closed rather than holding the new session up. A borrowed event
  receiver retains its session owner and cannot be returned to a replacement
  attachment, even while that attachment has its own pull pending. Canceled
  requests release their pending registration immediately. Cancellation during
  a partial socket write closes the session before another request can write.

## Verified

2026-09-29 swarm coordination update: the UI state suite covers budget defaults,
usage-triggered checks while turns are running, coalescing, partial work and
review rendering. Rust tests cover exclusive claims, durable partial results,
council approval, separate authorship for review, and completion. Synthetic
real-daemon tests exercise targeted delivery and the `assign` → `claim` →
`submit` → `review` → `finish` lifecycle, including external `status` reads.
These validate the contracts, not the quality of real-model collaboration;
a matched real-provider comparison remains in the queue. No installed app or
existing bot instructions were changed by this source update.

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
bundle, so it could not be screenshotted here) and a real provider.

The tree uses the daemon's `created_by` (bots created from a shell tool since
schema 22, with the creator's identity since 23) or the `created` event; a bot
without a creator, or whose creator's name has since changed hands, is a root.

`/new` gives a bot the shared client policy ([CLIENT.md](CLIENT.md)); the
create notice says what went in. An oversized or unreadable AGENTS.md or skill
refuses the creation with the CLI's `--agents` error rather than creating the
bot without it. It also supplies the same default compaction
instructions as the CLI, so app-created bots can summarize older context.
Completed thoughts retain locally observed thinking time; historical thoughts
without a recorded duration show no invented time.

On 2026-09-28 a swarm was started from the sheet in demo mode in headless
Chromium: its agents posted, a post naming one woke it alone, and an agent
opened beside from the board, with no page errors. The post tool itself runs
against a real daemon in `tests/test_swarm.py`: an agent's post reaches the
agent working, wakes the idle one it names and no other, and is refused for a
bot that is not a member or while the swarm is stopped. A council swarm's
proposal wakes its three seats, a second yes opens the stream and wakes the
proposer, a non-seat's vote is refused, and a post from a stream carries its
tag and reaches nobody outside it (same test file), and each post's line
records how many agents it was sent to. A coordinator's `start` runs from its
shell against a real daemon there too: two agents on its model in its folder
with half the budget each, briefed, the board in that store's swarms, and
the script refused from a bot that is not a coordinator. Start, Add and Stop are tested in
`app/src-tauri/src/swarm.rs` against a stand-in daemon: each agent gets its
row's model, identity and share of the budget, one that fails is reported
while the rest start, a start with no agent leaves nothing behind, an
identity without `shell` is refused before any agent exists, Add skips a
name another bot holds, and Stop ends unfinished turns newest first,
its helpers' and their helpers' too, one made after its first look
included, but not a bot no agent made, and
lets a reused name leave; the board says 50% and then, past 90% at once,
only 90%, each once, counting a helper's tokens. The council was also
driven in demo mode: roles, two proposals, one approved by the seats, a
stream two more agents joined, and the other left open for you.

On 2026-09-29 the host connection ran against real OpenSSH 9.6 on Linux,
with `sshd` on the same machine and a synthetic account holding a release
`agent`: an unknown host key, a refused key, no `agent` on PATH and no
provider each gave their reason; then the master connected, `agent start`
ran in the login shell and named the socket and home, the forward carried a
`bots` request, a killed master was replaced on the next connect, a daemon
shut down on the host was started again through the same forward, and
closing removed the control and forwarded sockets. The Tauri crate built and
its tests ran on Linux; the macOS app was not run, so no window on a host has
been seen by eye.

On 2026-09-27 the shell was driven in demo mode in headless Chromium:
projects and tasks in the sidebar, a card opened beside and swapped, the three
menus, fork, confirmed delete, folding and a new project, with no page errors.
A task's runs rendered while it worked matched a full redraw of the same pane.

On 2026-10-09 the navigation shell was driven in demo mode in headless
Chromium (Home, a look beside, a project and a thread in tabs, the finder,
crumbs back up) with no page errors, and measured ("here": the tree committed as
49c1ca6, which records these numbers) against main (169cf40) on one synthetic
fleet: 40 projects of 25 threads, one thread each with 120
rounds, five page loads per build, the same machine. Window ready (attached,
rows drawn): p50 802 ms on main, 198 ms here, since Home draws 40 rows where
the tree drew a 300-row window; DOM nodes after load 3155 and 572; JS heap
5.2 and 5.3 MB. ⌘K to a thread's newest message on screen, first visit p50
26.4 / p95 37.8 ms on main and 23.2 / 32.4 here; between two visited p50
25.4 / 39.4 and 24.5 / 26.5. A tab click between two visited threads: p50
31.7 / p95 32.1 ms. The macOS webview was not measured.

On 2026-09-29 (Linux container) a schedule's fire ran against a real daemon
in `tests/test_schedule.py`: it sent its message to its resting bot as a new
turn, skipped the bot while a turn held it, gave a one-off to a working bot
after its turn, started a stopped daemon on the socket the schedule was made
with, ended with a visible row when its bot was made again under the same
name, and did nothing a year early; `add` from an agent's shell was refused
without launchd and left no plist. The plist, calendar expansion and the
app's move are tested in `app/src-tauri/src/schedule.rs`, and so is each
lifecycle step above against a stand-in launchd that tracks which labels
are loaded and refuses loads and unloads on demand: after every refusal the
plist, the loaded job and the last result are checked together. The coordinator's
task updates are tested in `app/tests/state.test.cjs` and were driven in demo
mode in headless Chromium. The real-launchd test passed on a Mac (2026-09-29, at 2e484ed): launchd
fired the one-off at its minute and it ended itself; replace, `rm` of a
plist launchd had dropped, `rm` of a job without its plist, and a last `rm`
answering `schedule_not_found` all held. Schedules have since become
triggers: that module and test are now `trigger.rs` and `tests/test_trigger.py`. After the
rename (2026-10-09, Linux container) the same real-daemon tests passed under
the new names, and the conversion of earlier schedules passed against the
stand-in launchd: both converted, the running one's old job unloaded last,
its result and an ended one's moved, an unreadable plist left in place.
The same day, file, commit and fire-by-name triggers were added and passed
the real-daemon tests too: a message starts with its fire line, a commit
trigger fired again on the same HEAD sends nothing and on a new commit sends
again, and `add`'s idempotence, `fire`'s ask and the `WatchPaths` plist are
covered against the stand-in launchd. A file trigger firing on a write and
`fire` through the `QueueDirectories` queue need a Mac (`AGENT_TEST_LAUNCHD=1`).
Then `--start`, `--reply-to`, `--if` and `--runs` passed against a real
daemon: a started agent made once in the trigger's folder under the agent
that added it, then messaged; a second trigger for that name refused at its
fire; a turn's answer queued to the reply agent with its line; a gate that
exits 1 costing no turn.

## Next

1. Run it against a real daemon and model by eye; fix what the screenshot
   shows.
2. Refuse a `~/.agent/env` that a macOS ACL makes readable by other
   accounts; today only its POSIX mode is checked.
3. Stop a daemon the app started for a store other than `~/.agent`'s when
   the cask is uninstalled; the uninstall hook stops only the default one.
4. The rest of the projects design (the "Agent App Concepts" prototype), in
   the order [NEXT item 47](NEXT.md) gives.
5. Remote workspace reads, step 2 of [Hosts over SSH](#hosts-over-ssh):
   the client library's reads (policy compose, project file, branch, model
   list) as `agent` subcommands that print JSON, run over the host's
   ControlMaster, so `/new`, New project and branches work on a host.

## Regression checks

Run `node --test app/tests/state.test.cjs` for malformed tool arguments,
reconnect serialization, historical process results across batches, whole-node
eviction, a 10,000-peer fan-out, incremental text/thinking rendering, tool-row
windowing, CSS control-character escaping, creation-event validation, concurrent
submission IDs, fork-history paging, snapshot/history ordering, history paging
past activity summaries, pinned submission identities, oversized-item isolation,
compaction policy propagation, creation refusal when the workspace policy
cannot compose, completed thought timing, and the shell: projects from
coordinators and lineage, the list one level below what is open, crumbs back
up, rows that look in beside and open tabs on a double-click, tabs opening,
closing and following a deleted agent up, full screen from beside,
per-pane sends with the sticky queue or steer pick, model choices within a
provider, the agent menu's enabled items and its refresh on a status change,
fork naming and placement, side chats (a running source, the allowed list,
the first message going to the copy), a worktree bot's branch in its head,
project creation (no file for a refused model),
steers pinned to their turn, model picks pinned to identity, the demo
daemon's steer delivery, a message's sender (another agent's, live, steered
in, queued and read back, unlinked once its name holds a new agent, and the
app's own task updates and triggers by origin), and runs
folded with failures on their line. Also covers coordinator task updates: batched at rest, excluding requested and replayed turns, retained after send failures. Messages: Markdown with raw HTML, unsafe links and remote images kept out, fenced blocks drawn by language, charts, file links, files drawn by kind, each message parsed once, and streamed blocks drawn once with fences kept whole. `cargo test -p agent-app link_tests` covers which links the core opens and how it reads a file.
`cargo test -p agent-app` includes a failed project-file write leaving
neither a partial file nor a temporary, and triggers' calendars, plists,
watched paths, idempotent `add`, `fire`, `--runs`, `--if`, the commit check,
move, and each lifecycle step with launchd refusing. With `AGENT_TEST_RUNTIME=1` after a release build
and `cargo build -p agent-app`, `python3 -m unittest tests.test_trigger`
fires triggers against a real daemon; on a Mac, `AGENT_TEST_LAUNCHD=1`
adds its one launchd test, which loads real jobs (under a scratch `HOME`, so
nothing loads at the next login) and checks that launchd fires a one-off,
which ends itself, fires a file trigger on a write (the same `add` again
being that trigger, a different one refused naming `when`), runs `fire`
by name, and that `rm` removes a job whose plist or load is already gone.
App tests also cover hosts over SSH against a stand-in
`ssh` that runs the remote command here and forwards by linking: `~/.ssh/config`
aliases, includes and quoting, `ssh -G` read to its bound, the ssh arguments, `agent start`'s answers, attaching
through the master and again after it is killed, a host printing without end
cut off at 1 MiB, an `ssh -G` cut off with what it started, a refused login and a
missing `agent` reported and backed off, an older daemon replaced through the
host's own `agent shutdown`, one app process per host, a window on a host
never starting a local daemon, and the connection lifecycle: after opening, a
master killed from outside, a store replaced, the last window closing and
quitting, the process table and `~/.agent/hosts/` hold only what that step
leaves; a crash's master is retired, and its files swept, by the next launch;
quitting closes every host at once; a master flooding its stderr, and an ssh
that times out or exits, end with their groups. Discovery bounds cumulative directory entries as well as bytes, and discards
truncated config lines. Option probes only omit explicitly unknown options;
other failures are retried instead of cached. Stale hosts retire with bounded
concurrency under one deadline. A test runs the real `ssh -G`
with a config that sets the opposite of every forced option and checks each
still takes effect. `a_real_hosts_connection_leaves_nothing_behind`
(ignored; `AGENT_TEST_SSH_HOST`) runs the same lifecycle against a real host. The page tests cover saved state keyed by
store, another store answering on reattach followed from its start (with
the new host's home replacing the last one's), a host window's home and what it leaves out, and the Hosts list.
`cargo test --workspace` includes the silent-listener readiness deadline,
fork workspace parity between durable records, live events, and replay, and
the app's policy errors for oversized and unreadable AGENTS.md files.

A synthetic local debug-build probe with a 100,000-node in-memory history read
the same 400 older items in three matched runs: individual ancestry checks took
33.8–34.1 seconds; `history_items` took 87–88 ms. The returned items were identical.
This measures the ancestry-walk reduction, not an end-to-end fleet capacity claim.

Pulled event batches apply in order, with one visible-history load and render
per batch. Creation/fork bursts rebuild the fleet tree at most once per pull,
while retaining the 300-row rail window. The shared client rejects a ready
handshake unless its protocol is exactly `agent_client::PROTOCOL`, now 5.

The lifecycle regression suite compares committed thinking/answer transcripts
between live delivery and replay, reconciles fork snapshot/replay ordering,
and rejects late process/history replies from a replaced session or bot.
Disconnect drops non-durable stream buffers; committed nodes remain the source
of transcript content.

On reconnect, an advanced snapshot head invalidates the older decoded history
cache and reseeds it from durable lineage. This recovers messages whose activity
events were pruned while disconnected; newer replay nodes and ranges are kept.
The rail slides a fixed 300-row window in either direction and anchors an
overlapping row to preserve scroll position. Only selection or fleet-shape
changes recenter it.

Snapshot lineage also restores peer cards after creation events are pruned,
including children listed before their parents. Anthropic content blocks keep
their stored order in both live and replayed transcripts. Ready submissions
retain their turn ID so Escape can cancel work waiting for a runtime slot.
