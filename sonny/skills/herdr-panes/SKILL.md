---
name: herdr-panes
description: Use this, not the herdr skill, when `sonny where` names Herdr or the person mentions Herdr, a pane, tab or split; run `sonny where` first whenever they ask which terminal you are in. Put an agent's pane where the person asks (its own tab, or a split beside another), and see the other agents the person runs in Herdr.
---

## Which terminal

The person can have this project open in more than one multiplexer at once, so run `sonny where` first: before you open, read or move anything in a terminal, and whenever the person asks where you are. It prints, in one line, the multiplexer and the workspace or session your `show_agent` calls act in now: where the person last used a window of yours, or of the agent that started you. Follow this skill when it names Herdr, and the `cmux-tabs` or `rex-windows` skill when it names another. When it says nothing opened would be seen, the person is in no multiplexer, and there is nowhere to put an agent's window. Run it again when the person says they moved to another window.

Herdr is a terminal multiplexer the person watches the work in. Your shell is not in a Herdr pane, by design: it runs in the agent daemon, so `HERDR_ENV`, `HERDR_PANE_ID` and the other pane variables are unset. That does not mean the person is outside Herdr, and it is no reason to stop. Herdr's own `herdr` skill, which asks for `HERDR_ENV=1`, does not apply here; this skill's commands work from your shell. Each project has one Herdr workspace: the one its lead runs in. In it, every agent has a pane labelled with its full name (`shop.fix-cart`), and a tab named after what it is doing. Your commands, long ones included, run in the shell tool, never in a pane.

## Tabs or a split

A tab is the default. It is one more stream of work that the person can click into when they choose, to watch it, steer it and manage it there. A split puts something beside you, in the tab the person is looking at, so it asks for their attention now. Open one only when you want them to see an agent right away, one they should watch or talk to side by side with you. When unsure, use a tab.

Sessions are for leads: each project's lead has a Herdr workspace of its own, and its agents get tabs and splits in it.

## Where things go

- An agent you start gets a tab of its own when its turn starts, without your doing anything.
- Never make a Herdr workspace, tab or pane yourself (`herdr workspace create`, `herdr tab create`, `herdr pane split`), and never use `--current` or `$HERDR_PANE_ID`: they may name a pane that is not yours.
- Never close a tab or pane the person has typed in or asked you to open; ask them first.

- When the person asks for an agent somewhere else ("put it next to the lead", "open that in a split", "give it its own tab"), or they should watch one beside you now, move it with the `show_agent` tool, before or after starting it: `split` puts it on that side of you, or of the agent `beside` names, and without `split` it gets its own tab again.

- Another project's lead you hand work to (`PROJECT.lead`) has a Herdr workspace of its own, not a tab in yours. When the person wants to watch it, or you start a lead that is not open anywhere yet, open it there with `show_agent` and no `split`; one already open is left as it is.

## Herdr knows you

Herdr's agent panel lists every agent Sonny runs, you included, as `Sonny: NAME`, working, idle or waiting for the person; Sonny reports that for you, so never report to Herdr yourself (`herdr pane report-agent`, `report-metadata`, `agent rename`). Agents the person runs in Herdr outside Sonny (Claude Code, Codex and others) show there too, by name or, unnamed, by pane ID. To see them: `HERDR_BIN agent list`; read one: `HERDR_BIN agent read NAME --source recent-unwrapped --lines 80`; wait for one to finish: `HERDR_BIN agent wait NAME --timeout 600000`. Write to another agent (`HERDR_BIN agent prompt NAME 'text'`) only when the person asks you to.
