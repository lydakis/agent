# Client policy

The daemon composes no text. A bot's instructions are whatever the creating
client sent, stored once, immutable for the bot's life. `agent-client`
(`client/`) is where the human-facing clients agree on what that text is, so a
bot created from the app or the CLI with `--agents` reads the same way. The
crate also holds the socket protocol client the app uses.

Implemented 2026-09-19.

## Layers

1. **The harness preamble.** How to delegate through this runtime: `agent
   run --detach --new --bot NAME` from the shell tool, with `--model` one of
   those `agent models` lists (named as a command, so the text stays one
   cached prefix whatever the list holds), collect with `wait`,
   and that `AGENT_BOT`/`AGENT_BOT_ID` identify the bot itself
   and `AGENT_PARENT`/`AGENT_PARENT_ID` its creator. Calls to that creator use
   `--bot-id` so a reused name cannot receive the work, and are for questions, not
   results: `wait` already hands the creator the bot's final reply. The tool descriptions the daemon sends
   carry the rest. It gives the bot no role and no way of working; a caller
   that wants one passes it. This is the CLI's default and only instruction text.
2. **AGENTS.md files.** `~/.agents/AGENTS.md` first, then, in each folder
   from the filesystem root down to the workspace, its `AGENTS.md` and then
   its `.agents/AGENTS.md`, so the nearest file is read last and wins where
   they disagree. A file reached twice, such as the home folder's
   `.agents/AGENTS.md` (that first file) or a link to its folder's
   `AGENTS.md`, is read once. Each is appended under a heading naming its
   path. Empty files are skipped.
3. **Skills.** Folders `<name>/SKILL.md` in `<workspace>/.agents/skills` and
   `~/.agents/skills` (the workspace's winning on a name clash), the layout
   of [agentskills.io](https://agentskills.io/specification), become an
   index: name, the front matter's `description` (else the first line),
   path. The bot opens a skill with its `read` tool when the subject comes
   up; nothing else is sent, so an unused skill costs one line.
4. **Profiles.** Files `<role>.md` in `<workspace>/.agents/agents` and
   `~/.agents/agents` become a second index, with the command that starts a
   peer in a role. A profile is markdown with optional YAML front matter:
   `description`, `model`, and `tools` (a list of this runtime's tool
   names) are read, any other key is ignored, so an agent file written for
   another harness loads as it is. The body is the role. The app's own
   roles (`coordinator`, `swarm-flat`, `swarm-council`, the client roles in
   `policy.rs`) are left out of the index: a file of that name replaces the
   app's text for that role, not a role to start a peer in, so no agent is
   offered a nested coordinator or a swarm member outside a swarm.
5. **Role.** A bot started in a profile (`--profile ROLE`, or the app's
   coordinator) gets that body last, under `# Role: ROLE`. The app ships a
   `coordinator` profile ([app/agents/coordinator.md](../app/agents/coordinator.md));
   a folder's or the user's `coordinator.md` replaces it.

Everything a client reads lives under `.agents`, the folder Codex, OpenCode
and Pi already read skills from (sources read 2026-09-27: Codex 9db8162,
OpenCode b471c2b, Pi 2b0a123). Claude Code keeps `.claude/skills` and
`.claude/agents`; their files use the same format and can be copied or
linked into `.agents` (George, 2026-09-27: one folder, and an agent can port
other harnesses' files when asked).

The text is a stable prefix on purpose: after the first turn it rides the
provider's prompt cache, and it changes only when a file changes. The whole
composition is bounded at 60 KiB, under the daemon's 64 KiB limit. A workspace
whose files exceed it fails to compose rather than being silently cut. The CLI
and the app both report `instructions_limit` with the file that tipped it, or
`instructions_unreadable` with the file that could not be read, and create
nothing: a bot without its workspace's rules is worse than no bot.
Skill discovery visits workspace overrides first and accounts each index row
against the remaining byte budget before reading more paths or file heads.
It fails as soon as the index cannot fit, then sorts only the bounded result. It also stops at
4096 folder entries, indexed or not, so a folder of other files cannot slow
every new bot; past that, composition fails with `instructions_limit`.
A skill, profile or AGENTS.md file that exists but cannot be read, a link to
a missing file included, is an error; only an absent one falls back to the
user's.

## Who uses it

- **CLI**: plumbing by default, the preamble alone. `--agents` on `run`
  composes the policy for the workspace; exclusive with `--instructions`.
- **app**: the policy is the default for `/new`. The create notice says what went in, for example `preamble + 2 AGENTS.md + 1 skills`; a policy that cannot compose refuses `/new` and keeps the command in the composer.
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
