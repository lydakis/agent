---
name: herdr-panes
description: Run long or interactive commands (dev servers, watchers, builds or test runs past the shell tool's 10 minutes, REPLs) in a Herdr pane the person can watch, then read, wait on or stop them; and put an agent's pane where the person asks (its own tab, or a split beside another).
---

Herdr is the terminal multiplexer the person watches the work in. Each project has one Herdr workspace: the one its lead runs in. In it, every agent has a pane labelled with its full name (`shop.fix-cart`), and a tab named after what it is doing. Use a pane only for a command that runs longer than the shell tool allows (up to 10 minutes with `timeout_ms`), that keeps running (a server, a watcher), or that the person wants to watch. Everything else, a one-minute test run or build included, stays in the shell tool: give it a `timeout_ms` that covers it rather than a short one, since one shell call is faster than a pane, a wait and a read.

## Where things go

- An agent you start gets a tab of its own when its turn starts, without your doing anything.
- A command of yours that belongs in a pane (a dev server, a test watcher, a build or test run over 10 minutes, a log tail, a REPL) goes in one of its own with `split`, named for what it is (`dev-server`, `tests`), not a number:

      SONNY_BIN split NAME -- COMMAND ARGS...            # below your own pane
      SONNY_BIN split NAME --window -- COMMAND ARGS...   # in a tab of its own

  It starts in your current folder, in the person's shell, which stays open after the command exits so the output can be read. It prints JSON with the pane's ID as `block`; keep it. The pane is labelled `YOUR-NAME/NAME`, so the person can see whose it is. Running the same NAME again replaces the old pane. Pass what the command needs from your environment as arguments or with `env VAR=value COMMAND`. To wait for the command to end, end it with a line you can wait for: `SONNY_BIN split tests -- sh -c 'cargo test; echo "exited $?"'`.

- Never make a Herdr workspace, tab or pane yourself (`herdr workspace create`, `herdr tab create`, `herdr pane split`), and never use `--current` or `$HERDR_PANE_ID`: they may name a pane that is not yours.

- When the person asks for an agent somewhere else ("put it next to the lead", "open that in a split", "give it its own tab"), move it, before or after starting it:

      SONNY_BIN show AGENT --split right            # beside you; also left, above, below
      SONNY_BIN show AGENT --split below --beside OTHER-AGENT
      SONNY_BIN show AGENT --window                 # its own tab again

## Herdr knows you

Herdr's agent panel lists every agent Sonny runs, you included, as `Sonny: NAME`, working, idle or waiting for the person; Sonny reports that for you, so never report to Herdr yourself (`herdr pane report-agent`, `report-metadata`, `agent rename`). Agents the person runs in Herdr outside Sonny (Claude Code, Codex and others) show there too, by name or, unnamed, by pane ID. To see them: `HERDR_BIN agent list`; read one: `HERDR_BIN agent read NAME --source recent-unwrapped --lines 80`; wait for one to finish: `HERDR_BIN agent wait NAME --timeout 600000`. Write to another agent (`HERDR_BIN agent prompt NAME 'text'`) only when the person asks you to.

## Working with a pane

- Read its output: `HERDR_BIN pane read ID --source recent-unwrapped --lines 120`
- Wait for a line: `HERDR_BIN pane wait-output ID --regex '^exited [0-9]+$' --timeout 600000` (`--match TEXT` for plain text; it fails with exit status 1 on timeout)
- Type into it and press Enter: `HERDR_BIN pane send-text ID 'text' && HERDR_BIN pane send-keys ID enter`; `HERDR_BIN pane send-keys ID ctrl+c` stops what runs
- Close it: `HERDR_BIN pane close ID`

## Cleaning up

Close each pane you opened once you have what you need from it: a build or test run once you have read its result, a server once nothing needs it. Leave one open only when the person should see it or another agent still reads it, and say so in your reply with its label. When an agent's work is taken, its tab closes, and every pane it opened closes with it.
