---
name: cmux-tabs
description: Use this, not the cmux skill, whenever the person mentions cmux or a tab, pane or split. Run long or interactive commands (dev servers, watchers, builds or test runs past the shell tool's 10 minutes, REPLs) in a cmux tab the person can watch, then read, wait on or stop them; and put an agent's tab where the person asks (its own tab, or a split beside another).
---

cmux is the terminal the person watches the work in. Your shell is not in a cmux tab, by design: it runs in the agent daemon, so `CMUX_SURFACE_ID`, `CMUX_WORKSPACE_ID` and the other tab variables are unset. That does not mean the person is outside cmux, and it is no reason to stop; this skill's commands work from your shell. Each project has one cmux workspace: the one its lead runs in. In it, every agent has a tab titled with its full name (`shop.fix-cart`). Use a tab only for a command that runs longer than the shell tool allows (up to 10 minutes with `timeout_ms`), that keeps running (a server, a watcher), or that the person wants to watch. Everything else, a one-minute test run or build included, stays in the shell tool: give it a `timeout_ms` that covers it rather than a short one, since one shell call is faster than a tab, a wait and a read.

## Tabs or a split

A tab is the default. It is one more stream of work, beside the lead's tab, that the person can click into when they choose, to watch it, steer it and manage it there. A split puts something beside you, in the workspace the person is looking at, so it asks for their attention now. Open one only when you want them to see something right away: a server or a CLI's output you are showing them, or an agent they should watch or talk to side by side with you. When unsure, use a tab.

Workspaces are for leads: each project's lead has a cmux workspace of its own, and its agents and commands get tabs and splits in it. When the person asks for a workspace, they mean a project's: open another project's lead in its own with `show_agent`, or make the project with `new_project` and then show its lead.

## Where things go

- An agent you start gets a tab of its own when its turn starts, without your doing anything.
- A command of yours that belongs in a tab (a dev server, a test watcher, a build or test run over 10 minutes, a log tail, a REPL) goes in one of its own with the `run_in_pane` tool, named for what it is (`dev-server`, `tests`), not a number. A tab is the default; `split` (right, left, above, below) puts it beside you, to show it now.

  It runs as `sh -c` runs the command, in your folder (or `cwd`), in the person's shell, which stays open after the command exits so the output can be read. The result holds the tab's ID as `block`; keep it. The tab is titled `YOUR-NAME/NAME`, so the person can see whose it is. Running the same name again replaces the old tab. Your shell's variables are not there, so put what the command needs in it: `env VAR=value COMMAND`. To wait for the command to end, end it with a line you can wait for: `cargo test; echo "exited $?"`.

- Never make a cmux workspace, tab or split yourself (`cmux new-workspace`, `new-surface`, `new-split`, `new-pane`), and never leave out `--surface`: without it, cmux picks a tab that may not be yours.
- Never close a tab the person has typed in or asked you to open; ask them first. Close only what you opened for your own work.

- When the person asks for an agent somewhere else ("put it next to the lead", "open that in a split", "give it its own tab"), or they should watch one beside you now, move it with the `show_agent` tool, before or after starting it: `split` puts it on that side of you, or of the agent `beside` names, and without `split` it gets its own tab again.

- Another project's lead you hand work to (`PROJECT.lead`) has a cmux workspace of its own, not a tab in yours. When the person wants to watch it, or you start a lead that is not open anywhere yet, open it there with `show_agent` and no `split`; one already open is left as it is.

## cmux knows you

cmux's sidebar shows what each agent Sonny runs is doing, you included (working, waiting for the person, done), and rings the tab of one that needs them; Sonny tells cmux that for you, so never do it yourself (`cmux set-status`, `set-progress`, `log`, `notify`). Agents the person runs in cmux outside Sonny (Claude Code, Codex and others) are tabs too. To see every tab: `CMUX_BIN tree --all`; read one by its ID as below. Type into another agent's tab only when the person asks you to.

## Working with a tab

- Read its output: `CMUX_BIN read-screen --surface ID --lines 120`
- Wait for a line, polling, inside one shell call with a `timeout_ms` that covers it: `until CMUX_BIN read-screen --surface ID --lines 40 | grep -Eq '^exited [0-9]+$'; do sleep 5; done`
- Type into it and press Enter: `CMUX_BIN send --surface ID 'text\n'`; `CMUX_BIN send-key --surface ID ctrl+c` stops what runs
- Close it: `CMUX_BIN close-surface --surface ID --force` (`--force` closes it while a program still runs in it)

## Cleaning up

Close each tab you opened once you have what you need from it: a build or test run once you have read its result, a server once nothing needs it. Leave one open only when the person should see it or another agent still reads it, and say so in your reply with its title. When an agent's work is taken, its tab closes, and every tab it opened closes with it.

## When cmux refuses

Your shell is not a program cmux started, so cmux takes its calls only in Automation mode, which lets the person's own programs reach it. When a call says cmux refused or denied the connection ("only processes started inside cmux can connect"), tell the person, with what cmux said, that cmux's Settings > Automation needs "Automation mode" for agents to reach it; never change cmux's settings yourself.
