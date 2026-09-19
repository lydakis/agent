# Client policy

The daemon composes no text. A bot's instructions are whatever the creating
client sent, stored once, immutable for the bot's life. `agent-client`
(`client/`) is where the human-facing clients agree on what that text is, so a
bot created from the app, the TUI, or the CLI with `--agents` reads the same
way. The crate also holds the socket protocol client both the TUI and the app
use.

Implemented 2026-09-19.

## Three layers

1. **The harness preamble.** How to delegate through this runtime: `agent
   run --detach --new --bot NAME` from the shell tool, collect with `wait`,
   background shells the same way, and that `AGENT_BOT` is the bot's own name
   and `AGENT_PARENT` its creator. The tool descriptions the daemon sends
   carry the rest. This is the CLI's default and only instruction text.
2. **AGENTS.md files.** `~/.agent/AGENTS.md` first, then every `AGENTS.md`
   from the filesystem root down to the workspace, so the nearest file is
   read last and wins where they disagree. Each is appended under a heading
   naming its path. Empty files are skipped.
3. **Skills.** `~/.agent/skills/*.md` and `<workspace>/.agent/skills/*.md`
   (the workspace's winning on a name clash) become an index: name, first
   line, path. The bot opens a skill with its `read` tool when the subject
   comes up; nothing else is sent, so an unused skill costs one line.

The text is a stable prefix on purpose: after the first turn it rides the
provider's prompt cache, and it changes only when a file changes. The whole
composition is bounded at 60 KiB, under the daemon's 64 KiB limit. A workspace
whose files exceed it fails to compose rather than being silently cut; the
TUI and the app then fall back to the preamble and say so in the create
notice, the CLI reports `instructions_limit` with the file that tipped it.

## Who uses it

- **CLI**: plumbing by default, the preamble alone. `--agents` on `run` and
  `fork` composes the policy for the workspace; exclusive with
  `--instructions`.
- **TUI** and **app**: the policy is the default for `/new`. The create
  notice says what went in, for example `preamble + 2 AGENTS.md + 1 skills`.
- A **fork** with `--agents` or `--instructions` gets new text; its source is
  untouched. That is how an edited AGENTS.md reaches a fresh bot while every
  existing bot stays immutable.

## Verified

Unit tests in `client/src/policy.rs` cover discovery order (nearest file
last), the skills index, the size bound failing instead of truncating, and a
bare workspace. A CLI behavior test creates one bot without and one with
`--agents` against the synthetic model and checks what the daemon sent:
preamble only, then preamble plus the workspace's AGENTS.md and skill index,
and that `--agents` with `--instructions` or on an existing bot is a usage
error.
