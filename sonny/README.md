# Sonny (prototype)

A terminal client for the daemon, beside the app, that lays its work out in
Rex. It speaks the daemon's socket protocol through `agent-client`, like the
app; nothing in the daemon or CLI knows it exists.

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
`--no-rex` turns the layout off. On first use Sonny writes the `rex` skill
(`skills/rex/SKILL.md`) to `~/.agents/skills/rex`, so any agent can run a
long command in a Rex window the person can watch, and read it back.

Enter sends (and steers a running turn), Esc interrupts, `y`/`n` answer a
pending approval, Ctrl-C clears the composer or quits.

The look follows Codex's TUI (openai/codex, `codex-rs/tui`): finished history
goes into the terminal's own scrollback above a small inline viewport,
replies stream a line at a time, a tool call is one `• Ran …` line with a
short `└` look at its result, edits show as a diff, and a status row says
`Working (12s • esc to interrupt)`.

`demo/` has the scripted model and folder the recordings use.
