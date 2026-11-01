# Security Policy

This policy covers Agent, the daemon and `agent` CLI in this repository,
the `agent-client` crate in [client/](client), and [Sonny](sonny/README.md),
the terminal client built on them. Agent and Sonny release together under
one version tag.

## Supported Versions

Agent and Sonny are experiments with no users yet. Security fixes land on
`main` and ship in the next release. Earlier releases are not patched
separately; upgrade to the latest tag.

## Reporting a Vulnerability

Report vulnerabilities privately through
[GitHub private vulnerability reporting](https://github.com/lydakis/agent/security/advisories/new).
Do not open a public issue for an undisclosed vulnerability.

Include the affected revision, the access the attack needs, its realistic
impact, and the smallest reproduction you can give. Leave out real
credentials, tokens, transcripts and unrelated private data.

## System and Scope

Agent is one daemon per person that runs many model-driven **bots**, each a
durable conversation with tools: a shell, file reads, writes and edits,
pictures, `web_fetch`, and whatever a client declares. Programs drive it
over a Unix socket beside its SQLite store (`~/.agent/state.sqlite.sock`
by default) with a JSON Lines protocol; the `agent` CLI is one such
program. The daemon holds the conversation history, calls the model
providers, runs the tools, and keeps the approval gates a client sets.

Sonny is a client of that socket. It makes a lead bot for a folder and
tasks under it, runs the daemon's companions (the automatic approver, the
lookout, the tool host), schedules triggers with the system's scheduler,
keeps memory, skills and the commons under `~/.agents`, reaches the
person's MCP servers, and drives a browser. Everything Sonny adds runs as
the person, on the person's machine.

What this means for anyone running them:

- **Bots act with your permissions.** A bot's tools run as your user on
  your machine. A sandbox narrows what a bot's commands and writes may
  touch; nothing isolates the daemon itself, and nothing it runs is a
  different user.
- **Models read untrusted text.** Tool output, fetched pages, other bots'
  messages, MCP results, lookout sources and files in a checkout all reach
  the model. A model can be talked into acting on them. The sandbox, the
  approval gates and the marks on outside content limit what that can do;
  they do not make a model immune.
- **Conversation history is kept in the clear.** The store holds every
  turn, tool call and result, and it is not encrypted. Protect
  `~/.agent` as you protect your shell history and your keys.

## Threat Model and Security Invariants

The following properties must hold. A revision that breaks one is a
vulnerability, whether or not the code's own documentation agrees.

### The daemon and its socket

- The daemon listens on a Unix socket only. It opens no TCP port and no
  WebSocket listener; the WebSocket in [docs/WEBSOCKET.md](docs/WEBSOCKET.md)
  is an outbound provider transport. The one TCP listener in the
  repository is `agent login chatgpt`'s sign-in callback, bound to
  `127.0.0.1` in the CLI process for the minutes the sign-in takes.
- Anyone who can connect to the daemon's own socket has full control of
  every bot: the daemon checks no peer identity and no token on it, by
  design. The daemon binds the socket `0600`, whatever the umask, by
  setting the umask around the bind, so there is no instant in which the
  name exists with a looser mode. `~/.agent` is made `0700` when the
  daemon or the CLI makes it; one that already exists keeps the mode its
  maker gave it, so on a shared `$HOME` check it yourself. When the
  store's path is too long for a socket address, the socket moves to a
  folder under `/tmp` that is made `0700` and refused unless the caller
  owns it alone.
- A sandboxed bot's commands never reach the daemon's own socket. Each
  such bot gets a socket of its own (`SOCKET.d/BOT_ID`, in a folder made
  `0700`) that answers as that bot and refuses what would widen its
  confinement: `configure`, `answer`, `tool_result`, `approve_step`,
  `step_result`, serving approvals, `companions`, `resume`, `shutdown`,
  creating or forking a bot it does not own, submitting as the person or
  another bot, and deleting or interrupting a bot it did not make. The
  folder of those sockets is read-only inside every sandbox, so no bot can
  put a listener in another's place. A command that runs outside the
  sandbox, allowed to escalate or by an `allow` command rule, gets the
  daemon's own socket.
- The daemon collects and sends no usage analytics. Its outbound
  connections are to the model providers a bot's model names, the ChatGPT
  sign-in, and the pages `web_fetch` is asked for.

### Sandbox

