# Anthropic daemon live run

Observed 2026-09-15 America/New_York on Darwin arm64. The daemon ran a bounded,
paid sequence against the Anthropic Messages API on `claude-sonnet-5`, then
short tool-and-continuation checks on `claude-opus-5` and `claude-fable-5-1`, mirroring
the [OpenAI daemon run](OPENAI_SMOKE.md#daemon-live-run): the same three turns
on one bot, a delegated helper, and a separate high-effort bot to exercise
thinking blocks. The key came from the caller's environment and appears in no
store, log, or output. Total spend is a few cents.

## What the run changed first

The first attempt failed before any tool ran: `provider_http_400` with the
provider's message that `thinking.type.enabled` is not supported and adaptive
thinking with `output_config.effort` should be used. Current Claude models
reject a thinking budget outright; only Haiku 4.5 and older take one. The
adapter now sends `thinking: {"type":"adaptive","display":"summarized"}` plus
`output_config: {"effort": LEVEL}` for the `reasoning` level, and keeps the
budget form only for known legacy model ids. `reasoning` accepts `xhigh` and
`max` in addition to `low`, `medium`, and `high`. The error detail that made
this a one-line diagnosis is the bounded provider message the adapter retains.

## Results

| Bot / turn | Prompt (paraphrased) | Effort | Calls | Tools | Input | Cached | Output | Wall ms |
| --- | --- | --- | ---: | --- | ---: | ---: | ---: | ---: |
| live-claude 1 | Read notes.txt, write uppercase greek.txt, count lines | low | 3 | shell, shell | 5,317 | 0 | 183 | 4,979 |
| live-claude 2 | Without reading again, which was the second word | low | 1 | none | 1,963 | 0 | 5 | 1,157 |
| live-claude 3 | Delegate peer.txt to a new agent, wait, report | low | 4 | shell, wait, shell | 9,271 | 0 | 279 | 7,808 |
| helper 4 | Create peer.txt (delegated) | low | 2 | write | 3,381 | 0 | 94 | 3,295 |
| thinker 5 | Count characters with one command, say if prime | xhigh | 2 | shell | 3,439 | 0 | 183 | 3,610 |
| thinker 6 | Without running anything, the count with one more line | xhigh | 1 | none | 1,924 | 0 | 158 | 3,334 |

| opus5 7 | Read greek.txt, edit GAMMA to DELTA, cat it | xhigh | 4 | read, edit, shell | 7,063 | 0 | 335 | 7,874 |
| opus5 8 | Without running anything, what was replaced | xhigh | 1 | none | 2,065 | 0 | 25 | 1,750 |
| fable51 9 | Read notes.txt, write it reversed, cat it | xhigh | 4 | read, write, shell | 7,075 | 0 | 379 | 8,575 |
| fable51 10 | Without running anything, first line of reversed.txt | xhigh | 1 | none | 2,096 | 0 | 5 | 2,066 |

Wall time is `finished_ms - started_ms` from the turn record and includes tool
execution and the parked wait. The `opus5` and `fable51` rows are
`claude-opus-5` and `claude-fable-5-1` on the same daemon and store, run
concurrently with each other.

What this established for the Messages family through the daemon:

- Tool use round-trips: `tool_use` blocks were reconstructed from the stream,
  stored as native assistant items, and their `tool_result` messages accepted
  on the next call. At low effort Sonnet 5 preferred `shell` over `read` and
  `write` for file work; the helper used `write`.
- Continuation from the store: turn 2 answered from history with no tool.
- Signed thinking blocks: at `xhigh` the model produced two `thinking` blocks
  with signatures around a tool call. They streamed as `thinking_delta` (the
  adapter asks for summarized display), were persisted in the assistant item,
  and were resent unchanged on turn 6, which the API accepted.
- Delegation with no delegation feature: the model ran
  `$AGENT_BIN run --detach --new --bot helper -- ...` from its shell tool,
  called `wait` on `turn:helper/4`, was parked, resumed with the helper's
  status and final text, and verified the file. The helper wrote into the
  parent's workspace because that turn was submitted with `--workspace`.
- Usage arrived on every call as `input_tokens` and `output_tokens` with
  `cache_read_input_tokens` zero throughout: the adapter sets no
  `cache_control` breakpoints, so Anthropic prompt caching never engaged.
  That is the next cheap adapter improvement; the OpenAI run cached
  automatically once the context grew.
- Opus 5 and Fable 5.1 accept the same request shape: adaptive thinking with
  summarized display and `output_config.effort`. Each produced one signed
  thinking block with visible summary text, replayed unchanged on its
  continuation turn and accepted. Fable 5.1 returned `end_turn`, not
  `refusal`, on this task; the adapter maps a `refusal` stop reason to an
  explicit `provider_refusal` turn failure and does not send the optional
  server-side fallback parameter. Opus 5 exercised `edit` against a real
  model for the first time: a single-occurrence replacement, verified by the
  following `cat`. Fable 5.1 streamed two summary thoughts before its first
  tool call.
- No admission wait, timeout, provider error after the fix, or lag. One or
  two concurrent requests say nothing about load.

## Caching and mid-turn forks

A later pass added a `cache_control` breakpoint after the instructions plus
top-level automatic caching. Two more turns on the Sonnet bot (turns 11 and
12, an `ls` task and a recall) then reported `cache_read_input_tokens` of
2,633 and 2,741 on their second and following calls, after zero on every
earlier call. Anthropic reports cache reads and cache writes outside
`input_tokens`; the adapter now counts every processed input token in
`input_tokens` and keeps reads in `cached_input_tokens`, so budgets and turn
accounting see the same totals on both families. Each `usage` event also
carries writes as `cache_write_tokens` (added 2026-09-25), since Anthropic
bills them above the base input rate.

The same session forked the Sonnet bot at the tool result inside turn 1
(node 5, before that turn's final answer) into a new bot, and one turn on the
branch answered from that point ("the wc -l command showed that greek.txt
contains 3 lines") with the cached prefix read. A fork without a node took
the current head. Both are the store's fork-from-any-message rule exercised
against the real API: the prefix ending at an answered tool call is a valid
conversation for the Messages API.

Not established: multi-agent load, long contexts, Haiku or older models with
the budget form, a refusal path on Fable 5.1, or cost at scale. The
store is under ignored `.local/live-anthropic/` with the daemon log.

Binary SHA-256 for the successful runs: `e499ecfbfd68d029063d8440feaa99863c1a8c79ec0e282d0d00accec1949a97`.
