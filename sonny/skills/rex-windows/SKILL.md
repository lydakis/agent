---
name: rex-windows
description: Run long or interactive commands (dev servers, watchers, long builds or test runs, REPLs) in a Rex terminal block the person can watch, then read, wait on or stop them; and put an agent's window where the person asks (its own tab, or a split beside another).
---

Rex is the terminal multiplexer the person watches the work in. Each project has a Rex session labelled with the project's name: the part of your agent name before its first dot (`shop` for `shop.lead` or `shop.fix-cart`). In it, every agent has a block labelled with its full name (`shop.fix-cart`), and a tab named after what it is doing. A command that runs long, that the person may want to watch, or that another agent needs to read goes there instead of a background shell process. Quick commands stay in the shell tool.

## Where things go

- An agent you start gets a tab of its own when its turn starts, without your doing anything.
- A command that belongs to you (your dev server, test watcher, log tail) goes in a split beside your own block, so it sits next to its owner:

      REX_BIN split -s PROJECT -b "$AGENT_BOT" --split below --ratio 35 --label NAME --shell=none --focus=false --keep-open --json -- COMMAND ARGS...

- A command that stands on its own (a long build, a REPL the person will use) gets a tab of its own:

      REX_BIN window new -s PROJECT NAME --shell=none --focus=false --keep-open --json -- COMMAND ARGS...

- When the person asks for an agent somewhere else ("put it next to the lead", "open that in a split", "give it its own tab"), move it, before or after starting it:

      SONNY_BIN show AGENT --split right            # beside you; also left, above, below
      SONNY_BIN show AGENT --split below --beside OTHER-AGENT
      SONNY_BIN show AGENT --window                 # its own tab again

Label what you open with what it is (`dev-server`, `test-watch`), not a number. Each create prints JSON with `block_ids`: keep the block ID, since labels can repeat and the ID cannot. A block starts in your current folder but not with your environment, so pass what the command needs as arguments or with `env VAR=value COMMAND`.

## Working with a block

- Read its screen: `REX_BIN capture -b ID --trim --unwrap`
- Wait for it to exit, with its status: `REX_BIN wait ID --for 10m --json`
- Type into it and press Enter: `REX_BIN send -b ID 'text' && REX_BIN send-key -b ID enter` (a newline in the text is Ctrl-J to a full-screen program, not Enter)
- Stop it: `REX_BIN block close ID`
- What is open: `REX_BIN block ls -s PROJECT --json`

To run on another machine, put `-S HOST` right after the rex command; `REX_BIN hosts` lists the machines. Say in your reply what you started where, with the label, so the person can find it.