- A bot made with `sandbox: read-only` or `workspace-write` runs its shell
  commands, writes and edits under bubblewrap on Linux or Seatbelt on
  macOS, with the mode names, writable folders and profiles ported from
  Codex ([sandbox](docs/RUST_PROTOTYPE.md#sandbox)). Reads stay open in
  every mode: a confined command can read any file the person can,
  `~/.agent/env` and the store included, and its model provider sees what
  it reads.
- A sandboxed bot never runs unconfined for want of a sandbox: creating one
  fails with `sandbox_unavailable` when bubblewrap or `sandbox-exec` is
  missing or cannot make its namespaces. Bubblewrap is found at fixed
  system paths or the one a package build fixed, never on `PATH`.
- Inside a writable folder, `.git`, `.agents` and `.codex` stay read-only
  and cannot be created, and `.aws` stays read-only where it exists, since
  git runs hooks and config from there, agents trust what is there, and an
  AWS profile can name a program to run. A git folder a bot may commit to
  keeps its hooks, config, modules and `commondir` read-only. The daemon's
  store, the files SQLite keeps beside it, its socket and the bots'
  sockets are read-only inside every sandbox.
- A write or edit is checked by the target's real path and then carried
  out inside the sandbox, so a symlink swapped in after the check cannot
  carry it outside. A protected folder that is a symlink refuses the call.
- Network is off unless the bot's `network` is set. Off, a bot reaches
  only the Unix sockets its `unix_sockets` names, since a program on a
  socket runs what it is asked outside any sandbox: Seatbelt holds
  commands to those paths on macOS, and on Linux a seccomp filter refuses
  every Unix socket a command would make, so a bot that names any reaches
  them all. With the network on, a bot reaches every Unix socket but the
  daemon's and the other bots', as Codex's full network does. The Linux
  filter also refuses `io_uring` and foreign-architecture system calls.
- A call that would leave the sandbox (a shell call asking
  `require_escalated`, a write outside the writable folders) waits on the
  bot's approval gate and runs unconfined only when allowed. With no gate
  it is refused. A command runs outside without asking only when an
  `allow` command rule, which the bot's client gives it (Sonny's are in
  `~/.agent/rules`), matches every command in a
  plain script; any script with substitution, redirection or an unmatched
  command stays inside. A `forbidden` rule refuses a command even when it
  escalates, and a `prompt` rule makes it wait on the gate, or refuses it
  without one, even when the sandbox would confine it. The rules folder
  is read-only inside every sandbox (`serve --protect-path`), so no bot
  widens its own rules; rules its client couldn't read refuse every
  command.
- A bot made or forked by a sandboxed bot inherits its creator's sandbox
  and writable folders, may only narrow them, reaches the network only if
  both do. It goes by its creator's command rules, and can't be given
  others.
- A bot made with `danger-full-access`, or with no `sandbox`, runs
  unconfined with the caller's full permissions. That is the daemon's
  default; Sonny's default is a sandbox.
- Only `shell`, `write`, `edit`, `apply_patch` and `write_stdin` are
  confined. `read`, `history` and `web_fetch` run in the daemon itself,
  so `web_fetch` works for a bot whose network is off.

### Approvals

- The daemon holds the mechanism and no policy: a gate names tools whose
  calls wait for a verdict from whoever serves its tag. A call needs an
  allow from every gate, the first deny denies it, and who decided is
  recorded on the call. A gate's expiry denies a call nobody judged. The
  daemon's default is no gate.
- Only a bot's client can set or replace its gates; a sandboxed bot's own
  commands cannot. A fork keeps its source's gates, and a bot another bot
  makes takes its creator's.
- Under a sandbox, only a call that leaves it is judged; a confined call
  runs without a verdict. Sonny's served tools wait for one too, since
  they act outside the sandbox.
- Sonny's automatic approver (`auto`, the row a new lead starts in) sends
  every gated call to a judge model with Codex's guardian policy and
  denies what the judge calls dangerous or unclear, what it could not
  assess, and a round the judge never answered. Known secrets in what the
  judge is shown are redacted before they leave the machine. Three
  denials in a row, or ten among the last fifty, trip a breaker that
  interrupts the turn. The judge reads `AGENTS.md` files as the person's
  standing instructions, so a checkout's `AGENTS.md` is as trusted as the
  checkout.
- Any agent can pull the andon cord (`pause`), which cancels running turns
  and ends background processes and is attributed to the agent that pulled
  it; only a client on the daemon's own socket can `resume`.

### Credentials and secrets

- Provider keys come from the environment or `~/.agent/env`, which Sonny
  writes `0600` and refuses to read when group or others can. Nothing
  writes a key into a shell profile or into `config.toml`.
- The ChatGPT sign-in keeps its tokens in `~/.agent/chatgpt.json`,
  written `0600` through an exclusive temporary file, and refreshes them
  itself. Codex's `~/.codex/auth.json` is read only to see that a sign-in
  exists; its tokens are never read out. Bedrock uses static `AWS_*`
  variables or `aws configure export-credentials`. No keychain is read.
- A bot's shell gets the daemon's environment without the keys its
  providers use, and the provider keys and ChatGPT tokens are redacted
  from what tools return, as are values after fields named like
  `password`, `api_key` or `secret_access_key` and the bodies of PEM
  private-key blocks. Redaction matches the exact value in tool output
  only; an encoded copy, or a secret in model text, is not caught.
- Other secrets in the daemon's environment reach a bot's shell unless
  `[shell_environment_policy]` in `config.toml` leaves them out: as in
  Codex, `*KEY*`, `*SECRET*` and `*TOKEN*` are withheld only with
  `ignore_default_excludes = false`, and `exclude` names more. The
  daemon's companions (Sonny's approver, lookout and tool host) inherit
  its environment whole, provider keys included.
