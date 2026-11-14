---
name: sonny-config
description: Where Sonny and its agent daemon keep their settings, every key of ~/.agent/config.toml with its values and default, a project's own file, and which folders an agent's sandbox writes. Read before changing or explaining a setting, or when the sandbox keeps a command out of a folder it needs, instead of looking it up elsewhere.
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
- `~/.agent/keys/env`: provider keys only, one `NAME=value` a line, readable by
  the person alone. `sonny setup --provider NAME --key-stdin` puts a key
  there; never write one anywhere else (see the sonny-setup skill).
- `.agents/project.toml` in a project's folder: that project's own picks,
  over the config's (below).
- `~/.agents/agents/NAME.md`: the roles and profiles (`lead`, `task`,
  `superlead`, the person's own); a project's `.agents/agents` comes first.
- `~/.agents/skills/NAME/SKILL.md`: skills every agent reads.
- `~/.local/state/sonny/agents/LEAD` (under `$XDG_STATE_HOME` when set):
  each lead's state folder, `$SONNY_STATE` in its shell and its tasks'.
  Sonny makes it; the lead, its tasks and the agents they make write it in
  every permissions row that writes, and keep there what outlives a task,
  such as a monitor's files. Settings and skills stay where they are above.
- `[sonny.mcp_servers.NAME]` in the config: MCP servers, kept with
  `sonny mcp add|remove|list|import`.
- `[sonny.sources.NAME]` in the config: news from outside for the lookout,
  a `command` run `every` so often.
- `[sonny.permissions.NAME]` in the config: a `/permissions` row of the
  person's own, as Codex's permission profiles: the row it `extends`
  (`auto`, the default, `ask` or `read-only`), then what it changes:
  `network` (`true` or `false`), `git` (`"write"`, commits run without
  asking, or `"read-only"`, they ask), `writable` (more folders, `~` for
  home), `unix_sockets` (with the network off, the only sockets, or
  folders of them, its commands reach, such as Docker's; with it on, every
  one is), `reviewer` (`"model"`, `"you"`, or `"none"`, which refuses what
  leaves the sandbox and tells the agent to do without), and a
  `description` for the list. Codex's own stricter workspace is
  `network = false` and `git = "read-only"`. `/permissions` lists it the
  next time it opens; `sonny.default_permissions` names it for new leads.
- No folder of the person's is shared between leads by default: a
  sandboxed agent writes its own folder, temp, its lead's state folder
  and the one `sonny zen` keeps, and past those only its row's
  `writable`. For a folder several leads' agents write (a tool's cache or
  ledger, `~/.docker`), the person adds it to a row's `writable` and
  picks that row: ask them for it, naming the folder, rather than keeping
  a copy per agent or running the command outside the sandbox.
- `[context_windows]` in the config: a model's context window in
  tokens, which sets when its agents compact, by `PROVIDER/MODEL` with `*`
  wildcards (`"bedrock/*openai.*" = 1000000`), over what its provider
  lists; the most specific pattern wins, and it may be lower on purpose.
  Read when the daemon starts.
- The price sheet, `~/.agent/prices.toml`, or the file the config's
  `prices = "~/path/sheet.toml"` names: what a million tokens cost, which
  `/usage` turns into each model's cost and a total, by `PROVIDER/MODEL`
  with `*` wildcards, the most specific pattern winning, one row each:
  `"anthropic/claude-opus-5*" = { input = 5, cached = 0.5, cache_write = 6.25, cache_write_1h = 10, output = 25 }`.
  `input` and `output` are required; `cached` and `cache_write` are
  `input`'s when unset, `cache_write_1h` is `cache_write`'s. Nothing is
  bundled: take the numbers from the provider's own pricing page. Read at
  each `/usage` refresh.
- Command rules are not in the config: `~/.agent/rules/*.rules` holds
  Codex-style `prefix_rule(pattern = ["mcpx"], decision = "allow")` lines
  (`allow` runs outside the sandbox without asking, `prompt` always asks,
  `forbidden` never runs, with a `justification`); `timeout`, `nice`,
  `nohup` and `time` before a command are seen through, and `env` or
  `NAME=value` only to ask or refuse. Agents can read but not
  write that folder from a sandbox, so suggest a rule to the person;
  "always allow" on an approval prompt adds one, and
  `sonny rules check -- COMMAND` shows what the rules decide. The files
  are Starlark, as in Codex (`host_executable` works; `network_rule`
  is refused).

## The browser's profile

The `browser` tool runs this machine's agent-browser; `machine` is only
for another machine linked here. Which Chrome profile a session starts
from is agent-browser's own setting, not Sonny's:
`~/.agent-browser/config.json` (or the file `AGENT_BROWSER_CONFIG` names),
key `profile`:

```json
{"profile": "Default"}
```

- `"Default"` starts each session from a copy of the person's own Chrome
  profile, signed in where they are; their Chrome is left alone. Another
  profile's name starts from that one; leave the key out for a fresh
  browser, signed in nowhere. Keep the file's other keys as they are.
- `sonny setup --browser-signed-in` writes `"Default"` when the file names
  no profile and Chrome here has one, and changes nothing when it names one.
- A link whose `browser_profile` is `""` starts its sessions fresh whatever
  this file names.
- The file is outside an agent's sandbox, and Sonny runs no browser for
  an agent whose sandbox can write that folder: change it through a
  command the agent's approvals allow, or ask the person to.
- A session copies the profile when its browser starts: after the file
  changes, or after the person signs in to a site, `close` the browser and
  open the page again.
- agent-browser's `--profile` flag is refused in the tool (it can start
  programs or change where the browser connects), so the file is the way.
- Sign-ins a command-line tool keeps for itself (its own cookie file or
  token) are not the browser's, and expire on their own; each signs in
  its own way, so keep their credentials where they are.
- A link's browser starts from `[sonny.links.NAME] browser_profile`
  (below), which `sonny setup --json` lists under `links_differ` where it
  is not the profile a session here starts from. It is copied once into a
  profile kept for the link, which its agents share in one Chrome, a tab
  each: a sign-in or an extension's sync there stays. Each time that
  Chrome starts, the person's native messaging hosts are linked in and
  their Cookies copied over the kept ones when theirs are newer, so a
  sign-in in their own Chrome reaches it once it starts again (it closes
  30 minutes after its agents' last call). Sonny starts that
  Chrome with the system's keychain (the person's Chrome, or
  `executablePath` in agent-browser's file), headless unless the file says
  `"headed": true`. To start it again
  from the person's profile, change `browser_profile` (another name, then
  back) or remove `~/.agent/sonny/links/NAME/chrome`.

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
| `threads_speed` | each new task's speed: `standard`, `fast` or `ultrafast` | `sonny.task_speed` |
| `profile` | the profile the lead runs as | `sonny.lead_profile` |
| `threads_profile` | the profile every other agent is told | `sonny.task_profile` |
| `subfolders = "project"` | folders below open this project's lead | |
