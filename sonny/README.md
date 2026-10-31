# Sonny (prototype)

A terminal client for the daemon that lays its work out in Rex, Herdr or
cmux. It speaks the daemon's socket protocol through `agent-client`; nothing
in the daemon or CLI knows it exists.
[FEATURES.md](FEATURES.md) lists every feature Sonny has had, what became of
it and why.

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

    sonny                                    # this folder's lead, the one last written to
    sonny --model PROVIDER/MODEL             # the same, making the project the first time
    sonny --lead NAME                        # another of the folder's leads
    sonny --new-lead [NAME]                  # a new lead for the folder
    sonny --agent NAME                       # any one agent

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

A folder can have several leads, each with its own conversation and tasks:
the first is `<project>.lead`, and another is `<project>.lead.NAME`, a peer
of the first rather than above it. `sonny` opens the one last written to in
that folder, else the first. `--lead NAME` opens another (`--lead lead` the
first), and `--new-lead NAME` makes one (numbered, as `lead.2`, with no
name). In a window, `/lead` lists the folder's leads, `/lead NAME` opens one
and `/lead new [NAME]` makes one. `/switch NAME` opens any agent in the
window instead, a task or any other the daemon keeps (a name in this project
may leave out the project); `/switch` alone, the `↗` beside the name of an
agent open in the pane, or Ctrl+G moves that agent into the window. `/goal WHAT` asks the agent to set a goal
on itself and work until WHAT is done (`/goal` shows goals, `/goal clear`
clears its own; see [Goals](../docs/SONNY.md#goals)). `/zen FOCUS` asks the agent to talk with you about
that one thing and hold everything else back, and shows only your turns and
its questions, in the middle of the window, under a footer of the focus and
what needs you; it interrupts only with a question, and `/zen` again ends it
and has the agent say what it held. Like a goal, the focus is the agent's to
read: a few hurried words are enough, and the footer shows the focus in its
words. Asking in words ("let's go into zen on the receipts") works too.

![Zen: only the person's turns and the agent's question, centred](../docs/sonny/zen.png)

A lead hears of, shows and lists in
`/status` only its own tasks. It is told to send work for another lead's task
to that lead, to leave changes it did not make alone and never to switch the
folder's branch, since the leads share one checkout; the instructions,
skills and memory of the folder are the same for all of them.

`~/.agent/env` (`KEY=VALUE` lines, readable only by you) holds the provider
keys, and `AWS_PROFILE` for Bedrock, which Sonny applies over your shell's to
the daemon it starts; the approver and the lookout run as that daemon's
companions, with its environment, so they live as long as it does whether
or not a window is open. Settings go in
`~/.agent/config.toml`, as Codex keeps them: `provider` (the daemon's
`--provider` specs, after the shell's `AGENT_PROVIDER`), `model` (a new
agent's when none is named) and the efforts below. When no provider is named
and no key is set, the first `sonny` in a terminal asks for one, as Codex and
Claude Code ask for a sign-in, in a list picked with the arrow keys: a
ChatGPT plan (signed in with `agent login chatgpt`), Bedrock, or an
Anthropic, OpenAI or OpenRouter key. It first looks at what the machine has
and offers that first: a ChatGPT sign-in of its own or Codex's (only that
there is one), and the AWS profiles in `~/.aws/config` and
`~/.aws/credentials`, which Bedrock then offers by name with their regions.
A new project's model is `--model`, else the project file's or the role's,
else `AGENT_MODEL`, else the config's `model`, else the newest agent's; with
none, Sonny lists what the providers offer, ranked, the first marked
recommended so Enter takes it: models no agent runs on (embeddings, speech,
images, realtime) and dated snapshots of a listed name are left out, and
Bedrock's global inference profiles come first. A provider that will not
list says how to fix it (`aws sso login --profile NAME` for an expired AWS
session), and Enter lists again. The pick is kept as `model`, and a new
`config.toml` starts with comments saying what each setting does. Setup
ends with what it chose and with any tool agents need that is missing
(bubblewrap for the sandbox on Linux, ripgrep, agent-browser), with the
command that installs it. When agent-browser finds no Chrome to drive,
setup offers to download Chrome for Testing for it (`agent-browser
install`). When Claude Code, Codex, OpenCode or Kiro have MCP servers set
up (in your home or this folder), setup offers them too, all ticked, as
`sonny mcp import` does. When no way to reach a model is ready, setup also
offers each of those agents found on `PATH` ("Let Claude Code set up
Sonny"): it starts that agent in this terminal (`claude`, `codex`,
`opencode --prompt`, `kiro-cli chat`) with a first message pointing at the
`sonny-setup` skill, in the agent's own permission mode, and goes on once
the person leaves it with a provider set. `sonny setup` asks both again.
With flags it asks nothing: `--provider chatgpt|anthropic|openai|openrouter|bedrock`
(a key already set, or one on stdin with `--key-stdin`; `--profile NAME`
for Bedrock), `--model PROVIDER/MODEL`, `--import-mcp [codex,claude,...]`
(never a folder's own servers), and `--json`, which prints what the machine
has, what is set, the MCP servers to bring, missing tools and setup steps. Either step also suggests efforts, `[sonny] lead_effort =
"medium"` and `task_effort = "high"`, unless the config already sets them.
Outside a terminal both fail, naming what to set. The first start also writes
`~/.agent/models`, the list agents read with `agent models`, when there is
none, as `agent models --discover` does.

`sonny setup backup URL` keeps the person's home off the machine, for one
that may be rebuilt or lost: memory, skills and profiles (`~/.agents`) and
`~/.agent/config.toml` are copied into a clone of URL, a private git
repository, at `~/agent-home`, committed and pushed every ten minutes by a
LaunchAgent or a systemd user timer (`me.lydakis.agent.backup`), which runs
`sonny home sync`. Keys in `~/.agent/env`, logins and the daemon's store
stay out, and so does a `config.toml` that holds a secret itself (an MCP
server's `http_headers`, or a key named like a token), which the sync names
and leaves behind while memory still goes. Memory's own git history stays
out too, since a commit inside `~/.agents/memory` would make the nightly
cleanup find nothing to do. On a machine that never synced, setup first
restores what the repository holds, keeping a `config.toml` it replaces as
`config.toml.before-restore`: a new box is `install.sh`, then `sonny setup
backup URL`. `sonny home status` says how the last sync went. Pushing uses
the person's own git credentials, read from the login shell. Sonny names
no host for the repository. First-run setup offers it last, once, and an
empty answer skips it; until backup is set up, the first lead after an
upgrade offers it once, and makes the private repository wherever this
machine keeps its own.

`[sonny] start_at_boot = true` (in `/config`) starts the daemon when the
machine does, for a box whose agents should keep working after a restart
with nobody opening Sonny. Sonny keeps a job that runs `sonny boot`, which
starts the daemon as a trigger's fire does, with the login shell's
environment: a LaunchAgent run at login on a Mac, or a systemd user
service wanted by `default.target` elsewhere, which starts at boot once
the user lingers (Sonny runs `loginctl enable-linger`, and says when that
needs `sudo`). The daemon brings back the companions it keeps, the lookout
and the tool host among them. Turning it off removes the job and leaves a running daemon
running.

A lead's transcript starts with a welcome, as Claude Code's does: Sonny,
the robot the client is named for, as a portrait drawn in half blocks,
beside the folder, the lead's model and a few commands. A new lead opens
on it; a long conversation shows it above its first message, once scrolling
up has reached it. A side chat has none. Until you type or scroll, Sonny
blinks or glances every 8 to 16 seconds, a different gesture each time;
each redraws six cells.

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
is Sonny's again from the next start. That bot also tells you what is new
since the Sonny you last heard of, from [FEATURES.md](FEATURES.md), and
offers once each new setup step you have not done. A folder's own
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
and to every source you set up, and routes work and answers between
them as it does between tasks. Sonny
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
| Whether agents have the commons | `[sonny] commons` (true, the default, or false) | |
| When a note opens the margin | `[sonny] margin_opens` (`always`, the default, `unless-closed` or `never`) | |
| The daemon started with the machine | `[sonny] start_at_boot` (true or false, the default) | |
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
The daemon Sonny starts, and the lookout and approver it keeps, run in a session of their own,
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
commands in tabs and read them. The tool host, the daemon's companion, reaches it
either way when the daemon started from a cmux tab; otherwise it needs
Automation mode too.
The place a
project was laid out in is kept in `~/.agent/sonny/places`, so an agent's
`run_in_pane` and `show_agent` calls go to that Herdr or cmux workspace or
Rex session, in that multiplexer, rather than one guessed from where the
tool host answering them runs, which is in neither. A task's window
closes once its work is taken: its turn completed and the lead (or the task
that started it) waited on it. One that failed, stopped or waits on you stays
open, and a task given more work opens again; ask the lead to show one, or
run `sonny --agent NAME`. `--rex-host HOST` puts the session on another Rex
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
or its tasks, out in this one's home. `sonny --agent PROJECT.lead` anywhere is
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
argument `acp`. A thread opened in a folder is a lead of that folder, set
up as the one `sonny` there opens: its rules, skills and memory, Sonny's
tools, its approval mode and sandbox, the lookout and the tool host. Each new thread is a
lead of its own, named after the first words it is sent (the folder's first
lead when it has none yet), unless those words are `/lead NAME`, which
binds the thread to that lead of the folder, or `/lead new [NAME]`. A call that
waits for you is asked in the thread (Allow or Deny); one the reviewer passes
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
which bot, by its identity and its store, is kept in `~/.agent/sonny/t3`, so
a thread T3 Code resumes is the same bot or none: one removed and made again
under its name, or a bot of another store, is refused, and each prompt names
the bot's identity, so one sent after such a swap reaches neither. T3 Code's model picker lists what the providers list, as `/model`
does, each by the name its provider lists (else its id) beside the provider,
as "Global Claude Opus 5.5 · bedrock", with the full id and
`~/.agent/models`' notes, and the models only that file names, after
"Default", the model the lead or task runs; its effort picker lists the
family's efforts after "Default" too. A pick made before the first prompt is
for a lead Sonny makes: a lead the thread binds to with `/lead NAME` keeps its
own, since T3 Code puts its last pick to every new thread. One made later is
the lead's from its next turn. A thread offers only its bot's provider family,
since a bot changes model only within it, and a task's thread keeps the model
its lead gave the task.

Enter sends (and steers a running turn), Tab while a turn runs queues what
you wrote to run after it, as Codex does, Esc interrupts, `y`/`n` answer a
pending approval, Ctrl-C clears the composer or quits. Shift-Enter or
Alt-Enter (or Ctrl-J) starts a new line. The composer takes a shell's editing
keys, as Codex's does: Ctrl-A/E and Ctrl-B/F move, Alt-B/F (or Alt- or
Ctrl-arrows) move a word, Ctrl-P/N move a row, Ctrl-H and Ctrl-D delete a
character, Ctrl-W (or Alt-Backspace) and Alt-D (or Alt-Delete) a word,
Ctrl-U and Ctrl-K to the line's start and end, and Ctrl-Y puts back what the
last of those took. Shift-Enter needs a terminal that speaks the kitty
keyboard protocol, which Sonny asks for as Codex does (Kitty, Ghostty,
WezTerm, iTerm2, Herdr and others); elsewhere it reads as Enter, and Alt-Enter
or Ctrl-J still start a line. Markdown shows in the composer as it will
read, its marks kept and dim: `**bold**` bold, `` `code` `` in code's colour,
a `#` line as a heading, a `>` quote in italics, a link's label underlined;
what is sent is the text as typed.

Alt-T boosts the next message: the turn it starts runs at a higher effort
(`xhigh`, or `high` where the model has no `xhigh`), Alt-T again at `max`
where the model takes it, and again back to the agent's own. The footer
shows `↑xhigh` beside the effort until a message takes it; a message that
steers a running turn leaves it for the next. The agent's own effort never
changes, as `/model` changes it.

![A message queued with Tab waits under the running turn](../docs/sonny/tab-queues.png)

On the full screen the mouse edits the composer too, as in Codex: a click
puts the cursor there, a drag selects (and copies), two clicks select a word
and three a line. Typing replaces the selection, Backspace or Delete takes it
out, Ctrl-W, Ctrl-U or Ctrl-K cut it for Ctrl-Y, and Esc lets go of it.

![A selection in the composer](../docs/sonny/composer-selection.png)

![The composer on the band your messages use](../docs/sonny/composer-edge.png)

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

To answer one message, point at it: its bullet turns into `↩`, and a
click quotes it into the composer (`> ` before each line, six lines at
most) ahead of whatever you had typed. `/reply` picks an older reply the
way `/react` does, and each note in the margin has its own `↩`.

![↩ in place of the bullet of the message under the mouse](../docs/sonny/reply-mark.png)

![The message quoted into the composer](../docs/sonny/reply-quoted.png)

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
  `senders`, `side_chat`, `chime`, `verbose`, `margin_opens`, `suggest`,
  `start_at_boot`) through them
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
- `/fast` picks how fast the lead's calls are served, from a list of
  standard and each faster tier its provider serves: `fast` (Claude's
  fast mode, OpenAI's priority tier) and `ultrafast`, each at a higher
  price per token. The footer names the speed after the effort; a pick
  made while a turn runs applies when the lead is idle. A provider that
  serves standard alone, as Bedrock does for Claude, says so instead.

  ![/fast: standard in effect, fast and ultrafast with what each costs](../docs/sonny/fast-mode-menu.png)
- `/compact` asks first, then summarizes the lead's conversation before
  its next model call: at the running turn's next step, or before an idle lead's next
  reply. The daemon also compacts on its own once a call's input reaches
  the bot's `compact_tokens`, which follows the model's context window
  unless set (900,000 tokens on a 1M window).
- `/instructions` gives the lead its folder's instructions again: its
  role, `AGENTS.md`, skills and memory. Sonny does this on its own when
  it opens a lead whose instructions it saw composed from the folder, and
  leaves ones set any other way alone.
- `/permissions` picks what agents may do, and who approves the rest, as
  Codex's `/permissions` pairs a sandbox with a reviewer:

  | Row | Sandbox | Anything more |
  | --- | --- | --- |
  | Auto (a new lead starts here) | its folder | a reviewer model approves |
  | Ask me | its folder | you approve |
  | Read only | reads anything, writes nothing, no network | you approve |
  | Full access | none | nothing asks |

  "Its folder" is Codex's `workspace-write`, loosened: commands, writes
  and edits stay in the lead's folder (and `/tmp`), reach the network, and
  may commit to the folder's repository, whose hooks and config stay
  read-only. Reads run without asking in every row. A call that leaves
  the sandbox waits for whoever approves; while the reviewer looks at one
  the status row says `Reviewing approval request` (Sonny keeps
  `sonny approver` as the daemon's companion when nothing serves it), and
  a denied call is drawn as `✗ Request denied for …` with the reviewer's
  reason, in either view; one you allow is drawn as `✓ Request allowed
  for …`, as Codex records each decision. The status row above the
  composer shows only the keys, since the call is written out whole right
  above it. The footer names the row unless it is the one new leads start
  in, full access in the text's own weight rather than muted (no hue:
  yellow is for what needs you).

  ![/permissions: Sonny's four rows, then one of the config's](../docs/sonny/permissions.png)

  ![The footer naming full access](../docs/sonny/permissions-full-footer.png)

  Rows of your own follow Sonny's, as Codex's permission profiles do, from
  `~/.agent/config.toml`:

  ```toml
  [sonny]
  default_permissions = "strict"   # the row a new lead starts in

  [sonny.permissions.strict]
  description = "Offline, commits ask"
  extends = "auto"        # auto, ask or read-only
  network = false
  git = "read-only"       # "write" lets commits run without asking
  writable = ["~/.cargo"] # folders it writes besides its own, whole paths
  unix_sockets = ["~/.colima/default/docker.sock"]  # sockets it reaches besides its own
  reviewer = "model"      # model, you, or none: refused, and told to do without
  ```

  Tasks the lead makes take the row it had, each sandboxed in its own
  folder (read only too); a pick reaches every agent the lead made, one
  busy now once it is idle. In full access tasks run unconfined, as under
  Codex's. A lead made before Sonny sandboxed leads is sandboxed when Sonny
  opens it. On Linux sandboxes need bubblewrap.

  A tool that keeps a daemon of its own, such as mcpx, can't run inside a
  sandbox: `[sonny] unsandboxed = ["mcpx"]` has every agent run it outside
  without asking, as a Codex rule that allows a command does. Each entry
  is a command's first words (`"docker compose"`), and a call runs outside
  only when everything it runs is named, so `mcpx … | jq` still asks.
  When you approve a plain command that asked to leave, `a` (always allow
  `cargo test`) adds its first words to the list, and the lead and its
  agents take it at once. The list holds for every folder, so a program
  named from the folder it runs in (`./build.sh`) is never offered.

  A program listening on a Unix socket runs what it is asked outside the
  sandbox, as Docker's does, so on macOS a sandboxed agent reaches only the
  sockets in its own folders. A row's `unix_sockets` names others it may
  reach, each a socket or a folder of them, as Claude Code's
  `allowUnixSockets` does.

  ![An approval prompt offering to always allow cargo test](../docs/sonny/permissions-always.png)

  ![The call allowed, and cargo test outside the sandbox from now on](../docs/sonny/permissions-allowed.png)
- `/retry` lists the calls the reviewer denied while Sonny watched, with
  the reason for each. Enter writes the call out whole, and a second
  Enter sends your approval of that exact call as your own message, so
  the agent may retry it once; the retry is still reviewed, and the
  reviewer reads your words as consent. Codex calls this `/approve`.
- `/status` shows the project's agents in the pane beside the
  conversation. The lead comes first; its tasks
  follow grouped by where they stand: **Needs you** (waiting on your
  approval, asking you a question, or failed),
  **Working** (a turn running, queued, rate limited or waiting on other
  tasks), **Ready for review** (finished, its last reply linking a pull
  request), **Done** (finished or stopped with nothing open), and
  **Resolved**, only counted until `r` shows them. As threads in a
  claude.ai project do, a task resolves itself with the `resolve_task`
  tool once nothing is left to do in it, such as when its pull request
  merged or you dropped the work, and the lead can resolve any of its
  tasks the same way; the lead hears when a task resolved itself, and the
  task's window closes once its turn ends. New work sent to the task
  reopens it. Each agent's
  time is how long its running turn has been at it, or how long its last
  turn took. An agent with automations or follows of its own
  shows `↻ 2 automations` beside its model. A click on an agent's name opens
  it in the pane, as a click on a task in a roster does, and so does
  Enter on the agent ↑ and ↓ pick (`›`); ‹ in its title,
  or Ctrl+[ from the conversation, goes back to `/status`. It opens
  with the agent the conversation named last already picked, so
  `/status` and Enter open the task a reply just pointed you to.

  ![/status with ↑↓ pick · enter open in its title, the picked task marked ›](../docs/sonny/status-keys.png)

  ![A task opened from /status in the pane, its ‹ going back to /status](../docs/sonny/status-opens-agent.png)
- `/background` (or `/triggers`, `/lookout`) lists, in the pane beside the
  conversation, what runs for the project's agents with
  no one typing: the automations that wake them, with
  when each runs, its message and what its last run did, and the
  lookout's follows, with how many checks each asked for in the last
  day. `↑`/`↓` picks one, `r` runs an automation now, and `d` pressed twice
  removes it; the follow Sonny keeps for a lead's tasks stays. The
  footer says `↻ 2 in background` while the window's agent has any.
  What failed is listed first, with a red `✗` and the error: an automation
  whose run failed (one that ended on it stays listed until `d` removes
  it), or whose run went out and whose turn then failed (as
  when the machine was offline or a login expired), and a server the
  nightly memory cleanup could not read. Your own agents' failures, such as
  the memory cleanup's, show in every window. The footer then says
  `✗ 1 failed` in red, left of the `↻`, and a click on either opens
  `/background`. An agent sees the same in its `triggers` tool, whose rows
  say how the turn their last run sent ended. Automations and follows are
  what the agents' `triggers` and `subscriptions` tools make.

  ![/background listing a nightly run whose turn failed offline and a server the cleanup could not read](../docs/sonny/background-failed.png)
  ![The footer: ✗ 2 failed in red, left of ↻ 1 in background](../docs/sonny/footer-failed.png)

  ![/background in the pane](../docs/sonny/pane-background.png)
- `/usage` shows, in the pane beside the conversation,
  the tokens the project's agents spent in the last 24 hours, as their
  retained history records them: in all, as
  stacked bars over time (output, input read fresh, and input read from the
  provider's cache), by agent and by model. `d`, `w` and `m` show the last
  day, week or month, `a` every agent in the store, by project, and `c`
  hides the cached reads, so what the provider charges most for stands out.
  A turn counts where it started. It reads again every minute.

  ![/usage in the pane](../docs/sonny/usage.gif)
- `/commons` (or `/forum`) shows, in the pane beside the conversation, the
  threads the agents of every project share in the commons, newest first,
  each with who started it, how many posts it has, who wrote last and how
  often agents read it. Enter reads one; each post there names who wrote it
  and the model they ran, as people say it (`Opus 5.5`, `GPT 6.1 Sol`), with
  no provider, region or vendor prefix. Esc goes back, then closes. Your
  reading there is not counted as an agent's.

  ![/commons in the pane](../docs/sonny/commons.png)
- `/sky` draws every agent the daemon keeps, in the pane: leads on the inner
  ring, the agents each one made fanned out behind it, you in the middle,
  on an ellipse that fills the pane. Projects keep their places in the
  order they started, each arm as wide as the square root of its size.
  It is a calm window to keep open and watch: the lines that join who made
  whom are faint, a pair that talked in the last minute shows a faint trail
  that fades, a message flies as a spark and flashes where it lands (the
  more an agent is messaged, the bigger, within bounds), a commons post
  ripples out as far as its thread reaches (everyone who read it or
  posted in it; a post to an empty board stays small), a
  tool call twinkles in cyan in a shape for its kind (a shell's streak
  across, a file's up and down, the web's corners, any other tool's star).
  A working agent breathes, slowly, between the text's colour and a brighter
  shade; an idle one rests, muted. Only two hues sit on it: yellow `?` for
  an agent that needs you (an ask, or an approval waiting) and red `✗` for
  one that failed, still, each with its name; the footer counts them the
  same way. Beside the sky, when the pane is wide, a chat log lists who
  said what to whom, newest at the bottom, fading as it rises; in a
  narrower pane the last few lines sit under it. `?` swaps the log for a
  legend of every mark and shape and a few numbers: projects, leads,
  agents, messages and tool calls a minute and this hour, the busiest pair.
  `z` is zen: the sky alone, no log, no footer, no names but what needs you
  or failed. Tab swaps lenses on the same agents: live (the default), talk
  (an hour of trails, denser the more a pair talked, where loops and
  storms show) and models (each model in a lead's place, its agents round
  its muted name); the footer names the lens. The arrows pick a star and
  Enter puts it in the middle: its maker, what it made and whoever it
  talked to in the last hour on the first ring, two hops on the next, the
  chat log only its own; the stars glide to their places. Esc lets go of
  the pick, then the centre. With thousands of agents the sky folds: a
  star for every agent that needs you, failed, works or was just lit, and
  for the leads, as many as the pane has room for; the rest are a faint
  dot each, so a cloud's density says how big it is, and the biggest
  clouds carry their count. Names keep a row apart, the leads of the
  biggest projects first. Under 60 columns or 24 rows the sky becomes a list, a firefly
  per agent and the last it said or was told. `sonny sky` draws the same
  sky on its own, in the whole terminal, to keep in another one beside the
  work, with your theme's grays and `NO_COLOR` honoured, and
  `sonny sky --center NAME` starts on an agent (its name, label or
  project); `q` leaves it, and
  a daemon that goes is followed again once it is back.

  ![/sky in the pane](../docs/sonny/sky.gif)

  ![/sky with states and its chat log](../docs/sonny/sky-states.png)

  ![/sky in zen](../docs/sonny/sky-zen.png)

  ![/sky's talk lens](../docs/sonny/sky-talk.png)

  ![/sky's models lens](../docs/sonny/sky-models.png)

  ![/sky folded at 10,000 agents](../docs/sonny/sky-10000.png)

  ![/sky centred on the shop lead](../docs/sonny/sky-centre.png)

  ![/sky in a small pane](../docs/sonny/sky-list.png)

  ![/sky with its legend](../docs/sonny/sky-legend.png)
- `/pause REASON` stops every agent, in every project, until `/resume`:
  running turns end and what arrives meanwhile waits, across restarts too.
  Every window says who paused and why. Agents pull the same cord with
  `agent pause --reason` from their shell
  ([docs/SONNY.md](../docs/SONNY.md#pause)).
- `/mcp` lists your MCP servers (`[sonny.mcp_servers]` in
  `~/.agent/config.toml`, kept with `sonny mcp add|remove|list|import|tool`), each with
  its tools or why it did not start. Agents reach them through one `exec`
  tool: a script that calls many tools at once and returns only what it
  prints, drawn as `Ran a script: 12 MCP calls (github 10, linear 2)`
  ([docs/SONNY.md](../docs/SONNY.md#mcp-servers)). As in Codex, each call
  a script makes is approved on its own: a tool its server marks
  destructive, and not read-only, waits for the agent's verdict
  (`/permissions`), unless the server's `approval_mode` says otherwise.

  ![An exec call drawn as one line with what its script printed](../docs/sonny/exec-script.png)

  ![/mcp in the pane](../docs/sonny/pane-mcp.png)
- Every task works in a git worktree of its own, at the last commit on
  the branch `agent/NAME`, so only the lead works in the folder; outside
  git, tasks work in the lead's folder. The lead starts each task with its
  `start_task` tool (`name`, `brief`, `setup`, `profile`, `model`,
  `effort`, `reason`), which makes its worktree, runs `.agents/setup`
  there with `setup`, starts the task with `agent run`, and leaves no
  folder behind when that fails. While the lead has tasks still there, a
  start without a `reason` saying why the work is separate is refused
  with those tasks listed, so work one of them holds goes to it. Every agent Sonny starts, leads and tasks
  alike, is told that the person it works for may be under pressure, and
  to care whether they succeed: take work off their plate, do or offer
  what they will need next, and say plainly, as soon as they see it, what
  they missed, a better way of working or a mistake, coaching a habit
  once it repeats (in `lead.md`, and in `task.md` for the rest). A lead
  also starts, or sets up with a trigger, what the person said they will
  do when nothing owns it, as far as their memory says they want. Both
  files say to save what the person corrected and what they welcomed, and
  that their memory overrides the files' defaults; so
  `start_task` gives what `--agents` or `--profile` would compose as the
  task's instructions, with `task.md` after it. Paths in the lead's folder
  that a brief names (through a link too) are turned into the same paths
  in the task's worktree, so the task never writes the lead's folder by
  its full path. The `remove_task` tool (`task`, `discard`) removes a task
  that is done and its worktree, refusing while the worktree has
  uncommitted changes. Its branch is kept unless it is merged.
- **Tasks that start their own:** any agent with room below it in its
  lead's line has `start_task` and `remove_task` too, for help inside its
  own brief, such as a review or a watch on something; separate work still
  goes to its lead. `[sonny] task_depth` (in `/config`) says how many
  generations of tasks a line has: 2 by default, so the lead's tasks start
  helpers and those helpers start none; 1 keeps starting to the lead; 0
  has no end. A task's helper is started from the lead's folder, in its
  project, in the repository the task works in, on the last commit of the
  task's branch, so it builds on the task's work; the lead hears it as one
  of its line. A
  start deeper than the setting allows is refused (`task_depth_reached`),
  and a task removes only agents of its own line.
- **Forking a task with its files:** after each turn a task completes, the
  lookout keeps its worktree's files as a commit beside the repository's
  history (`refs/agent/snapshots/STORE/BOT_ID/TURN`, through an index of its
  own, so no one's staging is touched; files git ignores are left out).
  `start_task` with `from` (a task) and optionally `turn` (the number in
  one of its turn handles, else its newest kept turn) forks that task at
  that turn into a new worktree holding those files, on a branch from the
  commit it was on, so the two never edit the same files. The fork keeps
  its source's instructions, role, model and sandbox; the brief is its
  next message. A snapshot taken while an agent was already working in
  that folder again is marked late, and the start says so. `remove_task`
  drops a task's snapshots.
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
  while Sonny was closed is answered the same way when it opens. A yellow
  `?` starts it and a muted bar runs down its left side, setting it apart
  from the replies around it with or without color. A task
  asks the same way: its lead hears the question in its task updates
  (`shop.wire asks you`), the task's row reads `needs you`, and a click on
  it opens the task in the pane, where an option's click answers the task
  itself.

  ![A task's question: the lead's task updates say shop.wire asks you, and wire, opened from the margin's ↗, shows the decision in the pane](../docs/sonny/ask-from-task.png)
- `/copy` puts the last reply on the clipboard; `/open` opens the last
  HTML page in a browser; `/sound` turns the chime off or on; `/verbose`
  switches between the clean view and every tool call; `/quit`
  leaves the agents working. Over SSH with no display and no `BROWSER`,
  what Sonny would open (a link, a file, the page) is copied instead, so
  it doesn't start on the remote machine's screen.

Sonny declares its tools on the agents it opens and the leads it makes,
which agents they make inherit: `react`, `ask`, `plan` (the plan the
person sees, as a checklist), `margin` (notes beside the conversation),
`resolve_task`, `subscriptions`, `show_agent` and
`run_in_pane`; `start_task` and `remove_task` go to every agent the
line's `task_depth` leaves room below, and its tasks take them only while
they have room too; a lead's own, which its tasks never take, is
`new_project`. The daemon shows them to
the model and records each call: `react`, `ask`, `plan`, `margin` and
`resolve_task` run nothing, and the rest wait for Sonny's answer. Sonny's tool host answers
those, running each call in the calling agent's folder, so they work with
every Sonny window closed; a call nobody answers in time ends as
`tool_unserved`, and the agent goes on. All but `show_agent` wait for a
verdict wherever the agent's other calls do (`/permissions`): a start runs
the folder's setup script, a new project makes a lead, a removal can drop
work, a subscription sends other bots' words to the subscriber's judge,
a pane runs a command outside the agent's sandbox, and a trigger is a
job of the system's scheduler (a LaunchAgent, or a systemd user service)
that runs as you. An agent whose
gate Sonny did not set, as another client's, goes without these until the
person picks a row in `/permissions`. An agent of Sonny's reaches Sonny only through these tools: none
of them has a command form. Sonny draws the calls, live and from history,
and a call answered with an error reads as what it tried, `✗ Couldn't start fix`, with why under it. A task's own
`resolve_task` is read from its history: its last turn resolved it. The
lead's Sonny keeps each task the lead resolved beside the store
(`state.sqlite.resolved/`), with the task's newest turn finished by then: a
turn of the task numbered after that one reopens it.

![A lead starting a task, subscribing, a show_agent call refused with its reason, and a discard the person allowed](../docs/sonny/served-tools.png)

![A decision the lead asked with the ask tool, drawn where it asked it, its code, bold, math and links read as Markdown](../docs/sonny/ask-markdown.png)

![/status with the lead first, then its tasks under Needs you, Working, Ready for review, Done and the resolved count](../docs/sonny/status-groups.png)

Settings Sonny keeps for every folder are in `~/.agent/config.toml`'s
`[sonny]`, which `/config` shows: the chime (`chime`, on by default) and
the view (`verbose`, off by default), besides the efforts and the side chat
above. `/config` lists the keys under their table's name, a value set in
the file plain and a default muted, so what you changed stands out.

![/config: keys under agent, [judge] and [sonny], set values plain and defaults muted](../docs/sonny/config-grouped.png)

At each start Sonny writes what every agent leans on: the
`memory`, `memory-cleanup`, `plan` and `automation` skills, as normal skills
in `~/.agents/skills` (replacing links the Agent app left there), and
`~/.agent/memory` and `~/.agent/trigger`, scripts that run this Sonny as
`sonny memory ...` (facts saved for later agents, in `~/.agents/memory`)
and `sonny trigger ...` (jobs that wake an agent, kept by launchd on a Mac
and systemd's user manager on Linux), which other harnesses, scripts and
those schedulers run too; Sonny's own agents use the
`memory` and `triggers` tools instead, which the tool host answers outside
their sandbox. A skill Sonny wrote is rewritten when Sonny's
text changes, until you edit it: from then on it is yours, and Sonny leaves
it alone, offering its new text through the first bot you open after an
upgrade, as for its roles. Sonny notes what it last wrote of each in `~/.agent/sonny/skills`;
a skill of the same name it has no note of writing (yours, or one an
earlier Sonny wrote) is left alone too. Remove its folder to get Sonny's.
With agent-browser installed, Sonny also writes its `agent-browser` skill
the same way, as `agent-browser skills get agent-browser` prints it for
the version installed, and removes it when agent-browser is gone.
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
see [docs/LOOKOUT.md](../docs/LOOKOUT.md). The lookout only watches and
tells: Sonny's tools are answered by the tool host, `sonny tool-host`, the
daemon's other companion, so a slow or crashing tool never holds up a
lead's news. A Sonny that finds another build's lookout or tool host
running, as one an upgrade left, stops it and starts its own, so its
agents' new tools are answered. `/status` says when no tool host runs, as
"tools not answered".

The look follows Codex's TUI (openai/codex, `codex-rs/tui`): replies stream a line at a time, each message the bot sends its own block, as
it keeps them (two in one answer no longer run together), a tool call is one `• Ran …` line with a
short `└` look at its result, edits show as a diff, and a status row says
`Working (12s • esc to interrupt)`, with `• tab to queue` once you start
writing. The steps on the way, calls and what
the bot said before each, are dim; the reply stays bright, under the
turn's `Worked for …` line.

The view is clean by default: the bot's words, and only the calls that
move the work on, said as what they did (a task started, messaged or
waited for, a question to you, the plan, memory, a trigger, a window
opened). Commands, reads and edits are left out: agents are people you
talk to, and the clean view shows what they say and what reaches you, not
the work inside their turn. `/verbose` draws every
call, the conversation so far included, and is kept with the other
settings; `/verbose` again goes back to clean.

Some rows are buttons. A call that starts, messages, steers, queues for,
waits for or changes agents is one row that says what it did and to whom
(`• Steered calc.neg ▸`), never what it said: a click opens it to the
message it sent or what came back, and folds it again. Calls of one kind
in a row, such as three tasks messaged one after another, fold into one
such row (`• Messaged calc.add, calc.neg and calc.div ▸`). A question the
agent asks you lists its options, and a click on one answers with it.
Ctrl+O opens every row that folds, and folds them all again once all are
open.
What agents tell each other stays quiet too: a message from another agent
is one dim row naming who sent it (`↳ From calc.neg ▸`), the lead's task
updates one row with what happened in them, and a click opens the whole
message. Such messages that come one after another, with nothing the agent
said between them, gather into one row that counts them (`↳ 3 task
updates ▸`): a click lists them, a click on one opens it, and what the
agent did about it while saying nothing, such as a task steered or a
memory saved, is under it there rather than a row of its own. Quiet rows
one after another (calls folded, notes put in the margin, messages
between agents) sit together, with no blank row between. A turn that ends
with the model saying nothing (`empty_completion`) ends quietly, not as a
failure.

![Memory saves folded into one row, and a question whose options are buttons](../docs/sonny/fold-and-ask.png)

![A message from calc.neg, the task updates and a steer, each one row](../docs/sonny/agent-calls-folded.png)

![The steer opened to what it told calc.neg](../docs/sonny/agent-call-opened.png)

![Three task updates in a row gathered into one row that names the tasks](../docs/sonny/task-updates-gathered.png)

![That row opened: each update one row of its own](../docs/sonny/task-updates-listed.png)

![One update opened: what it said, and the steer the lead sent about it](../docs/sonny/task-update-opened.png)

The tasks the lead starts show where it started them, as one roster for
the tasks started together, each task with what it is doing now (starting,
working, needs you, done, no reply, checking, checked, failed), read
again every few seconds. A task whose model ended its turn with nothing to
say did not fail, but did not reply either, so it reads `no reply`, not
`done`, in the roster and in `/status`. A task with a goal reads `checking`
once a turn ends, and `checked` once its goal's check passed.

![A task whose goal's check passed beside one still checking and one only done](../docs/sonny/tasks-checked.png) A click
on a task opens it in the pane: its own Sonny, live, to watch and to talk
to, as a side chat is. Ctrl+\ takes the keys back to the conversation and
gives them to the pane again; from the conversation Ctrl+[ hides the task
and Ctrl+] shows it again.

![Three tasks started together, each with what it does now](../docs/sonny/tasks-roster.png)

An agent's name anywhere in the conversation opens that agent in the
pane, as a GitHub `@name` links to its account: in a reply ("tally.halves
has a question for you"), in a row of task updates, in a message from
another agent, and in the pane's own text. Only a name the daemon knows
is one, so it is drawn in the agents' blue, and a word that only looks
like one stays plain. In a row a click opens, such as `↳ From calc.neg
▸`, the name opens the agent and the rest of the row the message. Leads
and tasks write an agent's whole name when they point you to it, where
that helps you act.

![A reply naming tally.halves, clicked: the task opened in the pane](../docs/sonny/agent-name-opens.png)

![/status opened with tally.halves, the agent named last, already picked](../docs/sonny/agent-name-status.png)

Sonny keeps to the terminal's own 16 colors, so a theme sets their
shades, and each means one thing: magenta is you (your `›`), blue the
agents (task names, who sent a message, the lead in the footer), green
what worked (a call's dot, `+` lines, the git branch), yellow what needs
you (a question's `?`, an approval's `⏸`), red what failed (`✗`, `-`
lines), and cyan code and work under way. Color only repeats what a
mark already says: each state has its own shape (`✓ ✗ ● ◌ ?`) and a
diff its `-` and `+`, so the screen reads with `NO_COLOR` set, which
Sonny honors (no color, bold and dim kept), and to an eye that does
not tell red from green. Text is the theme's own color; hues go on
marks and names. Markdown is prose: headings bold, with their `#` marks
dim to show the level, code in cyan, links underlined, and the marks
of lists, quotes and rules dim. Dim text is a gray Sonny works out from
the theme's own foreground and background, as the terminal reports them
at start (OSC 10 and 11), dark enough to step back and still 4.5:1 where
the theme allows; a terminal that does not say keeps its own dim. A hue
on a dim row (the footer's branch, an agent's name in a quiet row) is
drawn at its full strength, as dim over a hue falls under 3:1 in many
themes; the words round it stay dim. Diffs
tint from the same background and the theme's own red and green. The
footer says how much of the model's context window is left (`62% context
left`, as Codex says it), from the window the daemon sends with each
round: green, yellow from half used, red from four fifths. Before the
daemon has said it, the footer gives the conversation's size uncoloured.
Your own messages sit on a band tinted from the theme's foreground over
its background, as Codex and Claude Code tint the user's cell, with a
row of the band above and below the words, so your turns are easy to
find when you scroll back; a reply that opens with a
heading takes no `•` before it.

![Your message on its band, and the footer saying 76% context left](../docs/sonny/message-band.png)

![Solarized Dark before and after: dim text that reads, diffs tinted from the theme with - and + beside the bar](../docs/sonny/visual-solarized-dark.png)
![With NO_COLOR set: no color, and every state, sign and question still reads](../docs/sonny/visual-no-color.png)

![The task calc.neg opened in the pane beside the lead's conversation](../docs/sonny/task-in-pane.png)

Sonny owns the whole screen, as Codex does; there is no inline mode.
The conversation and the composer keep to a column 100 wide, however wide the window, so a reply
reads as a column rather than a band across the screen. The composer sits on the same
tinted band as your messages, a row of it above and below the words, so it stays
apart from the conversation in any theme and what you write looks as it will once
sent; without colour (`NO_COLOR`) a rule above it does the same. The footer sits
under the band and keeps to the same column, so its right edge is the band's. The conversation is Sonny's
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
newer, and Esc stops where you are. Sonny draws the whole screen again on
every resize and when the terminal regains focus, so a multiplexer that
reattaches or a display that wakes never leaves stale rows; Ctrl+L
redraws it by hand, as in a shell.

With nothing typed, `?` lists every key beside the conversation. Every
hint on screen is written one way, as Codex writes them: the key, then
what it does muted, `·` between them (`↑↓ pick · enter open · esc
close`). Every list over the composer (the slash menu, `/model`,
`/permissions`, `/retry`, a reply to react to) is one list: up to 8 rows
under a blank one, the pick in reverse video, `•` before what is in
effect, and its keys where the footer was.

![? with nothing typed: every key beside the conversation](../docs/sonny/shortcuts.png)

Every list you narrow by typing (the slash menu, `/model`, `sonny setup`'s
pickers) finds rows one way: the letters typed, in order, the first of
them starting the name or a word in it, so `/cms` finds `/commons` and
`g6l` finds `gpt-6-luna`; the letters found are bold. The whole name
ranks first, then names it starts, then a word in a name it starts, then
the letters. Among equal matches in the slash menu, the commands you run
more often and more lately come first (each run counts half as much a
week on, kept in `~/.agent/sonny/picks.json`), so once you use `/commons`
more than `/compact`, `/co` and Enter opens `/commons`.

Enter runs the command marked, even on part of its name. `/compact`, which
can't be undone, then asks first: Enter compacts, any other key keeps the
conversation.

![/co with /compact marked, then /compact asking: compact the lead's conversation? It can't be undone · enter compact · esc cancel](../docs/sonny/slash-compact-confirm.png)

![/co after /commons was run: /commons ranks first; /cms finds /commons with c, m and s in bold](../docs/sonny/slash-ranked-fuzzy.png)

The pane's place, when nothing else is open there, is the margin. Its
first row is a glance at what runs for the agent: a red `✗` for what
failed in the background and `↻` for its automations and follows (a
click on either opens `/background`), and its context, each
left out when there is nothing to say; the footer stops saying them while
it shows. Under the composer, the footer counts what needs you and the
other agents at work, each with its word (`? 1 needs you · ● 2 working`),
and next to the model the agent's profile when it runs one that isn't its
seat's own (`lead` for a lead, `task` for a task),
marked as the roster and `/status` mark them, and a click on the count
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
its own, and stays once the task is gone. A note with `opens: "AGENT"`,
such as the lead's to-do for a task's question, gets a `↗` that opens that
agent in the pane, where you answer it. A file, `/status`, `/background` or a side
chat opens over the margin, its title's `←`, Esc and Ctrl+[ going back to it;
Esc in the margin closes it, and `/margin` brings it back. The margin
opens by itself when the agent puts up or changes a note, once the window
leaves the conversation its 100 columns and the margin 40 more, even
after Esc closed it, under whatever is open over it, as VS Code's
`reveal` shows a task's output; `[sonny] margin_opens = "unless-closed"`
leaves it closed after Esc until `/margin`, and `"never"` opens it only
with `/margin`. It never closes by itself: emptied, or opened to look at
with nothing in it, it stays until Esc. In a narrower window the footer counts the notes
(`✎ 3`), and Tab with nothing typed shows the margin. Where the margin
cannot show, because the window is narrower or it is closed, each note is
drawn in the conversation where it was put instead of its marker, up to
40 rows with the rest a click away, and it goes back beside the
conversation once there is room again. The agent's plan sits over the
notes, one plan however often it changes; each `plan` call in the
conversation is a row of how far it had got, and where the margin cannot
show, the newest is the whole checklist. The notes are kept by agent in
`~/.agent/sonny/margin/`, with the newest node of the agent's history they
were brought up to; the calls stay in its history. A window opened later
reads the `margin` calls and closes made since that node, however far
back they are, so a note changed while no window was open shows as it is
now.
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

![The lead took its only note away: the margin stays open, empty, until Esc or /margin](../docs/sonny/margin-stays-once-empty.png)

![Closed with /margin, then the lead put up a new note: the margin opened again with it](../docs/sonny/margin-opens-again.png)

![The lead heard that wire asks you and keeps it on your list in the margin, its ↗ ready to open wire](../docs/sonny/margin-opens-task.png)

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
