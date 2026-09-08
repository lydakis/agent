# Rust prototype measurements

Measured 2026-09-07 evening America/New_York (captures use 2026-09-08 UTC).
The initial target passed: **19.69 MiB median sampled peak RSS for the shared
Rust process at 32 simultaneous agents**, below the predeclared 40 MiB target.
This is a synthetic ephemeral streaming workload, not durable coding-agent
capacity or a world-fastest claim. **The targets have unequal feature footprints;
this table is exploratory, not an apples-to-apples harness efficiency ranking.**
See [the feature inventory and comparison contract](COMPARISON_CONTRACT.md).

## Fresh three-engine comparison

Each agent completes three turns, adding 64 KiB of user text per turn and
receiving twenty 256-byte text chunks spaced 25 ms apart. The provider validates
all retained user/assistant history. All three engines achieved 32 concurrent
provider requests and completed all 96 turns in every run of this case.

Medians of three measured fresh-process runs, with RSS min/max in parentheses:

| Target | Sampled peak RSS, MiB | Observed CPU seconds | Connections for 96 requests | Turn p99 |
| --- | ---: | ---: | ---: | ---: |
| Rust | 19.69 (19.59–19.83) | 0.072 | 33 | 539 ms |
| Pi | 166.98 (166.19–169.66) | 0.787 | 51 | 622 ms |
| Codex + adapter | 227.62 (226.30–231.19) | 3.154 | 96 | 879 ms |

The observed costs differ substantially, but this experiment cannot separate
implementation efficiency from omitted, disabled, or differently scoped features.
The previously highlighted cross-engine savings percentages are withdrawn as
harness efficiency claims; the raw observations above remain unchanged.
The CPU metric excludes some short-lived/final intervals; it is not exact total
CPU. Turn timing includes the synthetic provider delay and observer scheduling.
Three repeats do not establish statistical confidence.

All 72 runs in the six-case, three-engine matrix completed: 18 excluded warmups
and 54 measured runs. Median sampled peak RSS in MiB:

| Simultaneous agents | New user text per turn | Rust | Pi | Codex + adapter |
| --- | --- | ---: | ---: | ---: |
| 1 | 4 KiB | 7.94 | 107.70 | 118.64 |
| 1 | 64 KiB | 8.31 | 107.19 | 121.42 |
| 8 | 4 KiB | 9.30 | 119.48 | 147.00 |
| 8 | 64 KiB | 11.30 | 126.50 | 163.44 |
| 32 | 4 KiB | 12.39 | 137.64 | 190.89 |
| 32 | 64 KiB | 19.69 | 166.98 | 227.62 |

Every measured run achieved its configured provider concurrency. Two measured
runs had a sampler-cost warning: Rust at 8 agents/4 KiB and Codex at 32 agents/4 KiB.
In each, sampling consumed more than 10% of target wall time. No quality warning
occurred in the 32-agent/64 KiB headline case. These are observed samples, so brief
memory peaks between 100 ms samples can be missed.

## Larger Rust-only screen

A separate workload adds 4 KiB per turn and spaces the same twenty chunks by
100 ms to keep more requests overlapping. It is not directly comparable to the
25 ms matrix. Three measured runs plus one excluded warmup at each level:

| Configured agents | Sampled peak RSS, MiB | Observed CPU seconds | Achieved provider concurrency |
| --- | ---: | ---: | ---: |
| 128 | 25.66 (25.34–25.91) | 0.218 | 128 |
| 512 | 77.22 (76.72–77.50) | 0.789 | 512 |

Both levels completed every turn with no quality warnings. The initial 1,000-agent
series had a successful warmup and measured run (about 141 MiB RSS), then failed
before any request reached the provider. That partial series is retained and
must not be reported as a passing repeated capacity result. A diagnostic rerun
with stderr capture passed its warmup and all three measured runs (median
140.28 MiB RSS, achieved concurrency 1,000, no quality warnings). It emitted no
diagnostic error. At that checkpoint, the intermittent failure remained unexplained.
The [subsequent diagnosis and bounded-startup screen](LIFECYCLE_MEASUREMENTS.md)
record the reset code, admission change, and new repeat results. These historical
captures remain separate; do not select only their successful runs.

## Provenance and limits

- Release Rust 1.92.0, agent-runtime 0.1.0; Tokio 1.53.1, reqwest 0.13.4,
  rusqlite 0.40.2. Exact dependency graph is in Cargo.lock.
- Release binary SHA-256: `c407391e4596737e6b741db7213a57716b97b01d695b8910f59bd5e9b89919e7`.
- Cargo.lock SHA-256: `7ea5a68c49166fd7f5fcbea56d5743a22c2beebad2221a4cd7e05622be475d44`.
- Observer SHA-256: `1c3571bb3a950e8a207a3afa0da9375d3dc01c8ac9636c4979913d0f92e197f1`.
- Pi core/model 0.85.1; Codex 0.153.1; Node 22.22.3. Python 3.12.9,
  psutil 7.2.2. Individual results retain installed version and binary hashes.
- Darwin arm64, 10 logical CPUs, 32 GiB RAM, external power. Runs sequential,
  alternating engine order by case. No performance profiler ran concurrently.
- 100 ms resource samples, 500 ms descendant discovery, 30-second timeout,
  512 MiB sampled RSS guard separately for target/provider, 16-process guard.
- Raw captures stay ignored in `.local/bench/rust-comparison-01/` and
  `.local/bench/rust-scale-01/`. Failed captures are preserved.

The Rust benchmark uses the same history/provider core as the durable service,
but does not open SQLite or dispatch tools. All targets retain conversations
in ephemeral state. Codex includes its native harness and Node adapter; Pi and
Rust each use one target process. The fixture provider and Python observer are
measured separately and excluded from target RSS/CPU. RSS is neither private
memory nor PSS. Network counters are application body bytes, not wire bytes. At 32 agents/64 KiB,
request bodies totaled 13,117,638 bytes for Rust, 13,119,078 for Pi, and 13,770,150
for Codex. Connection reuse reduces connection churn, not the history the provider
requires on each request. Median fixture-provider RSS was 48.6 MiB for Rust,
50.7 MiB for Pi, and 48.6 MiB for Codex; observer RSS was about 27–29 MiB.

These localhost HTTP/1.1 results do not establish TLS cost, live-provider
compatibility, shell-tool cost, durability throughput, long-lived retention,
or behavior under slow consumers. The [prototype](RUST_PROTOTYPE.md) specifies
those implementation boundaries. Next measure durable lifecycle costs and
profile growing histories and backpressure under sustained load.
