# Sonny (prototype)

A terminal client for the daemon that lays its work out in Rex, Herdr or
cmux. It speaks the daemon's socket protocol through `agent-client`; nothing
in the daemon or CLI knows it exists.

    brew install lydakis/agent/sonny         # Sonny, and the daemon's `agent` formula with it
    brew install lydakis/agent/agent         # just the daemon
    sonny agent ls --pretty                  # the daemon's CLI, as Sonny runs it

Both formulas build from the release's source on your machine (macOS or
Linux), so nothing is signed; Sonny starts the `agent` formula's daemon by its
`opt` path. On Linux, each release also has
`sonny-VERSION-linux-ARCH.tar.gz` (x86_64 and aarch64): static builds of
Sonny in `bin/` and its daemon in `libexec/`. Unpack it anywhere and run
`bin/sonny`, or let [install.sh](../install.sh) fetch, check and link the
latest one into `~/.local`:

    curl -fsSL https://raw.githubusercontent.com/lydakis/agent/main/install.sh | sh

With `cargo install`, build both (`--path .` for `agent` and
`--path sonny`) and set `AGENT_BIN` to that `agent`; Sonny never takes an
`agent` from `PATH`, which may be another program. `sonny agent ARGS` runs
the one Sonny uses, with the keys in `~/.agent/env`, whatever else is called
`agent` on your `PATH`. It is also the one way in agents are told about:
Sonny sets `AGENT_CLI` to its own `sonny agent`, so the instructions it
composes, those the CLI composes for agents started through it, and the
skills it writes name `"…/sonny" agent` wherever they would say
`"$AGENT_BIN"`.

    sonny                                    # this folder's coordinator, <project>.lead
    sonny --model PROVIDER/MODEL             # the same, making the project the first time
    sonny --bot NAME                         # any one bot

Opened in a folder, Sonny is that project's lead, `<project>.lead`, made by
Sonny in its `lead` role, with the folder's
`.agents/project.toml` written when it had none. Every folder can have a
lead: outside git, the folder Sonny opens in is its own project, whatever
project holds the folder above it. In a git repository, Sonny opens the
nearest project in that repository, else makes the repository's top one,
so `src` reaches the repository's lead and a task's worktree its main
checkout's; a subfolder of a repository is its own project once it has a
file, which a lead makes with its new_project tool. A project whose file says
`subfolders = "project"` keeps the folders below it: Sonny opened in any of
them that has no project file of its own, a repository among them, opens
that project's lead (the nearest project above decides; the home folder's
file never does). A lead made below another project's folder says which
lead holds the folder above. Sonny refuses to make a lead in a task's own
folder under `~/.agent`.

`~/.agent/env` (`KEY=VALUE` lines, readable only by you) holds the provider
keys, and `AWS_PROFILE` for Bedrock, which Sonny applies over your shell's to
the daemon, the approver and the lookout. Settings go in
`~/.agent/config.toml`, as Codex keeps them: `provider` (the daemon's
`--provider` specs, after the shell's `AGENT_PROVIDER`), `model` (a new
agent's when none is named) and the efforts below. When no provider is named
and no key is set, the first `sonny` in a terminal asks for one, as Codex and
Claude Code ask for a sign-in: a ChatGPT plan (signed in with `agent login
chatgpt`), an Anthropic, OpenAI or OpenRouter key, or Bedrock with an AWS
profile. A new project's model is `--model`, else the project file's or the
role's, else `AGENT_MODEL`, else the config's `model`, else the newest
agent's; with none, Sonny lists what the providers offer and keeps the pick
as `model`. Either step also suggests efforts, `[sonny] lead_effort =
"medium"` and `task_effort = "high"`, unless the config already sets them.
Outside a terminal both fail, naming what to set. The first start also writes
`~/.agent/models`, the list agents read with `agent models`, when there is
none, as `agent models --discover` does.

A lead's transcript starts with a welcome, as Claude Code's does: Sonny,
the robot the client is named for, as a portrait drawn in half blocks,
beside the folder, the lead's model and a few commands. A new lead opens
on it; a long conversation shows it above its first message, once scrolling
up has reached it. A side chat has none.

Sonny's two roles are normal profiles the person can read and edit in
`~/.agents/agents`, beside their own: `lead.md`
([agents/lead.md](agents/lead.md)), the lead's role, and `task.md`
([agents/task.md](agents/task.md)), what every other agent Sonny starts
is told after its instructions and any role (`--profile`) it starts in:
tasks, a trigger's agents and `sonny --new` bots. Sonny writes both at
each start, as it writes its skills: it rewrites a file when its text
changes, and writes it again once it is deleted, but never over an edit,
since it notes a fingerprint of what it wrote in `~/.agent/sonny/agents`,
and a file that no longer matches, or one Sonny never wrote, is the
person's. When a new Sonny brings new text for a role or skill you
edited, it sets that text beside yours in `~/.agent/sonny/upgrades`, with
what you started from in `~/.agent/sonny/shipped` when it kept it, and
the first bot you open after the upgrade tells you and offers to bring
Sonny's changes into your copy. A copy left the same as Sonny's new text
is Sonny's again from the next start. A folder's own
`.agents/agents/lead.md` or `task.md` comes first. The task settings from `.agents/project.toml` are composed after
the lead's role, not kept in the file.

#### Roles for leads and tasks

A lead runs as the `lead` profile and every other agent is told the `task`
one, unless set to another. Another kind of lead is a profile of its own,
say `~/.agents/agents/my-lead.md`, that builds on the lead's:

```markdown
---
extends: lead
description: A lead that reviews each change before it lands
skills: [triage]
---
Review each change a task makes before it lands.
```

and a project file names it for its lead:

| What | `~/.agent/config.toml` | `.agents/project.toml` |
| --- | --- | --- |
| The profile a lead runs as | `[sonny] lead_profile` | `profile` |
| The profile every other agent is told | `[sonny] task_profile` | `threads_profile` |

Sonny ships one such profile, `superlead` ([agents/superlead.md](agents/superlead.md)),
written to `~/.agents/agents` beside the others and used by no lead
unless a project sets `profile = "superlead"`. It builds on the lead's:
the folder holds other projects, so the lead subscribes to their leads
and routes work and answers between them as it does between tasks. Sonny
itself knows only leads and tasks.

The project's wins, then the config's, then `lead` and `task`. A name that
is not a profile in the folder's or the person's `.agents/agents` (nor one
Sonny ships) stops the lead or task from being made, with
`profile_not_found`. The profile is read when the lead's instructions are
composed, so `/instructions` takes a change. `extends` and `skills` work
in any profile ([CLIENT.md](../docs/CLIENT.md)); a profile that extends
`lead` or `task` is not offered to agents as a role to start a peer in.

#### Effort for leads and tasks

Each agent's effort is the daemon's to keep; Sonny only picks what a new
one starts with, and imposes nothing. With nothing set, a new lead starts
at its model's own effort (`AGENT_EFFORT` when its model is
`AGENT_MODEL`'s), and each task at its lead's. Defaults go in
`~/.agent/config.toml` (`agent_client::config`), and a project's
`.agents/project.toml` overrides them for that project:

| What | `~/.agent/config.toml` | `.agents/project.toml` |
| --- | --- | --- |
| A new lead's effort | `[sonny] lead_effort` | `effort` |
| Each new task's model | (the lead's) | `threads_model` |
| Each new task's effort | `[sonny] task_effort` | `threads_effort` |
| The approver's judge | `[judge] model`, `effort` | |
| What `/side`'s copy may do | `[sonny] side_chat` (`answer` or `fork`) | |
| The chime, and every tool call drawn | `[sonny] chime`, `verbose` (true or false) | |
| A guess at your next message | `[sonny] suggest` (true, the default, or false) | |
| News from outside for the lookout | `[sonny.sources.NAME] command`, `every` | |

