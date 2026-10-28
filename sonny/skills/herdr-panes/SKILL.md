---
name: herdr-panes
description: Use this, not the herdr skill, whenever the person mentions Herdr or a pane, tab or split. Run long or interactive commands (dev servers, watchers, builds or test runs past the shell tool's 10 minutes, REPLs) in a Herdr pane the person can watch, then read, wait on or stop them; and put an agent's pane where the person asks (its own tab, or a split beside another).
---

Herdr is the terminal multiplexer the person watches the work in. Your shell is not in a Herdr pane, by design: it runs in the agent daemon, so `HERDR_ENV`, `HERDR_PANE_ID` and the other pane variables are unset. That does not mean the person is outside Herdr, and it is no reason to stop. Herdr's own `herdr` skill, which asks for `HERDR_ENV=1`, does not apply here; this skill's commands work from your shell. Each project has one Herdr workspace: the one its lead runs in. In it, every agent has a pane labelled with its full name (`shop.fix-cart`), and a tab named after what it is doing. Use a pane only for a command that runs longer than the shell tool allows (up to 10 minutes with `timeout_ms`), that keeps running (a server, a watcher), or that the person wants to watch. Everything else, a one-minute test run or build included, stays in the shell tool: give it a `timeout_ms` that covers it rather than a short one, since one shell call is faster than a pane, a wait and a read.

## Tabs or a split

A tab is the default. It is one more stream of work that the person can click into when they choose, to watch it, steer it and manage it there. A split puts something beside you, in the tab the person is looking at, so it asks for their attention now. Open one only when you want them to see something right away: a server or a CLI's output you are showing them, or an agent they should watch or talk to side by side with you. When unsure, use a tab.

Sessions are for leads: each project's lead has a Herdr workspace of its own, and its agents and commands get tabs and splits in it.

## Where things go

- An agent you start gets a tab of its own when its turn starts, without your doing anything.
- A command of yours that belongs in a pane (a dev server, a test watcher, a build or test run over 10 minutes, a log tail, a REPL) goes in one of its own with the `run_in_pane` tool, named for what it is (`dev-server`, `tests`), not a number. A tab is the default; `split` (right, left, above, below) puts it beside you, to show it now.

  It runs as `sh -c` runs the command, in your folder (or `cwd`), in the person's shell, which stays open after the command exits so the output can be read. The result holds the pane's ID as `block`; keep it. The pane is labelled `YOUR-NAME/NAME`, so the person can see whose it is. Running the same name again replaces the old pane. Your shell's variables are not there, so put what the command needs in it: `env VAR=value COMMAND`. To wait for the command to end, end it with a line you can wait for: `cargo test; echo "exited $?"`.

- Never make a Herdr workspace, tab or pane yourself (`herdr workspace create`, `herdr tab create`, `herdr pane split`), and never use `--current` or `$HERDR_PANE_ID`: they may name a pane that is not yours.
- Never close a tab or pane the person has typed in or asked you to open; ask them first. Close only what you opened for your own work.

- When the person asks for an agent somewhere else ("put it next to the lead", "open that in a split", "give it its own tab"), or they should watch one beside you now, move it with the `show_agent` tool, before or after starting it: `split` puts it on that side of you, or of the agent `beside` names, and without `split` it gets its own tab again.

- Another project's lead you hand work to (`PROJECT.lead`) has a Herdr workspace of its own, not a tab in yours. When the person wants to watch it, or you start a lead that is not open anywhere yet, open it there with `show_agent` and no `split`; one already open is left as it is.

## Herdr knows you

Herdr's agent panel lists every agent Sonny runs, you included, as `Sonny: NAME`, working, idle or waiting for the person; Sonny reports that for you, so never report to Herdr yourself (`herdr pane report-agent`, `report-metadata`, `agent rename`). Agents the person runs in Herdr outside Sonny (Claude Code, Codex and others) show there too, by name or, unnamed, by pane ID. To see them: `HERDR_BIN agent list`; read one: `HERDR_BIN agent read NAME --source recent-unwrapped --lines 80`; wait for one to finish: `HERDR_BIN agent wait NAME --timeout 600000`. Write to another agent (`HERDR_BIN agent prompt NAME 'text'`) only when the person asks you to.

## Working with a pane

- Read its output: `HERDR_BIN pane read ID --source recent-unwrapped --lines 120`
- Wait for a line: `HERDR_BIN pane wait-output ID --regex '^exited [0-9]+$' --timeout 600000` (`--match TEXT` for plain text; it fails with exit status 1 on timeout)
- Type into it and press Enter: `HERDR_BIN pane send-text ID 'text' && HERDR_BIN pane send-keys ID enter`; `HERDR_BIN pane send-keys ID ctrl+c` stops what runs
- Close it: `HERDR_BIN pane close ID`

## Cleaning up

Close each pane you opened once you have what you need from it: a build or test run once you have read its result, a server once nothing needs it. Leave one open only when the person should see it or another agent still reads it, and say so in your reply with its label. When an agent's work is taken, its tab closes, and every pane it opened closes with it.