- A secret found while importing an MCP server goes to `~/.agent/env`,
  never to `config.toml`. Sonny's home backup leaves `~/.agent/env`,
  logins, the store and any `config.toml` that holds a secret off the
  remote repository.
- The daemon never logs a credential's value, and this repository holds
  none. A custom `--provider` spec may name an `http://` URL; the key then
  travels in the clear to it.

### Durable state

- The store, `~/.agent/state.sqlite`, holds every bot's history, prompts,
  pictures, tool output, artifacts and approvals in the clear. The daemon
  makes it `0600` before SQLite first opens it, so the `-wal` and `-shm`
  SQLite makes beside it take that mode too, and tightens a store or
  sidecar an earlier daemon left looser. The daemon's log, the
  companions' folder and logs, and what the lookout keeps beside the
  store (its log, cursor, lock and marker, the sources', subscriptions'
  and goals' folders) are made `0600` and `0700` the same way. There is
  no encryption at rest. Tool output spilled to files goes in folders
  made `0700`, as `0600` files.
- `rm`, `prune` and `--keep-turns` delete rows; SQLite's free pages and
  WAL are not scrubbed. Compaction keeps the original history.
- A resume never silently makes a fresh bot. A fork is a new identity; a
  bot's continuation has one owner.

### Untrusted content

- A message one bot sends another reaches the model with a line naming the
  sender, and a lookout source's event arrives with the lookout's own
  origin, never as the person's words. A tool's result, a fetched page
  and an MCP result reach the model as tool output.
- `web_fetch` fetches only `http` and `https` URLs, follows at most ten
  redirects, reads at most 4 MiB, and sends no credentials. It does not
  refuse private, loopback or link-local addresses: a bot with
  `web_fetch` can read what the daemon's host can reach on its own
  networks, with the bot's network on or off. Set
  `web_search = "disabled"` to leave it out of new bots.
- Sonny marks a memory fact or commons post saved in a turn that read
  outside content through web search, `web_fetch`, the browser or an MCP
  server as `external`, and its cleanup removes a fact that reads as an
  instruction to agents. Content a shell command fetched (`curl`, `gh`, a
  cloned repository) is not marked.
- Skills, roles, `AGENTS.md` and memory are instructions the model reads.
  Sonny writes its own only under `~/.agents` and leaves alone any the
  person edited. A folder's `AGENTS.md`, `.agents/skills`, `.agents/agents`
  and `.agents/setup` are read from the checkout and are as trusted as the
  checkout: `.agents/setup` runs as the person, outside any sandbox, when a
  task starts, behind the lead's approval gate.

### Sonny's own surfaces

Sonny's tool host answers some tools outside every agent's sandbox, since
they are things the sandbox cannot do: triggers, memory, `start_task`,
`run_in_pane`, `browser`, `exec` for MCP, subscriptions, goals. Each that
can change something outside the agent's folder waits on the agent's
approval gate as a call that leaves the sandbox would; `memory` and
`show_agent` wait for none.

- **Triggers** are jobs of the system's scheduler (a LaunchAgent, a
  systemd user service) that run as the person. Adding one, and its `if`
  command, wait for a verdict; the `if` command runs with the provider
  keys stripped. Nothing of a trigger runs between its fires. A goal's
  check command runs the same way.
