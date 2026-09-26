# Client policy

The daemon composes no text. A bot's instructions are whatever the creating
client sent, stored once, immutable for the bot's life. `agent-client`
(`client/`) is where the human-facing clients agree on what that text is, so a
bot created from the app or the CLI with `--agents` reads the same way. The
crate also holds the socket protocol client the app uses.

Implemented 2026-09-19.

## Three layers

1. **The harness preamble.** How to delegate through this runtime: `agent
   run --detach --new --bot NAME` from the shell tool, collect with `wait`,
   and that `AGENT_BOT`/`AGENT_BOT_ID` identify the bot itself
   and `AGENT_PARENT`/`AGENT_PARENT_ID` its creator. Calls to that creator use
   `--bot-id` so a reused name cannot receive the work, and are for questions, not
   results: `wait` already hands the creator the bot's final reply. The tool descriptions the daemon sends
   carry the rest. It gives the bot no role and no way of working; a caller
   that wants one passes it. This is the CLI's default and only instruction text.
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
app then falls back to the preamble and says so in the create
notice, the CLI reports `instructions_limit` with the file that tipped it.
Skill discovery visits workspace overrides first and accounts each index row
against the remaining byte budget before reading more paths or file heads.
It fails as soon as the index cannot fit, then sorts only the bounded result.

## Who uses it

- **CLI**: plumbing by default, the preamble alone. `--agents` on `run`
  composes the policy for the workspace; exclusive with `--instructions`.
- **app**: the policy is the default for `/new`. The create notice says what went in, for example `preamble + 2 AGENTS.md + 1 skills`.
- A **fork** takes no instructions: it copies its source's, so its first call
  can read the source's prompt cache. An edited AGENTS.md reaches a new bot;
  a caller that wants an existing conversation to follow it says so in a
  message, and every existing bot stays immutable.

## Verified

Unit tests in `client/src/policy.rs` cover discovery order (nearest file
last), the skills index, the size bound failing instead of truncating, and a
bare workspace. A CLI behavior test creates one bot without and one with
`--agents` against the synthetic model and checks what the daemon sent:
preamble only, then preamble plus the workspace's AGENTS.md and skill index,
and that `--agents` with `--instructions` or on an existing bot is a usage
error.
