# Sonny (prototype)

A terminal client for the daemon, beside the app, that lays its work out in
Rex. It speaks the daemon's socket protocol through `agent-client`, like the
app; nothing in the daemon or CLI knows it exists.

    brew install lydakis/agent/sonny         # Sonny, with a daemon of its own version

On Linux, each release has `sonny-VERSION-linux-ARCH.tar.gz` (x86_64 and
aarch64): static builds of Sonny in `bin/` and its daemon in `libexec/`.
Unpack it anywhere and run `bin/sonny`.

    sonny                                    # this folder's coordinator, <project>.lead
    sonny --model PROVIDER/MODEL             # the same, making the project the first time
    sonny --bot NAME                         # any one bot

Opened in a folder, Sonny is that project's lead: the same `<project>.lead`
the app opens, made through the app's `~/.agent/project add` when the app is
installed, else in the app's coordinator profile. A lead moved to another
folder (`--workspace`) comes back with the next message Sonny sends it from
its own, history and all. When Sonny runs in a Rex block (`rex` found as
`$REX`, on the PATH, or in the Mac app) or a Herdr pane (`herdr` found as
`$HERDR` or on the PATH), the project gets one home there: in Rex a session
(the one Sonny was opened in, else the one the lead is in, else one labelled
with the project's name), in Herdr a workspace, chosen the same way. Each task
the lead (or one of its tasks) starts gets a window (a Herdr tab) there
running Sonny on that task, without taking focus, its block (pane) labelled
with the task's name. In Rex, each window reports OSC 7501 status, so Rex
shows which task is working, waiting on an approval or done. In Herdr, Sonny
supports Herdr the way its own agents do: each Sonny reports its bot through
`herdr pane report-agent`, so the agent panel lists it as `Sonny: NAME`
(working, idle, or blocked on an approval), the workspace rolls that up and
notifies, `herdr agent wait` and `herdr agent prompt` reach it by name
(`shop-fix-cart` for `shop.fix-cart`), and after a Herdr restart the pane runs
`sonny` on the same bot again (when `sonny` is on the PATH). The place a
project was laid out in is kept in `~/.agent/sonny/places`, so an agent's
`sonny split` and `sonny show` go to that Herdr workspace or Rex session, in
that multiplexer, rather than one guessed from the agent's shell, which runs
in the daemon and is in neither. A task's window
closes once its work is taken: its turn completed and the lead (or the task
that started it) waited on it. One that failed, stopped or waits on you stays
open, and a task given more work opens again; ask the lead to show one, or
run `sonny --bot NAME`. `--rex-host HOST` puts the session on another Rex
server; `--no-layout` turns the layout off. Sonny writes the skill for the
multiplexer it runs in to `~/.agents/skills` (`rex-windows` or
`herdr-panes`, from `skills/`), so any agent can run a long command where
the person can watch it, and read it back: `sonny split NAME [--window] --
COMMAND...` opens a block labelled `AGENT/NAME` below the agent's own (or
in a tab of its own) and prints its ID. Herdr has no exit status to wait
for, so its skill has agents end such a command with a line to wait for.
The skill tells agents to close what they opened once read; whatever a task
left open closes with its window.

Enter sends (and steers a running turn), Esc interrupts, `y`/`n` answer a
pending approval, Ctrl-C clears the composer or quits. `/` lists the
commands:

- `/side [QUESTION]` opens a side chat with a copy of the agent in this
  window, taken where it is now, mid-turn included, on the alternate
  screen. The copy answers from the agent's history and may call no tool;
  follow-ups stay with it, and Esc goes back. The agent carries on
  meanwhile, and what it said is waiting on the main screen. The daemon
  deletes the copy once it has been idle 15 minutes (`fork --expires`), and
  no copy shows among the project's agents, in `/status`, or to the
  lookout.
- `/model` picks the lead's model, from every model its provider family
  lists, and its effort (←/→), and writes them to the project's
  `.agents/project.toml` when the folder has one. A pick made while a turn
  runs shows in the footer and applies when the lead is idle; one that
  another client overtakes is dropped.
