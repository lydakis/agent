# Demo recording

`demo.gif` in the top-level README is recorded with [VHS](https://github.com/charmbracelet/vhs)
from `demo.tape`. The bot `lead` works in a copy of `tally/`, a tiny Python
project whose `total()` truncates cents instead of rounding them.

The model's words come from `scripted_model.py`, a local Responses endpoint, so
the recording needs no API key and comes out the same every time. Everything
else is real: the daemon, the store, both bots, the shell and edit tools, the
`agent run --detach` that starts the helper, and the `wait` that collects it.

From the repository root:

```sh
cargo build --release
vhs docs/demo/demo.tape
```

To record against a real model instead, set `AGENT_DEMO_LIVE=1`, a model, and
that provider's key. The model decides what to do, so the recording will differ
from the scripted one and may not delegate at all.

```sh
AGENT_DEMO_LIVE=1 AGENT_MODEL=anthropic/claude-opus-5-5 ANTHROPIC_API_KEY=... \
  vhs docs/demo/demo.tape
```

`setup.sh` runs hidden before recording starts: it copies `tally/` and a
throwaway store into a new temporary directory, which the tape removes when
it finishes.
