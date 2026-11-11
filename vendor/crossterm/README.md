# crossterm 0.29.0, patched for Sonny

The crates.io release as published (its `src` and licence), with one patch,
taken in by `[patch.crates-io]` in the workspace's `Cargo.toml`. Codex patches
crossterm the same way, for the same reason: colour answers.

## The patch

Sonny follows the terminal's light and dark as it switches: it asks for the
terminal's colour-scheme reports (mode 2031) and asks its colours again (OSC
10, 11 and 4) when it hears one, or when the window comes to the front. The
answers arrive as input, and crossterm 0.29 does not read them:

- `CSI ? 997 ; 1 n` (a 2031 report) left its parser waiting for a `u` or a
  `c`, swallowing every key typed until one came.
- An OSC answer (`ESC ] 11 ; rgb:… ESC \`) was read as Alt-] and then typed
  text.

Patched (`src/event.rs`, `src/event/sys/unix/parse.rs`, with tests):

- `Event::Reply(String)`: an OSC answer, or a `CSI ? … n` report, as it came;
  Sonny's `term.rs` reads it.
- Any other finished `CSI ?` sequence ends there, as upstream's fix for the
  same wedge (crossterm-rs/crossterm #1106, not yet released).
- `Event` is no longer `Copy` without the `bracketed-paste` feature (it never
  was with it, which is how Sonny builds it).

Upstream's lints are left as they are: its `Cargo.toml` allows them.

The colour tests are steadied, since the workspace's test run runs
crossterm's too (`src/style/types/colored.rs`). Whether colour is off is one
setting for the whole process, and the tests set it (and `NO_COLOR`) while
others format colours, which then printed nothing. Each test that sets it or
formats colours now holds one lock, and `test_no_color` settles the
remembered answer before it changes `NO_COLOR`.

## When it goes

Once a crossterm release reads both answers, depend on it and delete this
folder.
