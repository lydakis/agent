# agent-tui (prototype)

A terminal client for one bot, beside the app. It speaks the daemon's socket
protocol through `agent-client`, like the app; nothing in the daemon or CLI
knows it exists.

    agent-tui --bot NAME                                  # attach to a bot
    agent-tui --new --bot NAME --model PROVIDER/MODEL     # create one here, then attach

Enter sends (and steers a running turn), Esc interrupts, `y`/`n` answer a
pending approval, Ctrl-C clears the composer or quits.

The look follows Codex's TUI (openai/codex, `codex-rs/tui`): finished history
goes into the terminal's own scrollback above a small inline viewport,
replies stream a line at a time, a tool call is one `• Ran …` line with a
short `└` look at its result, edits show as a diff, and a status row says
`Working (12s • esc to interrupt)`. It reports OSC 7501 program status, so
Ghostty and Rex can mark its pane working, blocked or done.

`rex/agent-rex` opens a Rex session with one block per busy bot, each running
`agent-tui`. Its Rex flags are unverified.
