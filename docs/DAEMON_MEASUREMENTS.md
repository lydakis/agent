# Daemon lifecycle and follower measurements

This screen measures the Rust daemon with durable storage, shell execution,
and independent socket followers. It does not establish whole-harness parity
with Pi, Codex, or FX. The earlier streaming-only results measure different work.

Measured 2026-09-14 America/New_York (captures dated 2026-09-15 UTC), on Cabal,
Linux x86_64, external power, Python 3.14.7 and psutil 7.2.2. All 24 runs passed,
including six excluded warmups. Every run reached its requested provider
concurrency, validated the expected history and tool results, and had no quality
warnings. Socket runs also passed follower/replay equality checks.

## Results

Medians of three measured runs. Memory is sampled peak RSS in MiB; CPU is
observed cumulative seconds for the daemon plus descendants over the entire
lifecycle workload, including restart.

| Transport | Agents | Daemon RSS | Total tree RSS (range) | Tree CPU s | Peak processes | Peak tree threads |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Stdio | 1 | 9.42 | 19.25 (19.16–19.38) | 0.04 | 3 | 6 |
| Stdio | 8 | 9.98 | 88.38 (88.37–88.39) | 0.24 | 17 | 20 |
| Stdio | 32 | 12.86 | 168.94 (167.86–169.22) | 0.79 | 33 | 36 |
| Socket | 1 | 9.59 | 19.39 (19.36–19.41) | 0.04 | 3 | 4 |
| Socket | 8 | 10.45 | 88.48 (88.36–88.89) | 0.25 | 17 | 18 |
| Socket | 32 | 13.32 | 168.58 (168.54–168.87) | 0.78 | 33 | 34 |

At 32 agents, socket daemon peaks ranged from 13.28 to 13.34 MiB. The bulk of
the summed target RSS belonged to tool descendants: at most 16 shell/sleep pairs
ran concurrently. Summed RSS can count shared pages more than once; these are
not PSS or unique physical-memory measurements. The results support retaining
explicit tool-concurrency limits and measuring tool costs separately from the
agent loop. They do not demonstrate that arbitrary tools fit this footprint.

Median observed turn p99 was 1,064 ms for the 32-agent socket case, with run
p99 values from 1,045 to 1,177 ms. Stdio run p99 ranged from 1,050 to 1,766 ms.
These are small synthetic samples, not production tail-latency estimates or
evidence that one transport is faster. The provider deliberately delays tokens
and the shell deliberately sleeps. The socket provider's median peak RSS was
29.62 MiB at 32 agents, outside the charged target tree.

## Follow-up: shared versus private memory

A separate 32-agent socket screen on the same Linux runner enables
`--memory-detail`. One warmup and two measured runs all passed, each reaching
32 simultaneous provider requests and 33 target processes. All 53 measured
samples had readable detailed counters. The workload and tool registry match
the socket case above; the extra counter collection changes observer overhead.

| Sampled peak, MiB | Measured run 1 | Measured run 2 |
| --- | ---: | ---: |
| Summed target RSS | 168.82 | 168.83 |
| Target PSS | 18.36 | 18.59 |
| Target private memory (USS) | 15.94 | 16.38 |
| Daemon RSS | 13.40 | 13.63 |

PSS apportions shared resident pages among the processes mapping them. USS counts
private resident pages. These are entire target-tree counters, including the
daemon and tool descendants. PSS is not a hard bound on physical memory freed
when a workload exits; kernel allocations, unmapped file cache, and other
processes are outside it. Samples are not atomic or exhaustive transient peaks.

This corrects the interpretation of summed RSS. It is not a ninefold memory
optimization, and it does not predict the footprint of compilers, browsers, or
other real tools. Native read/write/edit tools avoid spawning a process for
common file operations. Arbitrary external programs still own their allocations;
the existing 16-shell concurrency cap bounds simultaneously admitted tool calls,
not the descendants or memory each command can create.

Capture: `.local/memory-detail-cabal/.local/memory-detail`, dated
2026-09-15T01:11:49Z, from source `31ed8e62152d` plus the working-tree snapshot.
Binary SHA-256: `d2be39058f04841f40cecaebc5017fb56e84fcce132c61ccfa33387073b9519c`.
Observer SHA-256: `dce0b7ebb97edb7feeef672c933577d0cd0258bd8359f8394b3d7bd2a4e5e1c1`.
This binary precedes the output-buffer optimization below; it must not be used
as evidence of that optimization's effect on daemon RSS or PSS.

## Parked turns

Observed 2026-09-15 on Darwin arm64 (10 logical CPUs, 32 GiB, external power)
with an ad hoc probe, not the lifecycle screen: one stdio daemon with
`echo,shell,wait` and `--max-active 0`; one anchor bot whose model call is held
open by the synthetic provider; N bots each submitting one turn whose only tool
call is `wait` on the anchor's turn handle. Daemon RSS was sampled by psutil
after all bots existed (baseline), one second after the Nth `turn_waiting`
event, and one second after the anchor was released and every waiter finished.