- **Lookout sources** are commands the person sets up in `config.toml`,
  run with `sh -c` as the person outside any sandbox.
- **MCP servers** run as the person outside every sandbox, started from
  `~/.agent/config.toml` with `~/.agent/env` in their environment. An
  agent reaches them only through `exec`, a QuickJS script with no files,
  network or timers, and each call a script makes is checked on its own by
  the server's `approval_mode`; the default asks only for a tool the
  server itself marks destructive. An agent's network setting does not
  stop `exec`. A folder's own MCP servers are never imported without the
  person ticking each, since whoever wrote the folder wrote them.
- **The browser tool** wraps page text in boundary markers the model is
  told never instruct it, and refuses what would reach past the browser:
  flags that launch another program or load an extension, plugins, the
  clipboard, the DevTools address, and writes outside the agent's folder.
  `eval`, `download`, `upload`, `state` and vault sign-ins wait for a
  verdict.
- **The commons** is a board shared by one person's agents across
  projects, as files under `~/.agents/commons`; each post's author is set
  by Sonny from the calling agent, never from what it wrote. A new thread
  starts turns in agents of other projects it invites, on a token budget,
  and invites do not chain through idle agents. It leaves the machine only
  through the home backup.
- **Home backup** pushes `~/.agents` (memory, skills, profiles, the
  commons) and a secret-free `config.toml` to a private repository the
  person names, with the person's own git credentials, every ten minutes.
  Sonny names no host for it. Restoring a backup restores the MCP servers'
  commands, which then run.

### Release and supply chain

- `Cargo.lock` is committed and every release builds `--locked`.
  `rust-toolchain.toml` pins the toolchain. Workflow actions are pinned by
  commit SHA, and Dependabot keeps crates and actions current.
- A tag push runs a workflow that holds no secrets and does nothing;
  the release and Homebrew workflows run from `main`'s own definitions on
  its completion, verify the tag points to a commit on `main` and that
  both manifests carry the tag's version, and refuse a tag that moved
  during the build.
- Release assets (the source archive, the formulas, the static Linux
  builds) carry a GitHub build attestation from the release workflow.
  Publishing to the Homebrew tap verifies it, so assets swapped on the
  draft afterwards publish nothing. `install.sh` checks the download's
  sha256 and, when `gh` is signed in, the attestation; it may run the
  system's package manager through `sudo` to install bubblewrap and
  ripgrep.
- The Homebrew formulas build from source on the person's machine. The
  Linux tarballs are static builds and are not code-signed.
- The repository has secret scanning, Dependabot alerts and Dependabot
  security updates on.

## Known limits

These are not vulnerabilities to report; they are the boundaries of what
is built.

- A bot without a sandbox, and every daemon companion, trigger, lookout
  source, goal check, `.agents/setup` and MCP server, runs with the
  person's full permissions. A workspace snapshot or a worktree is not a
  sandbox.
- The sandbox confines commands, writes and edits, not reads: a confined
  bot can read any file the person can and send it to its model provider.
- With the network on, Sonny's default, a sandboxed bot reaches every
  Unix socket on the machine. One that runs a program outside the sandbox
  on request, such as Docker's or ssh-agent's, is a way out of it.
- One allowed escalation, or one `allow` rule, gives a command the
  daemon's own socket and so full daemon authority. A rule matches a
  command's first words, so `allow` on `git` lets out every `git`
  subcommand, and the program is found on the daemon's `PATH`.
- A judge model is a model. The automatic approver reduces, and does not
  remove, the chance that an injected instruction is carried out.
- `web_fetch` reaches the daemon host's own networks.
- Nothing in the store is encrypted at rest, and nothing on the daemon's
  own socket authenticates beyond the socket's file permissions, which
  the daemon sets to its owner alone.
- Multi-daemon operation, moving bots between machines and remote socket
  access are not implemented ([docs/MULTI_DAEMON.md](docs/MULTI_DAEMON.md)).
