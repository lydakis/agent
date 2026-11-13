---
name: rex-windows
description: Use when `sonny where` names Rex or the person mentions Rex, a tab, window or split; run `sonny where` first whenever they ask which terminal you are in. Put an agent's window where the person asks (its own tab, a split beside another, or floating over it).
---

## Which terminal

The person can have this project open in more than one multiplexer at once, so run `sonny where` first: before you open, read or move anything in a terminal, and whenever the person asks where you are. It prints, in one line, the multiplexer and the workspace or session your `show_agent` calls act in now: where the person last used a window of yours, or of the agent that started you. Follow this skill when it names Rex, and the `herdr-panes` or `cmux-tabs` skill when it names another. When it says nothing opened would be seen, the person is in no multiplexer, and there is nowhere to put an agent's window. Run it again when the person says they moved to another window.

Rex is a terminal multiplexer the person watches the work in. Each project has one Rex session: the one its lead runs in. In it, every agent has a block labelled with its full name (`shop.fix-cart`), and a tab named after what it is doing. Your commands, long ones included, run in the shell tool, never in a block.

## Tabs or a split

A tab is the default. It is one more stream of work that the person can click into when they choose, to watch it, steer it and manage it there. A split puts something beside you, in the tab the person is looking at, so it asks for their attention now. Open one only when you want them to see an agent right away, one they should watch or talk to side by side with you. When unsure, use a tab.

Sessions are for leads: each project's lead has a Rex session of its own, and its agents get tabs and splits in it.

## Where things go

- An agent you start gets a tab of its own when its turn starts, without your doing anything.
- Never make a Rex session or window yourself (`rex new`, `rex window new`, `rex run`, `rex split`), and never use `$REX_SESSION` or `$REX_BLOCK`: they may name a terminal that is not yours.
- Never close a window or block the person has typed in or asked you to open; ask them first.

- When the person asks for an agent somewhere else ("put it next to the lead", "open that in a split", "give it its own tab"), or they should watch one beside you now, move it with the `show_agent` tool, before or after starting it: `split` puts it on that side of you, or of the agent `beside` names (`float` floats it over that window), and without `split` it gets its own tab again.

- Another project's lead you hand work to (`PROJECT.lead`) has a Rex session of its own, not a tab in yours. When the person wants to watch it, or you start a lead that is not open anywhere yet, open it there with `show_agent` and no `split`; one already open is left as it is.
