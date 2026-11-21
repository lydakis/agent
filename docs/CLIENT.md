# Client policy

The daemon never composes system instructions—it stores whatever text the creating client sends and keeps it until updated via `configure`.

`agent-client` (`client/`) defines the shared instruction composition used by **Sonny** and `agent run --agents`, so bots created from either client see the same rules, skills, profiles, and memory.

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

> Claude Code's `.claude/skills` and `.claude/agents` use the same Markdown format and can be copied or linked into `.agents`.

## Layers

Instructions are assembled in a fixed, cache-friendly order (stable layers first, frequently changing memory near the end):

1. **Harness preamble.** Teaches the bot how to coordinate via built-in tools (`list_agents`, `send_message`, `wait`, `history`) and its environment variables (`AGENT_BOT`, `AGENT_PARENT`). Imposes no persona or workflow. This is the CLI's default when `--agents` is not passed.
2. **`AGENTS.md` files.** Loaded in order from `~/.agents/AGENTS.md`, then walking from the filesystem root down to `<workspace>` (`AGENTS.md`, then `.agents/AGENTS.md` in each directory). Nearest files come last and take precedence. Duplicate paths (via symlinks or overlapping home/workspace) are included once; empty files are skipped.
3. **Skills index.** Discovers `<name>/SKILL.md` under `<workspace>/.agents/skills` and `~/.agents/skills` (workspace wins on name clash), following the [agentskills.io](https://agentskills.io/specification) layout. Only a one-line index (`name`, `description` or first line, `path`) is injected; the bot reads the full `SKILL.md` with `read` when needed.
4. **Profiles index.** Discovers `<role>.md` under `<workspace>/.agents/agents` and `~/.agents/agents` (workspace wins). Sonny's client roles (`lead`, `task`, and any profile extending them) are omitted from the peer index so agents are not offered nested leads.
5. **Memory indexes.** Includes `~/.agents/memory/MEMORY.md` and, inside a project, `~/.agents/memory/projects/<name>/MEMORY.md` (resolved via `.agents/project.toml`, following git worktrees back to the main checkout). Placed after skills and profiles because memory updates more often. Snapshot taken at bot creation ([memory details](SONNY.md#memory)).
6. **Role body.** When started with a profile (`--profile ROLE`, or Sonny's lead/task roles), appends the resolved role body last under `# Role: ROLE`.

### Profile format

A profile (`<role>.md`) is Markdown with optional YAML front matter (unknown keys are ignored so files from other harnesses load cleanly):

| Key | Purpose |
| --- | --- |
| `description` | One-line summary shown in the profiles index and `/new` |
| `model` | Default `PROVIDER/MODEL` when the caller doesn't specify one |
| `tools` | Default tool list for this runtime |
| `sandbox` | `read-only`, `workspace-write`, or `danger-full-access` (can only tighten, never loosen, the maker's sandbox; looser values fail with `invalid_setting`) |
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

Missing referenced profiles or skills fail with `profile_not_found` or `skill_not_found`. See [Sonny's lead and task profiles](../sonny/REFERENCE.md#profiles-for-leads-and-tasks) for project overrides.

## Size bounds and error handling

- **60 KiB cap:** Composed instructions stay under 60 KiB (within the daemon's 64 KiB limit) so the prefix rides the provider's prompt cache cleanly.
- **No silent truncation:** If files exceed the budget or cannot be read (including broken symlinks or non-regular files like FIFOs/sockets), composition fails with `instructions_limit` or `instructions_unreadable` naming the offending file.
- **Lead resilience:** Sonny composes leads with `instructions_leaving_out`—unreadable or over-budget files are omitted whole and reported so a broken skill link never prevents Sonny from opening.
- **Bounded directory scans:** Skill discovery stops after 4,096 directory entries (`instructions_limit`) so large folders cannot stall bot creation.

## Who uses it

- **`agent` CLI:** Uses the harness preamble alone by default. Pass `--agents` (or `--profile ROLE`) on `run --new` to compose full workspace policy (mutually exclusive with `--instructions`).
- **Sonny:** Composes full policy automatically for leads (with the `lead` role) and tasks (with `task` appended after any task profile).
- **Forks:** Always inherit their source bot's exact instructions so their first call hits the source's prompt cache. Use `configure` to push recomposed instructions to an existing bot.
