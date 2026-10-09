# Rust lifecycle, feature costs, and regression checks

Measured 2026-09-07 evening America/New_York (capture timestamps are UTC).
The sections up to the post-review checkpoint are **Rust-only** results. The
[Pi Durable baseline](#pi-durable-baseline) of 2026-10-02 is the first other
engine on this workload; it is an exploratory screen, not a ranking. The
earlier cross-engine streaming figures remain
[exploratory observations](COMPARISON_CONTRACT.md), not harness-efficiency rankings.

## Durable service and tools

Every case creates 32 bots in separate synthetic workspaces and runs three turns
per bot, adding 4 KiB of user text and receiving twenty 256-byte text chunks spaced
25 ms apart per turn. All prior messages and tool results are checked by a separate
provider. Afterward the service is killed, reopened, and checked for exact resume,
identical replay, item retrieval, duplicate-submit suppression, and a fork from
each bot's first completed checkpoint. Forked workspaces must have no tool artifact.

Echo and shell cases add one tool round trip per turn: 192 provider requests and
96 tool results, versus 96 provider requests in text-only cases. Shell commands
write an artifact, hold a child for 250 ms to make resource sampling useful, and
return known stdout. At most 16 commands run simultaneously. The shell workload
therefore includes actual shell and sleep processes and additional waiting/work.
It is not a general coding-task workload.

Medians of three measured fresh-process runs, after one excluded warmup:

| Case | Target-tree sampled peak RSS, MiB (range) | Observed CPU seconds | Peak target processes | Peak provider requests |
| --- | ---: | ---: | ---: | ---: |
| Text, echo registered | 15.00 (14.97–15.02) | 0.166 | 1 | 32 |
| Text, echo + shell registered | 15.09 (15.06–15.16) | 0.170 | 1 | 32 |
| Echo tool round trip | 15.23 (15.09–15.33) | 0.192 | 1 | 32 |
| Shell tool round trip | 69.67 (69.58–69.83) | 0.968 | 33 | 32 |

All 20 runs (5 cases including the preserved-binary regression below) completed
successfully. All measured cases achieved 32 provider requests and had no quality
warnings at the final sampling settings. Shell execution reached 33 charged
processes: the native service plus up to 16 shell/sleep pairs. Its retained idle
RSS after tools completed was 15.14 MiB.
Do not report only that idle figure as shell-workload peak memory.

Enabling the shell registry without exercising it stayed close to the text/echo
configuration in these samples. Executing shell adds child processes, I/O, tool
messages, durable events, and an extra model call. The feature ladder measures
that real workload cost; it does not attribute every extra byte to one feature.

## Matched regression checks

A preserved release binary from before this change and the final binary ran with
the **same echo registry, echo workload, durable contract, and observer**. Median
sampled peak RSS was 15.45 MiB before and
15.23 MiB after. Observed CPU was
0.191 seconds before and
0.192 after, with overlapping measured ranges.
This does not establish a speedup; it shows no large resource increase in this
bounded regression workload.

The matched ephemeral streaming regression used the same 32-agent/64 KiB case
as the earlier screen, with both binaries freshly rerun under the new observer.
Median sampled peak RSS was 19.56 MiB before and
19.55 MiB after.
Observed CPU ranges overlapped; request/response body bytes were identical.
First-chunk p99 medians were 29.6 ms before and
31.9 ms after, also with overlapping ranges.
The comparison tool classified this as a matched-configuration regression.
It is not a comparison to the durable cases above.

## The 1,000-stream failure and admission policy

The old failure reproduced before any fixture request was accepted. The new
sanitized diagnostic was `provider_connection_os_54`, macOS `ECONNRESET`.
Live read-only checks found `kern.ipc.somaxconn=128`; Python's server default
backlog is 100. A burst overflowing the fixture's listen queue is the leading
explanation, not a proven kernel-level attribution. No host setting was changed.

The shared provider now admits at most 64 requests awaiting response headers.
It releases each permit before reading SSE, and allows up to 60 seconds waiting
for admission. A boundary test holds headers back and confirms only 64 requests
arrive, then requires 70 established responses before any body can finish.
This bounds request startup rather than capping established streams at 64.
It can limit real-provider admission when headers arrive late; that tradeoff
requires live-provider evaluation. No retry was added to turn failures into passes.

The existing 1,000-agent, 4 KiB/turn, 100 ms/chunk workload then passed one warmup
and **five measured runs**, each with 1,000 concurrent provider requests and all
3,000 turns completed. Median sampled peak RSS was
129.25 MiB (range 129.03–130.12 MiB).
Measured quality warnings: 0 runs.
This is still an ephemeral text-core test. It establishes neither 1,000 durable
shell agents nor long-lived or live-provider capacity. Original failures remain
preserved, and five passing repeats cannot prove universal reliability.

## Provenance and measurement limits

- Final binary SHA-256: `d5aeaa2cde0f27b6dddde4633d6ba9adf743964a30f4c41aa6e997c7697edfe5`.
- Preserved baseline SHA-256: `c407391e4596737e6b741db7213a57716b97b01d695b8910f59bd5e9b89919e7`.
- Lifecycle observer SHA-256: `ef36b3af9df197838ffa62040a076bc1087a59547d1a8fda2e5c68724f254448`.
- Streaming observer SHA-256: `84cd24677ccb8f1950ada33ed889af336207895e6a81fd4e2ab21900fd3c12c1`.
- Rust 1.92.0, Python 3.12.9, psutil 7.2.2; Darwin arm64, 10 logical CPUs,
  32 GiB RAM, external power. Sequential runs without a concurrent profiler/build.
- Lifecycle: 200 ms samples including recursive child discovery, wider group
  scan every 500 ms, 450 ms idle observations; 30-second run limit, 512 MiB
  sampled RSS guard separately for target/provider, 48-process target guard.
- Streaming: 100 ms samples, 500 ms tree discovery; 30 seconds, 512 MiB per tree,
  16-process guard. One excluded warmup per case; three measured repetitions for
  regression/feature cases and five for the final 1,000-stream screen.
- Raw captures: ignored `.local/bench/feature-ladder-final/`,
  `.local/bench/matched-rust-final/`, `.local/bench/rust-scale-bounded-final/`.
  Earlier failed/noisy captures remain separate and retained.

The lifecycle observer starts sampling after service readiness. Earlier startup
peaks and short-lived children may be missed. Observed CPU is a lower bound,
not exact process-tree CPU; RSS is summed sampled RSS, not private memory/PSS.
The Python controller/observer and synthetic provider are measured separately
and excluded from target costs. The first 50 ms lifecycle screen triggered
observer-cost warnings; final 200 ms results trade resolution for lower overhead
and are not silently combined with that earlier screen.

No live API, TLS performance, provider usage accounting, long-history storage,
compaction, arbitrary coding workload, slow-reader performance, or power-loss
recovery was validated. SQLite FULL is the declared persistence setting; it is
not an assertion that every backend offers the same durability guarantee.
Follow [LONG_HISTORY.md](LONG_HISTORY.md) for the next storage/context milestone.

## Post-review checkpoint validation

After fixing store aliases, replay response bounds, saturated duplicate admission,
and provider-credential handling for tools, the durable shell case was repeated
with the same workload, observer hash, host/power metadata, and sampling settings.
One warmup and three measured runs passed. Every run completed 96 turns, achieved
32 concurrent provider requests, and charged up to 33 target-tree processes.
No measured run produced a quality warning.

| Metric | Earlier shell baseline | Post-review checkpoint |
| --- | ---: | ---: |
| Median sampled peak RSS, MiB (range) | 69.67 (69.58–69.83) | 69.58 (69.52–69.61) |
| Median observed CPU seconds (range) | 0.968 (0.963–0.971) | 0.939 (0.936–0.951) |
| Resume/replay/item/duplicate/fork phase, median ms (range) | 50.0 (48.8–64.6) | 64.6 (50.8–74.6) |

This remains roughly the same resource scale on this bounded workload. The earlier
baseline was not rerun or interleaved with these samples; do not attribute the
timing differences to the fixes or claim a speedup. Large replay pages, saturated
admission, aliases, and fake-credential handling have separate behavior tests;
this shell benchmark does not exercise those exceptional paths or real auth.
The earlier 1,000-stream screen was not repeated for this checkpoint.

- Checkpoint binary SHA-256: `1b31f7ac13154281ab772946729bc0586cee12f41ca0ccb33cbc96399d72a450`.
- Captures: ignored `.local/bench/review-fixes-shell/`; the baseline above remains
  in `.local/bench/feature-ladder-final/shell/`.
- Checkpoint validation: 16 Rust tests, 34 Python tests including real-process
  synthetic engine checks, formatting, strict Clippy, and a release build passed.

## Pi Durable baseline

Measured 2026-10-02 UTC (captures 06:06–06:09). **Exploratory, single host,
no ranking.** Agent and Pi Durable 1.0.0 ran the durable lifecycle workload
above through the same observer and provider; the
[adapter](BENCHMARKS.md#pi-durable-adapter) and the
[contract notes](COMPARISON_CONTRACT.md#pi-durable-lifecycle-comparison)
describe how. Each case: 32 bots in separate workspaces, three turns per bot
with 4 KiB of new user text and twenty 256-byte deltas 25 ms apart, full
history validated on every request, then kill, reopen, resume, re-read,
per-item reads, duplicate submission, and a fork at each bot's first answer.
One excluded warmup and three measured fresh-process runs; medians with
ranges.

Host: a shared 4-vCPU Intel Xeon (2.1 GHz) Linux 6.18 cloud container with
15 GiB RAM, not otherwise controlled; numbers from other hosts in these docs
are not comparable. Agent `agent-runtime 0.1.3` at `0a6f2b2`, release binary
`cbc4bdbb…`, rustc 1.98.0, bundled SQLite from `libsqlite3-sys` 0.38.2. Pi
Durable 1.0.0 (source `a13d35a7`), pi-ai 1.0.0, chord 1.0.0, Node v22.22.0
with its SQLite 3.50.4, lockfile `c1ae3871…`. Python 3.11.15, psutil 7.2.2.
Observer fingerprint `cf1c3756923e…`; sampling, guards and timeouts as in the
Rust lifecycle screen (200 ms samples, 512 MiB, 30 s, 48 or 65 processes).

Peak memory is summed sampled RSS of the charged tree: Agent's daemon and its
tool children; Node with Pi Durable and its tool children.

| Case | Engine, durability | Peak RSS, MiB | CPU s | Turn p50 / p99, ms | Restart to ready, ms | Resume, re-read, items, duplicate, fork (all 32), ms |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| Text | Agent, FULL | 21.0 (21.0–21.1) | 0.82 (0.80–0.82) | 521 / 528 | 210 (205–226) | 133 (115–139) |
| Text | Pi Durable, FULL | 138.0 (137.4–139.1) | 2.38 (2.38–2.49) | 525 / 651 | 243 (237–250) | 203 (166–216) |
| Echo round trip | Agent, FULL | 21.3 (21.3–21.4) | 0.95 (0.95–0.99) | 576 / 585 | 226 (206–230) | 170 (164–189) |
| Echo round trip | Pi Durable, FULL | 149.0 (148.3–150.2) | 3.51 (3.39–3.52) | 635 / 836 | 246 (238–264) | 240 (228–272) |
| Echo round trip | Pi Durable, NORMAL (package default) | 150.0 (148.9–152.6) | 3.12 (3.06–3.29) | 628 / 800 | 226 (224–251) | 198 (195–218) |
| Shell round trip | Agent, FULL | 134.3 (134.2–134.3) | 0.97 (0.97–1.00) | 831 / 856 | 255 (202–255) | 170 (161–178) |
| Shell round trip | Pi Durable, FULL | 253.2 (253.1–289.8) | 3.86 (3.86–3.96) | 923 / 1,070 | 254 (249–255) | 238 (220–255) |

Turn latencies are medians of each run's p50 and p99; the scripted stream
alone takes 500 ms, and a tool round trip adds the provider's 50 ms before
the call plus the tool's own time (250 ms of sleep in shell mode). Every
measured run completed all 96 turns with 32 of 32 provider requests in
flight, no invalid provider request, every tool result, and no quality
warning. Peak processes: Agent 1, 1, and 65 (shell); Pi Durable 1, 1, and
45–62 (shell). Peak threads: Agent 5 without tools running, Pi Durable 11.
Idle RSS after the turns equalled the peak except in shell mode (Agent 21.6,
Pi Durable 153.5 MiB). Retained store files at the end, WAL included, were
about 5.2–5.6 MiB for both; Agent's ranged from 1.3 MiB because its WAL size
at the kill varied.

What this shows, and what it does not:

- On this host, for this bounded workload, Pi Durable's Node process used
  about 117–128 MiB more sampled RSS than Agent's daemon before any tool
  process, and 2.9 to 4.0 times its observed CPU. Its turn p99 was 120–250 ms
  longer at 32 concurrent conversations; p50 differed by 4 to 92 ms.
- The two engines did not do the same internal work. Pi Durable commits
  partial answers every 100 ms and serializes every conversation's commits
  on one line, one transaction each; Agent writes completed items. With one
  bot and the same streaming, strace counted 9.0 syncs per turn for Pi
  Durable at FULL and 3.1 for Agent ([adapter notes](BENCHMARKS.md#pi-durable-adapter)).
  Its events arrive per commit and its "replay" is a transcript re-read, a
  smaller result than Agent's event-log replay. These differences are not
  attributed to CPU or memory here; no profile was taken.
- FULL against NORMAL inside Pi Durable: CPU 3.51 against 3.12 s and create
  59 against 36 ms on this host's virtual disk. That is one bounded
  observation of the sync cost on this storage, not a general figure.
- Nothing here measures crash during a turn, cancellation, slow consumers,
  retention, long histories, compaction, more than 32 conversations, or power
  loss. Node's heap is mostly runtime and package baseline at this scale;
  per-conversation growth was not separated.

Commands, run sequentially from the repository root after
`pnpm --dir bench/adapters install --frozen-lockfile --ignore-scripts` and
`CARGO_TARGET_DIR=.local/target cargo build --release --locked`:

```sh
.local/venv/bin/python -m bench.lifecycle --engine rust --agents 32 --mode text --tools echo --out .local/bench/pi-durable-2026-10-02/rust-text
.local/venv/bin/python -m bench.lifecycle --engine pi-durable --agents 32 --mode text --tools echo --out .local/bench/pi-durable-2026-10-02/pi-full-text
.local/venv/bin/python -m bench.lifecycle --engine pi-durable --agents 32 --mode echo --tools echo --out .local/bench/pi-durable-2026-10-02/pi-full-echo
.local/venv/bin/python -m bench.lifecycle --engine rust --agents 32 --mode echo --tools echo --out .local/bench/pi-durable-2026-10-02/rust-echo
.local/venv/bin/python -m bench.lifecycle --engine rust --agents 32 --mode shell --tools echo,shell --out .local/bench/pi-durable-2026-10-02/rust-shell
.local/venv/bin/python -m bench.lifecycle --engine pi-durable --agents 32 --mode shell --tools echo,shell --out .local/bench/pi-durable-2026-10-02/pi-full-shell
.local/venv/bin/python -m bench.lifecycle --engine pi-durable --synchronous normal --agents 32 --mode echo --tools echo --out .local/bench/pi-durable-2026-10-02/pi-normal-echo
```

Raw captures are in the ignored `.local/bench/pi-durable-2026-10-02/`. The
v3 echo and shell rows use streamed tool-call items, so they are not the same
workload as the 2026-09-07 Rust rows above (which were also on another host).

### Re-run, 2026-10-09

The same seven commands on another container of the same shape (shared
4-vCPU, 15 GiB, Linux 6.18), Agent at `3ad9e2f` (release binary
`6a50ffab…`), Pi Durable and its lockfile unchanged, captures under the
ignored `.local/bench/pi-durable-2026-10-09/`. Pi Durable captures now
name their contract as a committed-transcript re-read with no event replay.
Every measured run completed all 96 turns with no invalid request and no
quality warning.

| Case | Engine, durability | Peak RSS, MiB | CPU s | Turn p50 / p99, ms | Restart to ready, ms | Resume, re-read, items, duplicate, fork (all 32), ms |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| Text | Agent, FULL | 22.1 (21.9–22.2) | 0.79 (0.77–0.83) | 521 / 526 | 207 (202–215) | 134 (120–136) |
| Text | Pi Durable, FULL | 135.9 (135.5–136.5) | 2.22 (2.18–2.37) | 526 / 632 | 224 (222–224) | 161 (159–164) |
| Echo round trip | Agent, FULL | 22.1 (22.1–22.1) | 0.88 (0.83–0.91) | 573 / 579 | 225 (216–227) | 165 (139–170) |
| Echo round trip | Pi Durable, FULL | 150.3 (147.7–152.8) | 3.26 (3.19–3.26) | 633 / 813 | 222 (219–240) | 200 (180–207) |
| Echo round trip | Pi Durable, NORMAL (package default) | 147.5 (146.8–148.1) | 3.11 (3.11–3.12) | 624 / 793 | 231 (224–237) | 192 (190–204) |
| Shell round trip | Agent, FULL | 135.1 (134.9–135.1) | 0.96 (0.91–1.05) | 830 / 858 | 220 (213–280) | 160 (158–182) |
| Shell round trip | Pi Durable, FULL | 273.5 (258.0–282.5) | 3.62 (3.61–3.67) | 902 / 1,021 | 228 (226–235) | 206 (196–218) |

The conclusions above hold. Agent's daemon peaked about 1 MiB higher than
on 2026-10-02 (main moved from `0a6f2b2` to `b026767` in between, and the
host differs); Pi Durable's shell peak varied most, 253.2 then 273.5 MiB.
The summary change does not run in this workload, which never compacts.