`start_task` applies them in code, so the lead's prompt says nothing about
effort; the lead names a `model` or `effort` only for one the person asks
for. A task on its lead's model with neither set takes the lead's effort,
as before; one on another model takes that model's own. `/model` changes a
lead's effort after it is made.

A lead moved to another
folder (`--workspace`) comes back with the next message Sonny sends it from
its own, history and all. When Sonny runs in a Rex block (`rex` found as
`$REX`, on the PATH, or in the Mac app), a Herdr pane (`herdr` found as
`$HERDR` or on the PATH) or a cmux tab (`cmux` found as the tab's own CLI,
on the PATH, or in the Mac app), the project gets one home there: in Rex a
session (the one Sonny was opened in, else the one the lead is in, else one
labelled with the project's name), in Herdr or cmux a workspace, chosen the
same way. Each task the lead (or one of its tasks) starts gets a window (a
Herdr tab; in cmux a tab in the lead's pane) there running Sonny on that
task, without taking focus, its block (pane, cmux tab) labelled with the
task's name. In Rex, each window reports OSC 7501 status, so Rex
shows which task is working, waiting on an approval or done. In Herdr, Sonny
supports Herdr the way its own agents do: each Sonny reports its bot through
`herdr pane report-agent`, so the agent panel lists it as `Sonny: NAME`
(working, idle, or blocked on an approval), the workspace rolls that up and
notifies, `herdr agent wait` and `herdr agent prompt` reach it by name
(`shop-fix-cart` for `shop.fix-cart`), and after a Herdr restart the pane runs
`sonny` on the same bot again (when `sonny` is on the PATH; Herdr before
0.9.2 lists Sonny but takes no command to restart it). Sonny reports from a
pane of the Herdr server on its own machine: one reached by plain `ssh` from a
Herdr pane is not listed, one in a remote machine Herdr itself connects to is.
The daemon, lookout and approver Sonny starts run in a session of their own,
so closing their pane or stopping Herdr leaves them and their turns running.
In cmux, which reads OSC 7501 and OSC 777 itself, the workspace's sidebar row
shows which task is working, waiting on an approval or done, and a
notification rings the tab it came from. Each Sonny gives its tab a resume
command (`cmux surface resume set`, shown as `Sonny: NAME`), so after cmux
relaunches the tab runs `sonny` on the same bot again, once you allow
commands starting with `sonny` from the tab's Resume Commands menu (until
then cmux offers it to run by hand); quitting Sonny clears it. cmux lets
only programs started in its tabs reach it, by default; agents' shells run
in the daemon, not in cmux, so set cmux's Settings > Automation to
"Automation mode" (your own programs, no ancestry check) for agents to run
commands in tabs and read them. The lookout Sonny starts in cmux reaches it
either way; one started outside cmux needs Automation mode too.
The place a
project was laid out in is kept in `~/.agent/sonny/places`, so an agent's
`run_in_pane` and `show_agent` calls go to that Herdr or cmux workspace or
Rex session, in that multiplexer, rather than one guessed from where the
lookout answering them runs, which is in neither. A task's window
closes once its work is taken: its turn completed and the lead (or the task
that started it) waited on it. One that failed, stopped or waits on you stays
open, and a task given more work opens again; ask the lead to show one, or
run `sonny --bot NAME`. `--rex-host HOST` puts the session on another Rex
server; `--no-layout` turns the layout off. Sonny writes the skill for the
multiplexer it runs in to `~/.agents/skills` (`rex-windows`,
`herdr-panes` or `cmux-tabs`, from `skills/`), so any agent can run a long command where
the person can watch it, and read it back: the `run_in_pane` tool
(`name`, `command`, `split` right, left, above or below) opens a block
labelled `AGENT/NAME` in a tab of its own (or on that side of the agent's
own block) and gives its ID. The `show_agent` tool (`agent`, `split`,
`beside`) moves a task's window. Tabs are the default; the skills tell agents to split only
to show the person something now, such as a server's or a tool's output, or
an agent to watch beside them. Sessions are for leads, tabs and splits for
their tasks: `show_agent` on `PROJECT.lead`, from another project's lead (one
that leads the projects in its folder), opens that lead's Sonny in its own
project's session or workspace, made when needed in the same multiplexer,
and leaves one already open alone. Sonny never lays another project's lead,
or its tasks, out in this one's home. `sonny --bot PROJECT.lead` anywhere is
that lead's Sonny too, laying out its tasks where it runs. Opened in a
session that already holds other work, Sonny takes that session as the
project's home and leaves the rest alone: tasks get tabs of their own, and a
tab that holds the person's own panes keeps its name. Herdr and cmux have no
exit status to wait for, so their skills have agents end such a command with
a line to wait for.
The skill tells agents to close what they opened once read; whatever a task
left open closes with its window.