| Parked turns | Baseline RSS | Parked RSS | Bytes per parked turn | Threads | After all finished |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 500 | 10.69 MiB | 13.27 MiB | 5,407 | 4 | 23.92 MiB |
| 2,000 | 12.52 MiB | 35.58 MiB | 12,091 | 4 | 41.34 MiB |
| 4,000 | 12.58 MiB | 51.05 MiB | 10,084 | 4 | 51.05 MiB |

Parking 4,000 turns took 20 seconds, about 5 ms each, dominated by one model
call and three `synchronous=FULL` commits per park. Thread count did not grow.
RSS did not fall after the waiters finished, so the per-turn figure is the
allocator's high-water mark from processing each turn (history load, request
body, parser buffers), not the live state of a parked turn, which is a store
row and a registry entry of a few hundred bytes. Separating retained from live
memory needs the instrumented-allocator probe used for the tool-output change,
and this probe should move onto the lifecycle screen with the binary and
observer hashes recorded. Binary SHA-256 for these runs:
`5bd96bc606ef8a413bbf6f73a76ab157534c0bee542e9c85933f392201560ffe`.

## Contract

Each case creates 1, 8, or 32 named bots, each with its own synthetic workspace,
and executes three turns per bot. Each turn adds 4 KiB of user text, executes a
shell command that writes a known file and holds a child for 250 ms, then receives
twenty 256-byte text chunks at 25 ms intervals. A separate synthetic provider
validates every retained message and tool result. The registered tools are
echo, shell, read, write, and edit; only shell is executed by this workload.

The daemon uses SQLite FULL durability. The observer kills and restarts it,
verifies exact resume, replay, item retrieval and idempotent submission, then
forks each first completed checkpoint into another workspace. Forking must not
repeat historical tool side effects. In socket mode, one follower per original
bot is attached before work starts. Each follower's durable events must exactly
match database replay, both before and after restart.

Stdio uses the existing single firehose. Socket mode uses one control connection
plus one follower connection per bot. These are different client contracts,
reported as feature-cost observations rather than like-for-like speedups.

## Measurement boundaries

- Target tree: native daemon and its shell/sleep descendants. Daemon RSS is also
  reported separately; it must not substitute for the target-tree total.
- Python controller/follower readers and the synthetic provider are outside the
  target tree. Provider memory and observer CPU are recorded separately.
- Rust CLI client processes, live providers, file-tool workloads, sustained slow
  followers, and arbitrary long-history compaction are not measured here.
- Sampling is every 200 ms with wider process-group discovery every 500 ms.
  Idle phases last 450 ms. Startup completes before the first target sample;
  transient allocations and short-lived children can be missed. CPU is the
  observed cumulative total, not an exhaustive accounting of exited children.
- Bounds: 30 seconds per run, 512 MiB per target/provider tree, 48 target
  processes. Each case has one excluded warmup and three measured repetitions.

Reproduce from the repository root after a release build:

```sh
.local/venv/bin/python -m bench.lifecycle --transport socket --agents 32 \
  --mode shell --tools echo,shell,read,write,edit \
  --out .local/bench/daemon-socket-32
```

Repeat with `--transport stdio` and with `--agents 1` or `8`. Use a new output
directory each time. Results use `rust_lifecycle_v2`, retain the binary hash and
observer fingerprint, and record configured versus achieved provider concurrency.

## Correctness and allocation checks

The later output-buffer change reuses valid UTF-8 pipe buffers, skips replacement
allocation when a credential is absent, and shrinks spare capacity before
retaining a large artifact. Lossy decoding, redaction, preview truncation, and
full artifact content are preserved.

A matched local macOS probe executes one shell command producing 1 MiB of ASCII
output with two configured, absent synthetic credential values. An instrumented
Rust allocator records requested live heap and cumulative allocations during
tool execution. Three runs of each isolated debug build gave:

| Counter | Before | After |
| --- | ---: | ---: |
| Peak additional live heap | 4,195,990 B (4.00 MiB) | 2,229,910 B (2.13 MiB) |
| Cumulative allocation requests | 7,900,586 B | 5,803,434 B |
| Retained artifact length | 1,048,576 B | 1,048,576 B |

This is about 47% less peak additional heap for this specific output path,
not a 47% reduction in daemon or child-process RSS. Allocator metadata, physical
reallocation overlap, and shell descendants are not included. Native counters
and end-to-end workloads remain necessary. Shrinking the retained allocation
adds allocator work but avoids keeping a pipe buffer's excess capacity.

Probe sources, independent build outputs, hashes, and captures are under
`.local/tool-output-allocation/`, with the final measurements in `result.json`.
Before/after tool-source SHA-256 values are
`906d03e1c8a3a634abf4edf5512ee44b4f7f0d43acaef409b6dc4aebf6798339` and
`1e72892a61dada84b7326f31826a1afef45aaa1452adb37d9dac7d139137ca67`.
Final local validation: 30 Rust tests and 43 Python tests passed, with four
optional external-engine tests skipped. The final buffer-capacity adjustment
also passed the four focused tool contracts and all-target Clippy.

