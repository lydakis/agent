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

## Hundreds of bots

Observed 2026-09-15 America/New_York on the same host, after stored history
became unbounded and requests started streaming their context window from
the store (commit `33b1cde`; binary `5cc8f7b7…` for the first four rows,
`30e08c21…` for the last two, which only adds the transport cause chain to
connection failures). Same driver, prompt, and `--max-connecting 64`. Total
spend across these runs is a few dollars, most of it the two Sonnet 256 runs.

| Run | Model | Bots | Overlapping turns | Submit s | Total s | p50 ms | p95 ms | Max ms | Input tokens | Daemon RSS start → peak | Threads | Outcome |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| luna-256 | gpt-5.6-luna | 256 | 256 | 0.50 | 10.02 | 6,169 | 8,136 | 9,550 | 340,992 | 10.1 → 23.4 MiB | 35 | all completed |
| luna-512 | gpt-5.6-luna | 512 | 512 | 0.97 | 17.36 | 10,879 | 13,567 | 16,450 | 681,984 | 10.3 → 24.7 MiB | 32 | all completed |
| luna-1024 | gpt-5.6-luna | 1,024 | 1,024 | 2.04 | 27.39 | 16,816 | 20,800 | 25,563 | 1,362,633 | 10.2 → 29.0 MiB | 25 | 1,023 completed, 1 `provider_http_503` |
| sonnet-128 | claude-sonnet-5 | 128 | 128 | 0.26 | 4.17 | 2,573 | 3,244 | 3,934 | 433,919 | 10.0 → 19.4 MiB | 24 | all completed |
| sonnet-256 | claude-sonnet-5 | 256 | 202 | 0.50 | 5.39 | 3,860 | 4,706 | 4,907 | 684,830 | 10.1 → 24.4 MiB | 53 | 202 completed, 54 `provider_connection_failed` |
| sonnet-256b | claude-sonnet-5 | 256 | 256 | 0.50 | 6.32 | 4,732 | 5,518 | 5,818 | 867,872 | 10.4 → 21.3 MiB | 22 | all completed |

What the numbers say:

- The 1,024-bot run reached 1,024 overlapping turns and 29.0 MiB peak daemon
  RSS, 18.8 MiB above its initial sample. Overlap includes turns queued in the
  harness; it does not establish 1,024 simultaneous provider streams. These
  short-history bursts do not establish memory scaling with long contexts.
- Submitting 1,024 turns took 2.0 s. Luna median turn latency rose from 1.8 s
  at 32 submitted bots to 6.2 s at 256 and 16.8 s at 1,024. The initial
  attribution to provider queueing was superseded by the transport
  investigation below: the harness's single HTTP/2 connection and 64-permit
  startup bound restricted concurrency. These runs do not isolate provider
  queueing from harness queueing.
- **Providers fail at this scale and the harness records it as a failed
  turn, nothing more.** One of 1,024 luna turns got a 503. One Sonnet 256 run
  lost 54 turns to transport-level connection failures with no OS error code
  and no HTTP status; an identical rerun minutes later completed all 256.
  The failing binary discarded the cause, so the reason is not established;
  the daemon now keeps the transport cause chain (URL, HTTP, TLS, or socket
  layer) as the error detail so the next occurrence names itself. Nothing is
  retried: a failed turn is durable, its bot is idle, and the caller
  resubmits.
- Thread peaks (22 to 53) remain transient blocking-pool workers for shell
  spawns and TLS handshakes, not per-bot state.

Not established: thousands of bots on Anthropic, sustained load over minutes,
long contexts under concurrency, or provider rate limits beyond a single burst.
Captures: ignored `.local/bench/fleet-luna-256/`, `fleet-luna-512/`,
`fleet-luna-1024/`, `fleet-sonnet-128/`, `fleet-sonnet-256/`, and
`fleet-sonnet-256b/`.

## Connection sharding

The runs above were serialized by the harness, not the providers. The daemon
multiplexed every request over one HTTP/2 connection per provider, and both
`api.openai.com` and `api.anthropic.com` advertise `MAX_CONCURRENT_STREAMS`
of 100 on a connection (read from their SETTINGS frame), so at most 100 model
calls ran at once whatever the fleet size; the rest queued inside the HTTP
layer. Total time was calls divided by 100 times per-call service time, which
is the linear growth in the table. The transport now keeps one HTTP/2
connection per 100 active turns (41 at the default `--max-active 4096`) and
gives each request the least-loaded one for the life of its stream.

The second serializer was `--max-connecting 64`: both providers hold response
headers until the first token, so a permit covered the whole time to first
token and capped throughput at about 64 calls per first-token latency. The
bound is now off by default.

Observed 2026-09-15 America/New_York, same host, driver, and prompt, on binary
`e4e88a1d…` (sharded transport, before the default bound changed; the bound was passed explicitly).

