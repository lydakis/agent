# OpenAI live checks

Observed 2026-09-14 America/New_York: the Responses adapter passed a paid,
bounded check against OpenAI on `gpt-5-mini-2025-08-07` through a standalone
example that called the provider directly (three requests, one tool call, 427
input tokens, 195 output tokens, two retained encrypted reasoning items, usage
on every call; capture `.local/openai-smoke-result.json`). That example was
removed on 2026-09-15 with the in-memory history path: the daemon below is the
only surface, and paid checks go through it.

Requests explicitly include `reasoning.encrypted_content` so stateless calls
keep opaque reasoning across turns. `Provider::with_max_output_tokens` bounds
Responses generation, including reasoning; zero and unsupported families are
rejected. The runtime does not load env files; keep credentials out of tracked
files, benchmark captures, tool environments, and remote snapshots.

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
