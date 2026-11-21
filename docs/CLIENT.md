# Client policy

The daemon never composes system instructions—it stores whatever text the creating client sends and keeps it until updated via `configure`.

`agent-client` (`client/`) defines the shared instruction composition used by **Sonny** and `agent run --agents`, so bots created from either client see the same rules, skills, profiles, and memory. The crate also holds the socket protocol client Sonny uses.

Implemented 2026-09-19.

## Folder layout

Everything lives under `.agents` (project-local and user-global):

```text
~/.agents/
├── AGENTS.md                 # global standing rules
├── skills/<name>/SKILL.md    # reusable skills (indexed on demand)
├── agents/<role>.md          # reusable agent profiles
└── memory/
    ├── MEMORY.md             # user memory index
    └── projects/<name>/MEMORY.md

<workspace>/.agents/
├── AGENTS.md                 # workspace rules (plus AGENTS.md at root or parent folders)
├── project.toml              # project name & Sonny overrides
├── skills/<name>/SKILL.md    # workspace skills (override ~/.agents/skills)
└── agents/<role>.md          # workspace profiles (override ~/.agents/agents)
```

`.agents` is the folder Codex, OpenCode, and Pi already read skills from (sources read 2026-09-27: Codex 9db8162, OpenCode b471c2b, Pi 2b0a123). Claude Code keeps `.claude/skills` and `.claude/agents`; their files use the same format and can be copied or linked into `.agents` (George, 2026-09-27: one folder, and an agent can port other harnesses' files when asked).

## Layers

Instructions are assembled in a fixed, cache-friendly order (stable layers first, frequently changing memory near the end):

1. **Harness preamble.** Teaches the bot that other agents are reached through its tools: `list_agents` finds them, `send_message` gives one a message and returns a handle, `wait` on it returns the reply, and `history` reads an agent's turns; that `AGENT_BOT` names the bot and `AGENT_PARENT` its creator, which `send_message` reaches; and that its final reply reaches whoever waits on its turn. No command line or program path is taught: the tool descriptions the daemon sends carry the rest. It imposes no role or way of working; a caller that wants one passes it. This is the CLI's default and only instruction text when `--agents` is not passed.
2. **`AGENTS.md` files.** Loaded in order from `~/.agents/AGENTS.md`, then walking from the filesystem root down to `<workspace>` (`AGENTS.md`, then `.agents/AGENTS.md` in each directory). Nearest files come last and take precedence. Duplicate paths (via symlinks or overlapping home/workspace) are included once; empty files are skipped.
3. **Skills index.** Discovers `<name>/SKILL.md` under `<workspace>/.agents/skills` and `~/.agents/skills` (workspace wins on name clash), following the [agentskills.io](https://agentskills.io/specification) layout. Only a one-line index (`name`, the front matter's `description` or else the first line, `path`) is injected; the bot reads the full `SKILL.md` with `read` when the subject comes up, so an unused skill costs one line.
4. **Profiles index.** Discovers `<role>.md` under `<workspace>/.agents/agents` and `~/.agents/agents` (workspace wins). These are the roles a new agent can be started in (Sonny's `start_task` takes one by name). Sonny's client roles (`lead` and `task`, in `policy.rs`) are omitted from the index: a file of either name replaces Sonny's text for it, not a role to start a peer in, so no agent is offered a nested lead. A profile that `extends` one of them is left out too: it is a variant a lead or its tasks are set to run as.
5. **Memory indexes.** Includes `~/.agents/memory/MEMORY.md` and, inside a project, `~/.agents/memory/projects/<name>/MEMORY.md` each as it is (an index carries its own `# Memory in DIR` heading); one not written yet adds nothing. The project is the `name` in the nearest `.agents/project.toml`; from a git worktree it is looked up first at the same place in the main checkout, read from the worktree's `.git` and `commondir` files rather than by running git. Sonny's memory script keeps the indexes ([memory details](SONNY.md#memory)). Placed after skills and profiles because memory changes more often, and a snapshot: a bot's instructions are fixed when it is made, until a client sends them recomposed with `configure`.
6. **Role body.** When started with a profile (`--profile ROLE`, or Sonny's lead), appends the resolved role body last under `# Role: ROLE`. Sonny ships a `lead` profile ([sonny/agents/lead.md](../sonny/agents/lead.md)) and a `task` one ([sonny/agents/task.md](../sonny/agents/task.md)), whose body follows the rest for every agent Sonny starts but a lead, and writes both to `~/.agents/agents` until the person edits them; a folder's file of the same name comes first. Either can be set to another profile, as Claude Code's `agent` setting picks the profile its main session runs as ([Sonny profile reference](../sonny/REFERENCE.md#profiles-for-leads-and-tasks)).

### Profile format

A profile (`<role>.md`) is Markdown with optional YAML front matter (unknown keys are ignored so files from other harnesses load cleanly):

| Key | Purpose |
| --- | --- |
| `description` | One-line summary shown in the profiles index and `/new` |
| `model` | Default `PROVIDER/MODEL` when the caller doesn't specify one |
| `tools` | Default tool list for this runtime |
| `sandbox` | `read-only`, `workspace-write`, or `danger-full-access`, as a Codex role's `config_file` sets `sandbox_mode`. An agent started in the profile (`--profile`, `spawn_agent`'s `profile`, Sonny's `start_task` and workflow workers) starts in it unless the call names its own sandbox. It never loosens what the agent would get from its maker: a stricter one replaces that (`read-only` with no writable folders, network, or sockets), and a looser one is refused as `invalid_setting` rather than tightened. Any other value is `profile_invalid`. |
| `extends` | Inherit body and unset keys from another profile (chains up to 8 deep; cycles fail with `profile_invalid`). Extending your own name builds on the shadowed file (e.g., project `lead.md` extending user `lead.md` extending Sonny's shipped `lead.md`) |
| `skills` | List of skill names whose full `SKILL.md` bodies are inlined under `## Skill: NAME` instead of indexed |

Example:

```markdown
---
extends: lead
description: Lead that reviews changes before merging
skills: [triage]
---
Review every task's diff before marking work ready to land.
```

Missing referenced profiles or skills fail with `profile_not_found` or `skill_not_found`. No harness has a key for one agent file building on another (Claude Code's and Codex's subagent files each stand alone, checked 2026-10-28), so `extends` takes the name tsconfig and ESLint use; `skills` is Claude Code's subagent key. Skills have no `extends`: the agent opens a skill itself, so a skill that builds on another says so in its text, which names the other's line in the index. See [Sonny's lead and task profiles](../sonny/REFERENCE.md#profiles-for-leads-and-tasks) for project overrides.

## Size bounds and error handling

- **Stable prefix:** The text is a stable prefix on purpose: after the first turn it rides the provider's prompt cache, and it changes only when a file changes.
- **60 KiB bound:** The whole composition is bounded at 60 KiB, under the daemon's 64 KiB limit.
- **No silent truncation:** A workspace whose files exceed the bound fails to compose rather than being silently cut. The CLI, and Sonny for the agents it starts other than a lead, report `instructions_limit` with the file that tipped it, or `instructions_unreadable` with the file that could not be read, and create nothing. A skill, profile, or AGENTS.md file that exists but cannot be read, a link to a missing file included, is an error; only an absent one falls back to the user's.
- **Regular files only:** Each file is opened without waiting: a FIFO, socket, or device named like an instructions file, memory index, or worktree's `.git` is `instructions_unreadable` ("it is not a regular file") rather than a read that waits until another program writes to it. A caller may run the composition under `noting`, which keeps the path each read waits on its file system for until it answers, so a wait that never ends (a mount whose server is gone) can say where it is held.
- **Lead resilience:** Sonny composes leads with `instructions_leaving_out`: each unreadable or over-budget file is left out whole, never cut, and listed with why, so the window opens, says what is missing, and tells the lead with its next model call (George, 11-08: a broken skill link is no reason for Sonny not to open). A lead whose own role file cannot be used runs as Sonny's role of that name, and says so.
- **Bounded directory scans:** Skill discovery visits workspace overrides first and accounts each index row against the remaining byte budget before reading more paths or file heads. It fails as soon as the index cannot fit, then sorts only the bounded result. It also stops at 4,096 folder entries, indexed or not (`instructions_limit`), so a folder of other files cannot slow every new bot.

## Who uses it

- **`agent` CLI:** Uses the harness preamble alone by default. Pass `--agents` (or `--profile ROLE`) on `run --new` to compose full workspace policy (mutually exclusive with `--instructions`).
- **Sonny:** Composes full policy automatically for leads (with the `lead` role) and tasks (with `task` appended after any task profile).
- **Forks:** Always inherit their source bot's exact instructions so their first call hits the source's prompt cache. Use `configure` to push recomposed instructions to an existing bot.

## Verified

Unit tests in `client/src/policy.rs` cover discovery order (nearest file last), the skills index, the size bound failing instead of truncating, and a bare workspace. A CLI behavior test creates one bot without and one with `--agents` against the synthetic model and checks what the daemon sent: preamble only, then preamble plus the workspace's AGENTS.md and skill index, and that `--agents` with `--instructions` or on an existing bot is a usage error.
