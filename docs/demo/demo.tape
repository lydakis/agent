# Records docs/demo/demo.gif. From the repository root, after
# `cargo build --release`:
#
#   vhs docs/demo/demo.tape
#
# See setup.sh for recording against a real model instead of the scripted one.

Output docs/demo/demo.gif

Set Shell "bash"
Set FontSize 15
Set Width 1400
Set Height 560
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms

Hide
Type "source docs/demo/setup.sh 2>/dev/null; clear" Enter
Sleep 2s
Show

Type 'agent run --pretty --new --bot lead -- "Fix the failing test, and have a helper check the README"'
Sleep 500ms
Enter
Wait+Screen@180s /agent: (✔|✘)/
Sleep 4s

Type "agent ls --pretty"
Sleep 400ms
Enter
Sleep 4s

Hide
Type "demo_stop" Enter
Sleep 1s
