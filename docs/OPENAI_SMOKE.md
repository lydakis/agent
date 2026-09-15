# OpenAI adapter live check

Observed 2026-09-14 America/New_York. The existing Rust Responses adapter passed
a paid, bounded check against OpenAI using `gpt-5-mini-2025-08-07`. No SDK was
added. The provider continues to share its HTTP transport and stream native
history items by reference.

The [probe](../examples/openai_smoke.rs) permits at most four requests, each with
`max_output_tokens: 2048`. It uses a synthetic marker and only the native echo
tool. It emits safe status and token counts, not prompts, outputs, or credentials.
It is an explicitly invoked example, never part of automatic test execution.

Result: three requests, one tool call, 427 input tokens, 195 output tokens, and
two retained encrypted reasoning items. The streamed answer included the tool's
result. After reconstructing the native history from serialized items, a further
request recalled that result without another tool call. Every call returned usage.
The safe local capture is `.local/openai-smoke-result.json`.

This tests the real provider adapter and tool executor. It does not test a live
daemon crash, SQLite recovery, historical forks, arbitrary tools, large contexts,
or provider capacity. Those lifecycle behaviors currently have synthetic coverage.

## Adapter changes

- A validated, optional `Provider::with_max_output_tokens` bounds Responses
  generation, including reasoning. Zero and unsupported provider families are
  rejected. The daemon's default remains unchanged; this is not a daemon-wide
  spend limit.
- Requests explicitly include `reasoning.encrypted_content` for compatibility
  with older Responses implementations. Native output items remain intact.
  Current OpenAI documentation says stateless calls return encrypted reasoning
  by default, so omission of `include` is not itself a demonstrated current API bug.

Sources checked on the observation date: [reasoning and stateless continuation](https://developers.openai.com/api/docs/guides/reasoning),
[Responses request parameters](https://developers.openai.com/api/reference/resources/responses/methods/create),
and [GPT-5 mini capabilities and snapshot](https://developers.openai.com/api/docs/models/gpt-5-mini).

## Run deliberately

With `OPENAI_API_KEY` already supplied through the caller's environment:

```sh
CARGO_HOME=.local/cargo cargo run --release --locked --example openai_smoke
```

The runtime does not automatically load env files. Keep credentials out of
tracked files, benchmark captures, tool environments, and remote snapshots.

## Daemon live run

Observed 2026-09-15 America/New_York on Darwin arm64, binary SHA-256
`69f220c499495775601f58876b1c73c127151aa026be43ab1338bf5508fc2468` (source
`0b5c073` plus no uncommitted runtime changes). This is the first paid run
through the actual daemon: `agent run` started a socket daemon with
`--provider openai`, `--tools shell,read,write,edit,wait`, `--reasoning low`,
and the resolved limits `processes 640, active 4096, connecting 64`. The key
was supplied from the caller's environment; it never appears in the store,
the daemon log, or tool output. Two stores were used: a first run on
`gpt-5-mini-2025-08-07`, then a fresh store on `gpt-5.6-luna` for the turns
below. Total spend across both is a few cents.

| Turn | Prompt (paraphrased) | Model calls | Tools | Input tokens | Cached | Output |
| ---: | --- | ---: | --- | ---: | ---: | ---: |
| 1 | Read notes.txt, write uppercase greek.txt, count lines | 4 | read, shell, write, shell | 3,382 | 0 | 192 |
| 2 | Without reading again, which was the second word | 1 | none | 1,040 | 0 | 6 |
| 3 | Delegate peer.txt to a new agent, wait, report | 4 | shell, wait, shell | 5,166 | 4,747 | 220 |
| helper (turn 4) | Create peer.txt, verify, report | 3 | write, shell | 2,101 | 0 | 112 |

The `gpt-5-mini` run of the turn-1 task took 4 calls, 3,005 input and 222
output tokens, and produced a reasoning summary that streamed as
`thinking_delta`; `gpt-5.6-luna` produced none at `low` effort.

What this established, in daemon terms rather than adapter terms:

- Encrypted reasoning items (about 1.4 KB each) were persisted as history
  nodes, reloaded from SQLite for turns 2 and 3, resent in the request body,
  and accepted by the API. Turn 2 answered from history without a tool call.
- Every model call produced a durable `usage` event. Prompt-cache hits appeared
  once the context exceeded the cache threshold: 4,747 of 5,166 input tokens
  on turn 3's later calls.
- `read`, `write`, `edit` (not called), and `shell` ran against the real
  model's arguments without a rejected call. Shell results and previews were
  rendered by `--pretty`; the JSONL stream is the same events.
- Delegation worked live with no delegation feature: the model ran
  `$AGENT_BIN run --detach --new --bot helper -- ...` from its shell tool,
  received `turn:helper/4`, called `wait` on it, was parked (a `turn_waiting`
  event, no task), resumed when the helper finished, and received the
  helper's status and final text in the tool result. The helper ran as an
  ordinary peer with its own turn and usage.
- The turn workspace is where `run` was invoked: turn 1 passed `--workspace`,
  turns 2 and 3 were invoked from the repository root, so their shell tools
  and the helper ran there. That is the documented per-submission semantics
  and the caller's responsibility; the stray file was removed.
- No admission wait, idle-timeout expiry, provider error, or lag occurred.
  With one or two concurrent requests this run says nothing about the
  64-request startup bound or tail latency under load.

Not established: Anthropic live behavior, any multi-agent load, long
contexts, the edit tool against a real model, or cost at scale. The store is
under ignored `.local/live/` with the daemon log; `agent shutdown` ended the
daemon cleanly.
