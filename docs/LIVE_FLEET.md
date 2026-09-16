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
  30 turns per second. This run predates retention and supplies the storage
  baseline for the following screen.

Not established: rate limits at higher token rates or on other tiers, hours
of load, long contexts under sustained load (the window was fixed at eight
items), or Anthropic beyond 16 bots. Captures: ignored
`.local/bench/sustained-luna-64/` and `sustained-sonnet-16/`.

## Retention under sustained load

Observed 2026-09-16 America/New_York, same host and driver, binary with the
retention primitives, `bench.sustained --retain-turns 8`: 64 bots on
gpt-5.6-luna for two minutes, each bot pruned to its newest eight turns'
records after every turn.

| Run | Turns | Failed | Steady turns/s | p50 ms | Events rows at end | Main store file | Growth per turn |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| sustained-luna-64 (no retention, above) | 9,050 | 4 | 30 | 1,907–2,014 | 73,270 | 21.3 MiB | 2.8 KB |
| sustained-luna-64-retain | 3,914 | 4 | 31–33 | 1,777–1,877 | 4,212 | 5.6 MiB | 1.4 KB |

With the policy on, the events table stops growing at 64 bots times eight
turns, freed pages are reused, and what remains per turn is the transcript
(about 870 bytes of items) plus turn and checkpoint rows. The runs have different
durations and provider timings, so their throughput difference does not establish
retention's performance effect. The remaining growth is the model's
own history, which compaction has to address; a bot that is finished is
freed entirely with `delete`. One provider call took 39.6 s at the very end,
which is why the run's last window is a single turn. Captures: ignored
`.local/bench/sustained-luna-64-retain/`.

## Retention correctness and synthetic overhead

Observed 2026-09-16 on Darwin arm64, release builds with Rust 1.98.0. Compared
the initial retention binary `670048e1…` with the corrected binary `95656e92…`.
The corrections preserve running background commands, allocate non-reusable
turn IDs, report expired outcomes explicitly, and retire completed tasks before
publishing their live terminal events. They also index the retention boundary,
search retained records instead of all historical turns, and combine finish,
prune, and outcome capture into one database-worker job.

Matched workload: one bot over stdio, a loopback synthetic Responses provider,
one `echo` call and two model requests per turn, eight context items, eight
retained turns, SQLite FULL durability. Stores were seeded with 100 or 100,000
complete four-item turns; the larger histories were populated offline from
the same synthetic turn template. Both candidates opened and closed their
store once before measurement, excluding migration from the steady-state
process. Each run then warmed up for 16 turns and measured 400 turns. Three
paired runs alternated candidate order. No tests or builds ran concurrently.

Values below are medians of the three runs; RSS is the daemon alone, sampled
every 10 ms, and excludes the controller and synthetic provider. Latency spans
submit through receipt of `turn_finished`; CPU is daemon user plus system time.

| Stored turns | Binary | Median turn ms | p95 turn ms | CPU ms/turn | Peak sampled RSS MiB |
| ---: | --- | ---: | ---: | ---: | ---: |
| 100 | Before | 2.96 | 3.85 | 2.40 | 11.91 |
| 100 | Corrected | 2.96 | 6.31 | 2.34 | 12.09 |
| 100,000 | Before | 53.17 | 57.17 | 52.20 | 15.00 |
| 100,000 | Corrected | 2.50 | 3.24 | 1.99 | 14.06 |

All measured turns completed. Each run made exactly 800 model requests;
canonical request payload bytes matched between candidates at each history
size: 1,441,330 and 1,448,000 respectively. SQL plans confirm the old retention
boundary sorted historical turns, and its record deletions built historical
turn lists. The corrected queries use the boundary index and retained records.

The long-history improvement is clear on this workload. Short-history median
latency is unchanged, while tail results are noisy: the first paired screen
had p95 3.16 → 3.18 ms; the reopened screen above had 3.85 → 6.31 ms. This does
not establish a short-history tail improvement or unchanged fleet-wide tails.
A focused follow-up of seven alternating pairs at 100 stored turns produced
median p50 2.69 → 2.63 ms, p95 3.32 → 3.22 ms, and CPU 2.18 → 2.04 ms/turn.
Thus the earlier short-history p95 increase did not persist in the larger check.
Short-history RSS increased by about 0.1–0.2 MiB. In the process that performed the
100,000-turn migration, measured-turn RSS was 16.06 MiB versus 14.95 before;
reopening the migrated store produced the lower steady-state value above.
Startup latency, migration peak, many-bot contention, shell-process memory,
and real-provider performance were not measured by this screen.

Driver, seed stores, exact binary hashes, SQL plans, and captures remain in ignored
`.local/retention-fixes/measure.py`, `query-plans.json`, `results.json`,
`reopened/results.json`, and `short-tail-check/results.json`.

