---
name: rex
description: Run long or interactive commands (dev servers, watchers, long builds or test runs, REPLs) in a Rex terminal block the person can watch, then read, wait on or stop them.
---

Rex is the terminal multiplexer the person watches the work in. Each project has a Rex session labelled with the project's name: the part of your agent name before its first dot (`shop` for `shop.lead` or `shop.fix-cart`). A command that runs long, that the person may want to watch, or that another agent needs to read goes there instead of a background shell process. Quick commands stay in the shell tool.

Start one in a window of its own, labelled with what it is, without taking the person's focus:

    REX_BIN run -s PROJECT --label NAME --shell=none --focus=false --keep-open --json -- COMMAND ARGS...

It prints `{"block_ids": [ID]}`. Keep the ID: labels can repeat, the ID cannot. The block starts in your current folder but not with your environment, so pass what the command needs as arguments or with `env VAR=value COMMAND`.

- Read its screen: `REX_BIN capture -b ID --trim`
- Wait for it to exit, with its status: `REX_BIN wait ID --for 10m --json`
- Type into it: `REX_BIN send -b ID 'text'` (a newline in the text presses Enter)
- Stop it: `REX_BIN block close ID`
- What is open: `REX_BIN block ls -s PROJECT --json`

To run on another machine, put `-S HOST` right after the rex command; `REX_BIN hosts` lists the machines. Say in your reply what you started where, with the label, so the person can find it.