The fixes preceding this screen cover edit expansion bounds, observable follower
eviction, paged artifact retrieval, and shutdown. A socket whose event queue fills
is closed even if its writer is blocked; the client exits with an error and can
recover durable completion by following from its last received cursor. Burst
eviction is tested separately, not included in the successful-stream timings.

Artifact pages reconstruct a 1 MiB shell output through the real protocol.
Storage tests additionally cover escaped Unicode, byte boundaries, EOF, invalid
offsets, and ownership. Shutdown tests require the stdio process to exit without
stdin EOF and the socket client to receive its acknowledgment.

A separate local allocation probe replaces 2,000 occurrences of a one-byte string
with a 64 KiB string. Before the fix, it allocated the expanded output before
rejecting it, reaching 140,083,200 bytes peak RSS. The corrected path rejects it
before allocation and reached 6,979,584 bytes. These are single macOS measurements
of a debug-linked tool probe using `/usr/bin/time -l`, not daemon capacity claims.
The input file is unchanged on rejection.

Local validation: 30 Rust tests and 42 Python tests passed, with four optional
external-engine tests skipped. Clippy with warnings denied, formatting, and diff
checks passed. All provider traffic in these checks is synthetic.
The final Cabal snapshot passed the same 30 Rust tests and 42 Python tests before
the matrix ran.

## Provenance

- Source: `31ed8e62152d4f7dfe533362546db530be38b88c` plus the uncommitted
  implementation and review fixes in the final Errand snapshot.
- Final job: `cabal/01M2H8J37AA2MTBT2SK770WQS4`.
- Release binary SHA-256:
  `26d5c614c8f4c36b5feccbb59767c2bccd6bca25bd0403f9145ee3b5792474db`.
- Observer SHA-256:
  `8ab90ff512fcbd3ce905962d7871a64484a66cd3676e38a90407f439285480e5`.
- Ignored raw captures:
  `.local/bench/daemon-cabal-final/.local/bench/daemon-final/`.
- Ignored validation log:
  `.local/fable-review/cabal-final-validation/.local/daemon-validation.log`.

The local observer fingerprint matches all six captures. A preceding job,
`cabal/01M2H875NE06X978P7MN9XSF48`, was stopped after the additional shutdown
acknowledgment fix was identified; its output is excluded. No remote source
changes were applied locally. Only documentation/comment changes followed the
final measured snapshot.

## Accounting and artifact regression screen

Observed 2026-09-15 on Darwin arm64, external power, Rust 1.98.0, Python
3.12.9 and psutil 7.2.2. This compares the working-tree binary before and after
fixing incomplete-call accounting, idle-daemon result lookup, and inherited
artifact access. It is a same-engine regression screen, not a capacity claim.

Both binaries used the unchanged `bench.lifecycle` socket workload: 32 agents,
three turns each, one echo call per turn, all five file/echo/shell tools
registered, one follower per bot, SQLite FULL durability, replay, restart and
historical forks. Each run completed 192 provider calls and 96 tool results,
reached 32 concurrent provider requests, and passed follower/replay equality.
The observer reported no quality warnings. Tool subprocess cost is not exercised
by this echo workload.

Six measured runs per binary, plus two excluded warmups each: an initial
three-run screen followed by three alternating before/after pairs to check a
slow tail-latency observation. All measurements are retained, including that
observation. Values below are medians, with the full measured range in parentheses.

| Metric | Before | After |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.12 (17.03–17.17) | 17.00 (16.86–17.03) |
| Observed target CPU, seconds | 0.303 (0.273–0.310) | 0.290 (0.280–0.313) |
| Per-run p95 turn latency, ms | 594.6 (590.8–602.0) | 594.4 (588.3–651.1) |
| Provider request body bytes per run | 3,254,232 | 3,254,232 |
| Provider response body bytes per run | 2,948,154 | 2,948,154 |

This screen shows preserved median latency and network traffic, with slightly
lower sampled memory and median CPU. CPU ranges overlap, and the after binary
had one slower p95 sample; these results do not establish a general speedup or
an improved worst-case bound. Host exclusivity was not established.

The successful-call path still uses one atomic transcript/usage commit.
Budget checks no longer fetch and copy bot metadata on every model round.
Model-facing artifact line pages are assembled on the storage worker, so the
async runtime receives the bounded page instead of the whole retained blob.
The echo screen does not quantify that artifact-read improvement. Dedicated
regressions verify failed-call accounting, idle query restart, inherited output
access and rejection of later source turns.

Raw captures and the alternating driver are under ignored `.local/review-fixes/`.
The initial screens use `python -m bench.lifecycle --agents 32 --mode echo
--tools echo,shell,read,write,edit --transport socket --repeat 3`, with separate
`--binary` and `--out` paths. Both records have identical workload, observer,
host, transport and sampling settings. Binary SHA-256:

- Before: `7dad3b893e7b3a77d10620c710fa9c33a716d860b55370d5e16f8536fe0e7afe`
- After: `b20d0b20159e3aa64a1d5e860a56cf9f85215d8536d121a4128b89d56326358f`