| Run | Model | Bots | Connections | Connecting bound | Overlapping turns | Total s | p50 ms | p95 ms | Daemon RSS peak | Outcome |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| luna-1024 (one connection, above) | gpt-5.6-luna | 1,024 | 1 | 64 | 1,024 | 27.39 | 16,816 | 20,800 | 29.0 MiB | 1 `provider_http_503` |
| luna-1024-sharded | gpt-5.6-luna | 1,024 | 41 | 64 | 1,024 | 25.78 | 11,172 | 13,432 | 34.3 MiB | 1 `provider_http_503` |
| luna-1024-sharded-c0 | gpt-5.6-luna | 1,024 | 41 | none | 805 | 12.50 | 1,924 | 3,167 | 41.2 MiB | 1 `provider_http_503` |
| sonnet-256b (one connection, above) | claude-sonnet-5 | 256 | 1 | 64 | 256 | 6.32 | 4,732 | 5,518 | 21.3 MiB | all completed |
| sonnet-256-sharded | claude-sonnet-5 | 256 | 41 | 64 | 256 | 5.80 | 4,215 | 5,083 | 26.6 MiB | all completed |
| sonnet-256-sharded-c0 | claude-sonnet-5 | 256 | 41 | none | 256 | 3.48 | 2,050 | 2,296 | 28.6 MiB | all completed |

With sharding and no startup bound, median turn latency was similar across
these batch sizes: luna p50 1.8 s at 32 submitted bots and 1.9 s at 1,024;
Sonnet 2.0 s at 32 and 2.05 s at 256. The 1,024-bot batch finished in 12.5 s,
of which 2.4 s was submitting turns, and reached 805 overlapping turns as
some finished before submission ended. Peak daemon RSS was 41.23 MiB, with
41 TLS connections observed. This does not establish that latency or memory
at 1,024 simultaneous turns, nor a per-stream memory cost: RSS and overlap
peaks are sampled independently, and overlap includes harness queueing and
tool work. OpenAI returned one 503 in each 1,024-bot run. Sustained runs are
needed to distinguish remaining harness limits from provider limits. Captures: ignored
`.local/bench/fleet-luna-1024-sharded/`, `fleet-luna-1024-sharded-c0/`,
`fleet-sonnet-256-sharded/`, and `fleet-sonnet-256-sharded-c0/`; the
connection counts were sampled with `lsof` on the daemon during the runs.

## Sustained load

Observed 2026-09-16 America/New_York on the same host, binary
`7748a10c…` (sharded transport, startup bound off by default), with
`bench.sustained`: N bots each resubmit the same shell-plus-answer turn the
moment their previous turn finishes, for M minutes, through one stdio daemon
with `--context-items 8` so requests stay the same size as histories grow.
The driver samples the daemon every 10 s and buckets outcomes in 30 s
windows. Spend: about 14 million input tokens on luna and 3.6 million on
Sonnet 5.

| Run | Model | Bots | Minutes | Turns | Failed | Steady turns/s | p50 ms | p95 ms | Slowest turn | Daemon RSS | Open files | Store growth |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| luna-64 | gpt-5.6-luna | 64 | 5 | 9,050 | 4 × `provider_http_503` | 30 (60 model calls/s) | 1,907–2,014 | 2,981–3,646 | 46.9 s | 27.0 → 23.3 MiB | 56 | 25 MiB (2.8 KB per turn) |
| sonnet-16 | claude-sonnet-5 | 16 | 2 | 983 | none | 8 | 1,872–1,921 | 2,392–2,716 | 6.8 s | 17.7 → 19.7 MiB | 31 | 6 MiB |

What the numbers say:

- **Nothing drifts.** Over five minutes and 9,050 turns the luna daemon's
  RSS stayed between 23 and 27 MiB (it fell mid-run when the allocator
  returned pages), threads stayed at 5, and open files at 56 for the whole
  run: 41 provider connections plus the store and stdio. Every 30 s window
  completed 883 to 929 turns with p50 within 100 ms of the first window.
  Sonnet showed the same shape at 16 bots.
- **No provider rate limit appeared** at about 2.7 million input tokens per
  minute and 60 requests per second on OpenAI, or at 1.8 million per minute
  on Anthropic. OpenAI returned four 503s in 9,050 turns, all in the first
  two and a half minutes; each became a failed turn and the bot's next
  submission succeeded. That is the only provider-side limit observed so far.
- **Tail latency is the provider's.** The slowest luna turn took 46.9 s
  against a p95 of about 3 s; it completed normally, well inside the 120 s
  idle read timeout, and no other turn was delayed by it.
- **The store grows linearly** at about 2.8 KB per turn for this workload
  (items, events, turn rows, and tool intents), 25 MiB for five minutes at
  30 turns per second. Retention is still unimplemented and this is the
  number it will have to bound.

Not established: rate limits at higher token rates or on other tiers, hours
of load, long contexts under sustained load (the window was fixed at eight
items), or Anthropic beyond 16 bots. Captures: ignored
`.local/bench/sustained-luna-64/` and `sustained-sonnet-16/`.