#### T3 Code

`sonny acp` is Sonny for [T3 Code](https://github.com/pingdotgg/t3code): it
speaks the Agent Client Protocol on stdio, so T3 Code runs it as a provider,
one process for each thread. In T3 Code's Settings, add an "ACP Registry"
provider, configure it by hand as a local command, and give it the
executable `sonny` (or the full path `command -v sonny` prints) with the one
argument `acp`. A thread opened in a folder is that folder's lead, the
one `sonny` there opens, set up the same way: its rules, skills and memory,
Sonny's tools, its approval mode and sandbox, and the lookout. A call that
waits for you is asked in the thread (Allow or Deny); one the judge passes
runs without asking. The turns the lead takes on its own, as when the lookout
wakes it because a task finished, show in the thread between yours, as work
the agent started (the protocol's version 2 draft, which T3 Code speaks).
Each task the lead starts opens as a T3 Code thread with the task's brief as
its first message; that thread is the task's, so it shows the task's work and
steers it. A task started while your prompt runs is the lead thread's child,
nested under it, opened with T3 Code's `delegate_task` (T3 Code opens it in
the lead's folder; the task still works in its own worktree), and your
prompt's answer waits until it is open. T3 Code's message that a child ended
starts no turn: the lead hears of its tasks from the lookout. A task started
in a turn the lead took on its own, which T3 Code takes no child of, opens as
a thread of its own with `t3_thread_launch`, in the task's worktree (in the
project's folder when T3 Code does not take the worktree as one of the
project's). Sonny calls these tools by the credential T3 Code scopes to the
lead's thread, which no program Sonny starts inherits. Sonny takes the person's words out of the instructions T3 Code
wraps a prompt in, which name tools Sonny's agents do not have, so T3 Code's
Plan mode does not reach them; plan with the lead instead. What thread is
which bot is kept in `~/.agent/sonny/t3`, so a thread T3 Code resumes is the
same bot. T3 Code's model picker lists what the providers list, as `/model`
does, with `~/.agent/models`' notes and the models only it names, after
"Default", the model the lead or task runs; its effort picker lists the
family's efforts after "Default" too. A pick made before the first prompt is
for a lead Sonny makes: the folder's lead that is there already keeps its own,
since T3 Code puts its last pick to every new thread. One made later is the
lead's from its next turn. A thread offers only its bot's provider family,
since a bot changes model only within it, and a task's thread keeps the model
its lead gave the task.

Enter sends (and steers a running turn), Esc interrupts, `y`/`n` answer a
pending approval, Ctrl-C clears the composer or quits. Shift-Enter or
Alt-Enter (or Ctrl-J) starts a new line. The composer takes a shell's editing
keys, as Codex's does: Ctrl-A/E and Ctrl-B/F move, Alt-B/F (or Alt- or
Ctrl-arrows) move a word, Ctrl-P/N move a row, Ctrl-H and Ctrl-D delete a
character, Ctrl-W (or Alt-Backspace) and Alt-D (or Alt-Delete) a word,
Ctrl-U and Ctrl-K to the line's start and end, and Ctrl-Y puts back what the
last of those took. Shift-Enter needs a terminal that speaks the kitty
keyboard protocol, which Sonny asks for as Codex does (Kitty, Ghostty,
WezTerm, iTerm2, Herdr and others); elsewhere it reads as Enter, and Alt-Enter
or Ctrl-J still start a line.

On the full screen the mouse edits the composer too, as in Codex: a click
puts the cursor there, a drag selects (and copies), two clicks select a word
and three a line. Typing replaces the selection, Backspace or Delete takes it
out, Ctrl-W, Ctrl-U or Ctrl-K cut it for Ctrl-Y, and Esc lets go of it.

![A selection in the composer](../docs/sonny/composer-selection.png)

After a turn you started from this window ends well, a copy of the agent
guesses what you will type next, as Claude Code's prompt suggestions and
Codex's composer predictions do, and the empty composer shows it dim for
Tab to take. Tab puts it in the composer to edit or send; it never sends
itself. The copy is forked where the turn ended, with the agent's tools and
model so its prompt cache is warm, but allowed no call; it is asked for 2 to
12 words in your style, or nothing when the next step is not obvious, and is
deleted once it answers. That is one small model call per turn of yours
(a steer counts for the turn it joins): none follows a turn the lead, the
lookout or another window started, one that failed or was stopped, or one
that ended on a question, so watching a task costs nothing.
`[sonny] suggest = false` turns it off. Typing hides the guess, a new turn
drops it, and Tab does what it does otherwise while none shows. Every Sonny
has it, so a side chat or a task opened in the pane does too.

![The guess, dim in the empty composer after a reply](../docs/sonny/suggest-shown.png)

![Tab took it into the composer, unsent](../docs/sonny/suggest-taken.png)

![The same in a side chat in the pane](../docs/sonny/suggest-side.png)

Pictures go with a message: Ctrl-V takes the one on the clipboard, as in
Codex and Claude Code (terminals paste only text), and an image file
dropped on the terminal, or its path pasted, is attached rather than typed.
Each shows as `[Image #N]` where it goes in the message; a PNG longer than
1568 pixels on a side is scaled down first. Web addresses, a link's text and
paths of files that are there open on a click (Cmd-click in most terminals,
Ctrl-click in Herdr).

`!COMMAND` runs a command of your own with your shell, in the agent's
folder, in any agent's window: the lead's or a task's. You see what it
printed at once, and as soon as it ends the agent is given it too, in
Codex's `<user_shell_command>` record (the command, its exit code and its
output, a long output's start and end), and answers it as Claude Code's
does: an idle agent in a turn of its own, a busy one in its running turn,
as a steer. What it printed is not your words, so it approves nothing the
agent then does. Esc stops a command still running.

![A command run with !, its output, and the agent answering it](../docs/sonny/shell-passthrough.gif)

An emoji sent alone is a reaction, not a message: it goes under the
agent's last reply and is attached to the agent's next call, so it starts
no turn. While something waits to go with the next call, the footer shows
a small 📎 (`📎 2` for two). `/react` picks an older reply, and sending an emoji already on a
reply takes it off. The agent reacts too, with a `react` tool Sonny
declares on it: 👍 or 🎉 under
your message when that is all it needs, or 👀 while it looks, taken off
once it answers. Every reaction sits right under the message it is on,
however far up that is, and never as a row of its own: yours under the
reply, the agent's under your message, and a lead's under your words in
one of its tasks, with its name (`👀 calc.lead`) under the agent's own.

![The agent's 🎉 under a message, and a 👍 on its reply](../docs/sonny/reactions.png)

![/react picking an older reply](../docs/sonny/reactions-picker.png)

`/` lists the commands:

- `/side [QUESTION]` opens a side chat with a copy of the agent in this
  window, taken where it is now, mid-turn included, in the pane beside
  the conversation.
  The copy answers from the agent's history and may call no tool, unless
  `~/.agent/config.toml` sets `[sonny] side_chat = "fork"`: then it is a
  full fork that can do anything the agent can, with its tools, in its
  folder, under its approvals. Follow-ups stay with it, and Esc goes
  back. In the pane, every key goes to the side chat but Ctrl+\, which
  goes back to the main composer while the side chat stays open. The agent
  carries on meanwhile, in view. The daemon deletes the copy once it has been idle 15
  minutes (`fork --expires`), and no copy shows among the project's
  agents, in `/status`, or to the lookout.

  ![A side chat forked with every tool writes a file beside the lead](../docs/sonny/side-fork.png)
- `/config` (or `/settings`) lists every key `~/.agent/config.toml` takes,
  with its value or what its default is, over the composer. ↑/↓ picks
  one; ←/→ steps a key with a few values (efforts, `web_search`,
  `senders`, `side_chat`, `chime`, `verbose`, `suggest`) through them
  and its default; Enter types one that takes a name (`model`, `provider`,
  `judge.model`, the profiles), empty for its default; ⌫
  takes a key back to its default. Each change is written at once, the
  file's comments and order kept, and a file that does not read is
  refused by name, as every reader refuses it.

  ![/config over the composer, side_chat set to fork](../docs/sonny/config.png)
- `/model` picks the lead's model, from every model its provider family
  lists, and its effort (←/→), and writes them to the project's
  `.agents/project.toml` when the folder has one. A pick made while a turn
  runs shows in the footer and applies when the lead is idle; one that
  another client overtakes is dropped.
- `/compact` summarizes the lead's conversation before its next model
  call: at the running turn's next step, or before an idle lead's next
  reply. The daemon also compacts on its own once a call's input reaches
  the bot's `compact_tokens`, which follows the model's context window
  unless set (900,000 tokens on a 1M window).
- `/instructions` gives the lead its folder's instructions again: its
  role, `AGENTS.md`, skills and memory. Sonny does this on its own when
  it opens a lead whose instructions it saw composed from the folder, and
  leaves ones set any other way alone.
- `/approvals` picks whether calls wait for a verdict: `auto`, a judge
  model (a new lead starts here, and Sonny starts `agent approver` when
  nothing serves it), `ask`, you, or `full`, none. Tasks the lead makes take
  the gate it had; a pick reaches every agent the lead made too, one busy
  now once it is idle. While the judge reviews a call the status row says
  `Reviewing approval request`, and a denied call is drawn as
  `✗ Request denied for …` with the judge's reason, in either view.
  The mode sets the lead's sandbox too, as Codex's modes do: in `auto`
  and `ask` its commands, writes and edits stay in its folder (and
  `/tmp`), reach the network, and may commit to the folder's repository,
  whose hooks and config stay read-only; a call that leaves the sandbox
  waits for the verdict. In `full` the lead runs unconfined. Every task
  is sandboxed the same way in its own folder, but in `full`, where
  nothing would answer a call that asks to leave the sandbox, tasks run
  unconfined too, as under Codex's full access; a pick sets the tasks'
  sandboxes along with their gates.
  A lead made before Sonny sandboxed leads is sandboxed when Sonny opens
  it. On Linux sandboxes need bubblewrap. A tool that keeps a daemon of its
  own, such as mcpx, can't run inside a sandbox: name it in
  `~/.agent/config.toml`, `[sonny] unsandboxed = ["mcpx"]`, and every agent
  runs it outside without asking, as a Codex rule that allows a command
  does. Each entry is a command's first words (`"docker compose"`), and a
  call runs outside only when everything it runs is named, so
  `mcpx … | jq` still asks. Sonny gives a lead the list again when it opens
  it.
- `/approve` lists the calls the judge denied while Sonny watched, with
  the reason for each. Enter writes the call out whole, and a second
  Enter sends your approval of that exact call as your own message, so
  the agent may retry it once; the retry is still reviewed, and the judge
  reads your words as consent. This is Codex's `/approve`.
- `/status` shows the project's agents in the pane beside the
  conversation. The lead comes first; its tasks
  follow grouped by where they stand: **Needs you** (waiting on your
  approval, asking you a question, or failed),
  **Working** (a turn running, queued, rate limited or waiting on other
  tasks), **Ready for review** (finished, its last reply linking a pull
  request), **Idle** (finished or stopped with nothing open), and
  **Resolved**, only counted until `r` shows them. As threads in a
  claude.ai project do, a task resolves itself with the `resolve_task`
  tool once nothing is left to do in it, such as when its pull request
  merged or you dropped the work, and the lead can resolve any of its
  tasks the same way; the lead hears when a task resolved itself, and the
  task's window closes once its turn ends. New work sent to the task
  reopens it. Each agent's
  time is how long its running turn has been at it, or how long its last
  turn took. An agent with triggers or lookout subscriptions of its own
  shows `↻ 2 triggers` beside its model.
- `/background` (or `/triggers`, `/lookout`) lists, in the pane beside the
  conversation, what runs for the project's agents with
  no one typing: the triggers that wake them, with
  when each fires, its message and what its last fire did, and the
  lookout's subscriptions, with how many checks each asked for in the last
  day. `↑`/`↓` picks one, `f` fires a trigger now, and `d` pressed twice
  removes it; the subscription Sonny keeps for a lead's tasks stays. The
  footer says `↻ 2 in background` while the window's agent has any.

  ![/background in the pane](../docs/sonny/pane-background.png)
- `/usage` shows, in the pane beside the conversation,
  the tokens the project's agents spent in the last 24 hours: in all, as
  stacked bars over time (output, input read fresh, and input read from the
  provider's cache), by agent and by model. `d`, `w` and `m` show the last
  day, week or month, `a` every agent in the store, by project, and `c`
  hides the cached reads, so what the provider charges most for stands out.
  A turn counts where it started. It reads again every minute.

  ![/usage in the pane](../docs/sonny/usage.gif)
- `/mcp` lists your MCP servers (`[sonny.mcp_servers]` in
  `~/.agent/config.toml`, kept with `sonny mcp add|remove|list|tool`), each with
  its tools or why it did not start. Agents reach them through one `exec`
  tool: a script that calls many tools at once and returns only what it
  prints, drawn as `Ran a script: 12 MCP calls (github 10, linear 2)`
  ([docs/SONNY.md](../docs/SONNY.md#mcp-servers)).

  ![An exec call drawn as one line with what its script printed](../docs/sonny/exec-script.png)

  ![/mcp in the pane](../docs/sonny/pane-mcp.png)
- Every task works in a git worktree of its own, at the last commit on
  the branch `agent/NAME`, so only the lead works in the folder; outside
  git, tasks work in the lead's folder. The lead starts each task with its
  `start_task` tool (`name`, `brief`, `setup`, `profile`, `model`,
  `effort`), which makes its worktree, runs `.agents/setup` there with
  `setup`, starts the task with `agent run`, and leaves no folder behind
  when that fails. Every agent Sonny starts, leads and tasks
  alike, is told that the person it works for may be under pressure, and
  to care whether they succeed: take work off their plate, do or offer
  what they will need next, and say once when a better way of working
  would help them (in `lead.md`, and in `task.md` for the rest); so
  `start_task` gives what `--agents` or `--profile` would compose as the
  task's instructions, with `task.md` after it. Paths in the lead's folder
  that a brief names (through a link too) are turned into the same paths
  in the task's worktree, so the task never writes the lead's folder by
  its full path. The `remove_task` tool (`task`, `discard`) removes a task
  that is done and its worktree, refusing while the worktree has
  uncommitted changes. Its branch is kept unless it is merged.
- **Decisions:** when the lead reaches a fork in the road, an
  architectural choice or a step that cannot be undone, it calls its `ask`
  tool with the question, why it asks, two to four options each with what
  picking it means, and the one it recommends and why. Sonny draws the
  decision in the conversation where it was asked, its words read as
  Markdown with the same renderer a reply uses, and the composer's hint
  names it: what you type next answers it (an option's number picks it),
  and goes to the lead as `You asked: QUESTION` and your answer. Esc sets
  it aside for a message of your own, and Esc again goes back to it. The
  lead does not wait: it goes on with the option it recommends when that
  can be undone, or ends its turn. Open questions are read from the
  history Sonny replays (its `ask` calls not yet answered), so one asked
  while Sonny was closed is answered the same way when it opens.
- `/copy` puts the last reply on the clipboard; `/open` opens the last
  HTML page in a browser; `/sound` turns the chime off or on; `/verbose`
  switches between the clean view and every tool call; `/quit`
  leaves the agents working.

Sonny declares its tools on the agents it opens and the leads it makes,
which agents they make inherit: `react`, `ask`, `plan` (the plan the
person sees, as a checklist), `margin` (notes beside the conversation),
`resolve_task`, `show_agent` and
`run_in_pane`; a lead's own, which its tasks don't take, are `start_task`,
`remove_task`, `new_project` and `subscriptions`. The daemon shows them to
the model and records each call: `react`, `ask`, `plan`, `margin` and
`resolve_task` run nothing, and the rest wait for Sonny's answer. Sonny's lookout answers
those, running each call in the calling agent's folder, so they work with
every Sonny window closed; a call nobody answers in time ends as
`tool_unserved`, and the agent goes on. All but `show_agent` wait for a
verdict wherever the agent's other calls do (`/approvals`): a start runs
the folder's setup script, a new project makes a lead, a removal can drop
work, a subscription sends other bots' words to the subscriber's judge,
a pane runs a command outside the agent's sandbox, and a trigger is a
job of the system's scheduler (a LaunchAgent, or a systemd user service)
that runs as you. An agent whose
gate Sonny did not set, as another client's, goes without these until the
person picks a mode in `/approvals`. An agent of Sonny's reaches Sonny only through these tools: none
of them has a command form. Sonny draws the calls, live and from history,
and a call answered with an error says why under it. A task's own
`resolve_task` is read from its history: its last turn resolved it. The
lead's Sonny keeps each task the lead resolved beside the store
(`state.sqlite.resolved/`), with the task's newest turn finished by then: a
turn of the task numbered after that one reopens it.

![A lead starting a task, subscribing, a show_agent call refused with its reason, and a discard the person allowed](../docs/sonny/served-tools.png)

![A decision the lead asked with the ask tool, drawn where it asked it, its code, bold, math and links read as Markdown](../docs/sonny/ask-markdown.png)

![/status with the lead first, then its tasks under Needs you, Working, Ready for review, Idle and the resolved count](../docs/sonny/status-groups.png)

Settings Sonny keeps for every folder are in `~/.agent/config.toml`'s
`[sonny]`, which `/config` shows: the chime (`chime`, on by default) and
the view (`verbose`, off by default), besides the efforts and the side chat
above.
At each start Sonny writes what every agent leans on: the
`memory`, `memory-cleanup`, `plan` and `automation` skills, as normal skills
in `~/.agents/skills` (replacing links the Agent app left there), and
`~/.agent/memory` and `~/.agent/trigger`, scripts that run this Sonny as
`sonny memory ...` (facts saved for later agents, in `~/.agents/memory`)
and `sonny trigger ...` (jobs that wake an agent, kept by launchd on a Mac
and systemd's user manager on Linux), which other harnesses, scripts and
those schedulers run too; Sonny's own agents use the
`memory` and `triggers` tools instead, which the lookout answers outside
their sandbox. A skill Sonny wrote is rewritten when Sonny's
text changes, until you edit it: from then on it is yours, and Sonny leaves
it alone, offering its new text through the first bot you open after an
upgrade, as for its roles. Sonny notes what it last wrote of each in `~/.agent/sonny/skills`;
a skill of the same name it has no note of writing (yours, or one an
earlier Sonny wrote) is left alone too. Remove its folder to get Sonny's.
[docs/SONNY.md](../docs/SONNY.md) has the details of triggers, memory,
skills and MCP.
Sonny starts its lookout, `sonny lookout`, when it opens a project: one
for the daemon, watching for every bot that subscribed, and Sonny
subscribes each lead to its tasks. As soon as the lead rests, it tells it
every message someone else wrote in its tasks and how each turn ended, in
one "Task updates" note, by the handles its wait tool reads them with; the
lead decides what matters. While the lead is mid-turn, a judge asks of each
event only whether it cannot wait, and steers the lead at once if so. A
turn a note started that ends with nothing to say stays folded under the
note, with no sound. It judges with the config's `[judge] model`, the
approver's setting, else the lead's own model, at `[judge] effort` when
set, and with the judge `off` sends the task updates alone. Any agent can
subscribe to more with its `subscriptions` tool (other agents' tasks, bots
by name, sources outside such as a chat you poll, questions of its own),
and `/background` lists what it watches;
see [docs/LOOKOUT.md](../docs/LOOKOUT.md). The lookout also answers
Sonny's tools. A Sonny that finds another build's lookout running, as one
an upgrade left, stops it and starts its own, so its agents' new tools are
answered.

The look follows Codex's TUI (openai/codex, `codex-rs/tui`): replies stream a line at a time, a tool call is one `• Ran …` line with a
short `└` look at its result, edits show as a diff, and a status row says
`Working (12s • esc to interrupt)`. The steps on the way, calls and what
the bot said before each, are dim; the reply stays bright, under the
turn's `Worked for …` line.

The view is clean by default: the bot's words, and only the calls that
move the work on, said as what they did (a task started, messaged or
waited for, a question to you, the plan, memory, a trigger, a window
opened). Commands, reads and edits are left out. `/verbose` draws every
call, the conversation so far included, and is kept with the other
settings; `/verbose` again goes back to clean.

Some rows are buttons. Calls of one kind in a row, such as three tasks
messaged one after another, fold into one row (`• Messaged calc.add,
calc.neg and calc.div ▸`) that a click opens to each call and folds
again. A question the agent asks you lists its options, and a click on one
answers with it. What agents tell each other stays quiet: a message from
another agent, or the lead's task updates, is one dim row with who sent it
and its first words, and a click opens the whole message. A turn that ends
with the model saying nothing (`empty_completion`) ends quietly, not as a
failure.

![Memory saves folded into one row, and a question whose options are buttons](../docs/sonny/fold-and-ask.png)

![A message from the task calc.neg opened under its row, and the lead's task updates as one row](../docs/sonny/agents-message-opened.png)

The tasks the lead starts show where it started them, as one roster for
the tasks started together, each task with what it is doing now (starting,
working, needs you, done, said nothing, failed), read again every few
seconds. A task whose model ended its turn with nothing to say did not
fail, but did not reply either, so it reads `said nothing`, not `done`, in
the roster and in `/status`. A click
on a task opens it in the pane: its own Sonny, live, to watch and to talk
to, as a side chat is. Ctrl+\ takes the keys back to the conversation and
gives them to the pane again; from the conversation Ctrl+[ hides the task
and Ctrl+] shows it again.

![Three tasks started together, each with what it does now](../docs/sonny/tasks-roster.png)

Sonny keeps to the terminal's own 16 colors, so a theme sets their
shades, and each means one thing: magenta is you (your `›`), blue the
agents (task names, who sent a message, the lead in the footer, the
headings of a reply), green what worked (a call's dot, the git branch),
yellow what needs you, red what failed, and cyan code and work under
way. The context in the footer turns from green to yellow at 100k
tokens and to red at 200k.

![The task calc.neg opened in the pane beside the lead's conversation](../docs/sonny/task-in-pane.png)

Sonny owns the whole screen, as Codex does; there is no inline mode.
The conversation and the composer keep to a column 100 wide, however wide the window, so a reply
reads as a column rather than a band across the screen. The conversation is Sonny's
to draw, so it rewraps when the window changes size and the wheel scrolls
it, older messages loading as you go past the first; rows that arrive
while you have scrolled back leave the view still, and PgDn or Enter
takes you back to the latest. Copying needs no key held:
a drag selects and copies when you let go, a double click takes a word and
a triple click its line, Ctrl+C copies a selection instead of
interrupting, and Esc clears it. The copy is the text as written: a line
wrapped over rows comes back as one line, without the bullets and indents
Sonny drew around it. A click on a web link opens it in the browser, and
a click on a file opens it in a pane beside the conversation: code in
syntax colors with its line numbers, at the line the link names
(`src/main.rs:120`), Markdown as replies read, with its pictures and
the HTML a README is written with and its math (`$$ … $$`, `\[ … \]`,
`$…$` and `\(…\)`) drawn as in a reply, and a picture, SVG, Mermaid
file or HTML page drawn as it looks. Markdown and plain text read at the 100
columns the conversation is written at; code, pictures, diagrams and
pages fill the pane. A folder shows as a tree, folders first: a click
on a folder opens it in place, a chain of folders that each hold only
the next shows on one row, and what git says of a file shows at its
row's end (M changed, A added, D deleted, R renamed, U new, a dot on a
folder that holds a change, ignored ones dim). A click on a picture,
diagram or page in the conversation opens it in the pane, bigger. A
link in what the pane shows opens there too, and the pane goes back and
forward as a browser does: ‹ and › in its title, Ctrl+[ and Ctrl+]
wherever the keys are, or ← and → (Backspace back) while keys go to it,
each page as it was left. Back from the first page closes the pane, and
Ctrl+] opens again the pane closed last, Esc's included. Without the
kitty keyboard protocol Ctrl+[ is Esc itself, and a program in the pane
keeps both brackets. The pane scrolls,
selects and copies as the conversation does; a click on its name opens
the file in the app the system opens it with, and Esc closes it. Links
are the terminal's own links too, so its link click (Cmd+click, or
Shift+Cmd+click in Ghostty) opens a file outside Sonny from the
conversation. Keys go
to the pane after a click in it; Ctrl+\ (or Tab, in a pane that is not a
chat) takes them back to the composer, and Ctrl+\, or Tab with nothing
typed and no guess showing, puts them in the pane again; the pane's title
names the key that does. Sonny binds no Shift+Tab: multiplexers
switch tabs with it. A window too narrow for
both shows the one the keys are in. With nothing typed, Home goes up to
the conversation's first turn, drawing the turns over it on the way, and
End comes back to the newest. Ctrl+F finds text in
the conversation, from the newest place back: ↑ or Enter steps older, ↓
newer, and Esc stops where you are.

The pane's place, when nothing else is open there, is the margin. Its
first row is a glance at what runs for the agent: `↻` for its triggers
and subscriptions (a click opens `/background`), and its context, each
left out when there is nothing to say; the footer stops saying them while
it shows. Under the composer, the footer counts the other agents at work
(cyan `●`) and what needs you (yellow `●`), and a click on the count
opens `/status`. Under the glance are the notes the agent keeps there with its `margin`
tool: a diagram, a table of findings, a file it is discussing (kept fresh
as the file changes), or a live view it updates while it works. Each
note is pinned where the agent first put it: a quiet `✎ title · in the
margin` row there scrolls the margin to it when clicked, and the note's
`↑` scrolls the conversation back there, drawing older messages again
when it is further back. Its `×` takes the note away and tells the agent
with its next call. Posting a note's id again changes it in place, so a
live note stays one note. A lead can show a task's note as
`from: "task/id"`, which lookout names when the task puts one up: it
shows as the task keeps it, follows the task's title until the lead gives
its own, and stays once the task is gone. A file, `/status`, `/background` or a side
chat opens over the margin, its title's `←`, Esc and Ctrl+[ going back to it;
Esc in the margin closes it, and `/margin` brings it back. The margin
opens by itself with its first note, once the window leaves the
conversation its 100 columns and the margin 40 more, and goes once it
holds nothing; closed with Esc, it stays closed as notes come; in a narrower one the footer counts the notes
(`✎ 3`), and Tab with nothing typed shows the margin. Where the margin
cannot show, because the window is narrower or it is closed, each note is
drawn in the conversation where it was put instead of its marker, up to
40 rows with the rest a click away, and it goes back beside the
conversation once there is room again. The agent's plan sits over the
notes, one plan however often it changes; each `plan` call in the
conversation is a row of how far it had got, and where the margin cannot
show, the newest is the whole checklist. The notes are kept by agent in
`~/.agent/sonny/margin/`, and the calls stay in its history.
A note is drawn the way the conversation and the pane draw things:
Markdown with its tables and math, Mermaid diagrams, HTML pages,
pictures, and code in syntax colors.

![The agent puts a diagram and a live table in the margin while it works; × takes the table away, /status opens over the margin and Esc goes back, Esc closes the margin and /margin brings it back](../docs/sonny/margin.gif)

![Margin notes in Markdown with a table and math, a Mermaid diagram and a picture, each pinned to a row in the conversation](../docs/sonny/margin-rich.png)

![Further down the same margin: the picture, an HTML dashboard page, and a file note in syntax colors](../docs/sonny/margin-rich-more.png)

![The plan over the margin's notes, and each plan call a row of how far it had got](../docs/sonny/margin-plan-wide.png)

![The same conversation in a narrower window: the notes drawn where they were put, and the newest plan as the whole checklist](../docs/sonny/margin-inline-narrow.png)

![/status laid over the margin: its title's ← goes back, and the footer counts the note the margin holds](../docs/sonny/margin-status.png)

![The margin opened by itself with the lead's first notes, two tasks' tables shown as the tasks keep them](../docs/sonny/margin-opens-with-a-note.png)

![Both notes taken away with ×: the margin goes and the conversation has the window back](../docs/sonny/margin-gone-once-empty.png)

![A drag selects two lines and copies them, a double click takes len(values), a triple click a whole line, and the wheel scrolls back](../docs/sonny/fullscreen-copy.gif)

![A click on calc.py:2 opens calc.py beside the chat at line 2, in syntax colors](../docs/sonny/pane-code.png)

![README.md beside the chat, read as a reply: headings, a list, a table and a Python block in color](../docs/sonny/pane-markdown.png)

![A README beside the chat with its logo, a screenshot and a badge's words, from Markdown and HTML pictures](../docs/sonny/pane-readme-pictures.png)

![The same README further down: its Mermaid diagram drawn, and a details block with its summary, code and keys](../docs/sonny/pane-readme-html.png)

![A Mermaid diagram clicked in the conversation, drawn across the pane](../docs/sonny/pane-mermaid.png)

![An HTML page clicked in the conversation, drawn in the pane](../docs/sonny/pane-html.png)

![A folder beside the chat as a tree: docs opened in place, what git says of each file at its row's end, ignored build dim](../docs/sonny/pane-tree.png)

![The tree, then a guide opened from it, then the math notes it links to; ‹ goes back to the guide and to the tree as it was left, › forward again](../docs/sonny/pane-tree-back.gif)

![A Markdown file's math beside the chat, drawn as the reply draws it, in dollars and in brackets](../docs/sonny/pane-math.png)

![A screenshot the agent named in its reply, drawn in the conversation](../docs/sonny/chat-picture.png)

![/status beside the conversation, its times still counting](../docs/sonny/pane-status.png)

![A side chat beside the conversation, answering a question about it](../docs/sonny/pane-side.png)

`demo/` has the scripted model and folder the recordings use.