### Checkpoint identity and retention follow-up

Observed 2026-09-16, same synthetic contract and host as above. Compared
`95656e92…` with `bc2478aa…`, which prevents checkpoint ID reuse, makes CLI
retries fail explicitly when their result expired, and applies retention to
parked-turn interruption. Node allocation uses the highest surviving ID and
a floor saved atomically with deletion; it adds no per-message counter write.
Node metadata reads and node, turn, and event insertions reuse prepared SQL
statements. Normal CLI runs require no new RPC; a pruning notice triggers
reconciliation of the selected turn.

Seven alternating pairs at each history size, each with 16 warm-up turns and
400 measured turns after opening and closing the store once. The controller,
provider, context, retention, durability, and measurement boundaries match the
preceding screen. Tests and builds did not run concurrently. All 11,200 measured
turns completed; request counts and canonical payload bytes matched each pair.

| Stored turns | Binary | Median turn ms | p95 turn ms | CPU ms/turn | Peak sampled RSS MiB |
| ---: | --- | ---: | ---: | ---: | ---: |
| 100 | Before | 2.51 | 3.04 | 2.00 | 12.08 |
| 100 | Corrected | 2.57 | 3.19 | 2.03 | 12.08 |
| 100,000 | Before | 2.56 | 3.28 | 2.06 | 14.02 |
| 100,000 | Corrected | 2.52 | 3.23 | 2.01 | 14.05 |

These remain close to baseline: CPU changed +1.3% at 100 stored turns and
−2.6% at 100,000, with effectively unchanged sampled memory. The short-history
p95 increased by 0.15 ms. This is not evidence of an overall speedup; the large
long-history improvement from the earlier retention-query fix is preserved.
Startup, deletion latency, migration peak, and fleet-wide contention remain
outside this screen. Earlier candidates that wrote the node counter on every
message are retained as exploratory captures, not included in this table.

Ignored evidence: `.local/retention-followup/measure-final.py`,
`final/results.json`, and `measure-final.log`, with earlier candidates in
`reopened/results.json` and `cached/results.json`.

### Ordered completion and graceful shutdown

Observed 2026-09-16, Darwin arm64, Rust 1.98.0. Compared `bc2478aa…`
with `445f9af9…`. Completion now commits and publishes in the service loop,
before another submission can overtake its terminal event. Shutdown uses the
same path and drains socket writers concurrently under one five-second
deadline. Moving the bot name through the completion job removes one string
allocation per finished turn; the number of database jobs is unchanged.

The single-bot screen uses the preceding matched contract: seven alternating
pairs, 16 warm-up and 400 measured turns per run, eight context items and
eight retained turns, reopened stores with 100 or 100,000 complete turns.
The socket screen uses 32 bots, one warm-up turn each, then ten batches of
32 turns per run, nine alternating pairs, default context limits and no
retention. Both use two synthetic provider requests and one echo call per
turn, SQLite FULL durability, and daemon-only CPU/RSS. The socket screen
checks exact live/replay equality; it does not establish 32 simultaneous
provider streams. No builds or tests ran alongside measurements.

Medians across runs, before → after:

| Workload | p50 ms | p95 ms | CPU ms/turn | Peak sampled RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| 100 stored turns | 2.57 → 2.61 | 3.28 → 4.44 | 2.04 → 2.08 | 12.09 → 12.13 |
| 100,000 stored turns | 2.64 → 2.47 | 3.46 → 3.09 | 2.05 → 1.96 | 14.08 → 14.09 |
| 32 bots over sockets | 33.34 → 31.63 | 44.52 → 42.98 | 2.38 → 2.42 | 14.33 → 14.31 |

The short-history tail increase triggered a separate seven-pair recheck on
the same final binary and contract: p50 2.77 → 2.60 ms, p95 3.45 → 3.18 ms,
CPU 2.21 → 2.07 ms/turn, and RSS 12.11 → 12.08 MiB. The increase did not
repeat. These screens show essentially stable sampled memory and small,
mixed CPU changes, not an established overall speedup or a consistent
slowdown. Long-history CPU fell 4.4%; socket-fleet CPU rose 1.5%.

All 22,560 measured turns completed across the final screens and recheck;
request counts and canonical payload bytes matched within each workload.
Shutdown delivery is covered by behavioral tests, not included in these
steady-state timings. Startup, migration, real providers, and larger fleets
remain outside the measurement boundary.

Ignored captures: `.local/completion-fixes/measure.py`, `allocation/results.json`,
`short-tail-check/results.json`, `fleet.py`, and `fleet-allocation-results.json`.
Earlier candidates remain in `final/` and `verified/` as exploratory captures.

### Retention scoped to one bot

