# Sonny

Sonny is a terminal client for [Agent](../README.md). Give a project's
**lead** several tasks; it starts agents, follows their progress, and brings
you the questions and decisions that need your attention. Each agent keeps
its conversation, so you can leave and return later.

[Install](#install) · [First run](#first-run) · [Everyday controls](#everyday-controls) ·
[Permissions](#permissions) · [Full reference](REFERENCE.md)

## Install

On macOS or Linux with Homebrew:

```sh
brew install lydakis/agent/sonny
```

This builds Sonny from a release's source and installs Agent, ripgrep, and
agent-browser. If Homebrew refuses the tap as untrusted, run
`brew trust lydakis/agent` first.

On Linux, the installer downloads the latest release's static builds for
x86_64 or aarch64 and checks the published SHA-256 checksum:

```sh
curl -fsSL https://raw.githubusercontent.com/lydakis/agent/main/install.sh | sh
```

Read [install.sh](../install.sh) for its steps. It links `sonny` into
`~/.local/bin` and installs missing bubblewrap and ripgrep packages through
your package manager. Make sure `~/.local/bin` is on your `PATH`.
`PREFIX` changes the install location, `SONNY_VERSION` selects a release,
and `SONNY_PACKAGES=0` skips package installation.

### Updating

```sh
sonny update
```

This updates a Homebrew or installer install to the latest release (a
nightly install to current `main`). It replaces the running daemon with the
new one, and running turns continue on it. Open Sonny windows reopen on the
new version and keep what you typed. See
[docs/SONNY.md](../docs/SONNY.md#updating) for each step.

### Nightly builds

Nightly builds Sonny and its matching daemon from current `main`:

```sh
brew unlink sonny  # if stable Sonny is installed
brew install lydakis/agent/sonny-nightly
```

To update:

```sh
brew update
brew reinstall lydakis/agent/sonny-nightly
```

Reinstall fetches current `main`, even when the formula's version has not
changed. No `--HEAD` is needed. `sonny --version` and
`sonny agent --version` show the built commit.

### Build from source

From the repository root, with the pinned Rust toolchain installed:

```sh
cargo install --locked --path .
cargo install --locked --path sonny
export AGENT_BIN="$HOME/.cargo/bin/agent"
```

Sonny needs the explicit `AGENT_BIN` path for a Cargo installation; it does
not select `agent` from `PATH`. `sonny agent ARGS` runs the daemon CLI Sonny
uses, regardless of other programs named `agent`.

## First run

Open Sonny in your project's folder:

```sh
sonny
```

Setup looks for existing provider credentials, helps you choose a model,
and offers to import MCP servers from Claude Code, Codex, OpenCode, or Kiro.
Run `sonny setup` to revisit setup, or `/config` to change settings.
[Provider setup](REFERENCE.md#provider-setup) covers scripting and additional
options.

Give the lead a concrete request, for example:

> Fix the failing tests and have another agent check the README examples.

The lead can start tasks in their own worktrees. In Rex, Herdr, or cmux,
Sonny can open those tasks in tabs. `/status` shows their progress, and
clicking an agent's name opens its conversation. Questions and approvals
from tasks also appear in the lead's window.

Reopen `sonny` in the same folder to return to the lead you last wrote to.
Closing its window leaves the daemon and running turns alive.

## Everyday controls

| Action | Control |
| --- | --- |
| Send a message, or steer a running turn | Enter |
| Queue a message after the running turn | Tab while a turn runs |
| Insert a newline | Alt-Enter or Ctrl-J; Shift-Enter in supported terminals |
| Interrupt the turn | Esc |
| List agents and open their conversations | `/status` |
| Switch this window to another agent | `/switch` |
| Create an agent with a profile and workspace | `/new [NAME]` |
| Ask a copy of this agent a separate question | `/side [QUESTION]` |
| Focus on one topic and hold other updates | `/zen FOCUS` |
| Change model and effort | `/model` |
| Change settings | `/config` |
| Review automations and other background work | `/background` |
| Read and remove what agents remember | `/memory` |
| Inspect token usage | `/usage` |
| List commands or keyboard shortcuts | `/`, or `?` with an empty composer |

Side chats inherit the agent's tools and can edit files by default. Set
`[sonny] side_chat = "answer"` in `~/.agent/config.toml` to make them answer
without tools. See the [command reference](REFERENCE.md#commands) for details.

## Permissions

New leads start in **Auto**. `/permissions` changes the mode for the agent,
its descendants, and agents it creates later; busy agents switch after their
current turn ends.

| Mode | Commands, writes, and edits | Calls outside the sandbox |
| --- | --- | --- |
| Auto | Confined to the workspace and allowed folders | Reviewed by a model; uncertain calls ask you |
| Ask me | Confined to the workspace and allowed folders | Ask you |
| Read only | No writes or network from commands | Ask you |
| Full access | Unconfined | Run without asking |

### What the sandbox closes

The sandbox restricts writes and command access, but still permits reads of
files you can read. Workspace modes allow network access. Daemon and provider
web tools have separate settings. Read the
[sandbox boundaries](REFERENCE.md#what-the-sandbox-closes) before configuring
an agent that handles untrusted content; [reviewer behavior](REVIEWER.md)
explains how Auto decides.

## Configuration

Settings live in `~/.agent/config.toml`; credentials live in
`~/.agent/keys/env`, readable only by you. Project settings live in
`.agents/project.toml`. Use `/config` to inspect defaults and change settings.

### Roles for leads and tasks

Customize the `lead` and `task` profiles, or define your own. See
[roles and project overrides](REFERENCE.md#roles-for-leads-and-tasks).
Sonny preserves your edits to shipped roles and skills and offers new
versions during upgrades.

### Effort for leads and tasks

Set lead and task defaults globally or per project. See the
[settings table](REFERENCE.md#effort-for-leads-and-tasks) for model, effort,
speed, display, and terminal layout options.

## Integrations and further reading

- [Memory, skills, automations, and MCP](../docs/SONNY.md): capabilities agents use across conversations.
- [Lookout](../docs/LOOKOUT.md): task updates and configured outside sources.
- [Terminal layout](REFERENCE.md#terminal-layout): Rex, Herdr, and cmux setup and behavior.
- [Feature history](FEATURES.md): what changed and why.
- [Documentation index](../docs/README.md): runtime, CLI, design, and measurements.

### T3 Code

Use `sonny acp` as a local ACP provider. See
[T3 Code setup](REFERENCE.md#t3-code) for configuration and thread behavior.

### Other ACP clients

`sonny acp` also serves other Agent Client Protocol clients. See
[ACP sessions and integration details](REFERENCE.md#other-acp-clients).
