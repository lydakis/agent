# Live fleet check

Observed 2026-09-15 America/New_York on Darwin arm64 (10 logical CPUs, external
power). Bounded, paid runs of many bots at once through one daemon against
real providers, to see the request-startup bound and the daemon under
concurrent live traffic for the first time. Every earlier live run had one or
two requests in flight. Total spend across the runs is under two dollars.

Each run starts a daemon with the given `--max-connecting`, then submits N
detached turns concurrently, each on a fresh bot with the same prompt (one
shell call plus a final answer, so two model calls per bot), waits on every
handle, and reads the turn records. Concurrency is the peak number of turns
whose start and finish timestamps overlap. Latency is per turn, from
acceptance to the terminal event, and includes the shell tool. Daemon RSS
and threads are sampled every 200 ms by the driver, which runs outside the
daemon. Reproduce with `bench.live_fleet`; captures are under ignored
`.local/fleet-*/` and `.local/bench/fleet-*/`.

| Run | Model | Bots | Connecting bound | Overlapping turns | Total s | p50 ms | p95 ms | Input tokens | Daemon RSS peak | Threads |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| sonnet-c8 | claude-sonnet-5 | 32 | 8 | 32 | 5.55 | 4,513 | 5,413 | 108,480 | 15.6 MiB | 11 |
| sonnet-c64 | claude-sonnet-5 | 32 | 64 | 32 | 2.37 | 2,010 | 2,116 | 108,448 | 17.7 MiB | 10 |
| sonnet-96 | claude-sonnet-5 | 96 | 64 | 96 | 2.86 | 2,085 | 2,573 | 325,376 | 19.0 MiB | 13 |
| luna-32 | gpt-5.6-luna | 32 | 64 | 32 | 3.00 | 1,820 | 2,276 | 42,624 | 18.0 MiB | 34 |

All 192 bots completed; no submission failed, no turn errored, and no provider
rate limit or admission timeout appeared. Submitting 96 bots took 0.21 s.
(An earlier 32-bot run on the older gpt-5-mini also completed, six times
slower because of its long low-effort outputs; it is superseded by the
gpt-5.6-luna row and not retained here.)

What the numbers say:

- The startup bound behaves as designed. At 32 bots with 8 permits, the
  fleet finished in 5.55 s with p50 4.5 s; with 64 permits, 2.37 s and 2.0 s.
  The difference is queued header waits, not provider time. At 96 bots the
  bound admitted the first 64 immediately and the rest as headers arrived,
  costing about 0.5 s on p95 versus the 32-bot run.
- The daemon's cost per concurrent bot is small: 96 overlapping turns raised
  RSS from 10.6 to 19.0 MiB, about 90 KB each, with the histories of these
  short conversations loaded for their requests. Threads stayed at 10 to 13
  on Anthropic and reached 34 on OpenAI, where all 32 shell commands were
  spawned within the same instant on the blocking pool; those threads are
  transient and not per-bot state.
- gpt-5.6-luna and claude-sonnet-5 were within a few hundred milliseconds of
  each other per turn on this task, at similar input token counts.
- The 120 s idle read timeout was never approached; the longest single turn
  was 5.5 s.

Not established: behavior at hundreds of bots, provider rate limits at this
organization's tier, long contexts under concurrency, or tool-heavy turns.
The prompt is deliberately trivial so that the harness, not the task, is
measured. The `--max-connecting` bound applies to requests awaiting response
headers; established streams are unbounded, which is what let 96 turns
overlap with 64 permits.

The first 96-bot attempt was aborted by the driver, not the daemon: `wait`
accepts at most 64 handles per request, the driver passed 96, and it then
shut the daemon down with every turn running. The daemon had accepted all 96
submissions in 0.19 s. The driver now waits in chunks.