- `/instructions` gives the lead its folder's instructions again: its
  role, `AGENTS.md`, skills and memory. Sonny does this on its own when
  it opens a lead whose instructions it saw composed from the folder, and
  leaves ones set any other way alone.
- `/approvals` picks whether calls wait for a verdict: `auto`, a judge
  model (a new lead starts here, and Sonny starts `agent approver` when
  nothing serves it), `ask`, you, or `full`, none. Tasks the lead makes keep
  the gate it had.
- `/status` shows the project's agents, and its running Errand jobs.
- `/copies` (or `--copies worktree|sandfs`) picks how the lead's tasks get
  their own copy of the folder: git worktrees (the default), or
  [sandfs](https://github.com/lydakis/sandfs) copies (experimental), which
  take the folder's uncommitted changes along, cost no disk until written,
  and work outside git too. Every task gets one; only the lead works in the
  folder. The lead starts each task with `sonny task NAME [--setup] [run
  flags] -- BRIEF`, which makes its folder that way, runs `.agents/setup`
  there with `--setup`, starts the task with `agent run`, and leaves no
  folder behind when that fails; `sonny task changes NAME [--apply]` lists,
  or writes back, what a sandfs copy of a plain folder changed. A plain
  folder on another disk than sandfs's home (`~/.local/share/sandfs`), such
  as an external volume, can't be copied, so its task gets a sandfs
  workspace over the folder itself: the task's changes still stay in the
  workspace, but it also sees what changes in the folder meanwhile.
  `sonny task` also works from a plain terminal, with the `agent` installed
  beside Sonny. sandfs is
  built into Sonny, so it needs no install, only what a mount needs (FUSE 3
  on Linux, nothing on macOS). Its mounts are served by Sonny run again in
  the background, which outlives Sonny; Sonny mounts task copies a restart
  unmounted again when it opens the lead.
- **Needs you:** when the lead needs the person to decide something, it
  runs `sonny ask "QUESTION" [--choice OPTION]...` from its shell. Sonny
  pins the open questions above the composer, oldest first, until each is
  answered: what you type next answers the first (a choice's number picks
  it), and goes to the lead as `You asked: QUESTION` and your answer. Esc
  sets the questions aside for a message of your own, and Esc again goes
  back to them. They wait in `~/.agent/sonny/asks/`, one file each, kept apart by store and bot id.
- `/copy` puts the last reply on the clipboard; `/open` opens the last
  HTML page in a browser; `/sound` turns the chime off or on; `/quit`
  leaves the agents working.

Settings Sonny keeps for every folder are in `~/.agent/sonny/settings.json`.
When `errand` is installed, Sonny writes an `errand` skill so any agent can
run a command on another machine, draws those runs as `Ran COMMAND on
PEER`, and in Rex opens a window following each detached job.

Sonny starts its lookout, `sonny lookout`, when it opens a project: one
for the daemon, watching every lead on it. It tells a lead what its tasks
did in one "Task updates" note, by the handles its wait tool reads them
with, and wakes it early only when one needs it: a message that redirects
other tasks, a conflict, a stuck or silent task, a result the person should
hear. It judges with `AGENT_JUDGE`, the approver's setting, so Bedrock alone
is enough, and with no judge sends the task updates alone. Any agent can
subscribe to more with `sonny lookout add` (other agents' tasks, bots by
name, questions of its own), and `/lookout` lists what it watches; see
[docs/LOOKOUT.md](../docs/LOOKOUT.md).

The look follows Codex's TUI (openai/codex, `codex-rs/tui`): finished history
goes into the terminal's own scrollback above a small inline viewport,
replies stream a line at a time, a tool call is one `• Ran …` line with a
short `└` look at its result, edits show as a diff, and a status row says
`Working (12s • esc to interrupt)`.

`demo/` has the scripted model and folder the recordings use.
