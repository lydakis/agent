# Initial engine screen

Historical screen, measured 2026-09-07. Its recommendation to continue evaluating
Pi for reuse was superseded by George's explicit custom-Rust project decision.
See the [new comparison](RUST_MEASUREMENTS.md). Pi had lower sampled resource cost at 8 and 32 concurrent
conversations in this experiment. Codex had lower observed CPU cost at one
conversation and became ready sooner. Neither has passed the full product
contract, and this is not a decision to adopt Node.js for Agent.

## Conditions and provenance

- Pi: published `@earendil-works/pi-agent-core` and `pi-ai` **0.85.1**, pnpm lock
  SHA-256 `10119a59376505b2db1b15b635014312c963307fd894c04997f7ae7a9e5d23b8`.
- Codex: **0.153.1**, native binary resolved from its npm distribution. The
  version and binary hash are recorded in each result's `target_metadata`.
- Both adapters: Node **22.22.3**. Observer: Python **3.12.9**, psutil **7.2.2**.
- Host: Darwin arm64, 10 logical CPUs, 32 GiB RAM, external power. These are
  local measurements; Linux and remote-host costs have not been established.
- Three measured fresh-process runs plus one excluded run for each of six cases
  per engine. Targets ran sequentially; order alternated across cases.
- Three turns per agent; each adds 4 or 64 KiB of user text and receives 5 KiB
  of text in twenty deltas spaced 25 ms apart. The entire preceding synthetic
  conversation is validated at the provider on every request.
- Both use ephemeral state. Pi declares no tools; Codex retains native harness
  machinery under the adapter's reduced configuration. No tools were executed.
  See [the exact setup and limitations](BENCHMARKS.md#real-engine-adapters).
- Sampling: 100 ms counters, 500 ms tree discovery. Guards: 512 MiB per tree,
  16 processes, 30 seconds per run. These are screening bounds, not final
  product acceptance targets.

All **48 runs passed**, including 12 excluded runs. Every measured run reached
its declared provider concurrency, with no invalid or aborted requests. No run
triggered the sampling-overhead warning. The provider's CPU remains reported
separately and is not subtracted from target cost.

The benchmark source fingerprint is
`68bb6a18f62846d1b7c5d6d21e8f14e1e3a1485b64655c9980ba7f71060c8957`.
Private raw captures and per-case comparisons are under ignored
`.local/bench/engines-matrix-final/`. This document retains only synthetic aggregates.

## Resource results

Values are medians across three measured runs. CPU is sampled cumulative
user+system time across the target tree, including startup. RSS includes the
Node adapter: Pi used one process; Codex used the adapter plus one shared native
app-server. RSS is a sampled sum, not private memory or per-agent allocation.

| Active streams | New user text/turn | Pi peak RSS, MiB | Codex peak RSS, MiB | Pi observed CPU, s | Codex observed CPU, s |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 4 KiB | 106.9 | 119.0 | 0.525 | 0.208 |
| 1 | 64 KiB | 107.4 | 121.8 | 0.448 | 0.220 |
| 8 | 4 KiB | 119.3 | 145.8 | 0.502 | 0.836 |
| 8 | 64 KiB | 127.4 | 160.0 | 0.500 | 0.845 |
| 32 | 4 KiB | 139.4 | 190.2 | 0.638 | 2.743 |
| 32 | 64 KiB | 166.5 | 227.6 | 0.736 | 3.042 |

At 32 streams with 64 KiB input, Pi's sampled RSS ranged **166.3–168.5 MiB**;
Codex's ranged **226.5–234.5 MiB**. Observed CPU ranges were **0.734–0.736 s**
and **3.003–3.053 s** respectively. These ranges describe three runs, not
confidence intervals. Small differences in the table should not be interpreted
as stable effects without longer, repeated measurements.

## Network and latency at 32 streams

These cases each complete 96 turns. HTTP body bytes exclude headers, HTTP
chunk framing, TCP/TLS overhead and unrelated networking. Finalized SSE events
repeat the text, so response body size is larger than generated output.

| New user text/turn | Engine | Request body bytes | Response body bytes | Connections, median (range) | Median of per-run turn p99, ms |
| ---: | --- | ---: | ---: | ---: | ---: |
| 4 KiB | Pi | 1,322,598 | 3,017,112 | 53 (49–53) | 590 |
| 4 KiB | Codex | 1,977,094 | 3,017,112 | 96 (96–96) | 840 |
| 64 KiB | Pi | 13,119,078 | 3,017,112 | 50 (50–52) | 606 |
| 64 KiB | Codex | 13,770,726 | 3,017,112 | 96 (96–96) | 752 |

Codex request body size includes its native context and schema overhead; the
4 KiB case varied slightly across runs. Both received exactly the same generated
text, **491,520 bytes** per case/run. Pi reused connections; this Codex setup
used one connection per request. This identifies a transport behavior worth
profiling, not proof about production connection reuse or provider billing.

Ready time at 32 streams with 64 KiB input was approximately **222 ms for Pi**
and **163 ms for Codex**. The injected stream delay dominates turn duration.
Observed p99 includes adapter/event delivery and scheduling; 96 turns per run is
a small tail sample. Neither runtime is a universal winner across these metrics.

## Decision and next experiment

Keep Pi as the leading component-reuse candidate for the next narrow experiment,
and Codex as the existing shared-server baseline. There is no demonstrated need
yet to maintain a new provider/model loop or a Codex fork.

Next test the actual lifecycle contract: named Bob, exact reload/resumption,
forking a historical checkpoint into an independent branch, a deterministic tool
call, cancellation, and bounded event replay with a slow reader. Compare equivalent
durability and history retention. Pi's bare `Agent` wrapper alone has not established
those behaviors; evaluate its existing session machinery before adding storage.

Only after a missing behavior or a measured hotspot survives that reuse check
should a narrow Rust prototype compete on the same workload. This screen says
nothing about thousands of agents, long-lived memory growth, fork sharing,
multiple real providers, paid-model latency, or model quality.
