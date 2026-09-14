# FX native embedded core versus Rust

Observed 2026-09-12. This screen compares ephemeral text conversations through
the real Rust and FX model loops against synthetic loopback inference. It does
not compare coding quality, durable agents, live model providers, or the FX CLI.
No model credentials or paid inference are used.

## Targets and provenance

- FX: npm `libfx@0.0.8`, installed with lifecycle scripts disabled and package
  integrity pinned in `bench/adapters/pnpm-lock.yaml`. Native backend is required;
  WebAssembly fallback is disabled. The target includes Node, the native addon,
  its ACP bridge, and host fetch. This is the researched release, not a claim to
  test the latest FX release.
- The upstream v0.0.8 tag resolves to
  `43c11dcc34a94a76df870af70bdb824579bf18a0`. That source reference is separate
  from the actual npm native-addon hash recorded with each run.
- Rust: this repository's release build using `cargo build --release --locked`,
  invoked in benchmark mode. SQLite and service persistence are bypassed.
  The executable hash and Cargo.lock hash are recorded with each run.
- Both targets run on the same host, sequentially. The provider and Python
  observer run as separate processes outside the target tree. The source
  fingerprint, runtime versions, machine characteristics, and sampler settings
  are recorded in every result.

## Workload and limits

Use `python -m bench.matrix --engines rust fx --out .local/bench/fx-matrix` after
installing the locked benchmark dependencies and building Rust. See
[benchmark setup](BENCHMARKS.md#fx-native-embedded-core).

The matrix has six cases: 1, 8, or 32 simultaneous agents, each with either
4 KiB or 64 KiB of new user text per turn. Each agent completes three turns;
each response has twenty 256-byte text deltas, delayed 25 ms each. Prior user
and assistant text is validated on every request. Histories grow within a run.
These are short conversations, not the proposed enormous-history workload.

Each engine/case has one excluded warmup and three measured fresh processes.
Engine order alternates across cases. Limits are 30 seconds, 512 MiB sampled RSS
per target/provider tree, and 16 processes per tree. Samples are 100 ms apart;
process discovery runs every 500 ms. A failed run stops the matrix and remains
in the capture. Sampling can miss transient peaks and CPU work after the last
live observation. Thread counts are sampled OS threads, not stack allocations.

## Comparability

Both cores have empty tool lists, the same short system instruction, and the
same validated conversation and generated text. FX uses Gateway SSE, whereas
Rust uses Responses SSE. Request serialization, stream envelopes, repeated
terminal text, metadata discovery, and native implementation features differ.
FX's catalog traffic is counted, including its response-body bytes and used
connections. Headers, TCP framing, TLS, and actual provider inference are not
measured. Network/body-byte differences cannot be attributed solely to the
harness implementation.

The report permits this protocol difference only under `--exploratory`, names
the gap, and emits no percentage rankings. Host, observer, workload, and sampler
settings must still match. Small timing differences are descriptive; the fixed
synthetic streaming delay dominates turn duration. No run establishes a
production capacity or whole-harness efficiency claim.

## Results

The Linux x86_64 screen completed all 48 runs: 12 excluded warmups and 36 measured
runs. Every run completed all expected turns, retained the required conversation,
and achieved the configured provider concurrency. There were no invalid or
aborted inference requests, leftover descendants, or sampler quality warnings.
Both targets were one process; FX's process includes Node and the native addon.

Host: Linux 7.1.9-arch1-2, 4 logical CPUs, about 15.52 GiB RAM, external power.
FX used Node v24.19.0. The observer used Python 3.14.7 and psutil 7.2.2. Rust
was built with Cargo 1.98.0. No host exclusivity or thermal stability is claimed.

Peak RSS is the median of three measured sampled peaks, with the observed range
in brackets. CPU is the median of observed user plus system CPU seconds for the
whole target process lifetime, including startup and the adapter. It is a lower
bound, not exact execution accounting.

| Agents | New text per turn | Rust peak RSS, MiB | FX + Node peak RSS, MiB | Rust CPU, s | FX + Node CPU, s |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 4 KiB | 6.80 [6.69, 6.82] | 107.91 [107.89, 108.59] | 0.01 | 0.28 |
| 1 | 64 KiB | 7.20 [7.18, 7.24] | 112.40 [112.22, 112.45] | 0.02 | 0.29 |
| 8 | 4 KiB | 7.27 [7.10, 7.27] | 123.02 [122.94, 123.40] | 0.09 | 0.65 |
| 8 | 64 KiB | 8.89 [8.76, 8.93] | 149.30 [149.23, 152.43] | 0.10 | 0.81 |
| 32 | 4 KiB | 8.99 [8.91, 9.04] | 164.73 [164.54, 165.81] | 0.29 | 1.49 |
| 32 | 64 KiB | 14.94 [14.93, 14.98] | 267.77 [266.50, 269.07] | 0.30 | 2.10 |

Rust had two sampled threads in every case. FX had 13, 27, and 75 at 1, 8,
and 32 agents respectively, identical across history sizes and measured runs.
These counts include all Node/native workers; they are not a count of just the
runtime threads created by `createCore`.

For the 32-agent, 64 KiB case:

- Median whole-run duration was 1.659 s for Rust and 1.987 s for FX. Reported
  within-run p99 first-logical-chunk latency was 58.76 ms and 203.65 ms; turn
  latency was 564.93 ms and 743.82 ms. These are medians of small-sample quantiles,
  not production tail estimates. Each response has 500 ms of scheduled delay.
- Request-body bytes were 13,117,638 for Rust and 13,111,140 for FX. Response-body
  bytes were 3,017,112 and 610,464 respectively, including FX's catalog bodies.
  Both delivered exactly 491,520 generated text bytes. The different envelopes
  and Responses terminal-text repetition explain why body counts are not a
  protocol-independent network efficiency result.
- Rust used 33 connections; FX used 32 or 33 and made 32 catalog GETs per run.
  Metadata lookup is therefore concrete additional work to profile or share
  within compatible account/model settings, not an inferred paid-model cost.
- Median provider peak RSS was 42.70 MiB for Rust and 44.08 MiB for FX, outside
  the target boundary. Provider CPU, observer cost, and all samples remain in
  the raw capture.

The difference is worth investigating, especially FX's history-dependent memory
growth and thread/bridge costs. This screen cannot isolate Node's fixed cost,
native allocations, allocator retention, or serialization copies. It does not
show that the standalone Zig CLI needs the same memory, and it is not a reason
to discard FX's broader features or claim full-harness parity.

Capture: `.local/bench/fx-cabal-capture-retry/.local/bench/fx-matrix/`.
Validation log: `.local/bench/fx-cabal-validation/.local/fx-validation.log`.
The initial full artifact transfer ended with an unexpected EOF; scoped export
succeeded. The exported results were rechecked locally with the same comparison
code, and the current benchmark source fingerprint matches the remote capture.

| Artifact | SHA-256 |
| --- | --- |
| Observer source | `739b6a939f98235dadc0e23e8938a6e7bfc7973650a3b7b1384a4ba2140bdd47` |
| Rust executable | `fabe67d635c802772e230d278782b49f7daa39de76af9fdef923b2dfa7387447` |
| Rust Cargo.lock | `beca5842d8bf6688cca00c6cc16cef754568d6ff42dad68dc18af0608659ce4e` |
| FX Linux x64 addon | `00dd9b63c7b751038fc90735425d105c598ffa33da4ca320b08df206dfba927e` |
| Node executable | `bc17c508ffeed0ec622934f9b7fa72f8e78da65350e63c3eceb56fa688aa5e12` |
| Adapter dependency lock | `dbff64f20636a094e4250e27457d4266c8e4f1de221b2a300bf77331f41d47d4` |

Validation: 36 Python tests passed on the runner, including the real FX adapter,
full-history validation, protocol-comparison guards, and existing Rust durable
lifecycle tests. Three optional Pi/Codex/Rust engine tests were skipped; the Rust
streaming path was exercised by the full matrix. The local FX integration also
passed on macOS, but that smoke run is not a second platform's performance matrix.
Thread-counter assertions, JavaScript/Python syntax, local doc links, and diff
checks passed locally. There are no Rust implementation changes in this slice.

## Reuse and possible contributions

FX is open source under [Apache-2.0](https://github.com/vercel-labs/fx/blob/43c11dcc34a94a76df870af70bdb824579bf18a0/LICENSE).
Its [contribution guide](https://github.com/vercel-labs/fx/blob/43c11dcc34a94a76df870af70bdb824579bf18a0/CONTRIBUTING.md)
requests small changes with explicit contracts and focused verification.
The project already has [native capacity and Pi comparison benchmarks](https://github.com/vercel-labs/fx/tree/43c11dcc34a94a76df870af70bdb824579bf18a0/benchmarks/libfx).

Source-backed investigation targets, not measured cost attribution:

- The [native addon](https://github.com/vercel-labs/fx/blob/43c11dcc34a94a76df870af70bdb824579bf18a0/src/napi_core_main.zig)
  starts one runtime thread per core and admits at most 64 cores per process.
  Profile thread/bridge allocations before proposing scheduler changes or
  increasing the limit.
- The embedded [checkpoint codec](https://github.com/vercel-labs/fx/blob/43c11dcc34a94a76df870af70bdb824579bf18a0/src/core/agent/runtime/checkpoint.zig)
  serializes history into a blob capped at 4 MiB. The present screen does not
  measure checkpointing, restoration, fork sharing, or compaction.
- A useful initial contribution could extend their capacity benchmark with
  validated multi-turn histories and history-size sweeps. Reuse their existing
  machinery; do not propose replacing it with this repository's whole kit.

Any proposed optimization needs a reproducer on the then-current FX revision,
an allocation/CPU profile or a controlled before/after measurement, and preserved
behavior. No upstream issue, PR, or performance claim has been published.
