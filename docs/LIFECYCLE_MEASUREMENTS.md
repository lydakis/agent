# Rust lifecycle, feature costs, and regression checks

Measured 2026-09-07 evening America/New_York (capture timestamps are UTC).
These are **Rust-only** results. Pi/Codex lifecycle adapters with matching
semantics do not exist yet. The earlier cross-engine streaming figures remain
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
