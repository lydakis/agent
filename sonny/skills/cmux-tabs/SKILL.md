---
name: cmux-tabs
description: Use this, not the cmux skill, when `sonny where` names cmux or the person mentions cmux, a workspace, tab, pane or split; run `sonny where` first whenever they ask which terminal you are in. Put an agent's tab where the person asks (its own tab, or a split beside another), and see the other agents the person runs in cmux.
---

## Which terminal

The person can have this project open in more than one multiplexer at once, so run `sonny where` first: before you open, read or move anything in a terminal, and whenever the person asks where you are. It prints, in one line, the multiplexer and the workspace or session your `show_agent` calls act in now: where the person last used a window of yours, or of the agent that started you. Follow this skill when it names cmux, and the `herdr-panes` or `rex-windows` skill when it names another. When it says nothing opened would be seen, the person is in no multiplexer, and there is nowhere to put an agent's window. Run it again when the person says they moved to another window.

cmux is a terminal the person watches the work in. Your shell is not in a cmux tab, by design: it runs in the agent daemon, so `CMUX_SURFACE_ID`, `CMUX_WORKSPACE_ID` and the other tab variables are unset. That does not mean the person is outside cmux, and it is no reason to stop; this skill's commands work from your shell. Each project has one cmux workspace: the one its lead runs in. In it, every agent has a tab titled with its full name (`shop.fix-cart`). Your commands, long ones included, run in the shell tool, never in a tab.

## Tabs or a split

A tab is the default. It is one more stream of work, beside the lead's tab, that the person can click into when they choose, to watch it, steer it and manage it there. A split puts something beside you, in the workspace the person is looking at, so it asks for their attention now. Open one only when you want them to see an agent right away, one they should watch or talk to side by side with you. When unsure, use a tab.

Workspaces are for leads: each project's lead has a cmux workspace of its own, and its agents get tabs and splits in it. When the person asks for a workspace, they mean a project's: open another project's lead in its own with `show_agent`, or make the project with `new_project` and then show its lead.

## Where things go

- An agent you start gets a tab of its own when its turn starts, without your doing anything.
- Never make a cmux workspace, tab or split yourself (`cmux new-workspace`, `new-surface`, `new-split`, `new-pane`), and never leave out `--surface`: without it, cmux picks a tab that may not be yours.
- Never close a tab the person has typed in or asked you to open; ask them first.

- When the person asks for an agent somewhere else ("put it next to the lead", "open that in a split", "give it its own tab"), or they should watch one beside you now, move it with the `show_agent` tool, before or after starting it: `split` puts it on that side of you, or of the agent `beside` names, and without `split` it gets its own tab again.

- Another project's lead you hand work to (`PROJECT.lead`) has a cmux workspace of its own, not a tab in yours. When the person wants to watch it, or you start a lead that is not open anywhere yet, open it there with `show_agent` and no `split`; one already open is left as it is.

## cmux knows you

cmux's sidebar shows what each agent Sonny runs is doing, you included (working, waiting for the person, done), and rings the tab of one that needs them; Sonny tells cmux that for you, so never do it yourself (`cmux set-status`, `set-progress`, `log`, `notify`). Agents the person runs in cmux outside Sonny (Claude Code, Codex and others) are tabs too. To see every tab: `CMUX_BIN tree --all`; read one: `CMUX_BIN read-screen --surface ID --lines 120`. Type into another agent's tab only when the person asks you to.

## When cmux refuses

Your shell is not a program cmux started, so cmux takes its calls only in Automation mode, which lets the person's own programs reach it. When a call says cmux refused or denied the connection ("only processes started inside cmux can connect"), tell the person, with what cmux said, that cmux's Settings > Automation needs "Automation mode" for agents to reach it; never change cmux's settings yourself.
