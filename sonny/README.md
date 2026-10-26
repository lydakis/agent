# Sonny (prototype)

A terminal client for the daemon, beside the app, that lays its work out in
Rex. It speaks the daemon's socket protocol through `agent-client`, like the
app; nothing in the daemon or CLI knows it exists.

    brew install lydakis/agent/sonny         # Sonny, with a daemon of its own version

    sonny                                    # this folder's coordinator, <project>.lead
    sonny --model PROVIDER/MODEL             # the same, making the project the first time
    sonny --bot NAME                         # any one bot

Opened in a folder, Sonny is that project's lead: the same `<project>.lead`
the app opens, made through the app's `~/.agent/project add` when the app is
installed, else in the app's coordinator profile. A lead moved to another
folder (`--workspace`) comes back with the next message Sonny sends it from
its own, history and all. When `rex` is found
(`$REX`, the PATH, or the Mac app), the project gets one Rex session: the
one Sonny was opened in, else the one the lead is in, else one labelled with the project's name. Each task the lead (or one of its tasks) starts gets a
window there running Sonny on that task, without taking focus. Each window
reports OSC 7501 status, so Rex shows which task is working, waiting on an
approval or done. A task's window closes once its work is taken: its turn
completed and the lead (or the task that started it) waited on it. One that
failed, stopped or waits on you stays open, and a task given more work opens
again; ask the lead to show one, or run `sonny --bot NAME`. `--rex-host HOST` puts the session on another Rex server;
`--no-rex` turns the layout off. On first use Sonny writes the `rex-windows`
skill (`skills/rex-windows/SKILL.md`) to `~/.agents/skills/rex-windows`, so any agent can run a
long command where the person can watch it, and read it back:
`sonny split NAME [--window] -- COMMAND...` opens a block labelled
`AGENT/NAME` below the agent's own (or in a tab of its own) and prints its ID.
The skill tells agents to close what they opened once read; whatever a task
left open closes with its window.

The lead hears what its tasks did as the app tells it: once it rests, and at
most every ten minutes, one "Task updates" message lists the tasks it started
that ended a turn or wait for an approval, by the handles its wait tool reads
them with, with what changed in memory alongside. The app and Sonny send the
same news as one message, so it does not matter which is open.

Enter sends (and steers a running turn), Esc interrupts, `y`/`n` answer a
pending approval, Ctrl-C clears the composer or quits. `/` lists the
commands:

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
  [sandfs](https://github.com/lydakis/sandfs) copies, which take the
  folder's uncommitted changes along, cost no disk until written, and work
  outside git too. Every task gets one; only the lead works in the folder.
- `/copy` puts the last reply on the clipboard; `/open` opens the last
  HTML page in a browser; `/sound` turns the chime off or on; `/quit`
  leaves the agents working.

Settings Sonny keeps for every folder are in `~/.agent/sonny/settings.json`.
When `errand` is installed, Sonny writes an `errand` skill so any agent can
run a command on another machine, draws those runs as `Ran COMMAND on
PEER`, and in Rex opens a window following each detached job.

A lead also has a lookout: a decision model that watches its tasks and
wakes the lead only when it should step in. It is asked a few yes-or-no
questions (does the person's message redirect other tasks, does a task
conflict with another, is one stuck or off its brief, is there a result the
person should hear, a lasting preference worth saving) about each message in
a task, each finished turn, and each approval waiting over two minutes, with
the project's memory indexes in view. A high answer wakes the lead with a
short note of turn handles and what to do (urgent ones steer it), a middling
one waits for the next note, and the rest are only logged, to
`~/.agent/sonny/lookout.jsonl`. The note reaches the approver as the
person's words, so the lead can act on it (pass a preference on, save it
to memory) without asking, and it quotes only the person's own words. The
judge is `openai-decisions/gpt-6-luna` (needs `OPENAI_API_KEY`, about
$0.0002 a check); `--lookout typesafe/jev-*` picks Jev, `--lookout off`
turns it off. `sonny lookout-replay --judge SPEC` runs the same checks over
the store's retained history, and `demo/lookout_score.py` scores that
against labels.

The look follows Codex's TUI (openai/codex, `codex-rs/tui`): finished history
goes into the terminal's own scrollback above a small inline viewport,
replies stream a line at a time, a tool call is one `• Ran …` line with a
short `└` look at its result, edits show as a diff, and a status row says
`Working (12s • esc to interrupt)`.

`demo/` has the scripted model and folder the recordings use.
