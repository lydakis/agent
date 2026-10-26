---
name: rex-windows
description: Run long or interactive commands (dev servers, watchers, builds or test runs past the shell tool's 10 minutes, REPLs) in a Rex terminal block the person can watch, then read, wait on or stop them; and put an agent's window where the person asks (its own tab, or a split beside another).
---

Rex is the terminal multiplexer the person watches the work in. Each project has one Rex session: the one its lead runs in. In it, every agent has a block labelled with its full name (`shop.fix-cart`), and a tab named after what it is doing. Use a block only for a command that runs longer than the shell tool allows (up to 10 minutes with `timeout_ms`), that keeps running (a server, a watcher), or that the person wants to watch. Everything else, a one-minute test run or build included, stays in the shell tool: give it a `timeout_ms` that covers it rather than a short one, since one shell call is faster than a block, a wait and a read.

## Where things go

- An agent you start gets a tab of its own when its turn starts, without your doing anything.
- A command of yours that belongs in a block (a dev server, a test watcher, a build or test run over 10 minutes, a log tail, a REPL) goes in one of its own with `split`, named for what it is (`dev-server`, `tests`), not a number:

      SONNY_BIN split NAME -- COMMAND ARGS...            # below your own block
      SONNY_BIN split NAME --window -- COMMAND ARGS...   # in a tab of its own

  It starts in your current folder, through the person's login shell, and stays open after the command exits so it can be read. It prints JSON with the `block` ID; keep it. The block is labelled `YOUR-NAME/NAME`, so the person can see whose it is. Running the same NAME again replaces the old block. Pass what the command needs from your environment as arguments or with `env VAR=value COMMAND`.

- Never make a Rex session or window yourself (`rex new`, `rex window new`, `rex run`, `rex split`), and never use `$REX_SESSION` or `$REX_BLOCK`: they may name a terminal that is not yours.

- When the person asks for an agent somewhere else ("put it next to the lead", "open that in a split", "give it its own tab"), move it, before or after starting it:

      SONNY_BIN show AGENT --split right            # beside you; also left, above, below
      SONNY_BIN show AGENT --split below --beside OTHER-AGENT
      SONNY_BIN show AGENT --window                 # its own tab again

## Working with a block

- Read its screen: `REX_BIN capture -b ID --trim --unwrap`
- Wait for it to exit, with its status: `REX_BIN wait ID --for 10m --json`
- Type into it and press Enter: `REX_BIN send -b ID 'text' && REX_BIN send-key -b ID enter` (a newline in the text is Ctrl-J to a full-screen program, not Enter)
- Close it: `REX_BIN block close ID`

To run on another machine, add `--rex-host HOST` to `split`, and put `-S HOST` right after the rex command for the rest; `REX_BIN hosts` lists the machines.

## Cleaning up

Close each block you opened once you have what you need from it: a build or test run once you have read its result, a server once nothing needs it. Leave one open only when the person should see it or another agent still reads it, and say so in your reply with its label. When an agent's work is taken, its tab closes, and every block it opened closes with it.
