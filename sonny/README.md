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
installed, else in the app's coordinator profile. When `rex` is found
(`$REX`, the PATH, or the Mac app), the project gets a Rex session labelled
with its name, and each task the lead (or one of its tasks) starts gets a
window there running Sonny on that task, without taking focus. Each window
reports OSC 7501 status, so Rex shows which task is working, waiting on an
approval or done. `--rex-host HOST` puts the session on another Rex server;
`--no-rex` turns the layout off. On first use Sonny writes the `rex-windows`
skill (`skills/rex-windows/SKILL.md`) to `~/.agents/skills/rex-windows`, so any agent can run a
long command in a Rex window the person can watch, and read it back.

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
- `/copy` puts the last reply on the clipboard; `/open` opens the last
  HTML page in a browser; `/sound` turns the chime off or on; `/quit`
  leaves the agents working.

Settings Sonny keeps for every folder are in `~/.agent/sonny/settings.json`.
When `errand` is installed, Sonny writes an `errand` skill so any agent can
run a command on another machine, draws those runs as `Ran COMMAND on
PEER`, and in Rex opens a window following each detached job.

The look follows Codex's TUI (openai/codex, `codex-rs/tui`): finished history
goes into the terminal's own scrollback above a small inline viewport,
replies stream a line at a time, a tool call is one `• Ran …` line with a
short `└` look at its result, edits show as a diff, and a status row says
`Working (12s • esc to interrupt)`.

`demo/` has the scripted model and folder the recordings use.