Observed 2026-09-16, Darwin arm64, Rust 1.98.0. Compared `445f9af9…`
with `6e7707f4…`. Schema 11 indexes retention candidates in a separate
`retained_turns` table, so a prune neither scans unrelated bots' operational
records nor rewrites durable turn rows. Migration derives candidates from
surviving records. Running background commands keep their candidate until a
later prune can remove their completed results.

Matched isolation probe: Alice owns 100, 10,000, or 100,000 retained tool rows;
Bob has two turn rows and nothing to delete. Each candidate receives the same
seed store. Migration and one initial prune run before closing and reopening
the store. Five alternating pairs then measure 50 `prune Bob keep_turns=1`
RPCs per run. Alice's tool rows are checked afterward. No provider calls are
made; latency includes the stdio round trip. Medians across runs:

| Alice's retained tool rows | Prune ms, before → after | Daemon CPU ms/prune, before → after |
| ---: | ---: | ---: |
| 100 | 0.084 → 0.061 | 0.077 → 0.065 |
| 10,000 | 1.909 → 0.064 | 1.844 → 0.067 |
| 100,000 | 23.491 → 0.077 | 23.267 → 0.147 |

At 100,000 unrelated records this operation is about 300 times faster. The
record deletion now looks up candidate turns using `(bot,turn)`, then seeks
their records by turn ID. This ratio describes repeated pruning with nothing
to delete, not whole-harness throughput. Sampled daemon RSS at that size was
12.53 → 12.61 MiB.

The ordinary-turn screen repeats the preceding single-bot synthetic contract:
seven alternating pairs per history size, 16 warm-up and 400 measured turns,
one echo and two requests per turn, eight context items and eight retained
turns, FULL durability. All 11,200 measured turns completed, and request counts
and canonical bytes matched. Median results, before → after:

| Stored turns | p50 ms | p95 ms | CPU ms/turn | Peak sampled RSS MiB |
| ---: | ---: | ---: | ---: | ---: |
| 100 | 2.649 → 2.688 | 3.296 → 3.300 | 2.099 → 2.126 | 12.19 → 12.20 |
| 100,000 | 2.604 → 2.553 | 3.280 → 3.277 | 2.052 → 2.041 | 14.14 → 14.11 |

Ordinary-turn overhead stays close to baseline: CPU +1.3% at 100 turns and
−0.6% at 100,000, with essentially unchanged memory and p95. Tests and builds
did not run concurrently. Startup, migration peak, first-time pruning of a
large retained history, and large live fleets remain outside these timings.

Ignored evidence: `.local/retention-owner-fix/measure_scope.py`,
`scope-final-results.json`, `measure.py`, `final/results.json`, and `summary.json`.
The earlier turn-row-flag candidate in `steady/` increased long-history CPU
and RSS and was discarded; it is not part of the final implementation.

### Deleted-bot wake-ups

Observed 2026-09-16, Darwin arm64, Rust 1.98.0. Compared `6e7707f4…`
with `068225fe…`. A queued wake-up now checks only the bot name, parked
status, and turn ID through a cached indexed query. It skips deleted or
replaced turns, propagates database errors, and avoids loading the full bot
record or cloning the bot name. No additional database job is needed.

Matched synthetic screen: one bot over stdio, two Responses requests per
turn, eight context items, eight retained turns, SQLite FULL durability,
16 warm-up turns per run. Ordinary turns call `echo`; the resumption workload
calls `wait` on an unknown process handle, parks and resumes immediately,
and uses 64 KiB of synthetic instructions to exercise the previous metadata
copy. CPU and RSS cover only the daemon; RSS is sampled every 10 ms. Latency
spans submit through receipt of completion. Builds and tests ran beforehand.

Five alternating pairs of 300 measured turns per candidate and workload:

| Workload | p50 ms, before → after | p95 ms | CPU ms/turn | Peak sampled RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| Ordinary echo | 2.789 → 2.697 | 3.458 → 3.297 | 2.155 → 2.078 | 12.11 → 12.05 |
| Wait/resume | 3.947 → 4.133 | 4.601 → 5.171 | 2.992 → 3.170 | 12.80 → 12.91 |

The initial wait/resume increase prompted nine more alternating pairs of
500 turns on the same binaries and contract: p50 3.915 → 3.887 ms,
p95 4.600 → 4.754 ms, CPU 3.0461 → 3.0468 ms/turn, and RSS
13.08 → 13.14 MiB. The CPU increase did not persist; the recheck's p95
remained 0.15 ms higher. These results support roughly stable overhead on
these workloads, not an overall speedup or unchanged fleet-wide tails.
All 15,000 measured turns completed, with identical canonical provider payload
hashes within each pair. Startup, long histories, large fleets, and real
providers were not measured. Ignored evidence:
`.local/deleted-wakeup-fix/measure.py`, `results.json`, `summary.json`, and
`recheck/`.
