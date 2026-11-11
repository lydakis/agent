Feature: Each message's own number
Since 0.9.3 a message's handle is `BOT/N`; the form before it, `turn:BOT/N`, is refused as an invalid handle. The number is the same: `turn:calc.fix/48` is `calc.fix/48` now.

Look through the facts in `~/.agents/memory` (the person's and each project's) and the skills in `~/.agents/skills`, and rewrite each `turn:BOT/N` as `BOT/N`. Change nothing else in those files. Tell the person how many you rewrote and where. A checked-in file in a repository (a project's `.agents/skills`, say) that names one is the person's to change: tell them which, and leave it as it is.
