---
name: sonny-config
description: Where Sonny and its agent daemon keep their settings, every key of ~/.agent/config.toml with its values and default, and a project's own file. Read before changing or explaining a setting, instead of looking it up elsewhere.
---

# Sonny's settings

This file is written by Sonny {{VERSION}} each time it starts, from the
settings it reads, so it matches the Sonny installed here. It says what
each setting is; what to set is the person's call.

## Where they live

- `~/.agent/config.toml`: the settings below, for every project. `/config`
  in Sonny lists and changes them; a key set back to its default has its
  line taken out. Edit it by hand too: the person's comments and order
  stay. A setting is read when its time comes (a new agent, a new turn);
  one marked "read when it starts" waits for the daemon or Sonny to start
  again (`sonny agent shutdown` stops the daemon; Sonny starts it).
- `~/.agent/env`: provider keys only, one `NAME=value` a line, readable by
  the person alone. `sonny setup --provider NAME --key-stdin` puts a key
  there; never write one anywhere else (see the sonny-setup skill).
- `.agents/project.toml` in a project's folder: that project's own picks,
  over the config's (below).
- `~/.agents/agents/NAME.md`: the roles and profiles (`lead`, `task`,
  `superlead`, the person's own); a project's `.agents/agents` comes first.
- `~/.agents/skills/NAME/SKILL.md`: skills every agent reads.
- `[sonny.mcp_servers.NAME]` in the config: MCP servers, kept with
  `sonny mcp add|remove|list|import`.
- `[sonny.sources.NAME]` in the config: news from outside for the lookout,
  a `command` run `every` so often.
- `[sonny.permissions.NAME]` in the config: a `/permissions` row of the
  person's own, as Codex's permission profiles: the row it `extends`
  (`auto`, the default, `ask` or `read-only`), then what it changes:
  `network` (`true` or `false`), `git` (`"write"`, commits run without
  asking, or `"read-only"`, they ask), `writable` (more folders, `~` for
  home), `reviewer` (`"model"`, `"you"`, or `"none"`, which refuses what
  leaves the sandbox and tells the agent to do without), and a
  `description` for the list. Codex's own stricter workspace is
  `network = false` and `git = "read-only"`. `/permissions` lists it the
  next time it opens; `sonny.default_permissions` names it for new leads.
- `sonny.unsandboxed` in the config: commands, by their first words, every
  agent runs outside its sandbox without asking (`["mcpx", "cargo test"]`);
  "always allow" on an approval prompt adds one.

## ~/.agent/config.toml

A key `table.key` goes under `[table]`; a bare key goes at the top.

{{SETTINGS}}

## .agents/project.toml

| Key | What it sets | Over the config's |
| --- | --- | --- |
| `model` | the lead's model | `model` |
| `effort` | a new lead's effort | `sonny.lead_effort` |
| `threads_model` | each new task's model | the lead's |
| `threads_effort` | each new task's effort | `sonny.task_effort` |
| `profile` | the profile the lead runs as | `sonny.lead_profile` |
| `threads_profile` | the profile every other agent is told | `sonny.task_profile` |
| `subfolders = "project"` | folders below open this project's lead | |
