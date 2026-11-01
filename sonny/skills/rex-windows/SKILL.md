---
name: rex-windows
description: Use whenever a command runs long or is interactive (dev servers, watchers, builds or test runs past the shell tool's 10 minutes, REPLs): run it in a Rex terminal block the person can watch, then read, wait on or stop it; and put an agent's window where the person asks (its own tab, or a split beside another).
---

Rex is the terminal multiplexer the person watches the work in. Each project has one Rex session: the one its lead runs in. In it, every agent has a block labelled with its full name (`shop.fix-cart`), and a tab named after what it is doing. Use a block only for a command that runs longer than the shell tool allows (up to 10 minutes with `timeout_ms`), that keeps running (a server, a watcher), or that the person wants to watch. Everything else, a one-minute test run or build included, stays in the shell tool: give it a `timeout_ms` that covers it rather than a short one, since one shell call is faster than a block, a wait and a read.

## Tabs or a split

A tab is the default. It is one more stream of work that the person can click into when they choose, to watch it, steer it and manage it there. A split puts something beside you, in the tab the person is looking at, so it asks for their attention now. Open one only when you want them to see something right away: a server or a CLI's output you are showing them, or an agent they should watch or talk to side by side with you. When unsure, use a tab.

Sessions are for leads: each project's lead has a Rex session of its own, and its agents and commands get tabs and splits in it.

## Where things go

- An agent you start gets a tab of its own when its turn starts, without your doing anything.
- A command of yours that belongs in a block (a dev server, a test watcher, a build or test run over 10 minutes, a log tail, a REPL) goes in one of its own with the `run_in_pane` tool, named for what it is (`dev-server`, `tests`), not a number. A tab is the default; `split` (right, left, above, below) puts it beside you, to show it now, and `split` `float` floats it over your window, for something the person deals with once and closes: a sign-in, a quick look.

  It runs as `sh -c` runs the command, in your folder (or `cwd`), in the person's shell, which stays open after the command exits so the output can be read. The result holds the block's ID as `block`; keep it. The block is labelled `YOUR-NAME/NAME`, so the person can see whose it is. Running the same name again replaces the old block. Your shell's variables are not there, so put what the command needs in it: `env VAR=value COMMAND`. To wait for the command to end, end it with a line you can wait for: `cargo test; echo "exited $?"`.

- Never make a Rex session or window yourself (`rex new`, `rex window new`, `rex run`, `rex split`), and never use `$REX_SESSION` or `$REX_BLOCK`: they may name a terminal that is not yours.
- Never close a window or block the person has typed in or asked you to open; ask them first. Close only what you opened for your own work.

- When the person asks for an agent somewhere else ("put it next to the lead", "open that in a split", "give it its own tab"), or they should watch one beside you now, move it with the `show_agent` tool, before or after starting it: `split` puts it on that side of you, or of the agent `beside` names (`float` floats it over that window), and without `split` it gets its own tab again.

- Another project's lead you hand work to (`PROJECT.lead`) has a Rex session of its own, not a tab in yours. When the person wants to watch it, or you start a lead that is not open anywhere yet, open it there with `show_agent` and no `split`; one already open is left as it is.

## Working with a block

- Read its screen: `REX_BIN capture -b ID --trim --unwrap`
- Wait for it to exit, with its status: `REX_BIN wait ID --for 10m --json`
- Type into it and press Enter: `REX_BIN send -b ID 'text' && REX_BIN send-key -b ID enter` (a newline in the text is Ctrl-J to a full-screen program, not Enter)
- Close it: `REX_BIN block close ID`

## Cleaning up

Close each block you opened once you have what you need from it: a build or test run once you have read its result, a server once nothing needs it. Leave one open only when the person should see it or another agent still reads it, and say so in your reply with its label. When an agent's work is taken, its tab closes, and every block it opened closes with it.
