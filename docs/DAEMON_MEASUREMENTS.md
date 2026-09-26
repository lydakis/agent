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


## Caching and fork slice regression check

Observed 2026-09-15 on Darwin arm64, external power, Rust 1.98.0. The same
32-agent socket echo workload as the accounting screen above, one excluded
warmup and three measured runs, on the binary that adds Anthropic cache
breakpoints, cache-inclusive input accounting, and fork-from-any-message
(SHA-256 `362b1af4ddbcb1e1fc63056ff459e48cfc141988807108ed6a48c628d6d38fea`). All runs passed with 32 concurrent provider requests and
no quality warnings.

| Metric | Accounting screen "after" | This binary |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.00 (16.86–17.03) | 17.09 (16.92–17.19) |
| Observed target CPU, seconds | 0.290 (0.280–0.313) | 0.299 |
| Per-run p95 turn latency, ms | 594.4 (588.3–651.1) | 600.3 |

Within the earlier ranges; no regression is indicated for this short-history
workload. It does not constrain long-history fork cost (see below). The fork validation
parses a lineage once per fork and this workload forks each bot once, so its
cost is inside these numbers. Capture: ignored
`.local/bench/slice-fork-cache-socket-32/`.

## Fork validation fixes

Observed 2026-09-15 on Darwin arm64, external power, Rust 1.98.0. Compare the
caching/fork binary above with the fixed binary
`d80773f0d9db9260deb4efc56a39386652b7dad02fcf20823a8c5f6641965b22`.
The fixes reject forks ending on Responses reasoning, omit empty Anthropic
system blocks, and replace the full-history fork scan with indexed checkpoint
lookups and backward validation of only the uncheckpointed suffix. The selected
item's type is checked even at an existing checkpoint; reasoning bytes are never
rewritten. The added checkpoint index is compatible with existing version-6 stores.

The long-history probe seeds identical synthetic 3,000-node, 6,186,000-byte
histories and a completed head checkpoint. Each run starts a stdio daemon,
performs one excluded warmup fork, then measures 24 independent forks at that
same head. Three runs per binary alternate order. Seeding and daemon startup
are outside the measured interval; the controller and fixture are excluded from
daemon counters. RSS is sampled every 2 ms; CPU is the daemon's user plus system
time over the 24 forks. Values are medians of run metrics, with ranges.

| Long-history fork metric | Before | After |
| --- | ---: | ---: |
| Per-run median fork latency, ms | 31.28 (30.76–33.20) | 0.217 (0.193–0.229) |
| Daemon CPU for 24 forks, seconds | 0.697 (0.680–0.708) | 0.0047 (0.0043–0.0050) |
| Sampled peak daemon RSS, MiB | 16.03 (16.03–16.06) | 9.47 (9.44–9.52) |

This establishes an improvement for known-checkpoint forks of this history.
It does not establish constant-time arbitrary-node forks: a first fork inside
a turn still validates the suffix since its nearest checkpoint, and ancestry
checks still walk node metadata. These are sampled RSS measurements, not PSS
or total physical memory. One before run had a 293 ms individual fork outlier;
it remains in the raw capture and is not represented by the per-run medians.

The existing 32-agent socket echo screen was also rerun with identical workload,
observer, host, sampling, tools, and transport fields. One warmup and three
measured runs per binary alternated order. All eight runs passed, each reaching
32 simultaneous provider requests, completing 96 turns and 192 provider calls,
and verifying follower/replay equality without quality warnings.

| Socket lifecycle metric | Before | After |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.22 (17.19–17.22) | 17.11 (17.08–17.14) |
| Observed target CPU, seconds | 0.265 (0.262–0.266) | 0.252 (0.252–0.258) |
| Per-run p95 turn latency, ms | 591.3 (589.0–609.2) | 590.4 (584.2–603.5) |
| Provider request body bytes per run | 3,254,232 | 3,254,232 |
| Provider response body bytes per run | 2,948,154 | 2,948,154 |

CPU and sampled memory improved slightly in this screen; latency ranges overlap
and network traffic is unchanged. Host exclusivity was not established, so
these small samples do not establish a general speedup or a worst-case bound.
The substantial improvement is confined to the measured long-history fork path.
Raw captures, immutable binaries, and the synthetic comparison driver are under
ignored `.local/fable-slice-review/`; the matched results are in
`performance-fixes/`. No live provider calls were used for these fixes.

## Long history

Observed 2026-09-15 on Darwin arm64 (10 logical CPUs, 32 GiB, external power),
Rust 1.98.0, Python 3.12 and psutil 7.2.2, with `bench.long_history` on binary
`6fe30277db21586883a6b0b96b90117d9ad599c3771cec9858cbad2696c0ffb0` (the
long-history working tree before the schema migration was added; the store
code paths measured here are unchanged by it). One bot is seeded with N items
(N/2 turns of one ~300-byte prompt and one ~300-byte reply) through a stdio
daemon and a synthetic Responses provider, then the daemon is restarted and
each operation below is timed once from the controller, so these are single
observations with process-spawn and JSONL round trips included, not medians.
The context window is fixed at 64 KiB and 256 items throughout.

| Stored items | 1,000 | 10,000 | 100,000 |
| --- | ---: | ---: | ---: |
| Store files on disk, MiB (with WAL) | 4.8 | 11.7 | 82.5 |
| Request body, bytes / items | 64,279 / 222 | 64,392 / 222 | 64,505 / 222 |
| Daemon startup to readiness, ms | 9.5 | 11.3 | 17.5 |
| `resume` alone, ms | 0.14 | 0.14 | 5.8 |
| One turn, ms | 2.2 | 2.6 | 3.1 |
| Fork from the head, ms | 0.32 | 0.30 | 0.40 |
| Fork from the first checkpoint, ms | 0.61 | 4.3 | 42.1 |
| Turn on the head fork, ms | 1.7 | 1.8 | 2.1 |
| `history` read of turn 1 (one turn), ms | 3.5 | 11.0 | 87.1 |
| Daemon RSS after the turn, MiB | 10.5 | 12.3 | 12.6 |

In these single observations, request bytes and turn latency stayed close
from 1,000 to 100,000 stored items; post-turn RSS grew from 10.5 to 12.6 MiB.
This one-bot result cannot establish a memory improvement over the 32-agent
socket screen. The measured revision persisted its window start but walked
the suffix twice, once to validate ancestry and once to select items; the
review fixes below remove the redundant walk. Seeding ran at about 1.5 ms per
turn (three FULL commits and one synthetic model call) with no observed growth.

What still grows: anything that must prove an old node belongs to the current
lineage walks node metadata from the head to that node. A fork from the first
checkpoint and a `history` read of turn 1 are both linear in the distance from
the head (about 0.4 and 0.9 µs per node). Reads of recent turns stay cheap.
The `resume` figure at 100,000 items is one cold observation after a restart
on an 82 MiB file and is not separated from page-cache effects. Checkpoint
indexes for lineage membership are the remaining item in
[LONG_HISTORY.md](LONG_HISTORY.md). Captures: ignored `.local/bench/history-1k/`,
`history-10k/`, and `history-100k/`.

## Long history slice regression check

Observed 2026-09-15 on Darwin arm64, external power, Rust 1.98.0. The same
32-agent socket echo workload as the accounting screen, one excluded warmup
and three measured runs, on the long-history binary
`a5afce4fb5f7ab471122c0172e04041aa9f5fc0f876e0ec2c074721ab249006f` (streamed
context windows, the `history` tool, and the version-7 migration). All runs
passed with 32 concurrent provider requests and no quality warnings.

| Metric | Caching and fork check | This binary |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.09 (16.92–17.19) | 16.28 (16.17–16.30) |
| Observed target CPU, seconds | 0.299 | 0.311 (0.306–0.315) |
| Per-run p95 turn latency, ms | 600.3 | 598.7 (596.6–609.0) |

Peak RSS fell by about 0.8 MiB with no transcript in memory; CPU is within the
earlier spread for a workload whose histories are a few items long. The store
now reads each request's window in 64-item batches instead of one load, and
this screen does not exercise histories longer than the window. Capture:
ignored `.local/bench/slice-long-history-socket-32/`.

## Long-history review fixes

Observed 2026-09-15 on the same Darwin arm64 host, external power, Rust 1.98.0.
This comparison reruns the committed runtime, the long-history slice, and its
review fixes serially with the same observer, workload, and configuration.
Each binary has one excluded warmup and three measured runs, with execution
order rotated across rounds. An earlier warmup attempt exceeded the sampler
cost threshold and is retained separately; all runs in the completed matrix
passed without quality warnings.

Binary SHA-256 identifiers:

- Committed runtime (`00549d6`, runtime unchanged since `c2306fa`):
  `d80773f0d9db9260deb4efc56a39386652b7dad02fcf20823a8c5f6641965b22`.
- Long-history slice before fixes:
  `a5afce4fb5f7ab471122c0172e04041aa9f5fc0f876e0ec2c074721ab249006f`.
- Fixed working tree:
  `c4025ad99e30484484ad1c136ae436292cda3ea1f10b3cce88fbe8a3e70ada4a`.

The exercised contract is the 32-agent socket echo workload: three turns per
bot, 20 chunks of 256 bytes at 25 ms intervals, 4,096-byte initial history,
`echo,shell,read,write,edit` registered, FULL SQLite durability, replay, restart,
resumption, idempotent submission, and historical forks. Every run reached 32
concurrent provider requests and completed 192 requests and 96 tool results.
All three binaries sent exactly 3,254,232 request-body bytes and received
2,948,154 response-body bytes. Context omission and the new history tool are
not exercised by this short-history workload.

Values are medians, with the minimum and maximum across the three measured runs.

| Metric | Committed runtime | Slice before fixes | Fixed working tree |
| --- | ---: | ---: | ---: |
| Observed target CPU, seconds | 0.2912 (0.2870–0.3087) | 0.3198 (0.3024–0.3255) | 0.2937 (0.2918–0.2998) |
| Sampled peak target RSS, MiB | 17.03 (17.03–17.20) | 16.30 (16.23–16.36) | 16.44 (16.44–16.44) |
| Per-run p95 turn latency, ms | 596.8 (592.7–629.2) | 592.4 (590.8–594.1) | 590.2 (585.3–622.0) |

The fixes reduce median observed CPU by 8.2% against the slice, bringing it
within the committed runtime's measured range. Peak RSS is 144 KiB higher than
the slice and 0.59 MiB below the committed runtime. This is a small memory
tradeoff, not a claim that every resource metric improved. Cached SQLite
statements retain memory to avoid repeated compilation; the measurements do
not isolate their individual contribution. Latency ranges overlap and do not
establish a latency improvement. The hot path now combines window metadata
queries, removes redundant ancestry validation, caches recurring statements,
and copies blobs directly into streaming batches.

A separate direct-store probe seeds 100 independent bots with 2,000 items
(1,000 two-item turns) each. Both builds read turn 1 of the first and last bot
three times in alternating order, checking the two-item result. Seed data and
operation boundaries match; seeding is excluded from timings. These are lookup
costs, not end-to-end model-turn timings or active-agent capacity measurements.

| Median history lookup, ms | Slice before fixes | Fixed working tree |
| --- | ---: | ---: |
| First bot | 1.467 | 0.876 |
| Last bot | 136.469 | 0.774 |

The lookup now discovers both turn boundaries along the selected bot's ancestry
instead of testing every other bot's matching ordinal. It still walks metadata
between the requested turn and the selected head; checkpoint indexes remain
future work. The measured revision included the version 6 to 7 store
migration, which the working tree keeps. Historical-fork tests check exact
turn boundaries after both branches acquire different turns with the same
ordinal.

Raw runs, binary copies, probe source, and the excluded warmup attempt are under
ignored `.local/long-history-fixes/`. Observer SHA-256:
`2a988504abe984112ea42977b4ae92583968d49fae2d9ae3976654a9d74b0c59`.

## Recoverable history pages and benchmark boundary fixes

Observed 2026-09-15 on Darwin arm64, external power, Rust 1.98.0. The history
reader now returns original provider JSONL with byte offsets and bounded UTF-8
pages, including content beyond the former 2,000-character preview. It passes
at most 64 KiB of message data into Rust per page using SQLite blob slices,
avoiding the former full-item JSON parse and preview allocations. SQLite's
internal allocation cost is not isolated by this screen. Benchmark daemon
input accepts its 1 MiB event bound, including framing, before normalizing
large text deltas. The observer also closes its target stdin after teardown.

The matched 32-agent socket echo workload, tools, durability, and measurement
boundaries are the same as the preceding review-fix screen. Before and after
were run serially in alternating order, with one excluded warmup per binary.
The initial three measured runs were extended to five because CPU readings
were noisy; all five are included below. Every measured run passed without
quality warnings. One excluded after warmup exceeded the sampler overhead
threshold. All runs reached 32 active provider requests, 192 completed
requests, and 96 tool results. Network bodies were identical at 3,254,232
request bytes and 2,948,154 response bytes.

| Metric, median (minimum–maximum) | Before | After |
| --- | ---: | ---: |
| Observed target CPU, seconds | 0.3132 (0.2992–0.3383) | 0.3184 (0.2948–0.3510) |
| Sampled peak target RSS, MiB | 16.52 (16.41–16.66) | 16.33 (16.19–16.39) |
| Per-run p95 turn latency, ms | 598.7 (593.6–606.8) | 602.4 (597.6–623.0) |

Sampled peak RSS fell by 192 KiB. CPU and latency medians rose by 1.7% and 0.6%,
respectively, with overlapping ranges; this small screen does not establish a
CPU or latency improvement. It exercises the common lifecycle path, not the
new history paging behavior, so it cannot attribute the RSS difference to the
paging implementation or rank the old preview against complete retrieval.

Separate regressions reconstruct an omitted long Unicode message through the
actual history tool across multiple pages, check invalid offsets and exact
historical fork boundaries, and pass 65,536- and 131,072-byte chunks through
the real daemon benchmark driver. A 1,000-item long-history smoke run succeeded
with the corrected v2 fields: `startup_ms=10.03`, `resume_op_ms=0.14`, and
`history_read_ok=true`. These are single functional smoke observations, not a
before/after performance comparison. The earlier long-history table's startup
row is relabeled to reflect its actual boundary; its old captures excluded
resume despite the `restart_and_resume_ms` field name.

Captures, saved binaries, and test logs: ignored `.local/history-paging-fixes/`.
Before binary SHA-256:
`0c6f70d25a2df934ff3b637b289b8ef8fe05dc4bf264851d4c2244e9ee672441`.
After binary SHA-256:
`fe4ee38073f92122ffe4e4311084511bfefc8b1edfab5fc8ce3614f5282759c3`.
Observer SHA-256:
`6a8892631a81d7db39bbfb8ed3eda7c4bc1e46adcebd1b280a07f48a0c999009`.

## Context-aware history pages and bounded migration

Observed 2026-09-15 on Darwin arm64, external power, Rust 1.98.0. History
pages now respect the space remaining in the active turn, including the JSON
escaping and provider envelope of the stored tool result. Older turns can leave
the context window. A caller may request a smaller page with `limit`; the runtime
reserves half the remaining byte budget for subsequent work. A regression
reconstructs an omitted Unicode message under a 64 KiB context budget, including
a retrieval after 56,000 bytes of assistant work in the same turn.

The schema 6 to 7 migration now reads accepted events one at a time and reuses
prepared statements. A synthetic 72.71 MiB store contains 100 bots with 1,000
turns each, 200,000 message nodes, and 300,000 events. Each bot fits the old
schema's per-bot caps. Three fresh copies per binary were upgraded in alternating
order, with no excluded warmups. File copying is outside the measurement; these
are not cold-disk observations. Peak RSS is sampled every 5 ms, and CPU and
elapsed time end at daemon readiness. Every run verified schema 7, all message
nodes, and all 100,000 turn markers after migration.

| Migration metric, median (minimum–maximum) | Before | Fixed migration |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 33.09 (33.08–34.28) | 13.83 (13.77–14.02) |
| Target CPU to ready, seconds | 0.7281 (0.6938–0.7309) | 0.5318 (0.5243–0.5759) |
| Process start to ready, seconds | 0.8241 (0.7424–0.8349) | 0.6129 (0.5967–0.7031) |

For this fixture, median peak RSS decreased 58.2%, CPU 27.0%, and startup time
25.6%. This measures store upgrade cost, not active-agent capacity or the cost
of reading history during a turn. Migration regression coverage also verifies
that a malformed final accepted event rolls back the schema and earlier
backfill, and that repairing it allows retry without changing fork ordinals.

Migration baseline binary SHA-256:
`fe4ee38073f92122ffe4e4311084511bfefc8b1edfab5fc8ce3614f5282759c3`.
Fixed migration binary SHA-256:
`3c6d390c58a8b4179ab0806ec5fb6027af08a8db6a48b7fc1413c1a85125e497`.
Raw captures, binaries, and synthetic drivers: ignored
`.local/context-budget-fixes/`.

The final binary keeps the history future behind a box to isolate its state
from ordinary tool dispatch. A longer matched lifecycle screen uses the same
32-agent socket echo contract, tools, FULL durability, resume/replay, and fork
checks as above, with **10 turns per bot**. Three measured runs per binary follow
one excluded warmup, alternating order. All passed without quality warnings;
each reached 32 active requests, 640 completed requests, and 320 tool results.
Request bodies were identical at 32,411,360 bytes and response bodies at
9,827,180 bytes. This workload does not invoke the history tool.

| Lifecycle metric, median (minimum–maximum) | Before | Final |
| --- | ---: | ---: |
| Observed target CPU, seconds | 0.8975 (0.8654–0.9580) | 0.8990 (0.8812–0.9354) |
| Sampled peak target RSS, MiB | 17.23 (17.12–17.28) | 17.48 (17.34–17.55) |
| Per-run p95 turn latency, ms | 599.5 (596.3–615.7) | 602.5 (596.7–680.6) |

Median CPU rose 0.2%, RSS rose 256 KiB (1.5%), and median p95 latency rose 0.5%.
CPU and latency ranges overlap, but the final binary has one slower tail run.
This small screen supports similar median common-path cost, not strict
performance equivalence, an improvement, or the absence of tail regressions.
The initial three-turn screen of the unboxed implementation showed a 6.6%
median CPU increase. Its repeat was also noisy. Those captures are retained;
the longer final screen cannot by itself attribute a change to boxing.

Final lifecycle captures: `boxed-final-{before,boxed}.json`; final migration
capture: `migration-final-results.json`, under the ignored directory above.
The initial unboxed migration screen also reduced peak RSS and CPU; its binary
and captures are retained separately. Observer SHA-256 for lifecycle screens:
`6a8892631a81d7db39bbfb8ed3eda7c4bc1e46adcebd1b280a07f48a0c999009`.

## Filtered reasoning in history-tool reads

Observed 2026-09-15 on Darwin arm64, external power, Rust 1.98.0. The history
tool now omits only top-level `encrypted_content` from reasoning items. It
keeps their readable summaries and all records. Stored bytes, native provider
replay, raw item access, and Anthropic thinking/signatures remain unchanged.
Offsets address UTF-8 bytes in this filtered reading view. SQLite transforms
one item at a time outside the ordered ancestry query, then returns bounded
blob slices to Rust. SQLite can still parse/copy the entire individual item;
this is not a constant-memory JSON transformation.

A synthetic local Responses fixture seeds a six-item turn: one user prompt,
four reasoning items with Unicode summaries, and one assistant reply. In the
reasoning-heavy case, each reasoning item also has a 42,000-byte synthetic
opaque field. Five subsequent turns put the source outside an eight-item
context window. Each measurement then retrieves that complete earlier turn
30 times through the actual history tool, following every page to completion.
The fixture checks identical readable facts and preserved native reasoning on
an ordinary continuation before the source leaves context. It also runs the
same scenario with the opaque fields absent.

Before and after use saved release binaries and identical fixture code. Each
case has one excluded warmup per binary and three measured runs in alternating
order. CPU and elapsed time cover the 30 complete retrievals, excluding seeding
and teardown. RSS is sampled every 5 ms during retrieval, for the daemon only;
the observer/provider are excluded. Request bytes include the full JSON bodies
sent to the synthetic provider, including retained previous tool results and
tool schemas. These short local runs do not measure provider inference, token
billing, tail latency, or fleet capacity.

| Metric, median (minimum–maximum) | Before | Filtered view |
| --- | ---: | ---: |
| No blobs: daemon CPU, seconds | 0.0559 (0.0547–0.0581) | 0.0536 (0.0510–0.0540) |
| No blobs: sampled peak RSS, MiB | 11.08 (11.05–11.09) | 11.03 (11.00–11.08) |
| No blobs: complete retrievals elapsed, seconds | 0.0737 (0.0716–0.0794) | 0.0704 (0.0643–0.0829) |
| With blobs: daemon CPU, seconds | 0.2611 (0.2466–0.2692) | 0.0583 (0.0572–0.0601) |
| With blobs: sampled peak RSS, MiB | 14.55 (14.55–14.58) | 12.27 (12.09–12.42) |
| With blobs: complete retrievals elapsed, seconds | 0.3816 (0.3672–0.4378) | 0.0874 (0.0850–0.1017) |
| With blobs: history pages, total | 90 | 30 |
| With blobs: provider calls, total | 180 | 60 |
| With blobs: provider request bytes, total | 15,683,063 | 373,562 |

On this reasoning-heavy retrieval workload, median CPU decreased 77.7%, peak
RSS 15.7%, elapsed time 77.1%, and request bytes 97.6%. This deliberately
compares raw versus filtered reading semantics with identical readable facts,
not identical wire bytes or provider call counts. Savings include avoiding
repeated transmission of opaque blobs as retained tool results. The no-blob
case had 30 pages and 60 calls for both binaries, with identical history text;
its request bytes fell from 379,466 to 376,346 because the updated tool
description is 52 bytes shorter. No regression was observed in that control,
but it is too small to establish a general CPU or memory improvement.

Tests reconstruct filtered Unicode pages, verify byte-identical replay after
retrieval, preserve non-target fields and Anthropic blocks, and exercise actual
provider continuation plus history retrieval under a 64 KiB context budget.
Validation: 46 Rust tests and 88 Python tests passed, with four Python skips;
strict Clippy, formatting, and diff checks passed. No real provider calls.

Captures, fixture, binaries, and test log: ignored `.local/history-reading-view/`.
Before binary SHA-256:
`3c6d390c58a8b4179ab0806ec5fb6027af08a8db6a48b7fc1413c1a85125e497`.
After binary SHA-256:
`39723ae151d8a3626440a3804d428f705aae094472199ab8ea2fb3e0f43c6407`.
Fixture SHA-256:
`61068f0d404dfb8a5479573248ce5eb81f7306fbf0fd85a9df621735d97052f1`.

## Multiline history JSONL correction

Observed 2026-09-15 on the same Darwin arm64 host, external power, Rust 1.98.0.
History reads now compact multiline JSON items before counting or slicing their
reading-view bytes. Already single-line items retain their whitespace. Stored
items and provider replay remain byte-identical, and encrypted reasoning
filtering still applies. Regression coverage reconstructs four-byte Unicode
pages and exercises multiline SSE responses through the actual provider loop.

The same complete-retrieval fixture described above compares the filtered-view
binary against this correction: one excluded warmup and three alternating
measured runs per binary and case. This fixture emits single-line provider JSON,
so it measures the common-path cost; multiline correctness is tested separately.
Both versions return identical history text and request bytes, with 30 pages
and 60 provider calls per run. Measurement boundaries remain as described above.

| Metric, median (minimum–maximum) | Before | Corrected JSONL |
| --- | ---: | ---: |
| No blobs: daemon CPU, seconds | 0.0571 (0.0569–0.0605) | 0.0565 (0.0542–0.0587) |
| No blobs: sampled peak RSS, MiB | 11.047 (11.000–11.078) | 11.188 (11.172–11.203) |
| No blobs: retrievals elapsed, seconds | 0.0935 (0.0823–0.0979) | 0.0865 (0.0800–0.0939) |
| With blobs: daemon CPU, seconds | 0.0599 (0.0581–0.0619) | 0.0575 (0.0561–0.0577) |
| With blobs: sampled peak RSS, MiB | 12.375 (12.328–12.391) | 12.438 (12.359–12.625) |
| With blobs: retrievals elapsed, seconds | 0.0841 (0.0806–0.0886) | 0.0783 (0.0777–0.0826) |

CPU medians fell 1.0% and 4.0%; sampled peak RSS rose 144 KiB and 64 KiB.
This is a small memory increase, not strict performance equivalence. The short
screen does not establish a general speedup or tail-latency behavior. An initial
candidate compacted every item and showed 1.4–1.6% higher median CPU, 48–144 KiB
higher RSS, and a noisy 20% elapsed-time increase in the blob case. Its captures
are retained; the final implementation skips unnecessary single-line rewrites.

Captures and binaries: ignored `.local/history-jsonl-fix/`, including final
`results.json` and initial `normalize-all-results.json`. Before binary SHA-256:
`39723ae151d8a3626440a3804d428f705aae094472199ab8ea2fb3e0f43c6407`.
Final binary SHA-256:
`5cc8f7b7a1195c0d7e194cd2fe5728513a423d5b57c1c36f2df00acf2877fa2b`.
Final fixture SHA-256:
`83e25315cf471339e3dd6645b03148e73eed01ed4f73297c18866ce64f04bdb8`.

## Query plan audit

The initial audit was recorded 2026-09-16 on the same Darwin arm64 host,
SQLite 3.54, against a migrated copy of the five-minute sustained store.
The corrected `bench.query_plans` expands 93 distinct runtime statements,
including every table variant of deletion and pruning, and checks foreign-key
plans. It rejects unindexed scans of growing tables; structural scan exemptions
use exact names so `SCAN checkpoints` cannot be mistaken for recursive `SCAN c`.
Flags were full table scans on growing tables and temporary B-trees; `SCAN
CONSTANT ROW` from `EXISTS` subqueries, the recursive-CTE step scans that are
bounded by the window or one turn, and per-turn `ORDER BY rowid` over a turn's
few tool rows are structural and not counted.

Four statements scanned a growing table by an unindexed `status`: startup
recovery (`turns WHERE status='running'`, `processes ... status='running'` marked
lost), parked-turn resumption at startup (`turns WHERE status='waiting'`), and
the idle-exit check (`count(*) FROM processes WHERE status='running'`). One
more, the schema 6 to 7 migration's read of accepted events, is a one-time
scan by design. Partial indexes on the two active statuses fix the four; each
holds one entry per active row.

On a copy with 1,009,050 turn rows and 1,000,000 process rows (one million
synthetic completed turns added to the sustained store), warm in the page
cache:

| Statement | Before | After |
| --- | ---: | ---: |
| `turns WHERE status='running'` | 76 ms, SCAN turns | 0.5 ms, COVERING INDEX turns_running |
| `turns WHERE status='waiting' ORDER BY id` | 52 ms, SCAN turns | 0.02 ms, COVERING INDEX turns_waiting |
| `count(*) FROM processes WHERE status='running'` | 44 ms, SCAN processes | 0.01 ms, COVERING INDEX processes_running |

Warm figures understate the difference: a cold scan of a million-row table
reads the whole table from disk, and the idle-exit check ran the process scan
on every idle tick. After the indexes the re-run audit reports no full scan on
a growing table. The store-scale screen in NEXT.md item 3 remains the place to
measure these on a store that does not fit the cache.

Validation of the corrected offline guard used Python SQLite 3.47.1: the
current schema passed, while separate removal of `checkpoints_head`,
`processes_turn`, `turns_running`, `turns_waiting`, or `processes_running`
failed with a scan of the affected table. Its summary reports the actual
SQLite version; plans can differ from the daemon's bundled version. The timing
table above remains the initial observation, not a new runtime benchmark.
These audit fixes change no daemon code or runtime work.

## Heap profile at the fleet peak

Observed 2026-09-16 on the same host with a dhat build of the daemon
(`--features heap-profile`, debug info kept, own target directory) running the
synthetic ten-thousand-bot shape: 10,000 bots, `--max-active 1024`, 5,000
parked, replies held 5 s so the bound stayed full under the profiler's
order-of-magnitude slowdown. The profile records every allocation's stack;
`bench.heap_profile` attributes the bytes live at the global peak, which came
73 s into a 156 s run with 1,024 turns in flight.

Live at the peak: 34.22 MiB. Sampled RSS at the same phase was about 59 MiB,
so roughly 25 MiB of the process is allocator retention, mapped code, thread
stacks, and the SQLite cache rather than live data.

| Bytes at peak | Share | What |
| ---: | ---: | --- |
| 8.78 MiB | 25.7% | hyper's per-connection request dispatch channel (a preallocated block per connection) |
| 8.00 MiB | 23.4% | hyper's HTTP/1.1 read buffer, 8 KiB per connection |
| 8.00 MiB | 23.4% | hyper's HTTP/1.1 write buffer, 8 KiB per connection |
| 3.11 MiB | 9.1% | the boxed turn task: the turn loop's future, about 3.1 KB per active turn |
| 1.06 MiB | 3.1% | reqwest's connection wrappers |
| 1.02 MiB | 3.0% | byte copies (request and response bodies in flight) |
| 0.93 MiB | 2.7% | bot records loaded for turns in flight |
| 0.61 MiB | 1.8% | hyper's connection tasks |
| 0.41 MiB | 1.2% | one cancellation channel per active turn |
| under 0.1 MiB | | everything in this crate's own allocations |

By crate: tokio 39%, hyper 25%, bytes 24%, reqwest 3%, this crate 0.3%.

What it says: at 1,024 in flight about 25 MiB, three quarters of the live
heap, is HTTP/1.1 per-connection state, because the synthetic provider speaks
HTTP/1.1 and each in-flight request holds a connection. Real providers speak
HTTP/2 over the 41 sharded connections, so that block does not scale with
turns there, which is consistent with the 41 MiB peak at 1,024 live turns on
OpenAI. The daemon's own cost per active turn is the 3.1 KB task future plus
a few hundred bytes of channels; per parked turn it is under 1 KB; per bot
that merely exists it is nothing resident. The shaving list this yields, in
order of what it would buy: the turn future's size (boxing its large arms),
which is the only per-turn term the daemon controls; hyper's HTTP/1.1 buffer
sizes if an HTTP/1.1 provider ever matters; and allocator retention, which is
the largest gap between live heap and RSS and would need a different
allocator to test. None of these is worth taking before a workload needs it.
Capture: ignored `.local/bench/fleet-screen-10k-heap.json` with the run's
`fleet-screen-10k-heap/`.

## Pacing slice regression check

Observed 2026-09-16 on the same Darwin arm64 host, external power, Rust
1.98.0. The 32-agent socket echo workload as before, one excluded warmup and
three measured runs, on the pacing binary `58b839a5…` (per-model pools with a
fair gate at every model call, header learning, retries, 64 streams per
connection; the final binary `6f1b1429…` differs only in how a learned level
is merged and how the retry budget is counted, neither on the hot path). All runs passed with 32 concurrent provider requests and no
quality warnings. The synthetic provider sends no rate-limit headers, so the
pools stay unbounded and every call pays the gate's lock and arithmetic and
nothing else, which is the common path on a provider that has not refused.

| Metric | Query-plan check | This binary |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.52 (17.52–17.69) | 17.59 (17.59–17.69) |
| Observed target CPU, seconds | 0.289 (0.287–0.294) | 0.328 (0.322–0.335) |
| Per-run p95 turn latency, ms | 602.2 (591.6–602.6) | 598.7 (596.0–604.4) |

RSS and latency are unchanged. CPU is 39 ms higher over 192 calls, about
0.2 ms per call, which is more than the gate should cost and within the
spread this screen has shown between runs of the same binary before (0.289
to 0.328 across earlier checks); it is recorded, not explained, and the
next screen on an unchanged hot path will say whether it persists. Capture:
ignored `.local/bench/slice-pacing-socket-32/`.

## Pacing review fixes

Observed 2026-09-17 on Darwin arm64, external power, Rust 1.98.0. The
review fixes reject token estimates above a learned allowance instead of
holding the model's queue forever, count failed-attempt usage before retries
and later tool rounds, and separate successful work from failed terminal
outcomes in `fleet_screen_v2`. Budget checks update the loaded bot record;
they add no store reads or writes to successful model calls.

The final matched screen compares the original pacing binary `6f1b1429…`
with the fixed binary `f42ec74a…`. Seven before/after pairs alternate order;
each run warms up for 16 turns and measures 1,000 turns on one bot, two model
calls per turn, an eight-item context window, and eight retained turns.
The local synthetic provider uses TCP_NODELAY. CPU is the daemon's user plus
system time over the measured turns; peak RSS is sampled every 10 ms.
All 14,000 measured turns completed. Each run sent 2,000 model requests with
identical normalized payload hashes and 3,465,230 normalized request bytes.
There were no credentials or paid calls.

| Median across seven runs | Before | Fixed |
| --- | ---: | ---: |
| Daemon CPU, ms/turn | 1.854 | 1.902 |
| Sampled peak daemon RSS, MiB | 13.078 | 13.125 |
| Per-run p95 turn latency, ms | 3.048 | 3.017 |

CPU was 2.6% higher, RSS 48 KiB higher, and p95 latency 1.0% lower. Treat
this as roughly flat on this workload, not an established speedup or a claim
about long-context fleet capacity. The deterministic improvements are removal
of an indefinite queue stall and prevention of calls after reported usage
exhausts a bot's budget.

An earlier 32-bot socket comparison included the pre-pacing commit `608e11f`,
the original pacing binary, and an initial fix candidate `32f6dfc2…`: one
warmup and five measured runs each. Every run verified 192 model calls,
96 tool results, 32 overlapping provider requests, replay/follower equality,
and matching request/response byte counts. Five of the 15 measured runs
triggered sampling-overhead warnings, and tail latency varied substantially;
those results do not establish a speedup or explain the earlier 13% CPU
difference. Shorter sequential screens also showed enough variation to
motivate the longer, balanced final comparison above.

Validation: 56 Rust tests, strict Clippy, the budget/retry integration
regressions, mixed-outcome benchmark accounting, and a synthetic fleet
creation/run/park/restart check passed. Captures and scripts are ignored under
`.local/pacing-fix/`: the socket comparison at the root, exploratory sequential
runs in `sustained/` and `recheck/`, and the final comparison in `final/`.

## Allowance and retry accounting fixes

Observed 2026-09-17 on Darwin arm64, external power, Rust 1.98.0. This
follow-up compares `f42ec74a…` with `d87b5b04…` using the same sequential
echo contract above: seven alternating before/after pairs, 16 warmup turns
and 1,000 measured turns per run, two model calls per turn, an eight-item
context window, and eight retained turns. All 14,000 turns completed;
every run had the same normalized request hash and 3,465,230 request bytes.
The provider is synthetic and local; no credentials or paid calls were used.

| Median across seven runs | Before | Fixed |
| --- | ---: | ---: |
| Daemon CPU, ms/turn | 1.973 | 2.009 |
| Sampled peak daemon RSS, MiB | 13.109 | 13.125 |
| Per-run p95 turn latency, ms | 3.244 | 3.380 |

CPU rose 1.8%, peak RSS 16 KiB, and p95 latency 4.2%. The per-run ranges
overlap for all three metrics; this screen shows roughly flat cost, not a
normal-path speedup or proof of exact performance equality. Reservation
accounting uses a counter and notification per model pool and a stack guard
per call; the turn-wide round check adds no database operations.

A separate matched allowance probe ran three sequential calls on fresh bots,
with a 1,000-token/minute limit, an 800-token output cap, and ten billed tokens
per call. Headers reported 990, 980, then 970 tokens remaining. The third
turn fell from 49,927 ms before the fix to 1.46 ms after it, with identical
request-body hashes. This is one deterministic stall reproduction, not a
general throughput ranking; its logs are in `allowance-results.json`.

The behavioral improvements are removal of false allowance stalls, immediate
failure for a recognized permanent quota error instead of 64 attempts, and
enforcing 200 durable model rounds where the alternating billed-failure/tool
fixture previously made 400 calls. Committed tool plans still execute at the
limit. Cancellation releases reservations, settlement wakes waiting callers,
and out-of-order high token balances cannot replenish spent allowance.

Validation: all 58 Rust tests, strict Clippy with all features, and 13 focused
Python integration tests passed, including error redaction, lifetime budgets,
transient rate retries, and the round limit across parking and daemon restart.
Captures, full binary hashes, and scripts are ignored under
`.local/pacing-accounting-fix/`.

## Streaming admission and benchmark timing fixes

Observed 2026-09-17 on Darwin arm64, external power, Rust 1.98.0. Token
headers now release their accounted reservation immediately, while the
response may continue streaming. Reconciliation and release share one lock;
completion or cancellation cannot refund it again. This removes a redundant
completion-time lock on calls with token headers and adds no allocation or
database operation.

The matched normal-path screen compares `d87b5b04…` with `bcbc2516…` using
the same seven alternating pairs described above: 16 warmup turns and 1,000
measured turns per run, two model calls per turn, eight context items and
eight retained turns. All 14,000 measured turns completed with identical
normalized payload hashes and 3,465,230 request bytes per run.

| Median across seven runs | Before | Fixed |
| --- | ---: | ---: |
| Daemon CPU, ms/turn | 1.854 | 1.852 |
| Sampled peak daemon RSS, MiB | 13.125 | 13.188 |
| Per-run p95 turn latency, ms | 2.859 | 2.869 |

CPU fell 0.09%, p95 latency rose 0.33%, and sampled RSS rose 64 KiB. The
per-run ranges overlap; this is effectively flat, not a normal-path speedup.

A separate streaming probe used five alternating pairs. Each run submitted
two identical requests on fresh bots with a 1,000-token/minute limit, an
800-token output cap, ten billed tokens per call, and token-balance headers.
The first stream stayed open for a controlled 300 ms after its first delta.
Median submission-to-receipt latency for the second turn fell from 307.96 ms
to 2.09 ms. It completed while the first stream was open in all five fixed
runs and none of the baseline runs. Both binaries sent the same two request
bodies in every run. This demonstrates removal of unnecessary serialization,
not a general provider-throughput speedup.

`fleet_screen_v3` separately corrects its latency boundary to submission
through terminal-event receipt, excluding later observer batch delay. That
measurement correction is not a runtime performance improvement. A regression
with a one-second processing delay still reports the actual 20 ms turn time.
One-slot parking is rejected before startup; an eight-bot, two-slot synthetic
screen passed creation, completion, parking four turns, release, and restart.

Validation: 58 Rust tests, 16 focused Python tests, strict all-feature Clippy,
formatting, and diff checks passed. All provider traffic was synthetic and
local, with no credentials or paid calls. Captures, scripts, and full binary
hashes are ignored under `.local/streaming-pacing-fix/`.

## Unsent reservation refunds and unbounded fleet submission

Observed 2026-09-17 on Darwin arm64, external power, Rust 1.98.0. A pacing
reservation now distinguishes waiting for an HTTP startup slot from dispatch.
Cancellation or admission timeout before dispatch refunds its token estimate
and any request allowance actually debited. After dispatch, existing header
and usage accounting remains unchanged. This adds guard flags and arithmetic,
with no new allocation, lock acquisition, or database operation.

The normal-path screen compares `bcbc2516…` with `3f70009f…`: seven alternating
pairs, 16 warmup turns and 1,000 measured turns per run, two synthetic model
calls per turn, eight context items, and eight retained turns. All 14,000
measured turns completed. Every run sent the same normalized payload hash and
3,465,230 request bytes.

| Median across seven runs | Before | Fixed |
| --- | ---: | ---: |
| Daemon CPU, ms/turn | 1.956 | 1.893 |
| Sampled peak daemon RSS, MiB | 13.125 | 13.047 |
| Per-run p95 turn latency, ms | 3.132 | 2.933 |

CPU fell 3.2%, p95 latency 6.4%, and peak RSS 80 KiB. Per-run ranges overlap:
CPU 1.879–2.012 versus 1.865–2.058 ms/turn; RSS 12.984–13.203 versus
13.016–13.125 MiB; p95 2.877–3.187 versus 2.873–3.357 ms. This screen found
no regression; it does not establish a statistically significant speedup.

A separate cancellation probe warmed a 1,000-token/minute pool, occupied the
single HTTP startup slot with another model, and interrupted a queued call
with an 800-token output cap before it reached the provider. After releasing
the slot, the next call remained blocked past the one-second observation
window on the baseline; the fixed binary completed it in 1.98 ms. The provider
confirmed that the cancelled request was never sent in either run. This is a
bounded stall reproduction, not a general throughput comparison.

The fleet-driver probe used the same current binary with eight bots,
`--max-active 0`, and 100 ms synthetic replies. The old driver completed in
0.85 s with one turn in flight; the corrected driver completed in 0.11 s with
eight. This corrects the workload's concurrency, not runtime execution speed.

Validation: 60 Rust tests, 18 focused Python tests, strict all-feature Clippy,
formatting, and diff checks passed. Regression coverage includes cancellation,
admission timeout, request limits learned while queued, and positive versus
unbounded fleet submission. No credentials or paid providers were used.
Scripts, captures, and full binary hashes are ignored under
`.local/unsent-pacing-fix/`.

## Request refunds and controlled restart recovery

Observed 2026-09-17 on Darwin arm64, external power, Rust 1.98.0. Request
allowance now uses reservation accounting alongside token allowance. A newer
provider balance is reconciled net of pending requests before any cancellation
refund, including requests acquired before a limit became known. Request
reservations resolve at headers; absent request headers, the sent debit stays
spent. The change adds one counter per model pool, with no new allocation,
lock acquisition, or database operation.

The matched screen compares `3f70009f…` with `67001acb…` in two modes: ordinary
synthetic echo, and the same responses with request-limit headers (600,000 per
minute, 100,000 remaining). Each mode has seven alternating before/after pairs,
16 warmup turns and 1,000 measured turns per run, two calls per turn, eight
context items, and eight retained turns. All 28,000 measured turns completed.
Every run sent the same normalized payload hash and 3,465,230 request bytes.

| Median across seven runs | Before | Fixed |
| --- | ---: | ---: |
| Ordinary echo: daemon CPU, ms/turn | 1.896 | 1.897 |
| Ordinary echo: sampled peak RSS, MiB | 13.094 | 13.094 |
| Ordinary echo: per-run p95 latency, ms | 3.046 | 3.089 |
| Request headers: daemon CPU, ms/turn | 1.865 | 1.867 |
| Request headers: sampled peak RSS, MiB | 13.078 | 13.109 |
| Request headers: per-run p95 latency, ms | 2.927 | 2.933 |

CPU changed by +0.07% in both modes. P95 changed by +1.43% without request
headers and +0.19% with them. Median sampled RSS was unchanged or up 32 KiB.
Ranges overlap: ordinary CPU 1.851–2.178 versus 1.860–2.003 ms/turn, RSS
13.016–13.156 versus 13.047–13.188 MiB, and p95 2.962–16.950 versus
2.943–3.608 ms. With request headers, CPU was 1.859–1.900 versus 1.858–1.959,
RSS 13.047–13.109 versus 13.094–13.141 MiB, and p95 2.902–3.167 versus
2.902–3.594 ms. These screens show essentially flat overhead, not a speedup
or proof of exact equality.

The pacing regression uses a paused clock: after a fresh zero request balance
and cancellation of an unsent call, the baseline admitted another request
immediately. The fixed pool waits for the required one-second refill at
60 requests/minute. Coverage also checks out-of-order reports and responses
without request headers.

`fleet_screen_v4` now submits a bounded wave of synthetic turns held open until
the daemon is killed and its exit confirmed. It verifies that the expected
number of bots recover as interrupted. This removes the half-completed-fleet
race and works with a single bot. Restart timings have a new workload and
must not be compared to v1–v3 as equivalent work. A full eight-bot, two-slot
screen completed eight turns, parked four, drained five including the anchor,
and recovered two interrupted bots. Separate restart regressions cover one
bot, eight unbounded bots, and eight bots with a two-slot limit, followed by
successful new work after recovery.

Validation: 61 Rust tests, 19 focused Python tests, strict all-feature Clippy,
formatting, and diff checks passed. All traffic was synthetic and local;
no credentials or paid providers were used. Scripts, captures, and full binary
hashes are ignored under `.local/request-restart-fix/`.

## Retry accounting across interruption

Observed 2026-09-17 on Darwin arm64, external power, Rust 1.98.0. Compare
`67001acb…` with `64c2e927…`. Retry counters now outlive cancellation of a
model-call future, include partially elapsed pacing waits, and flush once per
execution segment on completion, failure, interruption, or parking. Unsent
retries cancelled during backoff or admission do not count as dispatched
attempts. An interrupt arriving during the flush is honored before retirement.
Hard process termination can still lose the current segment's unflushed counters.

The ordinary screen repeats the preceding section's contract: seven alternating
pairs per mode, 16 warmup and 1,000 measured turns per run, two calls per turn,
eight context items, and eight retained turns. All 28,000 measured turns
completed. Each run sent the same normalized payload hash and 3,465,230 bytes.

| Median across seven runs | Before | Fixed |
| --- | ---: | ---: |
| Ordinary echo: daemon CPU, ms/turn | 1.622 | 1.626 |
| Ordinary echo: sampled peak RSS, MiB | 13.094 | 13.141 |
| Ordinary echo: per-run p95 latency, ms | 2.494 | 2.509 |
| Request headers: daemon CPU, ms/turn | 1.599 | 1.608 |
| Request headers: sampled peak RSS, MiB | 13.078 | 13.109 |
| Request headers: per-run p95 latency, ms | 2.460 | 2.491 |

CPU changed by +0.24% and +0.58%; p95 changed by +0.59% and +1.25%.
Median sampled RSS increased by 48 KiB and 32 KiB. Run ranges overlap in
all three metrics. This is effectively flat overhead within local variability,
not proof of exact equality.

A separate affected-path screen runs seven alternating pairs, one warmup and
five measured turns per run, 256 context items, and eight retained turns. Each
turn reaches the same `tool_round_limit` after 100 successful echo tool rounds
and 100 billed retry failures. Both binaries record 100 retries, 200 model
rounds, and 1,000 input plus 1,000 output tokens per turn. All 70 measured turns
meet that contract; each run sends 1,000 requests, the same normalized payload
hash, and 12,965,452 request bytes. Batching reduces accounting updates from
100 to one per turn, while response, usage, and tool commits remain unchanged.

| Retry-heavy screen, median across seven runs | Before | Fixed |
| --- | ---: | ---: |
| Daemon CPU, ms/turn | 122.054 | 118.623 |
| Per-run mean turn latency, ms | 179.159 | 175.454 |
| Sampled peak RSS, MiB | 12.969 | 12.969 |

CPU decreased 2.81%, improving in six of seven pairs. Mean turn latency
decreased 2.07%, with substantial timing noise in two pairs. Memory was flat.
This supports a modest CPU improvement for repeated retries across tool rounds;
it is not a fleet-capacity or real-provider throughput claim.

Validation: 61 Rust tests, 23 focused Python tests, strict all-feature Clippy,
formatting, and diff checks passed. Three cancellation regressions fail against
the saved baseline and pass after the fix. Coverage includes dispatched versus
unsent retries, partial pacing waits, persistence after interrupt and restart,
and parking/resumption without double-counting. All traffic was synthetic and
local. Scripts, captures, and full hashes are ignored under
`.local/retry-accounting-fix/`.

## Top-level Responses rate-limit errors

Observed 2026-09-17 on Darwin arm64, external power, Rust 1.98.0. The
`64c2e927…` baseline is compared with `7663633c…`. The fix recognizes a
Responses `error` event's top-level `code=rate_limit_exceeded` regardless of
message wording. It adds a check only in the error branch, with no allocation,
lock, database operation, or change to successful-response parsing.

Seven alternating pairs use the preceding section's ordinary echo contract:
16 warmup and 1,000 measured turns per run, two calls per turn, eight context
items and eight retained turns. All 14,000 measured turns completed. Every run
sent identical normalized payloads and 3,465,230 request bytes. A separate seven
pairs use the preceding retry-heavy contract: one warmup and five measured
turns per run, each with 100 billed failures and 100 successful tool rounds.
All 70 measured turns reached the expected round limit with equal durable
usage/retry totals; every run sent 1,000 requests and 12,965,452 normalized
request bytes with matching payload hashes. This uses the existing nested
error format so both binaries perform equivalent work; top-level errors fail
prematurely on the baseline and therefore cannot form a throughput comparison.

| Median across seven runs | Before | Fixed |
| --- | ---: | ---: |
| Ordinary echo: daemon CPU, ms/turn | 1.897 | 1.886 |
| Ordinary echo: per-run p95 latency, ms | 2.969 | 2.921 |
| Ordinary echo: sampled peak RSS, MiB | 13.172 | 13.141 |
| Retry-heavy: daemon CPU, ms/turn | 132.835 | 133.610 |
| Retry-heavy: per-run mean latency, ms | 190.813 | 191.681 |
| Retry-heavy: sampled peak RSS, MiB | 12.906 | 12.922 |

Ordinary CPU changed by -0.55% and p95 by -1.63%; retry-heavy CPU changed by
+0.58% and mean latency by +0.46%. Sampled memory changed by -32 KiB and
+16 KiB. These small variations indicate effectively flat overhead, not a
speedup claim.

Validation: 62 Rust tests, 18 focused runtime tests, strict all-feature Clippy,
formatting, and diff checks passed. The new classification regression fails
before the fix. Runtime coverage verifies that a top-level error with different
message wording paces, retries, completes, and records one retry; non-rate-limit
errors remain terminal. All provider traffic was synthetic and local. Scripts,
captures, and full hashes are ignored under `.local/top-level-rate-fix/`.

## Fleet controller slice regression check

Observed 2026-09-17 on the same Darwin arm64 host, external power, Rust
1.98.0. The 32-agent socket echo workload as before, two series of one
excluded warmup and three measured runs, on binary `f3001f7f…` (follow-all,
any-mode waits, the `stats` op, and storage-worker counters). All runs passed
with 32 concurrent provider requests and no quality warnings. On this path the
slice adds three clock reads and three relaxed atomic adds per storage job, and
one hash lookup per fan-out for `*` followers.

| Metric | Pacing tree (Astra's review) | Series 1 | Series 2 |
| --- | ---: | ---: | ---: |
| Sampled peak target RSS, MiB | 17.52 (17.25–17.94) | 17.58 (17.52–17.73) | 17.78 (17.66–17.92) |
| Observed target CPU, seconds | 0.294 (0.294–0.311) | 0.315 (0.312–0.335) | 0.326 (0.296–0.351) |
| Per-run p95 turn latency, ms | 594.9 (593.6–600.6) | 617.2 (607.6–618.4) | 606.1 (595.4–624.8) |

The two series disagree with each other by as much as they disagree with the
baseline, and the second's ranges (0.296 to 0.351 s, 595 to 625 ms) span every
earlier reading of this screen. The host was not quiet: during the screens
the window server, a media analysis daemon, and a system predictor were each
using a third to a half of a core. Nothing added here runs per byte or per
message; the recorded cost is within this screen's own noise on this host,
and the sequential 1,000-turn screen Astra used for the pacing fixes is the
better instrument if a difference needs to be established. Captures:
ignored `.local/bench/slice-controller-socket-32/` and `-32b/`.

## Controller fixes and CLI consistency

Observed 2026-09-17 on Darwin arm64, external power, Rust 1.98.0. The
uncommitted controller baseline `f3001f7f…` is compared with final binary
`80941699…`. The fixes publish creation/fork events, preserve global replay
gaps after deletion, and correct `wait --any` exit status. CLI validation,
help, and JSON formatting now follow [one documented contract](CLI.md),
without a new dependency. Retention checks only whether a bot exists instead
of copying its full configuration. A single persistent watermark replaces
the fleet scan on each global replay page.

Seven rotating rounds compare the original, correctness-only (`5fab5e5e…`),
and final builds. Each run excludes 16 warmup turns and measures 1,000 turns,
two model calls per turn, eight context items, and eight retained turns.
All 21,000 measured turns completed; every run sent 2,000 requests with equal
normalized payload hashes and 3,697,230 normalized request-body bytes.
Provider and observer costs are outside daemon CPU/RSS. RSS is sampled every
10 ms; latency runs from submission through receipt of the terminal event.

| Median across seven runs | Original | Final |
| --- | ---: | ---: |
| Ordinary echo: daemon CPU, ms/turn | 2.023 | 1.983 |
| Ordinary echo: per-run p95 latency, ms | 3.607 | 3.512 |
| Ordinary echo: sampled peak RSS, MiB | 13.203 | 13.203 |
| Global replay: daemon CPU, ms/operation | 2.201 | 0.067 |
| Global replay: per-run p95 latency, ms | 2.973 | 0.236 |
| Global replay: ending RSS, MiB | 13.094 | 10.609 |

Ordinary CPU changed by -1.98%, p95 by -2.65%, and median peak RSS was
unchanged. These are effectively flat results, not a throughput speedup
claim: ordinary CPU ranges overlap (1.871–2.198 versus 1.892–2.047 ms/turn),
and one final run had a 9.646 ms p95. The correctness-only build's CPU median
was 1.995 ms/turn with the same median RSS; the small existence-check change
does not establish an independent speedup.

The separate global replay screen uses 10,000 **stored, idle** bots, with
16 warmups and 500 measured operations per run, seven alternating pairs.
Each operation subscribes from the latest cursor, receives `follow_live`,
and unsubscribes on the same socket; it verifies that no durable events are
replayed. Neither build calls a provider. CPU fell 96.97%, p95 fell 92.06%,
and ending RSS fell 2.484 MiB. This measures cursor catch-up across fleet
metadata, not active-agent capacity or full-transcript replay throughput.
The final query reads one watermark row instead of scanning every bot.

An experiment caching five additional retention statements was rejected:
in its seven-round comparison, CPU increased from 1.919 to 2.172 ms/turn
and median peak RSS from 13.219 to 13.453 MiB. Those extra cached statements
are not in the final implementation. The schema-14 migration scans existing
event IDs once to reconstruct older gaps; migration time is outside these
steady-state measurements.

All traffic is synthetic and local. Scripts, full hashes, raw measurements,
and rejected-experiment evidence are ignored under `.local/controller-fixes/`.
Validation on the final implementation: 65 Rust tests, 74 focused Python
integration tests, strict all-feature Clippy, formatting, and diff checks passed.

## Stats accounting fixes

Observed 2026-09-17 on Darwin arm64, external power, Rust 1.98.0. The
preceding binary `80941699…` is compared with `b6bc4ac2…`. Stats now reports
the shared transport's request loads once, retains provider-scoped model
pools, counts the stdio session, and reads database/WAL sizes using the
canonical path already resolved at open. No model-request counters, locks,
or path lookups were added. Transport load vectors fall from one per provider
to one per stats response.

Both screens use seven alternating before/after pairs, run sequentially.
Stats polling registers eight providers sharing 64 HTTP client shards,
excludes 32 warmup requests, then measures 4,000 stats requests per run.
All providers and pools remain idle; it verifies the same zero-load state
through each build's response shape. No provider calls occur. Ordinary echo
uses the preceding screen's 16 warmups, 1,000 measured turns per run, eight
context items, and eight retained turns. All 14,000 measured turns completed;
each run made 2,000 requests with equal normalized payload hashes and
3,697,230 normalized request-body bytes. Observer and synthetic provider
CPU/RSS are excluded.

| Median across seven runs | Before | After |
| --- | ---: | ---: |
| Stats: daemon CPU, ms/operation | 0.0485 | 0.0388 |
| Stats: per-run p95 latency, ms | 0.141 | 0.103 |
| Stats: ending RSS, MiB | 10.875 | 11.000 |
| Stats: encoded transport/provider fields, bytes | 1,315 | 328 |
| Ordinary echo: daemon CPU, ms/turn | 1.925 | 1.949 |
| Ordinary echo: per-run p95 latency, ms | 3.123 | 3.273 |
| Ordinary echo: sampled peak RSS, MiB | 13.203 | 13.219 |

Stats polling uses 20.0% less CPU, its p95 falls 27.3%, and its
transport/provider fields shrink 75.1%. Those bytes exclude the rest of the
response and its changing counters. Ending stats RSS rises 128 KiB, so this
is a CPU and response-size improvement, not a demonstrated memory saving.
Ordinary echo CPU rises 1.2%, p95 rises 4.8%, and peak RSS rises 16 KiB.
The ordinary CPU ranges overlap (1.853–2.008 versus 1.857–2.055 ms/turn),
as do p95 ranges (3.000–4.094 versus 3.009–3.548 ms). This screen does not
establish a meaningful ordinary-turn performance change or an active-fleet
capacity improvement. RSS is sampled every 10 ms for echo and once after
polling for stats; it is not a live-heap measurement.

Captures and scripts: ignored `.local/stats-fixes/`. Regression tests cover
an active request shared by two providers and WAL reporting through an alias,
including after that alias is removed. Validation: 65 Rust tests, 49 Python
CLI/daemon/runtime tests, strict all-feature Clippy, formatting, and diff
checks passed.

## Reservation labels and zero-timeout polling

Observed 2026-09-17 on Darwin arm64, external power, Rust 1.98.0. Baseline
`b6bc4ac2…` is compared with `43b61a78…`. Stats labels the existing pacing
counter `reserved_requests`; it adds no request-path bookkeeping. Zero-timeout
waits now return ready outcomes and pending handles in the CLI, protocol, and
tool. After checking outcomes, an expired deadline completes directly without
allocating a timer task or waiting for a timer tick.

Seven alternating before/after pairs each exclude 16 warmup turns and measure
1,000 synthetic echo-tool turns, with eight context items and eight retained
turns. Only `echo` is registered in both builds so the changed wait-tool
description/schema does not change the comparison's requests. All 14,000
measured turns completed; every run sent 2,000 requests with equal normalized
payload hashes and 2,097,230 normalized request-body bytes. CPU/RSS cover the
daemon only, with RSS sampled every 10 ms; latency runs from submission to
receipt of the terminal event. This is an ordinary-turn regression screen,
not a measurement of the new polling operation versus the old validation error.

| Median across seven runs | Before | After |
| --- | ---: | ---: |
| Daemon CPU, ms/turn | 1.8160 | 1.8169 |
| Per-run p95 latency, ms | 2.925 | 2.935 |
| Sampled peak RSS, MiB | 13.203 | 13.109 |

CPU changed by +0.05%, p95 by +0.36%, and median peak RSS fell 96 KiB.
Treat this as effectively flat ordinary-turn performance, not a speedup:
CPU ranges overlap (1.783–1.843 versus 1.813–1.854 ms/turn), as do p95
ranges (2.859–3.247 versus 2.876–3.101 ms). Captures and scripts are in
ignored `.local/poll-fixes/`.

Validation: 65 Rust tests, 51 Python CLI/daemon/wait tests, strict all-feature
Clippy, formatting, and diff checks passed. Regressions verify completed and
pending CLI polls, mixed tool results, pending process handles, waiter cleanup,
and a request whose pacing reservation ends while its stream remains active.

## Background admission bound

Observed 2026-09-18 on the same Darwin arm64 host, external power, Rust
1.98.0. The 32-agent socket echo workload as before, one excluded warmup and
three measured runs, on binary `46c617f2…` (background admission refused with
`capacity_exhausted` once as many jobs wait for a slot as can run, the
`queued_processes` stats field, and request read-ahead batches capped at 256
KiB as well as 64 items). The comparison binary `43b61a78…` is the tree at
the previous commit. All runs passed with 32 concurrent provider requests and
no quality warnings. On this path the slice adds one relaxed atomic load per
background start and one atomic decrement when the job takes its slot; the
read-ahead cap only sums per-item sizes the window query already returns.

| Metric | Previous commit | This slice |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.69 (17.50–17.69) | 17.73 (17.56–18.44) |
| Observed target CPU, seconds | 0.324 (0.322–0.329) | 0.299 (0.295–0.304) |
| Per-run p95 turn latency, ms | 596.8 (595.0–600.4) | 591.7 (590.6–642.4) |

Flat within this screen's noise; the echo workload starts no background
processes, so the admission check is never reached, and its windows are far
below the byte cap. The bound's behavior is covered by the wait-tool
regressions (a refused third job under a budget of one, and eight jobs
completing through a four-slot queue). Captures: ignored
`.local/bench/slice-backlog-socket-32/` and `slice-astra-controller-socket-32/`.

## Delivery modes

Observed 2026-09-18 on the same Darwin arm64 host, external power, Rust
1.98.0. The 32-agent socket echo workload as before, one excluded warmup and
four measured runs. Three binaries, screened back to back within a few
minutes: the previous commit `fd8ad1d8…` rebuilt from a worktree, the slice
with a storage round trip at every round boundary (`b29bb992…`), and the
slice as committed (`e4e7bccc…`), where the store keeps an exact count of
queued steers and a boundary with nothing to absorb costs one atomic load.
The workload queues nothing and steers nothing, so this is the cost the
mechanism adds to ordinary turns: three boundary checks per two-round turn,
plus one indexed `UPDATE` that finds no queued turn at each finish.

| Metric | Previous commit | Round trip per boundary | As committed |
| --- | ---: | ---: | ---: |
| Sampled peak target RSS, MiB | 17.59 (17.52–17.72) | 17.76 (17.72–17.81) | 17.79 (17.77–17.81) |
| Observed target CPU, seconds | 0.337 (0.331–0.357) | 0.337 (0.323–0.350) | 0.323 (0.316–0.333) |
| Per-run p95 turn latency, ms | 628.6 (618.2–646.0) | 619.5 (613.4–627.0) | 619.5 (616.7–640.6) |

The host was busier than for the admission-bound screen a few hours earlier
(the same previous-commit tree then read 0.299 s and 592 ms), which is why
the three are compared against each other and not against that section. As
committed, CPU and p95 are level with the previous commit within the
screen's noise; RSS is up about 200 KiB, inside the range this screen has
shown for one binary. Captures: ignored `.local/bench/slice-prev-socket-32/`,
`slice-delivery-socket-32/`, and `slice-delivery-b-socket-32/`.

Validation: 68 Rust tests including two store contracts for the line (queue,
steer, ready, promotion at finish, ending a queued turn, the exact steer
count) and 131 Python tests including five daemon-level delivery tests
(ordering, steer at a shell boundary, a steer during the final call, ready
turns under `--max-active 1` with interrupt and delete, restart) and one CLI
test for `run --delivery`.

### Delivery review fixes

Observed 2026-09-18 on Darwin arm64, external power, Rust 1.98.0. The saved
pre-fix binary is `1a432379…`; the final candidate is `796e1c41…`. The fixes
preserve queued cancellation while a bot runs, wake queued successors after
parked interruption or retirement of an already-released task, preserve FIFO
admission when capacity opens, and preserve one ready head on restart.
Retention protects unfinished work and its preceding retained turns. Completion
is captured before pruning so a later steer's row cannot erase a current
waiter's answer.

Admission uses one cached, indexed ready-head lookup on an idle bot. The
retention boundary uses existing active/queued/ready indexes and a cached
statement, with no new index or schema. The query-plan audit planned 113 runtime
statement variants with no growing-table scans (Python SQLite 3.47.1).

Three matched synthetic screens, each with one excluded warmup and three
measured runs per binary. Values below are medians; these small screens do not
establish a general speedup or a tail-latency guarantee.

| Workload | CPU seconds, before → after | Peak RSS MiB, before → after | Per-run p95 ms, before → after |
| --- | ---: | ---: | ---: |
| 32-bot socket echo lifecycle | 0.330 → 0.289 | 17.828 → 17.859 | 590.9 → 590.4 |
| 16 bots, 128 submissions, 112 queued | 0.175 → 0.174 | 13.203 → 13.078 | 104.1 → 105.1 |
| 16 bots, 128 turns, retention 1 | 0.183 → 0.187 | 12.750 → 12.891 | 13.9 → 12.9 |

The socket screen uses the existing lifecycle boundary and observer. Every run
completed 96 turns with 32 simultaneous provider requests and no invalid
requests. Its candidate p95 ranged from 585.7 to 682.5 ms, versus 589.9–590.9 ms
for the baseline; one slower candidate run remains visible despite similar
medians. Candidate RSS ranged from 17.797 to 18.281 MiB. Earlier alternating
screens also varied in CPU and latency, so the lower final CPU median is not
attributed to an optimization of ordinary turns.

The two additional screens alternate binaries against a local synthetic
Responses fixture, with FULL SQLite durability and default limits. They measure
daemon CPU and sample daemon RSS every 10 ms, excluding fixture/controller
resources and daemon startup. The queue screen gates each bot's first request
until all 128 submissions are accepted, verifies exactly 112 queued admissions,
and checks all 128 final answers. Timing spans submission through completion;
answer verification follows the timed interval. The retention screen drains and
checks each 16-turn round before the next, including result reads in its timed
interval. It exercises retention without the pre-fix queue crash. Queue plus
retention, steering, and cancellation are covered by regressions, not ranked
against the failing baseline.

An initial retention screen showed extra CPU. In the subsequent three-way
comparison, CPU medians were 0.183 s for baseline, 0.192 s for the uncached
candidate (`6d4b3274…`), and 0.187 s for the final cached candidate. Baseline
CPU ranged from 0.180 to 0.194 s; the final candidate ranged from 0.186 to
0.187 s. Caching avoids repeated SQL preparation, but the small samples do
not establish a broader throughput improvement. Final queued-work CPU and
latency ranges overlap the baseline; retention's median RSS increase is
144 KiB, and the socket median increase is 32 KiB.

Validation: 71 Rust tests, strict Clippy, and 72 focused Python tests across
delivery, CLI, wait, and runtime behavior, with affected tests rerun after the
final changes. Regression coverage includes queued and steer cancellation,
parked and released-slot completion wake-ups, retention of live tool intents,
completion under queued/steered retention, FIFO admission, and ready-head
recovery. Captures and bounded probe scripts are in ignored
`.local/delivery-fix/`: `baseline-cached-screen/`, `candidate-cached-screen/`,
`queue-cached-results.json`, `retention-cached-results.json`, and
`queue_bench.py`. Earlier diagnostic screens are retained there too.

### Bounded steering absorption and explicit overrides

Observed 2026-09-18 on Darwin arm64, Rust 1.98.0. The saved pre-fix release
binary is `796e1c41…`; the final candidate is `d14b6879…`. Incompatible explicit
workspace/model overrides now leave a steer queued to run separately, without
letting later steers overtake it. Each round boundary drains a finite snapshot
through storage batches capped at 32 steers and 256 KiB of raw UTF-8 prompts.
Each batch is released before the next is loaded. A partial queued-steer index
keeps these reads independent of ordinary queued work; the query-plan audit
planned 114 statement variants with no growing-table scans (SQLite 3.47.1).

The matched release-daemon screen gates the first request for each of 16 bots,
then queues seven steers per bot before releasing the gate. Both binaries
completed 16 original turns, absorbed 112 steers, returned the expected final
answers, and made exactly 32 provider requests. The fixture also checked the
request histories: 16 contained one user item and 16 contained eight. Batching
does not add provider calls; a separate 70-steer regression verifies this across
multiple storage batches.

One warmup per binary was excluded, followed by three measured runs per binary
in alternating order. Values are medians (ranges). The stdio daemon uses FULL
SQLite durability and default limits against a synthetic Responses fixture.
Daemon CPU is measured after bot creation through completion, RSS is sampled
every 5 ms, and latency runs from submission to receipt of the original turn's
terminal event. Startup, bot creation, and controller/fixture resources are
excluded.

| Metric | Before | After |
| --- | ---: | ---: |
| Daemon CPU, seconds | 0.0605 (0.0588–0.0668) | 0.0633 (0.0567–0.0672) |
| Sampled peak daemon RSS, MiB | 13.063 (13.047–13.063) | 13.063 (13.047–13.109) |
| Per-run p95 turn latency, ms | 65.7 (62.7–67.1) | 65.9 (60.7–71.7) |

Memory and latency medians are similar. The CPU median increased by 2.8 ms,
with overlapping ranges; this short screen establishes neither a CPU speedup
nor a general performance non-regression guarantee.

A separate debug-build storage probe drains the entire backlog of 128 prompts,
each 256 KiB, in both versions. Seeding is excluded. Both use a file-backed
store with a 2 MiB SQLite cache; the baseline absorbs everything in one call,
while the candidate releases each bounded result before continuing. This
changes one large transaction into 128 bounded transactions. Three runs per
binary alternate order; RSS is sampled every 2 ms after seeding through exit.

| Storage probe metric | Before | After |
| --- | ---: | ---: |
| Sampled peak RSS, MiB | 47.188 (47.031–47.906) | 15.156 (15.078–15.156) |
| Absorption CPU, seconds | 1.238 (1.229–1.245) | 1.184 (1.184–1.188) |

The same full-backlog storage work uses about 68% less peak RSS. These are
debug storage-probe measurements, not daemon capacity or model-context results.
A one-prompt control measured median peak RSS of 11.109 → 11.453 MiB and CPU
of 0.0131 → 0.0132 seconds, with overlapping ranges. Captures and bounded probe
scripts are in ignored `.local/steer-fix/`: `bench.py`, `memory.rs`,
`runtime-results.json`, and `memory-results.json`. Full release-binary hashes
are recorded in the runtime capture.

Validation: 73 Rust tests, strict Clippy, and 74 focused Python tests across
delivery, CLI, wait, and runtime behavior. The affected ten-test delivery suite
was rerun after the final snapshot refinement. New regressions cover explicit
workspace/model overrides, deferred-steer ordering, byte and item limits,
late-arrival exclusion from the current snapshot, and exact provider-call
counts across multiple batches.

### Cancellation-safe steering completion and queued retention

Observed 2026-09-18 on Darwin arm64, AC power, Rust 1.98.0. Saved pre-fix
release binary: `d14b6879…`; candidate: `f4fe934b…`. Interrupting an absorbing
turn now finishes its in-flight bounded batch, including event publication and
waiter notifications, then stops before another batch. Two inline atomic flags
coordinate the execution future with its cancellation branch; there is no
additional task, allocation, store query, or provider call on ordinary turns.
Cancelled and failed queued work now applies configured retention in the same
storage job as completion, after capturing the outcome for existing waiters.

The deterministic cancellation regression blocks event output after the first
32 steers commit, interrupts the absorbing turn, then releases output. All 32
terminal events and steer events arrive, all registered waiters resolve, and
the next batch stays queued. It failed before the fix. The original daemon
probe also found no stranded steered outcomes in six candidate runs. With
`--retain-turns 1`, cancelling eight queued turns now leaves only the newest
turn's terminal event; an existing waiter for the oldest still receives its
captured result even when that completion prunes itself.

The two matched release screens use 16 bots and 128 submissions against a
synthetic Responses fixture, FULL SQLite durability, and default limits.
The steering screen above verifies 112 steered outcomes, 16 completed turns,
32 provider calls, and exact request history counts. The queue screen verifies
112 queued admissions and 128 completed turns with their expected final text.
It gates the first request per bot until all submissions are accepted. Daemon
CPU excludes startup and bot creation; daemon RSS is sampled every 5 ms for
steering and 10 ms for queueing. Latency spans submission through terminal-event
receipt. Controller and fixture resources are excluded. Retention is disabled
in both performance screens; the formerly incorrect cancellation/retention
path is validated for behavior, not ranked as equivalent work.

An initial five-run alternating screen, after one excluded warmup per binary,
showed median CPU of 0.0581 → 0.0623 s for steering and 0.1618 → 0.1768 s for
queueing. Queue p95 increased from 100.8 to 108.9 ms. This prompted a second
alternating screen of ten measured runs per binary, again excluding one warmup.
Repeat-screen values below are medians (ranges):

| Metric | Before | After |
| --- | ---: | ---: |
| Steering CPU, seconds | 0.0632 (0.0578–0.0687) | 0.0620 (0.0544–0.0658) |
| Steering peak RSS, MiB | 13.125 (12.953–13.141) | 13.070 (12.969–13.125) |
| Steering per-run p95, ms | 65.2 (61.5–78.8) | 64.7 (55.4–111.0) |
| Queue CPU, seconds | 0.1738 (0.1581–0.1839) | 0.1790 (0.1634–0.1873) |
| Queue peak RSS, MiB | 13.211 (13.078–13.250) | 13.234 (13.078–13.281) |
| Queue per-run p95, ms | 107.3 (97.4–113.6) | 109.3 (102.1–114.9) |

The larger initial CPU difference did not repeat. Memory is similar; the
repeat queue CPU median is about 3% higher, with overlapping ranges. One
candidate steering run has a 111 ms p95 outlier. These observations establish
neither a general speedup nor a tail-latency non-regression guarantee.

Validation: 74 Rust tests, strict Clippy, 75 focused Python tests across
delivery, CLI, wait, and runtime behavior, formatting and diff checks. Captures,
full binary hashes, and probe scripts are in ignored `.local/completion-fix/`:
`steering-results.json`, `queue-results.json`, `steering-repeat-results.json`,
`queue-repeat-results.json`, and `interrupt-probe.log`.

## Explicit configuration

Observed 2026-09-18 on the same Darwin arm64 host, external power, Rust
1.98.0. The 32-agent socket echo workload as before, one excluded warmup and
four measured runs. The store no longer binds a provider set or toolset at
open; a turn compares its provider's family with the bot's at start, and the
client compares its stated daemon flags with `ready` at attach. Neither runs
per byte or per message. A first screen of the slice (`fad51e30…`, CPU
0.374 s) read well above the delivery slice's 0.323 s, so the committed tree
`e1ebc942…` was rebuilt from a worktree and screened back to back with the
slice binary.

| Metric | Committed tree | This slice |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.80 (17.66–17.89) | 17.96 (17.91–18.00) |
| Observed target CPU, seconds | 0.365 (0.335–0.371) | 0.373 (0.362–0.379) |
| Per-run p95 turn latency, ms | 618.3 (612.1–626.9) | 620.9 (615.4–625.0) |

The committed tree itself moved from 0.323 s to 0.365 s between the two
sessions, so the first reading was the host, not the slice. Side by side, CPU
and p95 are level within the screen's noise; RSS is up about 160 KiB, which
this screen has shown between runs of one binary before. Captures: ignored
`.local/bench/slice-prev2-socket-32/`, `slice-config-socket-32/`, and
`slice-config-b-socket-32/`.

Validation: 74 Rust tests, strict Clippy, formatting, and 139 Python tests,
including one runtime test that reopens a store without a bot's provider,
with the same provider name under another family, and with the provider back,
and one CLI test that attaches with unstated, restated, and mismatched
providers, tools, limits, and model.


## Configuration admission fixes

Observed 2026-09-18 on Darwin arm64, external power. Compared the pre-fix
working-tree binary `7b6fd12b…` with `33dcd025…`. Provider validation now runs
inside the existing admission/start storage job, before transcript mutation;
explicit model overrides no longer need a separate bot-inspection job. Duplicate
requests reconcile before validation. Client limit checks compare normalized
values and share the daemon's context minimums.

Four measured 32-agent socket echo runs per binary, each preceded by an excluded
warmup, alternating before/after order. Same workload, tools, observer, full
SQLite durability, follower/replay equality, resume and fork checks. All runs
completed 96 turns with no invalid provider requests. Values are medians of
per-run metrics, with ranges in parentheses.

| Metric | Before fix | After fix |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.766 (17.656–17.875) | 17.625 (17.562–17.641) |
| Observed target CPU, seconds | 0.3307 (0.3237–0.3309) | 0.3198 (0.3119–0.3269) |
| Per-run p95 turn latency, ms | 595.2 (591.0–597.6) | 591.1 (589.1–592.6) |

A separate warm-daemon CLI attachment screen ran `stats` with explicit matching
provider, model, tool and context flags. Each of four groups per binary excluded
five warmups and measured 40 invocations. Wall time includes process startup,
configuration validation, the stats request and JSON output. CPU and peak RSS
are per-client `wait4` measurements, excluding the already-running daemon and
Python controller. The table reports medians of each group's medians.

| CLI metric | Before fix | After fix |
| --- | ---: | ---: |
| Wall time, ms | 8.799 | 8.663 |
| CPU, ms | 5.530 | 5.437 |
| Peak RSS, MiB | 6.703 | 6.734 |

This screen shows no material slowdown; daemon CPU and RSS are modestly lower.
CPU and latency ranges overlap, so this is not a general speedup claim. The
client's temporary peak RSS rose by about 32 KiB. Network work and completed
lifecycle contracts match. These short synthetic conversations do not establish
long-history or large-fleet capacity.

Validation: 74 Rust tests, strict Clippy, 57 Python tests covering CLI, runtime,
and delivery, formatting and diff checks. Regression coverage includes disabled
idle exit and clamped limits, unchanged history on provider rejection, queued
work after a provider change, and idempotent replies after configuration changes.
Captures, binary hashes, measurement script and summaries are in ignored
`.local/config-fixes/` (`results.json`, `summary.json`, `measure.py`).


## Inherited-steer provider validation

Observed 2026-09-18, Darwin arm64 on external power, Rust 1.98.0. Compared
`33dcd025…` before the fix with retained candidate `40f6e761…`. An omitted
steer model can inherit the active override when the default provider is
unavailable or incompatible. The existing admission job performs one cached
primary-key lookup only after default validation fails; ordinary submissions
and steers with valid defaults gain no query, storage round trip, or retained
per-agent state. A steer that starts separately still validates its default
before appending its prompt.

The matched screen uses 16 active bots, seven steers per bot (112 total),
full SQLite durability, and a gated synthetic provider. Every run verifies 16
completed turns, 112 absorbed steers, exactly 32 provider requests, and the
expected input history and final answers. Daemon CPU spans submission through
completion; RSS is sampled every 5 ms. Latency spans initial submission through
completion, including the gate and steer submissions. It uses stdio, not CLI
processes or socket followers.

An initial four-pair screen suggested higher CPU. Ten alternating measured
runs per binary, after one excluded warmup each, did not repeat that result:
CPU medians were 0.05826 to 0.05739 seconds, p95 medians 61.26 to 61.62 ms.
A further ten-run comparison included an experimental cold helper (`1f3596e…`)
that reduced RSS but increased CPU relative to the inline fix; it was discarded.
The final comparison's per-run medians and ranges are:

| Metric | Before | Retained fix |
| --- | ---: | ---: |
| Daemon CPU, seconds | 0.05950 (0.05724–0.08639) | 0.05839 (0.05435–0.06286) |
| Sampled peak RSS, MiB | 13.109 (12.984–13.188) | 13.172 (13.047–13.250) |
| Per-run p95, ms | 63.14 (57.24–128.68) | 62.43 (60.21–96.98) |

CPU and latency overlap the baseline; there is no demonstrated general speedup.
Sampled RSS increased by about 64–120 KiB across the repeated screens. The newly
working missing-default case has regression coverage but no valid pre-fix
performance baseline. These measurements cover the equivalent valid-default
steering path, not long histories or fleet capacity.

Validation: 74 Rust tests, strict Clippy, 33 runtime/delivery tests plus two
query-plan tests. The focused inheritance regression also covers an incompatible
default, explicit matching and mismatching workspaces, an explicitly invalid
model, an idle bot, and cancellation that forces separate execution without
history mutation. Captures and scripts are in ignored `.local/inherited-steer-fix/`:
`runtime-results.json`, `repeat-results.json`, `cold-results.json`, and their
measurement scripts; the `inline` series in the final comparison is retained.

## Commit-ordered publication, per-bot steer flag, strict steering

Observed 2026-09-18 on the same Darwin arm64 host, external power, Rust
1.98.0. Compared the committed tree `8bd5fd5b…` (rebuilt from a worktree)
with the slice binary `c35037ce…`, back to back. The storage worker now
publishes each job's committed events, and the outcomes of turns the job
ended, to one publisher before taking the next job; tasks and the service
publish nothing durable and resolve no waiters. Per job that is one indexed
read past the watermark (empty for most jobs) and, per event, one JSON parse
and one bounded channel send; the task-side entry publishing it replaces is
gone, as is the absorbing turn's cancellation guard. Each live turn carries
its own steer flag, answered by the job that starts or resumes it. A
completion's retention pass keeps that turn's own records.

The 32-agent socket echo workload as before, one excluded warmup and four
measured runs per binary:

| Metric | Committed tree | This slice |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.75 (17.70–17.81) | 17.92 (17.70–18.20) |
| Observed target CPU, seconds | 0.332 (0.327–0.346) | 0.344 (0.331–0.351) |
| Per-run p95 turn latency, ms | 627.3 (618.1–651.2) | 621.7 (618.9–631.9) |

CPU is up 3.6% at the median with overlapping ranges; p95 and RSS are within
noise. The read-back per job is the candidate if that difference holds up on
a quieter host; it is not attributed here.

The one-parked-bot screen (ignored `.local/steer-hint/bench.py`): 48 bots run
ten two-round `shell` turns each against the instant synthetic model, with
and without one further bot parked on a held peer and holding a queued
steer. Daemon CPU and storage jobs span the 480 turns; three alternating
pairs after one warmup, medians:

| Binary | Case | Daemon CPU, s | Storage jobs | Wall, s |
| --- | --- | ---: | ---: | ---: |
| Committed tree | no steer | 0.875 | 6,564 | 0.677 |
| Committed tree | one parked steer | 0.890 | 7,993 | 0.698 |
| This slice | no steer | 0.894 | 6,551 | 0.690 |
| This slice | one parked steer | 0.901 | 6,561 | 0.698 |

On the committed tree one bot's pending steer added about 1,430 storage
jobs to 480 unrelated turns, three per two-round turn, exactly the boundary
checks its global flag forced. On the slice the parked bot adds its own ten
jobs and nothing else. Captures: `.local/bench/slice-prev3-socket-32/`,
`slice-order-socket-32/`, and `.local/steer-hint/{prev,new}.json`.

Validation: 76 Rust tests, strict Clippy, formatting, and 144 Python tests.
New regressions: a store test that the worker publishes only committed rows
in commit order with outcomes after the events that end their turns; a
cancellation test where every committed steer batch is published and its
waiters answered although the task was cancelled, in rising cursor order,
exactly once; a daemon test where the firehose over eight bots with queued
and steered work receives strictly rising cursors equal to the replay;
strict-steer tests at store, protocol, and CLI level; and the two retention
tests adjusted for a completion keeping its own records.

### Bounded publisher shutdown

Observed 2026-09-18 on the same Darwin arm64 host, external power, Rust
1.98.0. A background result task can retain the store after shutdown drops
the service. The publication stream then stays open; timing out an owned
publisher task handle detached it and left its stdout sender alive. Joining
the writer blocked the current-thread runtime indefinitely. Shutdown now
borrows that handle during the existing five-second drain and, on timeout,
cancels and awaits the publisher before joining stdout. The fix adds no
normal-turn queries, messages, allocations, or processing branches.

Compared the pre-fix binary `c35037ce…` with `af3c5811…`: one excluded
warmup each and four alternating measured pairs of the 32-agent socket echo
lifecycle workload, 96 completed turns and 192 synthetic provider requests
per run. SQLite FULL durability, follower/replay equality, restart, resume,
item retrieval, duplicate reconciliation, and historical forks were checked.
The Python controller/provider are outside the target accounting; these
figures do not measure Rust CLI invocation costs or background shutdown time.

| Metric | Before fix | After fix |
| --- | ---: | ---: |
| Sampled peak daemon RSS, MiB | 17.84 (17.83–17.94) | 17.66 (17.62–17.75) |
| Observed target CPU, seconds | 0.344 (0.340–0.357) | 0.334 (0.314–0.356) |
| Per-run p95 turn latency, ms | 629.0 (618.8–645.9) | 621.9 (620.0–623.1) |

No regression observed in this screen. CPU and latency ranges overlap;
these small samples do not establish a general speedup. Captures and the
paired driver are in ignored `.local/publisher-shutdown-fix/`.

The new regression starts a 30-second background command, keeps stdin open
and stdout draining, then shuts down. The baseline exceeded its eight-second
exit deadline; the fixed binary exits within it, releases the store for
restart, and preserves the completed turn's replay. Validation: 76 Rust tests,
20 focused Python tests, strict Clippy, formatting, and diff checks passed.

### Rechecking steers after queued cancellation

Observed 2026-09-18 on the same Darwin arm64 host with Rust 1.98.0. When
an incompatible workspace/model steer blocked absorption, the per-bot flag
was cleared. Cancelling that blocker did not re-arm it, so a strict steer
behind it could fail as stale despite another boundary remaining. The
cancellation job now sets the affected live turn's existing flag after the
queued turn ends. There is no added query in that job or change to ordinary
turn processing; only that bot rechecks its queue at the next boundary.

Compared `af3c5811…` with `c86e8d94…` on the one-parked-bot screen described
above: 48 workers, ten two-round shell turns each, with and without one
additional parked bot holding a steer. One warmup per binary/scenario was
excluded, followed by four alternating measured pairs. Medians:

| Case | Metric | Before | After |
| --- | --- | ---: | ---: |
| Ordinary work | Daemon CPU, s | 0.906 | 0.906 |
| Ordinary work | Storage jobs | 6,564 | 6,560 |
| Ordinary work | Wall time, s | 0.712 | 0.710 |
| Ordinary work | Daemon RSS after work, MiB | 14.41 | 14.46 |
| Unrelated parked steer | Daemon CPU, s | 0.902 | 0.904 |
| Unrelated parked steer | Storage jobs | 6,553.5 | 6,556.5 |
| Unrelated parked steer | Wall time, s | 0.702 | 0.703 |
| Unrelated parked steer | Daemon RSS after work, MiB | 14.55 | 14.46 |

No material steady-work regression observed, and no per-boundary storage
work returned on unrelated bots. This is a daemon-only screen using the
instant synthetic provider, not a peak-memory or cancellation-latency
measurement. One candidate ordinary-work run took 0.996 s; other measured
runs took 0.690–0.738 s. The small samples establish no speedup. Raw runs
and the driver are in ignored `.local/steer-rearm-fix/`.

A deterministic regression holds provider responses around the blocked
boundary, cancels the incompatible head, and checks both absorption into
the original turn and the model's subsequent input and answer. Both workspace
and model variants failed on the baseline and pass with the fix. Validation:
76 Rust tests, 19 focused Python tests, strict Clippy, formatting, and diff
checks passed.

## Explicit model and instructions

Observed 2026-09-18 on the same Darwin arm64 host, external power, Rust
1.98.0. Compared the committed tree `2580e39a…` (rebuilt from a worktree)
with the slice binary `cd07f38f…`, back to back. `serve` no longer takes a
model or instructions; `create` requires both; the CLI resolves them from
`--model` or `AGENT_MODEL` and from `--instructions` or its built-in text;
shell tool children receive `AGENT_MODEL`, the running turn's effective
model, so a bot's peers default to its own. On ordinary turns the only new
work is one environment variable per shell child.

The 32-agent socket echo workload as before, one excluded warmup and four
measured runs per binary:

| Metric | Committed tree | This slice |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.70 (17.59–17.80) | 17.77 (17.73–17.91) |
| Observed target CPU, seconds | 0.336 (0.334–0.339) | 0.342 (0.333–0.347) |
| Per-run p95 turn latency, ms | 619.3 (615.0–622.8) | 621.6 (619.9–622.5) |

Level within the screen's noise on every metric, as expected for a change
that moves a string from the daemon to the client. Captures: ignored
`.local/bench/slice-prev4-socket-32/` and `slice-defaults-socket-32/`.

Validation: 76 Rust tests, strict Clippy, formatting, and 148 Python tests.
New regressions: `create` without a model or without instructions is
refused by name; a bot created with stated values keeps them across a
daemon restarted by a client with other ideas; the CLI refuses a new bot
without `--model` or `AGENT_MODEL` and accepts the environment default;
`stats --model` is a usage error; `--instructions-file` resolves in the
client. The test helpers now state a model and instructions on `create`,
as any client must.

### Existing-peer model fix

Observed 2026-09-18 on Darwin arm64, external power, Rust 1.98.0.
Compared the pre-fix working tree binary `7850585e…` with `ab2e6f78…`.
The client now reads `AGENT_MODEL` only on creation; continuing a peer
keeps its stored model unless `--model` explicitly overrides that turn.
This removes an environment lookup and string allocation from continuation
and inspection commands, with no extra RPC or storage work.

Two synthetic local screens, each with one excluded warmup per binary and
four measured runs, alternating binary order:

- Peer screen: 16 callers each submit four detached turns through shell/CLI
  to 16 existing peers. All 128 turns complete, with 192 provider requests.
  Whole-tree CPU uses waited-child resource accounting, including the daemon,
  shells and CLI processes; provider/controller work is excluded.
- CLI screen: 32 sequential detached continuations, with completion checked
  after each. Each binary sends exactly 113,248 provider-request bytes.
  macOS `time -l` records each CLI's actual peak RSS. CPU includes the
  daemon, CLI processes and the identical timing wrappers.

| Metric, median across measured runs | Before | After |
| --- | ---: | ---: |
| Peer screen CPU, seconds | 0.678 | 0.679 |
| Peer screen p95 parent-turn latency, ms | 45.6 | 42.9 |
| CLI screen median per-process peak RSS, MiB | 6.67 | 6.62 |
| CLI screen CPU, seconds | 0.248 | 0.256 |
| CLI screen median invocation time, ms | 8.90 | 9.05 |

Peer CPU is level; CLI peak memory is slightly lower in this screen.
CPU and latency ranges overlap, so these short screens establish no speedup.
Whole-tree RSS sampling misses short-lived CLI processes and varied widely;
it does not establish a peak-memory comparison. The per-CLI measurements
above use actual process high-water marks instead. Captures and probe scripts:
ignored `.local/model-default-fix/` (`performance.json`, `cli-peaks.json`).

The peer regression fails before the fix and passes after it. It covers new
peer inheritance, continuation with a different stored provider/model, explicit
turn overrides, and preservation of the stored choice. Validation: 76 Rust
tests, 28 focused Python tests, strict Clippy, formatting, and diff checks.

## Per-bot tools

Observed 2026-09-18 on the same Darwin arm64 host, external power, Rust
1.98.0. Compared the committed tree `df1d4563…` (rebuilt from a worktree and
screened with its own bench code, since its `create` does not know a `tools`
field) with the slice binary `4682cf7d…`. The daemon registers every tool
this build knows and `serve` takes no `--tools`; `create` requires the bot's
selection; the model is shown that selection and dispatch enforces it with a
`tool_not_available` result; definitions live once in the registry and the
request encoding once per distinct selection and family, in a bounded map.
Per turn that is one map lookup at start and one scan of the bot's names per
tool call.

The 32-agent socket echo workload as before, one excluded warmup and four
measured runs per binary:

| Metric | Committed tree | This slice |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.66 (17.62–17.78) | 17.64 (17.56–17.78) |
| Observed target CPU, seconds | 0.323 (0.298–0.343) | 0.344 (0.315–0.351) |
| Per-run p95 turn latency, ms | 597.1 (591.3–623.0) | 604.6 (594.9–625.8) |

The CPU medians differ by 6% with ranges that overlap almost entirely; RSS
is level and p95 within noise. The two were not run back to back (the
baseline followed a full rebuild), so the median gap is not attributed to
the slice's per-turn lookup. Captures: ignored
`.local/bench/slice-prev5-socket-32/` and `slice-tools-socket-32/`.

Validation: 76 Rust tests, strict Clippy, formatting, and 152 Python tests.
New regressions: `create` refuses an unknown or repeated tool and a missing
selection by name; a bot created with `echo` alone is shown only `echo` and
gets `tool_not_available` when its model calls `shell`, while a bot with
both is shown both; a fork keeps its source's selection; a daemon restarted
with other ideas changes nothing; `run --tools` on an existing bot is a
usage error, as `--instructions` now is; `ls` lists each bot's tools.

### Tool-selection review fixes

Observed 2026-09-18, Darwin arm64 on external power, Rust 1.98.0. Compared
the pre-fix per-bot-tools binary `06fcc5ec…` with `1d3c41a8…`, using the same
current observer and 32-agent socket echo workload above. Run order was
baseline, candidate, candidate, baseline; each batch had one excluded warmup
and two measured runs, giving four measured runs per binary. Builds and tests
finished before measurement. Every run passed, achieved the requested provider
concurrency, and reported no quality warnings.

| Metric | Before fixes | After fixes |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.74 (17.59–17.86) | 17.78 (17.58–17.91) |
| Observed target CPU, seconds | 0.316 (0.298–0.323) | 0.307 (0.302–0.311) |
| Per-run p95 turn latency, ms | 590.7 (586.8–601.0) | 589.5 (587.8–599.6) |

These overlapping ranges show no material regression from the fixes; the
2.8% lower CPU median is not evidence of a speedup. This isolates the review
fixes, not the entire per-bot-tools slice versus the earlier committed runtime.
The workload checks durable replay/follow equality, restart, resume, and forks;
it measures the daemon tree, excluding Python observers and Rust CLI invocations.
Captures and the alternating driver are under ignored `.local/toolset-fix/`.

The migration now refuses populated stores whose tool selection was never
recorded, leaving their data and schema version intact; empty stores migrate.
It uses an existence query only on migration, with no turn-loop work added.
The live fleet driver now supplies its same five tools to every measured bot
and the warmup. Its regression checks actual synthetic provider requests;
this restores the intended workload rather than claiming an efficiency gain
from removing the accidentally advertised `history` tool. No paid calls ran.
Validation: 77 Rust tests, 66 Python tests, strict Clippy, formatting, and diff
checks. Both new regression assertions failed before the fixes.

## Cache-hit accounting

Observed 2026-09-18 on Darwin arm64, external power, Rust 1.98.0. The store
now keeps `cached_input_tokens` per turn and `input_tokens` plus
`cached_input_tokens` per bot, both reporting `cache_hit`, and `stats`
carries daemon-lifetime totals from three atomics. Per usage report that is
two more columns in the same `UPDATE`s and three relaxed adds; no new
statement, hop, or index.

The live check is one long conversation per provider through the real
endpoints (ignored `.local/cache-hit/run.py`): 48 turns of a fixed
500-byte prompt answered with one word, `--context-bytes 12288` so the
window's hysteretic start moves several times during the run, and the ratio
read back from `turns` after each turn. Spend was cheap tokens only.

| | gpt-5.6-luna | claude-sonnet-5 |
| --- | ---: | ---: |
| Overall cache hit, whole conversation | 0.724 | 0.851 |
| Turns whose prefix missed (window moved) | 13 of 48 | 6 of 48 |
| Cache hit on the other turns, median (min) | 0.911 (0.797) | 0.951 (0.938) |
| Largest request, input tokens | 3,672 | 7,927 |
| Turn latency, median, miss / hit, s | 1.04 / 0.80 | 1.48 / 1.57 |

The shape is the same on both: between window moves the provider serves
everything but the new prompt from its cache, and every move costs one turn
that misses in full, because the request prefix after the instructions
begins with a different item. The miss turns fell at
0, 1, 2, 3, 12, 16, 20, 24, 29, 32, 38, 43, 46 on luna (the first four are the cache warming up
below the provider's minimum prompt) and at 0, 20, 26, 32, 38, 44 on
Sonnet, whose tokenizer fits about twice as many turns in the same bytes.
So the hit ratio is set by move frequency, and move frequency by the
three-quarters target: dropping a quarter of the window per move here meant
a full miss every three turns on luna and every six on Sonnet. A lower
target would move less often, at the price of less context on the average
turn; with luna's numbers, a half-window target would raise the overall hit
from about 0.72 to about 0.83 while the average request shrank by a sixth.
Whether that trade is worth it is a context-quality question (item 15), not
a cost one, and the rule stays at three quarters until that evaluation
exists. Captures: `.local/cache-hit/luna.json` and `sonnet.json`.

The 32-agent socket echo screen, committed tree `a2140ed5…` (rebuilt from
a worktree, screened with its own bench) against the slice binary
`b28804b6…`, one excluded warmup and four measured runs each: RSS 17.75
(17.58–17.91) versus 17.90 (17.84–17.92) MiB, CPU 0.345 (0.328–0.351)
versus 0.339 (0.320–0.351) s, p95 604.3 (596.8–641.7) versus 604.6
(591.9–617.3) ms. Level within noise, as two more columns in existing
updates and three atomic adds should be. Captures: ignored
`.local/bench/slice-prev6-socket-32/` and `slice-cache-socket-32/`.

Validation: 78 Rust tests, strict Clippy, formatting, and 153 Python tests.
New regressions: a store test for per-turn and per-bot cached counts
and the ratio's rounding and zero case; a daemon test where a fixture that
reports cached tokens on one turn shows the expected per-turn, per-bot,
listing, and `stats` figures.

### Cache-accounting review fixes

The daemon now counts usage once when each provider attempt returns. Recording
a completion's usage after its output was rejected by storage does not add it
again. Schema-19 migration reconstructs cached counts from retained usage
events and validates them against existing turn and bot totals. Pruned or
invalid usage that prevents reconstruction fails the opening transaction with
`store_migration_usage_unavailable`, preserving the store and its version.
Backfill streams events through the turn/kind index, uses prepared updates,
and never loads conversation content. Current-schema startup and the turn loop
do not run the backfill.

Observed 2026-09-18. The matched 32-agent socket echo screen compared the pre-fix binary
`24d3af43…` with `492be79e…`, on Darwin arm64, external power, Rust 1.98.0.
Both used the same current observer. Order: baseline, candidate, candidate,
baseline; each batch had one excluded warmup and two measured runs. Builds and
tests finished before measurement. All runs passed replay/follow equality,
restart, resume, and fork checks, achieved requested provider concurrency, and
reported no quality warnings.

| Metric | Before fixes | After fixes |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.87 (17.66–17.98) | 17.85 (17.72–18.00) |
| Observed target CPU, seconds | 0.305 (0.282–0.315) | 0.288 (0.284–0.296) |
| Per-run p95 turn latency, ms | 588.4 (588.1–615.1) | 590.0 (587.0–606.8) |

Ranges overlap. No material regression is visible; the 5.5% lower CPU median
does not establish a speedup. This measures these fixes against the cache
accounting slice, not the entire slice against an earlier runtime. The charged
boundary is the daemon tree; Python observers and Rust CLI invocations are
outside it. Captures and the alternating driver: ignored `.local/cache-fix/`.

A separate synthetic migration screen seeded 100 bots with one retained usage
event per turn in schema 18, then opened copies with the fixed binary. Across
three runs each, startup through ready **including migration** took 12.9 ms
(12.2–13.3) for 1,000 turns and 30.6 ms (30.2–30.8) for 10,000. RSS at ready
was 11.05 and 13.22 MiB respectively; these are ready samples, not peak-memory
measurements. A bot's migrated totals were checked against the seeded values
in each run. This measures the one-time
reconstruction cost separately from the steady workload.

Validation: 79 Rust tests, 32 focused Python tests, strict Clippy, formatting,
and diff checks. Regressions cover migrated multi-report turns, failed usage,
forks, reopen, rollback when usage was pruned, and exactly-once daemon totals
for rejected completions and failed/retried provider calls. The two new
regressions failed before the fixes. Only synthetic providers were used.

## Paced turns and active slots

Observed 2026-09-19 on Darwin arm64, external power, Rust 1.98.0, binary
`652b9fa`'s tree. Two synthetic providers on one daemon: A refuses every
call with 429 and a one-second Retry-After, so its turns sit in the pacing
gate and retry; B answers instantly. Eight bots on each. Eight A turns are
submitted first and given a second to reach the gate, then eight B turns
with `delivery: queue`; the driver waits fifteen seconds for them
(ignored `.local/pacing-slots/bench.py`).

| `--max-active` | A turns waiting | B admitted as | B finished in 15 s | B latency, max |
| ---: | ---: | --- | ---: | ---: |
| 8 | 0 | running | 8 of 8 | 6 ms |
| 8 | 8 | ready | 0 of 8 | blocked |
| 9 | 8 | ready, then running | 8 of 8 | 4 ms |
| 12 | 8 | ready, then running | 8 of 8 | 6 ms |
| unbounded | 8 | running | 8 of 8 | 8 ms |

A turn waiting on a closed pool holds its active slot for as long as it
retries, up to 64 paced attempts or the 300 s retry budget. When the
throttled provider's turns fill the limit, a healthy provider's work does
not start at all; one free slot is enough for it to proceed, serially. So
the coupling is real and total at the boundary, and it reaches any fleet
that sets an explicit limit and mixes providers: the ten-thousand-bot
screen ran at 1,024, and one throttled provider could have parked 1,024
turns in its gate while every other provider's bots waited as `ready`.

After the fix, the same driver on the slice binary: a turn whose pool is
closed by a rate limit for 250 ms or more parks at the model-call boundary
as a durable `paced` row, holding no task and no slot, and the service
resumes it when its time comes. With the same eight throttled turns:

| `--max-active` | A turns paced | B admitted as | B finished in 15 s | B latency, max |
| ---: | ---: | --- | ---: | ---: |
| 8 | 8 | running | 8 of 8 | 2 ms |
| 9 | 8 | running | 8 of 8 | 7 ms |
| 12 | 8 | running | 8 of 8 | 7 ms |
| unbounded | 8 | running | 8 of 8 | 5 ms |

A throttled turn now costs the limit nothing while it waits; it is live
only for the attempt itself. On ordinary turns the change adds one
comparison per retry decision. Captures: ignored
`.local/pacing-slots/before.json` and `after.json`.

The ordinary-turn screen, committed tree `29cfe068…` (rebuilt from a
worktree, screened with its own bench) against the slice binary
`bbaf9d65…`, one excluded warmup and four measured runs each: RSS 17.59
(17.41–17.64) versus 17.80 (17.75–17.92) MiB, CPU 0.351 (0.347–0.354)
versus 0.344 (0.329–0.354) s, p95 593.4 (591.0–598.6) versus 597.3
(591.6–691.7) ms. CPU and p95 medians are level; one slice run carried a
692 ms p95 outlier with no matching CPU movement, and RSS is up about 200
KiB, both inside what this screen has shown for one binary. Nothing here
runs on an unpaced turn beyond one comparison per retry decision and one
timer arm in the run loop that sleeps while no turn is paced. Captures:
ignored `.local/bench/slice-prev7-socket-32/` and `slice-paced-socket-32/`.

Validation: 79 Rust tests, strict Clippy, formatting, and 156 Python tests.
New regressions: four throttled turns on a four-slot daemon park and a
healthy provider's four queued turns complete within three seconds, with
the paced turn still busy to submit and interruptible; a paced turn
survives a restart, resumes, and ends at the paced-attempt cap with its
retries kept across both segments, after which the bot runs again. The
existing retry and pacing tests hold with retry numbering and pacing time
continuing across a park.

### Paced-admission and retry-scope review fixes

Observed 2026-09-19, Darwin arm64, external power, Rust 1.98.0. The review
found that a call joining an already-closed pool still held an active slot,
and cumulative retries from an earlier model call could exhaust a later
call's budget. Admission now returns long rate-limit waits to the turn for
parking, including FIFO waiters present when another call closes the pool.
The unfinished call's attempts and retry-time budget persist with the park;
the next successful tool round starts a fresh call budget. Cumulative retries
count only dispatched retries. Park state and accounting commit together.

A synthetic four-second pool closure with one active slot compared pre-fix
binary `78ac55d4…` with `74e8cf67…`, in baseline/candidate/candidate/baseline
order. After one turn closed its pool, another bot joined that pool and
healthy-provider work was submitted. Healthy submit-to-finish latency was
4,005–4,012 ms before and 2.3–3.1 ms after. This isolates admission blocking;
it is not a model-throughput measurement.

The ordinary 32-agent socket echo lifecycle screen used the same observer,
three turns per agent, 20 chunks of 256 bytes at 25 ms intervals, a 4 KiB
history fixture, and the `echo,shell,read,write,edit` selection. Four alternating
batches each had one excluded warmup and two measured runs, giving four
measured runs per binary. Builds and tests finished before measurement.
All runs passed the lifecycle checks and reported no quality warnings.

| Metric, median (range) | Before fixes | After fixes |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.73 (17.66–17.81) | 17.90 (17.81–18.06) |
| Observed target CPU, seconds | 0.337 (0.335–0.339) | 0.328 (0.324–0.342) |
| Per-run p95 turn latency, ms | 593.1 (591.8–598.3) | 594.1 (591.5–594.9) |

CPU median is 2.8% lower, but overlapping ranges do not establish a speedup.
P95 is effectively level (+0.18%); RSS median increased 0.16 MiB (+0.9%).
The charged boundary is the daemon and descendants; Python observers and
Rust CLI invocations are excluded. Captures, binaries, and drivers are in
ignored `.local/paced-fix/`.

Validation: 80 Rust tests, 78 relevant Python tests, strict Clippy, formatting,
and diff checks. Both new integration regressions failed before the fixes.
Coverage also checks already-waiting FIFO callers, no reservation or retry
spent by admission parking, and exactly 64 attempts/63 dispatched retries
across a parked restart. Only synthetic providers were used.

### Elapsed parked-time accounting

Observed 2026-09-19, Darwin arm64, external power, Rust 1.98.0. A park now
stores its start timestamp and charges elapsed time when it resumes or
finishes, in the same transaction that clears the park. This includes daemon
downtime and capacity delays past the wake-up deadline, without charging
unused future waiting on interruption. It adds no query or write; ordinary
turns do not read the clock for this accounting.

Before the fix, interruption after roughly 50 ms charged 9,999 ms, and a
restart after 600 ms charged only the scheduled 299 ms. Both regression
tests now pass; another restart preserves the settled counter exactly.

The same 32-agent socket echo workload and charged boundary described above
compared binaries `74e8cf67…` and `bdaa0295…` in baseline/candidate/candidate/
baseline order. Each batch excluded one warmup and measured two runs, giving
four measured runs per binary. Builds and tests finished before measurement;
all lifecycle checks passed with no quality warnings.

| Metric, median (range) | Before elapsed-time fix | After elapsed-time fix |
| --- | ---: | ---: |
| Sampled peak target RSS, MiB | 17.80 (17.77–17.84) | 17.88 (17.80–18.06) |
| Observed target CPU, seconds | 0.335 (0.287–0.371) | 0.350 (0.323–0.367) |
| Per-run p95 turn latency, ms | 589.0 (587.4–594.8) | 596.2 (591.0–597.9) |

RSS median increased 0.08 MiB, CPU 4.5%, and p95 1.2%. Ranges overlap;
this small screen establishes neither a speedup nor a regression and does
not prove exact performance equality. Captures, binary hashes, and the driver
are in ignored `.local/paced-elapsed-fix/`.

Validation: 80 Rust tests, 31 relevant Python tests, strict Clippy, formatting,
and diff checks. The interruption and late-restart regressions failed before
the fix. Only synthetic providers were used.

## Pacing inputs per provider

The following screen records the initial implementation, including its
experimental sixteen-request cold-start cap. That cap was subsequently
removed: unknown pools now admit freely, with reported limits and refusals
providing feedback. These results do not describe the current cold-start
policy; the comparison below records the removal separately.

Observed 2026-09-19 on Darwin arm64, external power, Rust 1.98.0. Compared
the committed tree `8ba85986…` (rebuilt from a worktree) with the slice
binary `e3ee0cde…`. Three changes to the pacing gate: pools are keyed by the
family's idea of a quota, so a dated snapshot shares its alias's pool; the
estimate is a cost with input and output shares, paced per dimension the
provider publishes, which for Anthropic adds the input-token and
output-token limits beside the total; and a cold pool lets sixteen requests
out and holds the rest until any response's headers have arrived. On a
warm pool the gate does what it did, over three buckets instead of one;
the bootstrap branch is one flag test per admission.

The cold-burst screen (ignored `.local/bootstrap/bench.py`): 96 bots submit
one turn each at once to a daemon that has never seen the provider. The
fixture keeps a continuously refilling allowance of 60 requests per minute,
publishes the Responses rate-limit headers on every reply, answers 429 with
a one-second Retry-After when empty, and holds every reply's headers for
500 ms, as a real provider holds them until the first token. Two runs per
binary:

| | Committed tree | This slice |
| --- | ---: | ---: |
| Provider calls for 96 turns | 132 | 96 |
| Refused with 429 | 36 | 0 |
| Turns that retried | 36 | 0 |
| Wall to finish all 96, s | 37.8 | 37.2 |

Before, the whole burst went out before any headers came back, the 36
calls beyond the allowance were refused, and each of those turns retried.
After, sixteen went out, their headers taught the limit half a second
later, and the rest were paced into the allowance; the wall is the
allowance's own 36 seconds either way. With the fixture answering
instantly instead, neither binary is refused: the store spaces turn starts
by a few milliseconds and the first headers arrive within that, which is
why the screen holds headers.

The 32-agent socket echo screen, committed tree against the slice binary,
one excluded warmup and four measured runs each: RSS 17.80 (17.66–18.03)
versus 17.79 (17.69–17.91) MiB, CPU 0.323 (0.308–0.327) versus 0.322
(0.310–0.345) s, p95 592.3 (590.5–593.9) versus 590.8 (584.5–643.1) ms.
Level on every median; one slice run carried a 643 ms p95 outlier. Captures:
ignored `.local/bootstrap/{before,after}.json`,
`.local/bench/slice-prev8-socket-32/`, and `slice-pacing-socket-32/`.

Validation: 83 Rust tests, strict Clippy, formatting, and 159 Python tests.
New unit tests: a cold pool caps at sixteen until headers arrive and never
again after; Anthropic's output limit paces an output-heavy call while an
input-heavy one proceeds, and a dimension the provider never publishes
constrains nothing; dated snapshots map to their alias's pool and version
numbers do not. The startup-bound test warms its pool with one answered
turn first, since it is about `--max-connecting`, not cold pools.

Review follow-up: the first response now wakes bootstrap waiters even when
it carries no rate-limit headers. The same paused-clock probe, with sixteen
initial streams kept open, measured admission delay after those headers at
50 ms before and 0 ms after the fix. This isolates an unnecessary timer
wait; it is not a wall-clock throughput or CPU measurement. The existing
bootstrap test now requires admission within 10 ms of virtual time and
failed before the fix. An informed pool gains no extra notifications, and
the change adds no allocations, locks, or database work. All 83 Rust tests,
strict Clippy, formatting, and diff checks pass after this correction.

### Removing the cold-start cap

Decision and observation, 2026-09-19: unknown provider capacity does not
justify an inferred concurrency limit. The sixteen-request cap, its
`informed` flag, polling branch, and special notification were removed.
Reported limits still teach the shared buckets, and rate-limit refusals
still pause and park affected turns. Caller-selected local resource bounds
remain independent. Initial refusals are an accepted discovery cost.

Compared the capped binary `4b0ef97e…` with uncapped `10bbd5cf…` on Darwin
arm64, external power, Rust 1.98.0. Each run started a fresh daemon and
submitted 32 short synthetic turns. Every case used baseline/candidate/
candidate/baseline order, with two cold runs per binary and no warmup.
Fast fixtures returned headers immediately; slow fixtures delayed them by
200 ms. Ample fixtures allowed 100,000 requests/minute; the constrained
fixture allowed 30, continuously refilled, and returned a one-second
Retry-After with each refusal. Header-less fixtures omitted limit headers.
Builds and tests finished before measurement. All turns completed.

| Fixture | Batch wall ms, capped → uncapped | Per-run p95 ms, capped → uncapped | Refusals per run, capped → uncapped |
| --- | ---: | ---: | ---: |
| Ample, fast | 36.4 → 38.5 | 7.2 → 7.8 | 0 → 0 |
| Ample, slow | 424.8 → 224.5 | 411.2 → 213.0 | 0 → 0 |
| No limit headers, fast | 32.7 → 36.9 | 6.4 → 7.7 | 0 → 0 |
| No limit headers, slow | 425.4 → 228.7 | 409.0 → 217.7 | 0 → 0 |
| Constrained, slow | 4,836.0 → 4,643.4 | 2,609.1 → 2,417.7 | 0 → 2 |

Values are medians of two runs. Slow ample and header-less fixtures finish
about 46–47% sooner because admission no longer waits for the first replies.
The constrained fixture makes 34 provider calls instead of 32, with two
successful retries; slightly lower completion time here does not establish
a general improvement under rate limits.

Initial fast header-less CPU readings increased from 33.9 to 38.3 ms, so
that case received six further cold runs per binary in alternating ABBA
batches. Median CPU was then 37.9 (31.3–38.5) versus 35.8 (30.7–39.8) ms;
batch wall time 32.5 (26.7–32.7) versus 30.3 (26.0–33.9) ms; sampled peak
RSS 12.38 (12.30–12.41) versus 12.37 (12.23–12.44) MiB. These overlapping
ranges establish no fast-provider speedup or regression. Across the initial
matrix, median RSS changes stayed within 0.11 MiB. This is a short cold-start
screen, not sustained-load or active-capacity evidence.

CPU is the daemon's process CPU between submissions and observed completion;
RSS is sampled every 5 ms through bot creation, turns, and result inspection.
The Python controller and provider are excluded; these turns execute no
tools. Latencies use controller receive timestamps. Captures, full ranges,
binary hashes, and the driver are in ignored `.local/unrestricted-pacing/`.

Validation: all 83 Rust tests, 10 targeted Python runtime tests, strict
Clippy, formatting, and diff checks passed. The new burst regression failed
with the cap and now admits 128 requests without waiting for any headers.
The caller-selected startup-bound test again starts cold, without a warmup;
pacing, retry, interruption, and restart tests continue to pass.

## Storage counters by operation

Observed 2026-09-19 on Darwin arm64, external power, Rust 1.98.0. Every
store job now carries the name of the store method it performs, and the
worker keeps per label a count, queued and ran totals, the slowest run, and
two fourteen-bucket log-spaced latency histograms (bounds in `buckets_us`,
100 µs to 1 s), reported by `stats` under `store.operations`. Per job that
is one uncontended lock and a few adds after the three clock reads it
already paid; the map allocates once per distinct operation.

A sample from sixteen bots running four two-round `shell` turns each
against the instant synthetic model, the eight busiest operations by time
run (percentiles read from the histograms):

| Operation | Jobs | Ran, ms | Slowest, ms | Ran p50 | Ran p99 | Queued p99 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `append` | 128 | 28 | 0 | <250us | <500us | <2500us |
| `begin` | 64 | 17 | 0 | <500us | <1000us | <2500us |
| `finish` | 64 | 17 | 0 | <250us | <1000us | <2500us |
| `tool_finish` | 64 | 12 | 0 | <250us | <500us | <2500us |
| `tool_start` | 64 | 10 | 0 | <250us | <500us | <2500us |
| `window` | 128 | 5 | 0 | <100us | <500us | <2500us |
| `create` | 16 | 2 | 0 | <250us | <250us | <100us |
| `items_by_ids` | 128 | 2 | 0 | <100us | <100us | <2500us |

The screen the item was written for is the store-scale one (item 3); this
sample shows the shape a controller sees: which operations are slow, how
often, and whether the wait was in the queue or on the disk.

The 32-agent socket echo screen, committed tree `ad739fa2…` (rebuilt from a
worktree) against the slice binary `c47c1be0…`, one excluded warmup and four
measured runs each: RSS 17.77 (17.73–17.81) versus 17.92 (17.83–17.95) MiB,
CPU 0.309 (0.290–0.322) versus 0.315 (0.304–0.326) s, p95 593.4
(591.6–644.0) versus 593.0 (589.8–597.8) ms. Level within noise; RSS is up
about 150 KiB, inside this screen's own spread. Captures: ignored
`.local/bench/slice-prev9-socket-32/` and `slice-counters-socket-32/`.

Validation: 83 Rust tests, strict Clippy, formatting, and 159 Python tests.
The fleet stats test now checks that every job is counted under its method,
that each histogram's buckets sum to that method's count, and that the
per-operation counts sum to the worker's job count.

### Consistent counter snapshots

Review follow-up, 2026-09-19: aggregate atomics and operation counters could
describe different instants. A concurrent regression failed before the fix
with 382 total jobs and 495 in the operation breakdown. Stats now copies the
operation records under one short lock, then derives totals and formats JSON
outside it. This removes three atomic updates per job. Totals sum nanoseconds
before rounding; independently rounded operation times can still differ from
the rounded total by less than one millisecond per operation.

The release probe performed 10,000 storage jobs while repeatedly reading
stats: the original run found 620 inconsistent snapshots in 61,099 reads;
the fixed run found none in 61,880. Read counts depend on scheduling; this
probe establishes reconciliation, not relative throughput.

Matched 32-agent socket echo screen on Darwin arm64, external power,
Rust 1.98.0: pre-fix binary `c47c1be0…` versus fixed `561c7be0…`, in
before/after/after/before batches. Each batch excluded one warmup and measured
two runs, for four measured runs per binary. Identical three-turn workloads
completed with the lifecycle and follower/replay checks intact. Median
(minimum–maximum):

| Metric | Before | Fixed |
| --- | ---: | ---: |
| Daemon CPU, ms | 282.9 (279.0–290.7) | 284.9 (280.2–300.0) |
| Sampled peak RSS, MiB | 17.89 (17.88–17.94) | 17.95 (17.84–18.00) |
| Per-run p95 turn latency, ms | 588.16 (586.11–589.61) | 588.22 (586.64–589.64) |

The ranges overlap; this screen establishes neither a regression nor a
speedup. It does not measure sustained high-frequency stats polling. Captures,
binary hashes, and probes are in ignored `.local/review-store-counters/`.
Validation: 84 Rust tests, two focused daemon stats tests, strict Clippy,
formatting, and diff checks passed.

## Mass interrupt

Observed 2026-09-19 on Darwin arm64, external power, Rust 1.98.0, binary
`437fbc6`'s tree. Two fleets, interrupted all at once (ignored
`.local/mass-interrupt/bench.py`): in `stream` every turn is mid-request
against a provider that holds the response open; in `shell` every turn is
running a foreground `sleep 60`. All N interrupts are written back to back
on one stdio connection. Time runs from the first interrupt written to the
last `turn_finished` received; for `shell`, also until no child process
remains. Daemon CPU is the delta over the stop.

| Fleet | Bots | All sent | Last terminal event | Processes gone | Daemon CPU | Every turn ended |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| stream | 256 | 0.4 ms | 49 ms | n/a | 50 ms | `interrupted` |
| shell | 256 | 0.4 ms | 66 ms | 80 ms | 82 ms | `uncertain` |
| stream | 1,024 | 2.0 ms | 198 ms | n/a | 207 ms | `interrupted` |
| shell | 1,024 | 2.5 ms | 768 ms | 782 ms | 892 ms | `uncertain` |

This single screen observed about 0.2 ms of daemon CPU per streaming turn
and 0.8 ms per shell turn, the latter being the process-group kill, the
reap, and the finish job; the stream fleet's `finish` histogram put 989 of 1,024
finishes under 250 µs. A thousand bots mid-request are durably stopped in
0.2 s and a thousand with live commands in 0.8 s, their processes gone at
the same moment according to the sampler. This is exploratory: child-count
readiness includes both shells and their children, and daemon-descendant
enumeration can miss reparented processes. These samples do not establish
linear scaling or prove every owned process has stopped.

The screen exposed a usability gap: turns stopped mid-shell ended `uncertain`
and their bots refused new work until forked. Streaming turns ended
`interrupted` and stayed usable. The initial fix below recorded cancelled
results for explicit stops. Subsequent review found its claim that every tool
had stopped was too strong: native file I/O and background commands can outlive
cancellation. The current contract records unknown execution outcomes honestly
and keeps the bot usable after either cancellation or crash recovery.
After the change, the same screen at a thousand bots on the slice binary:

| Fleet | Bots | Last terminal event | Processes gone | Daemon CPU | Every turn ended |
| --- | ---: | ---: | ---: | ---: | --- |
| stream | 1,024 | 193 ms | n/a | 200 ms | `interrupted` |
| shell | 1,024 | 712 ms | 724 ms | 866 ms | `interrupted` |

In that initial fix, the shell fleet ended `interrupted` with a `cancelled` result recorded
for each killed command, and every bot accepted new work at once. This was a single interruption screen,
not a repeated performance comparison. The finish job answers the executing call in the
same transaction that ends the turn, one more node and event per bot.

The 32-agent socket echo screen, committed tree `a4215d72…` (rebuilt from a
worktree) against the slice binary `9e14f646…`, one excluded warmup and four
measured runs each: RSS 17.98 (17.81–18.08) versus 17.66 (17.58–17.73) MiB,
CPU 0.302 (0.289–0.321) versus 0.310 (0.296–0.330) s, p95 588.5
(587.7–612.1) versus 593.7 (590.0–596.9) ms. Level within noise; an
ordinary turn never takes the interrupt path. Captures: ignored
`.local/mass-interrupt/{before,after}.json`,
`.local/bench/slice-prev10-socket-32/` and `slice-interrupt-socket-32/`.

Validation: 84 Rust tests, strict Clippy, formatting, and 159 Python tests.
Those checks described the initial slice. The follow-up below replaces the
strong cancellation claim and the blocked recovery contract.

### Usable bots after unknown tool outcomes

2026-09-19 follow-up: cancellation and crash recovery now append an honest
`tool_outcome_unknown` result for executing calls without a committed result,
close planned calls as cancelled, and release the same named bot for more work.
No tool is replayed. Schema 20 repairs previously blocked bots once at startup,
including when operational records were pruned. Normal successful completion
keeps its existing pending-tool query; transcript repair is off that path.

Matched 32-agent socket echo lifecycle screen, pre-fix binary `9e14f646…`
versus candidate `585031a4…`. ABBA batch order, two excluded warmups and four
measured runs per binary. Same tools (`echo,shell,read,write,edit`), three turns,
4 KiB retained history, and restart/replay/fork/follower checks. All runs passed.
These measurements cover normal lifecycle overhead, not interruption latency.

| Metric, median (range) | Before | After |
| --- | ---: | ---: |
| Daemon CPU | 352 (325–371) ms | 346 (339–353) ms |
| Sampled peak RSS | 17.75 (17.67–18.02) MiB | 17.80 (17.61–18.22) MiB |
| Turn p95 | 623 (614–628) ms | 621 (618–650) ms |

Ranges overlap: no clear regression or established speedup. Captures and the
comparison driver are under ignored `.local/review-interrupt/`.

Validation: 85 Rust tests, 55 targeted Python runtime/wait/accounting tests,
strict Clippy, formatting, and diff checks. Coverage includes cancellation,
crash during a shell call followed by successful use of the same bot, queued
continuation, both provider result formats, pruned-record migration, and
idempotent reopening without duplicate tool results.

## Lost processes and pruned artifacts

2026-09-19. Two contract slices, no scheduler or storage-path change on the
ordinary turn. `process_lost` now means supervision ended: the daemon no
longer owns the command and never records its result, and a hard kill of the
daemon leaves the child running. The restart test kills only the daemon while
a background `sleep 1; printf written > survived` is parked on, confirms the
file is absent at the kill, recovers the handle as `process_lost`, and then
observes the write landing about a second later. An artifact read for a turn
retention has emptied answers `artifact_pruned` for the producer and for a
fork whose transcript holds the output node; the check runs only on the miss
path, walks the reader's lineage back to the turn's own prompt node, and
tests the turn's nodes for the call's result.

The 32-agent socket echo screen, committed tree `c971efbe…` (rebuilt from a
worktree) against the slice binary `a17d4b18…`, one excluded warmup and four
measured runs each, run back to back on external power: RSS 17.91
(17.77–18.02) versus 17.77 (17.66–17.81) MiB, CPU 0.323 (0.319–0.335) versus
0.326 (0.310–0.360) s, p95 618.1 (615.1–620.8) versus 624.7 (618.6–630.1) ms.
Level within noise, as expected for a change confined to artifact misses.
Captures: ignored `.local/bench/slice-prev11-socket-32/` and
`slice-retention-socket-32/`.

Validation: 85 Rust tests, strict Clippy, formatting, the query-plan audit,
and 161 Python tests.

## Bot identities

2026-09-19. Bots gained a store-wide integer identity that is never reused
after delete, and `submit` accepts `bot_id` so a retry pinned to an identity
the name no longer holds is refused rather than started as fresh work on the
namesake. The cost on the ordinary path is one primary-key lookup per
submission, on the storage thread inside the same job as `begin`.

The 32-agent socket echo screen, committed tree `c971efbe…` (rebuilt from a
worktree) against the slice binary `e378e056…`, one excluded warmup and four
measured runs each, run back to back on external power: RSS 18.00
(17.77–18.64) versus 18.22 (18.11–18.25) MiB, CPU 0.313 (0.303–0.318) versus
0.313 (0.301–0.321) s, p95 590.3 (588.8–620.0) versus 590.5 (585.8–654.8) ms.
Level within noise. The slice binary also carries the items 11 and 12 changes
screened above. Captures: ignored `.local/bench/slice-prev12-socket-32/` and
`slice-identity-socket-32/`.

Validation: 86 Rust tests, strict Clippy, formatting, the query-plan audit,
and 163 Python tests.

### Identity migration at fleet scale

2026-09-19 follow-up. The first schema-21 backfill counted every preceding bot
for every row, making startup quadratic. It now copies each existing row ID
once and seeds allocation from `MAX(id)`, preserving deletion gaps. Empty
stores start at zero. This changes only migration; ordinary turn execution and
already-migrated stores take the same paths as before.

Two runs per binary in ABBA order, using fresh copies of the same schema-20
store with 40,000 idle bots, empty instructions, and no turns. Pre-fix binary
`e378e056…`, candidate `ad6cff8f…`, Darwin arm64, Rust 1.98.0. No warmups excluded.
Readiness measures process spawn through the stdio `ready` event. CPU and peak
RSS use `/usr/bin/time -l` through clean shutdown; CPU has 0.01-second reporting
resolution. No provider requests occur. Every migrated store was checked for
40,000 distinct positive IDs and a sequence matching the maximum assigned ID.

| Metric | Before, two runs | After, two runs |
| --- | ---: | ---: |
| Ready | 18.766 / 18.721 s | 0.346 / 0.054 s |
| CPU | 18.28 / 18.29 s | 0.04 / 0.03 s |
| Peak RSS | 14.39 / 14.41 MiB | 14.33 / 14.30 MiB |

The CLI's automatic migration/startup path on another identical copy completed
`turns --bot b0` in 0.063 s, below its 10-second deadline. These are migration
measurements, not active-agent throughput claims. Captures and comparison
script: ignored `.local/review-identities/`.

Validation: 41 store-contract tests, strict Clippy, formatting, diff checks,
and the real CLI startup probe. Regression coverage includes sparse and empty
stores, allocation and forking after migration, and non-reuse after deleting
the highest identity and restarting.

## Store scale

2026-09-19. Corrected `bench.store_scale` screen: one store grown past 1 GB
and then 10 GB across 4,096 light bots and 8 heavy bots. The heavy group
receives a quarter of the growth turns. Both groups receive half text turns
(4 KiB prompts) and half shell turns (1,000,000 bytes of requested output,
retained as an artifact with a bounded preview). Each heavy bot receives both
shapes. The result records the actual submitted mix at each growth stage.

Darwin arm64, external power, 32 GiB RAM; binary SHA-256
`ad6cff8f98d0b5fa532fbe7111636000cb9cdf46569a426fd052f3ee9ed2392b`.
The binary is identical to the candidate in the preceding identity-migration
screen. These fixes change only benchmark code and documentation, not the
Rust runtime. This is one exploratory run, not a speedup or regression claim.

Each latency cell covers 32 completed turns. Light batches admit at most 32
turns at once; heavy batches admit at most 8 because each bot runs one turn
at a time. These are bounds, not achieved provider-stream concurrency. The
first/repeat paging columns are consecutive reads on the live store: neither
establishes a cold cache. Request body sizes come from the synthetic HTTP
server and include protocol fields and tool schemas as well as context.

| At target | 1 GB | 10 GB |
| --- | ---: | ---: |
| Store plus WAL before checkpoint probes | 992 MiB | 9,593 MiB |
| Cumulative growth submissions (excludes checkpoint probes) | 1,920 | 18,560 |
| Heavy bot history before deletion | 68 turns | 596 turns |
| Shell turns on the deleted heavy bot | 34 | 298 |
| Growth rate | 169 turns/s | 119 turns/s |
| Light bot, text turn p50 / p95 | 3.8 / 9.6 ms | 7.3 / 10.5 ms |
| Heavy bot, text turn p50 / p95 | 34.2 / 66.7 ms | 114.1 / 125.4 ms |
| Light bot, shell turn p50 / p95 | 206.7 / 362.6 ms | 291.2 / 361.4 ms |
| Heavy bot, shell turn p50 / p95 | 143.8 / 150.2 ms | 314.2 / 333.0 ms |
| Storage per light text turn | 1.3 ms | 1.7 ms |
| Storage per heavy text turn | 4.3 ms | 8.0 ms |
| Heavy text mean request body | 2.03 MiB | 7.24 MiB |
| `window` per call (light / heavy text) | 0.13 / 1.25 ms | 0.53 / 3.13 ms |
| `tool_finish`, 1 MB output, daemon-lifetime mean | 4.4 ms | 4.8 ms |
| `bots`, 17 pages of 256, first / repeat | 27.3 / 30.2 ms | 39.6 / 44.4 ms |
| `turns`, heavy bot, first / repeat | 1.3 / 1.0 ms (2 pages) | 40.8 / 8.1 ms (10 pages) |
| `events`, 256 rows, first / repeat | 1.1 / 0.9 ms | 5.7 / 1.2 ms |
| Fork, delete fork | 1.46, 0.85 ms | 0.55, 0.47 ms |
| Delete heavy bot | 40.2 ms | 692.8 ms |
| Clean restart to `ready` | 13.0 ms | 24.6 ms |
| Crash restart to `ready`, 32 held requests | 25.6 ms | 29.0 ms |
| Held requests observed / interrupted turns verified | 32 / 32 | 32 / 32 |
| Schema-21 migration replayed, restart to `ready` | 20.4 ms | 24.2 ms |
| Sampled daemon peak RSS through checkpoint | 52.8 MiB | 55.7 MiB |
| Sampled WAL peak through checkpoint | 5.0 MiB | 19.5 MiB |

The storage-per-turn rows sum the per-operation count times its mean running
cost during each batch and divide by 32; queue wait is excluded. Counts and
means come from the daemon's storage worker. `tool_finish` is the mean since
that daemon started, including growth and checkpoint turns, not only the
32-turn shell probe. Startup includes process launch and waiting for `ready`.
Every crash probe captures a fresh provider request count, waits for 32 new
held requests, fails on timeout, kills the daemon, and checks all 32 exact
turn IDs after restart. Recovery verification is outside the startup timing.

What this run supports:

- Startup, recovery, and these bounded reads remained in the tens of
  milliseconds on this host. The 10 GB store fits within its 32 GiB RAM;
  this does not establish a cold-storage or page-cache limit.
- Larger heavy histories increase context-reading work: the heavy text
  batch averages 10 `items_by_ids` jobs per call at 1 GB and 35.9 at 10 GB,
  with measured request bodies growing from 2.03 to 7.24 MiB. Storage time
  is only part of end-to-end latency, which also includes the synthetic
  provider's parsing and transport. This run does not isolate store size
  from history length or prove identical costs at other sizes.
- Deletion remains one storage job. The 693 ms deletion with 596 turns
  and 298 shell outputs identifies a concrete stall to address with bounded
  retention work. It does not establish that the cost depends only on the
  deleted bot's data.
- The WAL reached a sampled 19.5 MiB peak. Sampling is every 500 ms, so
  short memory/WAL peaks can be missed. The run does not establish checkpoint
  behavior over hours, and no CPU profile attributes the throughput limit.

The earlier `store-scale-1-10-c` interpretation is superseded: growth gave
heavy bots only text, its crash barrier reused stale counts, and consecutive
reads were labeled cold/warm. Its latency values are not comparable to this
corrected workload. The earlier `store-scale-1-10` and `-b` runs also suffered
from accumulated observer notifications. The current screen clears those
notifications per batch and snapshots phase peaks without later mutation.

Not covered: hours of sustained writes (item 16's soak), a store larger than
host RAM, achieved stream concurrency during growth, model quality, or total
provider/observer/descendant-process resources. RSS here is the daemon alone.
Capture: ignored `.local/bench/store-scale-reviewed-1-10/result.json`; the
10 GB temporary store was removed after completion. Validation: 21 focused
benchmark tests, including workload balance, timeout rejection, and two
successive real crash/restart checkpoints; Python compilation and diff checks.

## Retention in pieces and the storage reader

2026-09-19. Two follow-ups from the store-scale screen, measured with
targeted probes before and after, then the 32-agent socket screen. Slice
binary versus the committed tree `80c97dc` rebuilt from a worktree; Darwin
arm64, external power.

**Context reads.** Eight heavy bots with 300 turns of 32 KiB prompts, so each
model call carries the full 8 MiB default context, and 32 light bots. The
probe (ignored `.local/context-read/bench.py`) runs three batches of 32
turns three times each: heavy only (at most eight in flight, one per bot),
light only, and eight heavy with 24 light. Client latency, and the storage
worker's own per-batch accounting of `items_by_ids`, the batches that stream
window items into the request body.

| Batch | Before p50 / p95 | After p50 / p95 | `items_by_ids` queued, before → after |
| --- | ---: | ---: | ---: |
| Heavy only | 78–85 / 97–98 ms | 32–45 / 46–68 ms | 560–634 ms → 5–6 ms |
| Light only | 2.4–2.5 / 3.5–3.9 ms | 2.3–2.6 / 3.4–3.9 ms | 5–8 ms → 0–1 ms |
| Mixed, all 32 | 13.5–16.5 / 103–107 ms | 2.7–3.5 / 52–55 ms | 284–300 ms → 1–6 ms |

Before, the 34 item batches of every heavy call ran on the writer, and every
other job in the batch queued behind them: a light turn sharing the batch
went from 2.4 to 14 ms at the median. After, those batches run on a second
query-only connection on its own thread. The heavy calls still serialize on
that reader (their total ran time rose from 180 to 245–268 ms per batch,
the reader's cache being cold where the writer's was warm), but nothing else
waits for them: the mixed batch's light turns are back at their light-only
cost, and the mixed p95 is now the heavy turns themselves.

**Deletion.** One bot with 300 shell turns whose 1 MB outputs are artifacts
(307 MiB store), a socket daemon, the `delete` sent on one connection and 64
text turns on 64 other bots run on another connection meanwhile (ignored
`.local/retention-pieces/bench.py`). The same 64 turns with nothing else
running cost 2.9–3.2 ms at the median and at most 7 ms.

| Retention piece | Light turns during the delete, p50 / p95 / max | Delete wall | Jobs, slowest |
| --- | ---: | ---: | ---: |
| One job (before) | 3.1 / 10.1 / 99.8 ms | 155 ms | 1, 97 ms |
| 16 turns | 7.9 / 46.5 / 62.3 ms | 189 ms | 95, 8 ms |
| 4 turns (chosen) | 13.4 / 28.3 / 30.8 ms | 226 ms | 86, 4 ms |

The one-job delete stalls whoever is queued behind it for its whole length,
here 100 ms and at 10 GB 693 ms, and leaves the rest untouched. Pieces
spread that cost: every job of a turn in flight can land behind one piece,
so the worst case falls with the piece size while the median rises with
the number of pieces, and the delete itself takes longer. Four turns bounds
the worst wait at about thirty milliseconds for a 300 MB deletion and is the
default; the constant is one line. The piece loop runs on a task of its
own, not in the service loop: the first version looped inside `dispatch`,
and a turn submitted on another connection still waited the whole delete,
because the loop was holding every request behind it.

**Ordinary path.** The 32-agent socket echo screen, committed tree
`7b36ecd6…` against the slice binary, one excluded warmup and four measured
runs each, run in separate batches an hour apart: RSS 17.84 (17.75–17.91)
versus 18.22 (18.16–18.30) MiB, CPU 0.344 (0.326–0.353) versus 0.334
(0.322–0.369) s, p95 620.0 (615.9–638.2) versus 589.4 (587.6–590.9) ms. The
extra 0.4 MiB is the reader connection and its page cache. The p95 gap is
between batches, not back to back, and matches earlier runs of the same
committed tree at 590 ms, so it is host noise, not a speedup. Captures:
ignored `.local/bench/slice-prev13-socket-32/` and `slice-reader-socket-32/`,
`.local/context-read/{before,after}.json`,
`.local/retention-pieces/socket-{before,after,after4}.json`.

Validation: 87 Rust tests, strict Clippy, formatting, the query-plan audit,
and the Python suite. New coverage: a deletion interrupted between pieces
finishes at the next open and leaves the fork's shared prefix intact; a bot
being deleted refuses `submit` and `fork` and still answers `bot_exists` to
`create`; explicit prune pieces cover the same records as one pass; a
40-turn delete runs as several storage jobs through the protocol.

**Retention shutdown follow-up (2026-09-19).** Pending explicit prune/delete
loops now belong to a reaped task set. Shutdown cancels and awaits them before
joining stdout; committed pieces survive and deletion resumes at next open.
A regression seeds one million small cancelled turns, starts each operation,
and keeps stdin open through shutdown. Both paths timed out before the fix
and now exit within the test's three-second deadline with retention unfinished.
All 89 Rust tests, five focused runtime tests, strict Clippy, and formatting pass.

Matched 32-agent socket echo lifecycle screen on macOS arm64 with AC power:
pre-fix binary `283e7903…` versus candidate `1d6c80e3…`, before/after/after/before
batches, one excluded warmup plus two measured runs per batch (four per binary).
Both execute the same 96 turns, tools, restart, replay, and historical forks.

| Metric | Before median (range) | After median (range) |
| --- | ---: | ---: |
| Target peak RSS | 18.18 (18.13–18.33) MiB | 18.13 (17.95–18.19) MiB |
| Observed target CPU | 0.310 (0.307–0.313) s | 0.314 (0.306–0.322) s |
| Turn p95 | 588.1 (586.9–595.3) ms | 586.8 (585.4–587.4) ms |

The deletion-interference probe above also ran in before/after/after/before
order, two runs per binary, with 300 shell turns, a 307 MiB store, and 64 light
turns during deletion. Light-turn p50 was 13.33–14.12 ms before versus
13.08–13.32 ms after; p95 was 26.10–32.06 versus 22.17–27.29 ms, and maximum
latency was 32.58–34.90 versus 34.56–36.91 ms. Every deletion used 86 storage
jobs and removed the same 300 turns, 2,401 events, and 1,200 nodes. The task
tracking adds no storage jobs. The probe's delete wall time includes waiting
for the light batch, so it is not used as deletion-completion latency here.

These screens show no meaningful overall regression; the overlapping ranges
and slightly higher observed maximum do not establish a speedup. Captures:
ignored `.local/bench/retention-shutdown-fix/`.

**Deletion identity follow-up (2026-09-19).** Admission captures the current
bot ID before spawning deletion, and every piece checks that ID against the
bot record it already reads. This adds one admission read per deletion, with
no extra lookup per piece. Artifact deletion stays off the dispatch path so
other clients can continue while it runs. A stale task cannot delete a bot
that reuses the original name.

The deterministic store regression failed before the fix by deleting three
of the replacement's events; it now rejects the stale piece and preserves the
replacement's identity, status, and transcript. The original protocol probe
also passes 30 repetitions each with two and ten concurrent deletions and
name reuse. All 90 Rust tests, three focused runtime tests, two query-plan
tests, strict Clippy, formatting, and diff checks pass.

Matched 32-agent socket echo lifecycle screen on macOS arm64 with AC power:
pre-fix `1d6c80e3…` versus final candidate `cd0c733a…`, before/after/after/before
batches, each with one excluded warmup and two measured runs. Both binaries
complete the same 96 turns, tools, restart, replay, and historical forks.

| Metric | Before median (range) | After median (range) |
| --- | ---: | ---: |
| Target peak RSS | 18.10 (18.05–18.33) MiB | 17.93 (17.91–18.09) MiB |
| Observed target CPU | 0.294 (0.288–0.300) s | 0.301 (0.279–0.334) s |
| Turn p95 | 586.2 (585.2–610.3) ms | 587.9 (585.6–594.9) ms |

The CPU and latency ranges overlap. This lifecycle screen shows no meaningful
regression and does not establish a speedup; the lower sampled RSS is small.

The 307 MiB deletion-interference probe also ran in
before/after/after/before order, with 300 shell turns and 64 light turns during
deletion. Light-turn p50 was 10.48–11.73 ms before versus 13.06–13.94 ms after;
p95 was 25.13–26.87 versus 25.32–25.73 ms, and maximum latency was
28.50–36.46 versus 30.42–39.81 ms. The median increased by about 2.4 ms in
this screen; p95 was stable. This is a measured tradeoff, not a claim that
all latency percentiles improved. Every run removed the same 300 turns,
2,401 events, and 1,200 nodes in 86 deletion jobs. The candidate additionally
performed one admission `inspect`; there is no extra read per piece or turn.
As above, the probe's wall time includes waiting for the light batch, so it
is not used as deletion-completion latency. Captures: ignored
`.local/bench/retention-identity-fix-v2/`.

**Pruning identity follow-up (2026-09-19).** Explicit prune admission captures
the bot ID before spawning its task. Each piece checks that ID in place of
its existing existence query, rejecting stale work before any mutation. This
adds one small admission read per explicit request, no query per piece, and
no extra work for automatic retention. A deterministic regression failed on
the old code by pruning three replacement events; it now preserves the
replacement's events and transcript for both delayed first pieces and
continuations. All 91 Rust tests, three focused runtime tests, two query-plan
tests, and strict Clippy pass.

Matched 32-agent socket echo lifecycle screen on macOS arm64 with AC power:
pre-fix `cd0c733a…` versus candidate `6c494683…`, before/after/after/before
batches, each with one excluded warmup and two measured runs. Both binaries
complete the same 96 turns, tools, restart, replay, and historical forks;
all runs pass without benchmark quality warnings.

| Metric | Before median (range) | After median (range) |
| --- | ---: | ---: |
| Target peak RSS | 18.19 (18.14–18.48) MiB | 18.14 (17.89–18.22) MiB |
| Observed target CPU | 0.414 (0.379–0.439) s | 0.349 (0.332–0.416) s |
| Turn p95 | 593.6 (587.4–600.3) ms | 587.0 (586.1–749.8) ms |

Medians are lower, but the candidate has one higher tail-latency observation;
this screen does not establish a speedup or uniform latency non-regression.

A separate explicit-prune interference probe ran in the same order, two
runs per binary: 300 shell turns with 1 MB artifacts, a 307 MiB store, and
64 light turns on other bots during pruning. Every run removed 2,392 events
in 75 prune jobs and retained the final turn's eight events plus the bot's
creation event. Light-turn p50 was 5.92–6.70 ms before versus 6.05–8.48 ms
after; p95 was 23.02–24.92 versus 25.34–30.16 ms; maximum latency was
23.96–27.67 versus 27.02–37.91 ms. Total prune worker time was 110–128 ms
before versus 115–143 ms after, with the slowest piece at 4 ms before and
3–4 ms after. The higher interference latencies remain visible as a small
measured tradeoff; these runs do not isolate its cause. The probe's wall time
includes waiting for the light batch and is not treated as prune-completion
latency. Captures: ignored `.local/bench/prune-identity-fix/`.

**Deletion replay-gap follow-up (2026-09-19).** The first deletion piece
stores the original event range's upper bound on the bot as well as globally,
so a reader between pieces receives a conservative `pruned_before` warning.
The existing bot update returns that bound for the global update: SQL
statement count and indexed lookups are unchanged, with no added work per
continuation piece. The regression failed before the fix and now checks every
intermediate piece, both replay scopes, the upper cursor boundary, and an
unaffected bot. All 92 Rust tests, seven focused Python checks (including
three runtime tests), strict Clippy, formatting, and diff checks pass.

Matched macOS arm64 lifecycle screen: pre-fix `6c494683…` versus candidate
`4ac94445…`, 32 socket agents, echo tools, 96 turns plus restart, replay, and
historical forks. Before/after/after/before batches each had one excluded
warmup and two measured runs. All completed with no quality warnings.

| Metric | Before median (range) | After median (range) |
| --- | ---: | ---: |
| Target peak RSS | 18.16 (18.13–18.33) MiB | 17.91 (17.84–17.95) MiB |
| Observed target CPU | 0.328 (0.322–0.331) s | 0.342 (0.330–0.364) s |
| Turn p95 | 592.6 (587.1–608.6) ms | 587.3 (586.5–654.7) ms |

The deletion-interference probe used 300 shell turns with 1 MB artifacts,
a 307 MiB store, and 64 competing light turns. One candidate run had a large
latency spike, so the before/after/after/before sequence was repeated once.
All eight runs are retained below, four per binary. Every deletion removed
300 turns, 2,401 events, and 1,200 nodes in 86 storage jobs.

| Metric | Before median (range) | After median (range) |
| --- | ---: | ---: |
| Competing turn p50 | 13.09 (12.56–13.31) ms | 13.47 (12.97–31.78) ms |
| Competing turn p95 | 30.08 (25.20–32.76) ms | 30.77 (25.58–96.13) ms |
| Competing maximum | 38.92 (33.06–45.31) ms | 36.31 (33.32–128.82) ms |
| Total deletion worker time | 145 (132–155) ms | 145 (135–372) ms |

Typical deletion timings are similar; the isolated candidate spike did not
repeat, but its cause was not established. The lifecycle CPU median increased
by 15 ms. These results do not prove a speedup or uniform tail-latency
non-regression. As above, the probe's wall time includes waiting for competing
turns and is not deletion-completion latency. Captures: ignored
`.local/bench/deletion-gap-fix/`.

**Retention stdio backpressure follow-up (2026-09-19).** Deferred `prune`
and `delete` replies now use the ordinary five-second asynchronous response
deadline for the stdio owner. Socket replies remain nonblocking. Both call
one shared helper; no storage jobs, queries, or output buffers were added.
A bounded-output regression failed on the old path and now verifies stdio
recovery and socket eviction. Real-process probes paused stdout consumption
for three seconds during each operation: both delivered all 201 replies,
answered a subsequent stats request, and shut down cleanly. All 93 Rust tests,
three focused retention runtime tests, strict Clippy, and formatting pass.

Matched stdio lifecycle screen on macOS arm64: pre-fix `4ac94445…` versus
candidate `83dc1d9b…`, 32 agents, echo tools, 96 turns plus restart, replay,
and historical forks. Before/after/after/before batches each had one excluded
warmup and two measured runs. All passed with no quality warnings.

| Metric | Before median (range) | After median (range) |
| --- | ---: | ---: |
| Target peak RSS | 17.45 (17.41–17.61) MiB | 17.59 (17.41–17.89) MiB |
| Observed target CPU | 0.302 (0.282–0.316) s | 0.290 (0.279–0.300) s |
| Turn p95 | 589.1 (585.4–589.5) ms | 592.1 (588.0–622.1) ms |

The deletion-interference probe also used stdio, with 300 shell turns,
1 MB artifacts, a 307 MiB store, and 64 competing light turns. Two runs per
binary, in the same order, removed identical records in 86 storage jobs.
Competing p50 was 13.20–15.17 ms before versus 11.70–14.00 ms after; p95 was
28.52–30.59 versus 26.68–30.29 ms; maximum latency was 34.93–45.51 versus
29.91–34.55 ms. Total deletion worker time was 138–144 versus 135–137 ms.
As in earlier probes, wall time includes the light batch and is not pure
deletion-completion latency. The overlapping ranges suggest comparable
typical performance, not a proven speedup; lifecycle RSS and latency were
slightly higher. Captures: ignored `.local/bench/retention-stdio-fix/`.

## Absorption against context capacity

2026-09-19. A round boundary now absorbs queued steers only while the
running turn's own items plus each encoded steer stay within three quarters
of the context budget, the target the window keeps; the rest stay queued
and start as their own turns. Before, a burst of large steers could push the
running turn past its context and fail it with `context_limit`. The
regression test runs a 4 KiB context with three 1.5 KiB steers queued
during a shell call: one is absorbed, two complete as their own turns, and
the running turn completes.

Initial cost: one turn-usage walk over the running turn's own nodes per absorb
call, and each candidate steer is encoded before it is chosen instead of
after. The parked-steer screen (48 workers, ten two-round turns each, with
and without one bot holding a queued steer, ignored
`.local/steer-hint/bench.py`), committed tree `055257d3…` versus the slice
binary `4ffd9922…`, medians of three runs each:

| | Daemon CPU, plain / parked | Wall, plain / parked | Store jobs |
| --- | ---: | ---: | ---: |
| Before | 1.186 / 1.125 s | 1.09 / 1.08 s | 6,521 / 6,515 |
| After | 1.080 / 1.047 s | 0.90 / 0.86 s | 6,510 / 6,524 |

The 32-agent socket echo screen, same two binaries back to back, one
excluded warmup and four measured runs each: RSS 18.24 (18.14–18.45)
versus 18.25 (18.19–18.34) MiB, CPU 0.333 (0.301–0.361) versus 0.339
(0.330–0.356) s, p95 597.0 (588.9–612.1) versus 590.4 (584.1–595.8) ms.
Level within noise; the ordinary path never absorbs. Captures: ignored
`.local/bench/slice-prev14-socket-32/`, `slice-absorb-socket-32/`,
`.local/steer-hint/absorb-{before,after}.json`. The steer screen's after
numbers are lower, but its runs are short and noisy. Its timed workload leaves
the steer parked, so these measurements establish neither the cost of active
absorption nor a speedup. The active follow-up below measures that path.

Validation: 94 Rust tests, strict Clippy, formatting, the query-plan audit,
and the Python suite.

### Active-steering follow-up

2026-09-19, macOS 27 arm64 on AC power, Rust 1.98.0, locked offline release
builds. Baseline is commit `3652b7f`, binary SHA-256 `83dc1d9b…`; candidate is
that tree plus the absorption-budget changes above, binary `57cdc685…`.
The tracked `bench.active_steering` screen uses the same observer and synthetic
provider for both binaries. Each shape excludes one complete warmup per binary,
then runs two before/after/after/before blocks, four samples per binary.

Eight active bots each receive 40 strict steers at each of 20 gated model
boundaries: 6,400 absorbed steers, 168 model calls, and 320 absorption jobs per
run. Every steer resolves into its original turn, and every provider request
matches the complete expected history. Call counts, history hashes, and request
bytes match across binaries. The one-item response shape sends 21,812,512
request bytes per run; the 48-item shape sends 29,793,032. Before its final
response, the growing turn holds 1,761 items, exercising longer metadata walks
without exceeding the default context. No overload deferral is compared.

Medians, with minimum–maximum ranges in parentheses:

| Metric | One item: before | One item: after | 48 items: before | 48 items: after |
| --- | ---: | ---: | ---: | ---: |
| Daemon CPU, s | 1.644 (1.635–1.657) | 1.709 (1.682–1.744) | 1.960 (1.944–1.989) | 2.085 (2.064–2.110) |
| Sampled daemon peak RSS, MiB | 18.99 (18.81–19.36) | 19.07 (19.03–19.16) | 19.59 (19.50–19.77) | 19.57 (19.44–19.67) |
| Wall time, s | 2.416 (2.268–2.905) | 2.480 (2.360–2.864) | 2.670 (2.623–2.768) | 2.905 (2.756–3.217) |
| Boundary p50, ms | 20.77 (19.64–21.71) | 23.40 (22.66–24.60) | 27.07 (25.95–28.49) | 32.15 (31.62–33.30) |
| Boundary p95, ms | 34.06 (29.08–68.77) | 36.05 (33.52–53.19) | 41.17 (41.01–46.27) | 56.20 (50.25–81.11) |
| Absorption worker time, ms | 332 (301–399) | 382 (365–456) | 304.5 (296–313) | 426.5 (408–447) |

The candidate costs more under active steering: median daemon CPU rises 3.9%
and 6.4%, respectively, with non-overlapping observed CPU ranges. Memory is
essentially unchanged. The growing-turn shape's absorption worker time rises
40%, consistent with the added current-turn metadata walk on each batch.
This supports targeting that walk next; it is not an instruction-level profile
or proof that the walk explains every timing difference. The correctness fix
has a measurable cost, so performance neutrality is not established.

Boundary latency includes response delivery, append, absorption, context
construction, and receiving/decoding the next request in the Python fixture.
Wall time additionally includes sequential submissions and validation. Storage
execution counters include SQLite work and are reported in whole milliseconds;
they are wall time on the worker, not CPU. RSS is sampled every 5 ms. Provider
and observer CPU/memory are excluded. Runs are short, cache state is uncontrolled,
and tail latency is noisy; this is a matched synthetic workload, not a real
provider or active-capacity result. Raw captures, full binary hashes, operation
counters, and excluded warmups: ignored `.local/bench/active-steering/result.json`.

### Indexed turn accounting

2026-09-19. Replace the recursive `turn_usage` walk with indexed lookups of the
turn's first node, its parent, and the bot's current head. Subtract cumulative
bytes and depths to count the current turn. A partial unique `nodes(turn)` index
contains only turn-start nodes. Accounting work no longer grows with the
current turn's length, and the history tool uses the same query. There are no
cached counters to reconcile after forks or restart. The existing budget,
queue order, and encoded-item accounting are unchanged.

The index adds disk space and maintenance for one entry per started turn.
Opening an existing store builds it once by scanning nodes; that first-open
cost on a large existing store is not measured here. Subsequent opens reuse
the index. No transcript data or logical schema fields change.

Repeat the active-steering contract above on the same host and toolchain,
again with a full warmup per binary and two ABBA blocks per shape. Baseline
remains the pre-budget `83dc1d9b…` binary; indexed candidate is `c29fef24…`.
All runs match the expected 6,400 absorbed steers, 168 provider calls, 320
absorption jobs, complete histories, and request bytes.

Medians (minimum–maximum):

| Metric | One item: baseline | One item: indexed | 48 items: baseline | 48 items: indexed |
| --- | ---: | ---: | ---: | ---: |
| Daemon CPU, s | 1.694 (1.660–1.766) | 1.740 (1.650–1.761) | 1.971 (1.956–2.072) | 1.983 (1.936–2.097) |
| Sampled daemon peak RSS, MiB | 18.86 (18.67–19.00) | 18.88 (18.86–19.11) | 19.53 (19.45–19.95) | 19.37 (19.33–19.64) |
| Wall time, s | 2.307 (2.208–2.832) | 2.374 (2.268–2.530) | 2.777 (2.667–2.928) | 2.662 (2.600–3.135) |
| Boundary p95, ms | 27.17 (26.64–40.24) | 27.91 (25.40–42.34) | 42.29 (38.84–91.80) | 39.98 (39.07–53.05) |
| Absorption worker time, ms | 306 (303–357) | 308 (298–322) | 313.5 (299–419) | 300.5 (294–311) |

The previous large absorption regression is absent in this follow-up. Median
CPU remains 2.7% higher for one-item responses and 0.6% higher for 48-item
responses, with overlapping observed ranges; these short runs support near-
baseline performance, not a universal non-regression guarantee or a speedup.
The prior unindexed and current indexed measurements are separate campaigns;
do not treat their ratio as a matched optimization speedup. All boundaries and
limitations of the active-steering screen still apply. Captures:
ignored `.local/bench/active-steering-indexed/result.json`.

Ordinary-path check: the 32-bot socket echo lifecycle screen, baseline/indexed/
indexed/baseline batches, each with one excluded warmup and two measured runs
(four samples per binary). All runs complete 96 turns, achieve 32 overlapping
provider requests, preserve restart/replay/forks, and have no quality warnings.
Median target RSS is 18.22 (18.02–18.23) versus 18.11 (17.97–18.23) MiB;
observed target CPU is 0.298 (0.279–0.323) versus 0.290 (0.287–0.313) s;
turn p95 is 586.95 (586.07–589.01) versus 588.14 (586.89–590.27) ms.
Restart readiness is slightly slower: 16.87 (16.13–17.67) versus
18.95 (17.75–20.12) ms. This check includes ordinary turn-start index maintenance
and reopening an already indexed store; it does not measure first-open index
construction on a large store. Captures: ignored
`.local/bench/steering-index-lifecycle-*/result.json`.

Validation: 95 Rust tests, 18 delivery/active-steering Python tests, two
query-plan tests, strict Clippy, formatting, and diff checks pass. Coverage
includes absorbed UTF-8 content, prior-turn exclusion, fork isolation, stale
turns, old-store migration, and rejecting a plan that scans nodes after the
new index is removed.

## Pending-submission bounds

2026-09-19. `--max-pending` and `--max-pending-bytes` bound submissions
waiting to start, daemon-wide, with `pending_limit` answered before anything
is written. Two designs were measured; the first was dropped.

**Store triggers.** The first version kept a `pending` row exact with four
SQLite triggers on the turns table, so admission and `stats` read one row.
Every status change of every turn paid a trigger evaluation, and the
ordinary path showed it: on the 32-agent socket echo screen daemon CPU went
from 0.310 (0.304–0.331) to 0.334 (0.314–0.353) s, and on the
ten-thousand-bot fleet screen the burst went from 1,154 to 957 turns per
second with creation from 2,567 to 2,238 bots per second, one run each.
Rejected before commit.

**Worker-kept counters.** The candidate keeps the count and prompt
bytes in the storage worker's memory, updated at the four transitions that
move a turn into or out of the waiting set (submission, start, absorption,
cancellation), and recounted from the rows at open after recovery through
the partial status indexes. Nothing is read or written on the store for it;
a submission that would wait reads two integers when a bound is set and
nothing when none is. Baseline binary `61c0b77d…` against the slice binary,
same host, external power:

| Screen | Before | After |
| --- | ---: | ---: |
| 32-agent socket echo, CPU s, two pairs | 0.310 (0.304–0.331), 0.304 (0.302–0.309) | 0.330 (0.309–0.356), 0.320 (0.298–0.339) |
| 32-agent socket echo, RSS MiB | 18.17, 17.99 | 18.36, 18.28 |
| 32-agent socket echo, p95 ms | 588.6, 592.6 | 601.2, 593.9 |
| Fleet burst, turns/s, two runs | 1,154, 1,090 | 1,048, 1,086 |
| Fleet create, bots/s | 2,567, 2,433 | 2,257, 2,535 |
| 480-turn steer screen, CPU s, ABAB | 1.092, 1.080 | 1.063, 1.100 |

The 32-agent CPU medians are higher in both pairs with overlapping ranges.
Host noise is one possible explanation, but these measurements do not identify
the cause or establish performance neutrality. Other workloads cannot rule out
a cost in this one. Treat the repeated difference as unresolved until a
controlled follow-up or profiling explains it.
Captures: ignored `.local/bench/slice-prev15*-socket-32/`,
`slice-pending*-socket-32/`, `fleet-prev15*/`, `fleet-pending*/`,
`.local/steer-hint/pending-*.json`.

### Alternating follow-up

2026-09-19. Rebuilt baseline `b01c241` and compared it with the current
uncommitted pending-bounds candidate on the same macOS arm64 host, on AC
power, with no concurrent builds or tests. Both used the release profile,
the same lockfile and benchmark observer, socket transport, the echo tool,
FULL durability, and unbounded pending admission. Binary SHA-256:

- Baseline: `c29fef2444fc62ef07f9bd60d1567ee4c3e7ea5a59167b2a800befcac6e71613`.
- Candidate: `5962f9ce85581237ccbb8ae211ed27e4d28ba490c34bed0c7d4b00895da27b1e`.
- Observer source digest: `a392e822ad89fe435a024367e13ffd7654fe680b05d650b9c687b4a38ebaa86f`.

Each shape had one warmup per binary, followed by two baseline/candidate/
candidate/baseline blocks: four measured runs per binary. Both shapes used
32 bots, 20 chunks of 256 bytes with 25 ms chunk delay, and 4,096-byte
history fixtures; the longer shape used twelve turns per bot instead of
three. All twenty runs passed the lifecycle, replay, duplicate and
historical-fork checks and achieved 32 concurrent provider requests.
Request counts, tool-result counts, and request/response body bytes matched
exactly between binaries within each shape: 192 requests and 96 tool
results for three turns; 768 requests and 384 tool results for twelve.

Medians (min–max), retaining every measured run:

| Shape and metric | Baseline | Candidate |
| --- | ---: | ---: |
| Three turns/bot, observed target CPU s | 0.303 (0.295–0.334) | 0.306 (0.289–0.386) |
| Three turns/bot, peak RSS MiB | 18.20 (18.11–18.22) | 18.10 (16.98–18.38) |
| Three turns/bot, p95 turn ms | 589.9 (586.4–744.2) | 588.7 (586.3–605.2) |
| Twelve turns/bot, observed target CPU s | 1.280 (1.107–1.550) | 1.203 (1.091–1.336) |
| Twelve turns/bot, peak RSS MiB | 19.77 (19.53–19.97) | 19.45 (19.41–19.48) |
| Twelve turns/bot, p95 turn ms | 602.4 (589.5–635.0) | 598.3 (590.4–622.1) |

One twelve-turn baseline run raised `sampler exceeded 10% of wall time`.
It remains in the table and capture. Excluding only that flagged sample
changes the baseline CPU median to 1.195 s, leaving the candidate 0.7%
higher; the three-turn candidate median is 1.0% higher. Thus the apparent
longer-run CPU improvement is sensitive to an observer warning. CPU is
sampled process lifetime, not an exact accounting through process exit.
The smaller differences and overlapping ranges do not establish either
a regression or a speedup, and do not explain the earlier increases.
No runtime optimization was retained from this follow-up. A small CPU cost
remains unresolved; these results do not support claiming performance
neutrality or attributing the earlier gap to host noise.

Capture: ignored `.local/bench/pending-controlled/result.json`, with
per-run samples alongside it. The driver stopped on the observer warning;
that completed result was retained and the three remaining scheduled runs
were completed without rerunning or replacing any sample.

Validation: 96 Rust tests, strict Clippy, formatting, the query-plan audit,
and the Python suite. New coverage: counters through submission, ready,
absorption, cancellation, start, duplicate, and restart; both bounds
refusing and the running path never refused; the flags on `ready`, `stats`,
and the attach mismatch check.

## Context quality before compaction

2026-09-19. Item 32, slice one: an exploratory screen before compaction.
`bench.context_eval` runs one conversation per bot against a real model with
copies of the CLI's default instructions and the
`shell,read,write,edit,history` tools. Turn 1 states a workspace rule: every
created file must end with `# reviewed: CASTOR-42`. Twelve filler tasks each
create an `item_N.txt` file and report its byte count; a final task creates
`summary.txt` with a count. The `omitted` condition uses a small context
window; `retained` uses the 8 MiB default as a control.

Luna (`gpt-5.6-luna`) ran eight conversations per condition, with a 16 KiB
window in `omitted`. Sonnet (`claude-sonnet-5`) ran three with an 8 KiB
window, after an initial 16 KiB attempt never omitted the rule. Input usage
was about 0.8 M tokens on luna and 0.9 M on Sonnet including that initial
attempt and its retained control. The table below includes only luna's two
conditions and Sonnet's 8 KiB condition: 19 conversations, 266 turns.

| | Luna, retained | Luna, omitted | Sonnet, omitted |
| --- | ---: | ---: | ---: |
| Window had dropped the rule by the end of the final turn | 0/8 | 8/8 | 3/3 |
| Filler files honoring the rule, tasks 1–6 | 48/48 | 48/48 | 12/18 |
| Filler files honoring the rule, tasks 7–12 | 48/48 | 32/48 | 12/18 |
| Final file honoring the rule | 8/8 | 3/8 | 0/3 |
| Conversations that called `history`, any turn | 0/8 | 0/8 | 0/3 |

These are raw outcome counts, not verified scores for acting without the
rule. The original evaluator recorded `context_start` only after the final
turn. A turn can see the rule when issuing a file-writing tool call and
lose it on the next request after the tool result. Its final window alone
cannot establish what the model saw at the action boundary.

The zero history-call counts were checked against all stored `tool_started`
events in these captures. The original evaluator inspected only the first
256 events for final-turn calls; these captures have fewer than 256 events
per bot, but longer conversations could silently lose calls from its score.
The stores confirm no history calls in the 266 displayed-cohort turns.

Five luna conversations ended with their window starting at turn 4 and
honored every filler; three honored the final file. Three ended with starts
at turns 9 or 10 and missed later fillers. Copying visible examples is a
possible explanation, not an established cause: previous tool calls and
workspace files can carry the marker, including during the final task.
This screen does not isolate retrieval from those other sources.

Sonnet's first 8 KiB conversation wrote all thirteen files into the home
directory despite the shell starting in its workspace. Those files were
removed by hand. Its files score as missing, so each six-task half includes
six missing files; the other two conversations honored every filler but
neither honored the final file. The final 0/3 includes one missing file.

The corrected evaluator reads scalar window positions while bots are idle,
before and after each turn. A successful turn with the rule already omitted
before submission is `omitted`; one retaining it through completion is
`retained`. A turn crossing that boundary is `transition`, and failed turns
are `unknown`. Neither group contributes to the stable-context scores.
This conservative classification does not identify the exact action request
within a transitional turn. Per-file records preserve both positions and
summaries separate the four groups. All-turn history counts scan bounded
event pages incrementally from each bot's last cursor, and usage sums all
turn pages. Captures now include binary and evaluator hashes.

No Rust runtime code changed. Scalar window snapshots bracket turns outside
active model/tool execution, with the prior snapshot reused as the next
turn's starting position. Event scans visit each record once;
usage scans stream one page at a time. This is a quality screen, not a
runtime-performance benchmark. Five regression tests pass, including a
synthetic end-to-end reproduction of a rule visible at the action request
but omitted after its tool result, followed by a stable omitted turn.

The corrected evaluator has not been rerun against paid providers. Keep the
old captures as exploratory evidence; collect stable-context scores with
the corrected evaluator before claiming a compaction improvement.
Captures: ignored `.local/context-eval/{luna,sonnet,sonnet-8k}.json`, with
stores under `.local/context-eval/run/`; `sonnet.json` contains the initial
16 KiB attempt and retained control.

## The legible omission note

2026-09-19. Item 32, slice two. The context note now lists the omitted turns,
newest first up to `--note-turns` (default 48): each turn's ordinal and the
first line of its prompt cut to 120 bytes. It is data in the request, not an
instruction, and it changes only when the window's start moves, as the
request prefix already does. Built on the storage reader from the omitted
turns' prompt nodes alone; the listing costs about a kilobyte per request
at this history length, and about 7% more input tokens over the run below.

The evaluation of slice one, rerun on luna with the corrected evaluator,
eight conversations per condition, 16 KiB window in `omitted`, twelve
fillers, scored per file by the window's state before and after the turn:

| Final file honoring the rule | Baseline, bare count | Listing note |
| --- | ---: | ---: |
| Rule retained throughout | 8/8 | 8/8 |
| Rule omitted before the turn | 0/6 | 7/7 |
| Rule left during the turn | 0/2 | 1/1 |
| Filler files, rule omitted before the turn | 0/26 | 23/23 |
| Conversations that called `history`, any turn | 0/8 | 4/8 |
| Conversations that called `history` in the final turn | 0/8 | 3/8 |
| Input tokens, `omitted` condition | 364 k | 389 k |

Read per conversation: four of the eight read turn 1 through `history`
after seeing it listed as "Workspace convention, in force for every task in
this conversation from now on: every file you create must end with a…", and
the preview stops before the marker, so those four went and got it. The
other four never called `history` and honored the rule anyway; their windows
still held earlier `write` calls carrying the marker, as the baseline's did,
where nobody honored it. The listing changed behavior in both groups: told
that a convention exists, the model acted on the examples in view or
fetched the text. What it does not show is a model reading history without
being able to see there is something to read; that was the baseline.

The 32-agent socket echo screen, committed tree `3a9d9113…` against the
slice binary `1a4dcd70…`, one excluded warmup and four measured runs each:
RSS 18.12 (18.03–18.41) versus 18.07 (17.97–18.11) MiB, CPU 0.368
(0.358–0.376) versus 0.365 (0.352–0.375) s, p95 597.6 (593.3–606.2) versus
592.9 (590.0–595.9) ms. Level; the ordinary path omits nothing. Captures:
ignored `.local/context-eval/luna-corrected.json` (baseline),
`luna-note.json`, `.local/bench/slice-prev16-socket-32/`,
`slice-note-socket-32/`.

## The carry-forward note

2026-09-19. Item 32, slice three. A `note` tool lets a bot write or replace
up to 8 KiB of text that the runtime places ahead of the window in every
request; it is recorded in the same commit as the tool result and versioned
by that result's node, so forks inherit the version at their checkpoint and
deletion frees only a bot's own versions. The daemon writes nothing itself
and says nothing about the tool; the client decides whether to offer it.

Offered to luna with no instruction about it, in the same evaluation as
slice two (eight conversations, 16 KiB window, twelve fillers, `omitted`
condition only), on top of the listing note:

| | Listing note | Listing note and `note` tool offered |
| --- | ---: | ---: |
| Final file honoring the rule, rule omitted before the turn | 7/7 | 8/8 |
| Filler files, rule omitted before the turn | 23/23 | 19/19 |
| Conversations that called `history`, any turn | 4/8 | 5/8 |
| Conversations that wrote a note, any turn | n/a | 0/8 |
| Input tokens | 389 k | 414 k |

The outcome is the listing note's; the tool changed nothing because the
model never used it. Eight conversations, 112 turns, no call. Its schema
cost about 6% more input tokens. That is the survey's finding reproduced
here: a model-managed memory offered as a bare mechanism goes unused, and
the harnesses that rely on one instruct the model to use it. Whether an
instruction belongs in the client's default text is a client decision, and
this measurement is the number it should be made against; the daemon
mechanism is in place and tested either way.

The 32-agent socket echo screen, committed tree `3a9d9113…` against the
slice binary `383c133d…`: RSS 18.34 (18.19–18.56) versus 18.35
(18.31–18.44) MiB, CPU 0.414 (0.394–0.435) versus 0.380 (0.355–0.386) s,
p95 750.5 (613.6–1345.0) versus 616.9 (610.0–619.9) ms. The committed
tree's runs were the noisy ones this time; the slice binary's ranges sit
inside its earlier ones. Not a speedup. Captures: ignored
`.local/context-eval/luna-note-tool.json`,
`.local/bench/slice-prev17-socket-32/`, `slice-carry-socket-32/`.

## Jev data points for compaction

2026-09-19. Before Jev (TypeSafe's bounded-decision model, `POST
/v1/systemone`) goes anywhere near the compaction design, three probes
against today's evaluation transcripts, eight luna conversations of
thirteen turns each, run from ignored `.local/jev/`. Each request carries
the conversation's prompts and a batch of questions; every answer is a
probability, and the ground truth is known by construction.

**Relevance: which turns are load-bearing.** Turn 1 states the rule; turns
2 to 12 are fillers. Three phrasings of the per-turn question, twelve
questions per request:

| Question | Rule turn, min / median | Fillers, max / median | Separated by one global cut |
| --- | ---: | ---: | ---: |
| "Will the agent need something stated in turn N that is not in any later turn?" | 0.38 / 0.47 | 0.45 / 0.18 | 6 of 8 |
| "Does turn N state a rule, constraint, or preference that applies to tasks after it?" | 0.99 / 0.99 | 0.18 / 0.10 | 8 of 8 |
| Score 0–2, "how much would later tasks depend on turn N if it were forgotten?" | 1.79 / 1.82 | 0.44 / 0.20 | 8 of 8 |

The first phrasing is the honest failure: asked about dependence in the
abstract it barely separates the rule from the fillers. Asked whether a
turn states a standing constraint, or to grade dependence on a rubric, it
separates them cleanly in every conversation with a wide margin. The
question has to name the judgment a person would make in a second.

**Validation: did a summary keep the rule.** Two hand-written summaries of
the same conversation, one carrying the marker sentence and one without,
each asked two questions:

| Summary | "States the exact marker line" | "Preserves every requirement of the rule turn" |
| --- | ---: | ---: |
| Kept | 0.97 | 0.60 |
| Dropped | 0.04 | 0.04 |

The concrete check separates cleanly; the "every requirement" check is
right in direction but soft on the positive, so validation questions should
name the facts to look for, one per question, not ask for a blanket verdict.

**Gating: is this a stable point.** A transcript stopped with a shell call
pending scored 0.02 on "no tool call in progress and the current task
complete"; the same transcript after the result and reply scored 0.97.

**Cost.** Median latency 0.34–0.44 s per request end to end from this
host; 1,500–1,970 input tokens for a twelve-question relevance batch and
420–470 for a validation or gating question; input is billed and output is
not, so a relevance batch costs about $0.0001 and a check about $0.00003.
Sample sizes are eight conversations and one pair of summaries; this
establishes that the judgments are answerable, not their error rate at
fleet scale.

## Compaction

2026-09-19. Item 32, slice four. A bot created with compaction instructions
compacts at a round boundary once its window holds `--compact-at` percent
of the context budget: one summarizer call under the client's instructions
over everything older than the newest whole turns holding `--compact-keep`
percent verbatim, the previous summary merged in, the covered turns' user
prompts kept verbatim within bounds, the result versioned by the cut node
that becomes the context start. No instructions, no compaction.

The evaluation, luna, eight conversations, 16 KiB window, twelve fillers,
`omitted` condition, with the CLI's default compaction text and
`--compact-at 50` so compaction fires several times inside that small
window, against the two earlier runs on the same evaluator:

| | Bare count | Listing note | Listing note and compaction |
| --- | ---: | ---: | ---: |
| Final file honoring the rule, rule omitted before the turn | 0/6 | 7/7 | 8/8 |
| Filler files, rule omitted before the turn | 0/26 | 23/23 | 62/62 |
| Conversations that called `history` | 0/8 | 4/8 | 0/8 |
| Compactions per conversation | 0 | 0 | 3 to 7 |
| Summaries carrying the marker line, of all written | n/a | n/a | 41/41 |
| Normal-call input tokens, excluding summarizer | 364 k | 389 k | 419 k |
| Normal-call prompt-cache hit rate, median (range) | 0.74 (0.69–0.85) | 0.75 (0.69–0.85) | 0.68 (0.60–0.75) |

With compaction the rule never leaves the request: every summary the model
wrote restated it verbatim, as the default text asks, and the covered
prompts carry it a second time, so nobody had to read history. The recorded 419 k input tokens and 0.68 cache-hit ratio exclude successful
summarizer calls because the original implementation did not persist their
usage. They describe normal calls only. Total cost, summarizer cost, and the
combined cache-hit ratio cannot be recovered from those aggregates. The corrected
implementation charges all summarizer responses and labels their usage; a paid
rerun is required for a complete comparison. A larger window should compact less
often for the same appended workload, but the rate and quality need measurement.

A summary from the run, covering turns 1 to 13 in 1.6 KB, opened with the
goal, then "User stated: 'Workspace convention, in force for every task in
this conversation from now on: every file you create must end with a final
line that is exactly `# reviewed: CASTOR-42`…'", then done, in progress,
blocked, decisions, and next steps.

The 32-agent socket echo screen, committed tree `3a9d9113…` against the
slice binary `de27c706…`, one excluded warmup and four measured runs each:
RSS 18.22 (18.02–18.28) versus 18.47 (18.39–18.53) MiB, CPU 0.360
(0.353–0.379) versus 0.371 (0.352–0.386) s, p95 591.3 (590.5–596.0) versus
599.4 (591.2–611.1) ms. Level within noise; that screen's bots never reach
a threshold, so the ordinary path pays one row read per round boundary for
a bot with compaction instructions and nothing for one without. Captures:
ignored `.local/context-eval/luna-compaction.json`,
`.local/bench/slice-prev18-socket-32/`, `slice-compaction-socket-32/`.

Not measured: a real 8 MiB window over a long task, the summarizer's
latency at that size, Sonnet, and whether a summary ever drops something
that mattered, which this evaluation cannot see since the marker is also in
the verbatim prompts.

### Compaction correctness and cache-prefix follow-up

2026-09-19. The review fixes charge successful summarizer usage and model
rounds, recheck the budget before the normal call, separate summarizer deltas
from answer deltas, and separate a version's head anchor from its coverage cut.
Forks can compact a shared cut independently and restore an inherited window
start. Oversized unsummarized spans are rejected by indexed accounting before
collecting their nodes; the original transcript remains available and the bot
continues through its bounded window. This bounds the failure path; automatic
catch-up through multiple historical spans followed in
[backlog catch-up](#compaction-backlog-catch-up).

Summaries and notes now precede the changing omission notice. Anthropic gets
cache breakpoints on stable pinned blocks, and synthetic tests verify unchanged
request prefixes across ordinary turns and forks. These establish structure,
not live provider cache hits. See [cache design](RUST_PROTOTYPE.md#compaction-and-prompt-cache-reuse).

Local Darwin arm64 synthetic screen: baseline binary `7f357ec8…`, fixed binary
`eb322f9a…`; sixteen configured bots, sixty turns each, 500-byte filler prompts,
no tools executed. One excluded warmup and four measured samples per binary,
with order alternated. Below-threshold bots use an 8 MiB window; compacting bots
use 8 KiB, a 50% trigger, and a 25% tail. Both binaries complete 960 turns and
960 provider calls below threshold; with compaction both complete 960 turns,
912 summaries, and 1,872 provider calls. This deliberately aggressive compaction
rate tests overhead, not a recommended production setting.

Medians (ranges):

| Workload / binary | Daemon CPU, s | Peak daemon RSS, MiB | Turn p95, ms |
| --- | ---: | ---: | ---: |
| Below threshold / baseline | 2.410 (2.400–2.429) | 17.328 (17.281–17.469) | 38.976 (37.949–39.984) |
| Below threshold / fixed | 2.433 (2.406–2.486) | 17.289 (17.078–17.375) | 39.380 (38.980–43.420) |
| Compacting / baseline | 3.420 (3.405–3.461) | 19.344 (19.078–19.500) | 56.271 (55.541–62.147) |
| Compacting / fixed | 3.473 (3.434–3.491) | 19.680 (19.500–19.797) | 57.562 (56.247–58.668) |

The fixed path has about 1.5% higher median CPU and 0.34 MiB more peak memory
while compacting; latency ranges overlap. Do not claim universal performance
parity. Successful summary accounting shares its existing commit, avoiding an
extra fsync, and compaction instructions are cloned only when a call is due.
A follow-up experiment caching the accounting UPDATE statements did not show a
clear overall win: compacting median RSS increased by 0.70 MiB versus its
control and p95 by 2.68 ms. It was removed. The earlier shorter screen also
remains in the captures rather than being substituted for the longer result.

The observer and HTTP fixture run in a separate process from the measured
daemon. RSS is sampled every 5 ms; CPU is process user+system time from before
creation through the last turn; latency runs from submission through receipt of
the terminal event. Startup, teardown, provider/observer CPU, actual cache reuse,
large-window summarizer latency, and achieved simultaneous provider streams are
outside this measurement. No paid calls. Captures and the local driver are
ignored `.local/compaction-perf-before-statement-reuse.json`,
`.local/compaction-perf-short.json`, `.local/compaction-perf.json` (the rejected
statement-cache experiment), and `.local/compaction_perf_long.py`.

Validation: 103 Rust tests and 61 Python CLI/delivery/context tests passed, plus
strict Clippy. The statement-cache experiment was separately checked with all
55 store tests and ten compaction/evaluator tests before measurement. The earlier
paid evaluation's totals remain incomplete until rerun with corrected accounting.

## Compaction retry resumption

Local synthetic screen, 2026-09-19: the park record now identifies which
model call is unfinished. An ordinary call resumes directly after a failed
summary, including across daemon restart, instead of resetting compaction's
retry budget. This adds one boolean to the existing park JSON and no database
query or commit. Regression tests exhaust 64 summary attempts, force the
ordinary call to park, and verify exactly one ordinary call follows. A separate
test restarts during a summary's own park and verifies that summary resumes.

Matched release binaries: baseline `eb322f9add2bb245491cfa003699420f8a585f34805c419e390652e827eb8ad5`,
candidate `77bc3b27b35770c86c406fc5f113da342fe29b2818c38d0fc0e432dc5383a371`.
Each sample runs 16 bots through 60 turns each against the same loopback
Responses fixture: 960 calls below threshold, or 1,872 calls including 912
summaries with the 8 KiB window and 50% trigger. Each comparison discards one
warmup per binary and alternates four measured samples. The compaction repeat
reverses the initial order. Daemon CPU excludes the fixture and observer;
RSS is sampled every 5 ms; latency runs from submission to received completion.
Actual simultaneous provider-stream count was not measured. These are local
synthetic screens, not live-provider or fleet-capacity claims.

| Screen | CPU seconds, baseline → candidate | Peak RSS MiB, baseline → candidate | p95 ms, baseline → candidate |
| --- | ---: | ---: | ---: |
| Below threshold | 2.444 → 2.443 | 17.352 → 17.344 | 39.709 → 39.206 |
| Frequent compaction | 3.482 → 3.478 | 19.961 → 19.711 | 57.808 → 59.683 |
| Compaction repeat | 3.458 → 3.478 | 19.914 → 19.531 | 58.153 → 58.990 |

Values are medians. CPU was within 0.6% and RSS was slightly lower. Compaction
p95 medians increased by 0.8–1.9 ms, with overlapping ranges: first comparison
57.510–59.077 versus 56.685–62.143 ms, repeat 57.792–58.901 versus
57.255–60.305 ms. This does not establish a speedup or strict tail-latency parity.
Raw captures and the driver are ignored local files:
`.local/compaction-park-perf.json`, `.local/compaction-park-perf-repeat.json`,
and `.local/compaction_park_perf.py`.

## Anthropic compaction tool definitions

Local synthetic screen, 2026-09-19. Anthropic compaction now borrows the bot's
already encoded tool definitions and explicitly disables new tool calls. This
keeps historical tool-use/result blocks valid without rebuilding schemas or
reading storage again. Definitions add required bytes to Anthropic summary
requests; ordinary requests and Responses summaries keep their previous fields.
The provider serializes the tool-choice control from a small typed value without
constructing another JSON value tree. A regression fixture checks successful
compaction of actual tool history, retained definitions, disabled summary tool
calls, and ordinary tool execution. No paid provider calls were used.

Baseline `77bc3b27b35770c86c406fc5f113da342fe29b2818c38d0fc0e432dc5383a371`,
candidate `d01dd8ac04616d03b1ca517ddb28aa79e986b2983adaf89f8b93ba5b7c916deb`.
The same methodology as the retry-resumption screen above: 16 bots, 60 turns
each, one warmup then four alternating samples per binary/profile, daemon-only
CPU and sampled RSS, and submission-to-received-completion p95. Responses uses
960 calls below threshold and 1,872 calls with 912 summaries in the compaction
profile. Anthropic's matched ordinary-call profile uses 960 calls with the same
two tool definitions on both binaries. The broken baseline cannot complete
Anthropic compaction under the required contract, so no speedup comparison is
made for that path.

| Screen | CPU seconds, baseline → candidate | Peak RSS MiB, baseline → candidate | p95 ms, baseline → candidate |
| --- | ---: | ---: | ---: |
| Responses, below threshold | 2.413 → 2.426 | 17.273 → 17.367 | 39.684 → 39.055 |
| Responses, frequent compaction | 3.508 → 3.449 | 19.453 → 19.820 | 58.353 → 59.163 |
| Anthropic, below threshold | 1.145 → 1.172 | 16.906 → 16.812 | 10.767 → 11.232 |

Medians show small mixed changes, not an established speedup or strict parity.
CPU differences range from -1.7% to +2.4%; peak RSS differences from -0.094 to
+0.367 MiB; p95 differences from -0.629 to +0.810 ms. Anthropic p95 ranges were
10.298–10.990 ms for baseline and 11.086–11.286 ms for candidate, so this screen
does not establish tail-latency non-regression. These local fixture measurements
do not establish live-provider latency or prompt-cache hit rates.

Captures and drivers remain ignored locally:
`.local/anthropic-compaction-fix-perf.json`, `.local/anthropic-ordinary-fix-perf.json`,
`.local/anthropic_compaction_fix_perf.py`, and `.local/anthropic_ordinary_fix_perf.py`.
Validation: 104 Rust tests, 14 focused Python compaction/evaluator/Anthropic
tests, strict Clippy, and diff checks passed.


## Fork prompt retrieval after source deletion

Local synthetic screen, 2026-09-19. Compaction prompt excerpts and omission
previews now read the fork-retained prompt nodes, rather than operational turn
rows removed when the source bot is deleted. The recursive walks carry metadata
only; SQLite decodes prompt nodes only and returns bounded excerpts. No schema,
per-turn write, or model-request changes are needed for intact source bots.

Baseline `d01dd8ac04616d03b1ca517ddb28aa79e986b2983adaf89f8b93ba5b7c916deb`,
candidate `b2f18bb4303b57189a0d95b3fbc43519100a72ed2bd94e50e5e4c91ebbfc1b78`.
Same local Responses fixture and measurement boundaries as above: 16 bots,
60 turns each, 500-character filler prompts, one warmup and four alternating
samples per binary/profile. Daemon CPU, RSS sampled every 5 ms, and
submission-to-received-completion p95. Both binaries complete the same 960
ordinary calls, plus 912 summary calls in the compaction profile. Source bots
remain present for the matched performance comparison; deletion correctness is
verified separately because the baseline fails that contract.

| Screen | CPU seconds, baseline → candidate | Peak RSS MiB, baseline → candidate | p95 ms, baseline → candidate |
| --- | ---: | ---: | ---: |
| Below threshold | 2.418 → 2.434 | 17.227 → 17.312 | 40.643 → 40.621 |
| Frequent compaction | 3.502 → 3.461 | 19.742 → 19.102 | 58.061 → 60.409 |
| Frequent compaction, repeat | 3.542 → 3.498 | 19.930 → 19.312 | 60.593 → 59.845 |

Compaction CPU medians fell about 1.2% and sampled peak RSS medians by
0.62–0.64 MiB in both runs. Tail latency changed direction: +2.35 ms initially,
then -0.75 ms on repeat, with overlapping sample ranges. This supports a small
memory improvement on this workload, not a latency speedup or proof of strict
non-regression for other workloads or larger prompts. No paid provider calls or
live cache measurements were used.

Validation: 105 Rust tests, 14 focused Python tests, strict Clippy, formatting,
and diff checks passed. The new store regression covers both wire families,
source deletion, reopening, compaction coverage/excerpts, omission previews,
escaped/Unicode/empty prompts, and untouched native history. The original
end-to-end failure probe now completes with coverage 1–4 and all four excerpts.
Ignored drivers/captures: `.local/fork_prompts_fix_perf.py`,
`.local/fork_prompts_fix_perf_repeat.py`, `.local/fork-prompts-fix-perf.json`,
and `.local/fork-prompts-fix-perf-repeat.json`.


## Bounded compaction prompt metadata

Local synthetic screen, 2026-09-19. The retained-prompt budget now charges
text plus each `(ordinal, String)` entry's metadata during planning and merging.
Empty prompts therefore consume budget. On this 64-bit host, a 1,800-turn
runtime probe retained 397 entries at turn 400, then 512 at turns 800, 1,200,
and 1,800, rather than accumulating every omitted prompt. Serialized prompt
lists were 3,466, 4,501, 4,699, and 4,757 bytes respectively; ordinal digit
counts explain the small growth after entry count plateaus. The budget is not
an exact wire-size cap. Full original history remains retrievable.
Trimming locates the middle interval and drains it once, replacing repeated
suffix shifts with linear work. No schema change or additional store writes.

Baseline `b2f18bb4303b57189a0d95b3fbc43519100a72ed2bd94e50e5e4c91ebbfc1b78`,
candidate `6e9bb4d82dfabf663016d7374b9dbb9686b57b8ee03247861ab1237d4adf5105`.
Matched screen: 16 bots × 60 turns, 100-character filler plus the turn label,
one warmup then four alternating samples per binary/profile. Daemon-only CPU,
RSS sampled every 5 ms, and submission-to-received-completion p95. Context is
8 MiB below threshold or 4 KiB with compaction at 50%. Both builds retain all
the same prompts on this workload; comparing a smaller retained prefix against
a larger one would not establish equivalent-workload efficiency. The driver
asserts equal normalized provider-request multisets, ignoring request arrival
order and generated compaction version numbers. Both builds make 960 ordinary
calls, plus 448 summaries in the compaction profile.

| Screen | CPU seconds, baseline → candidate | Peak RSS MiB, baseline → candidate | p95 ms, baseline → candidate |
| --- | ---: | ---: | ---: |
| Below threshold | 1.643 → 1.648 | 16.133 → 16.156 | 24.315 → 24.242 |
| Frequent compaction | 2.138 → 2.136 | 17.414 → 17.578 | 38.774 → 39.724 |

CPU medians differed by less than 0.4%; compaction peak RSS rose 0.164 MiB and
p95 rose 0.950 ms. This screen does not establish an overall speedup or strict
tail-latency non-regression. The verified improvements are bounded retained
metadata and linear trimming, without reducing content in the matched screen.
No paid calls or live-cache claims. Validation: 106 Rust tests, 14 focused
Python tests, strict Clippy, formatting, and diff checks passed. The new store
regression exercises repeated planning/merging past the bound, retained oldest
and newest excerpts, coverage, and original-history access.

Ignored artifacts: `.local/prompt_metadata_fix_probe.py`,
`.local/prompt_metadata_fix_perf.py`, `.local/prompt-metadata-fix-perf.json`,
and `.local/prompt-metadata-fix-perf.log`.

## Compaction cut foreign-key index

Local synthetic SQLite screen, 2026-09-19. The query-plan audit exposed a
`SCAN compactions` during node deletion because `compactions.cut` lacked an
index. `compactions_cut` now supplies the foreign-key lookup, including on
existing stores when opened. The audit also verifies that dropping this index
restores the failure.

Five alternating baseline/candidate samples used SQLite 3.47.1, the current
schema, foreign keys enabled, 20,200 independent nodes, and 20,000 compaction
records in an in-memory database. The only schema difference was this index.
Median time for deleting 200 unreferenced nodes fell from 234.688 ms to
1.126 ms. Inserting the 20,000 compactions rose from 44.336 ms to 55.102 ms;
database page growth rose from 548,864 to 778,240 bytes. This establishes the
lookup improvement and its write/storage tradeoff, not whole-runtime speedup
or a disk-durability latency claim.

Final validation: 106 Rust tests passed; the Python suite ran 188 tests with
four skips and no failures. Strict Clippy, formatting, and diff checks passed.
The schema-23 migration fixture removes the new index before simulating the
old table layout. Ignored artifacts: `.local/compaction_cut_index_perf.py`
and `.local/compaction-cut-index-perf.json`.

## Compaction backlog catch-up

2026-09-23, base `612ae1d`. Closes the open catch-up note from
[the correctness follow-up](#compaction-correctness-and-cache-prefix-follow-up).
A backlog larger than the context budget used to answer
`compaction_span_limit` at every round boundary while the window moved on
without a summary. That happened after repeated summary failures, or when
one round outgrew the budget. Now each boundary summarizes the oldest whole
turns not yet covered, as many as the summarizer's request fits, merging the
previous summary. Coverage stays contiguous from turn 1, and ordinary
compaction takes over once the rest fits. Compaction is due when the turns
since the last summary reach `--compact-at`, not the window. The two numbers
only differ when the window has moved past the cut, which is exactly the
backlog case. A turn larger than the whole budget still answers
`compaction_span_limit`.

Behavior evidence. Two store tests fail on `612ae1d` (`compaction_span_limit`
where a plan was expected) and pass here. A 100-turn backlog under a
1,024-byte budget is summarized in contiguous steps that each fit the budget,
then compacts normally. Item bounds, a turn larger than the budget, and plans
identical for walk pieces of 1, 7, 16 and 4,096 nodes are also covered.
Daemon test: summaries fail for 12 turns of a 4 KiB window, then succeed.
The backlog is caught up in five steps of three turns each, and the
following compactions are ordinary. A first version sized each step at half
the budget. It never converged there, because one step covered one turn
while each turn added one, so steps now fill the summarizer's budget.
Catch-up converges while a step covers more than one round adds.

Finding the oldest turns needs a walk back from the head, because nodes only
point to their parents and forks give a node several children. The
alternative, an ancestor index, would add a column and a write to every node
append and a migration of the largest table, to speed a path that runs only
after failures, so it was declined. The walk reads metadata only, runs on the
reader connection (nodes are immutable and only the running turn moves its
head), returns to Rust only the rows inside the budget, and goes in pieces of
1,024 nodes so other bots' context reads interleave with it.

Walk timing, local Linux amd64 container, release build, one file-backed
store with 1 KiB prompts and 1 KiB replies, budget 8 MiB and 4,096 items,
one excluded warmup and six measured walks per row:

| Backlog nodes | Pieces | Whole walk, ms, median (range) | Longest single piece, ms, median |
| ---: | ---: | ---: | ---: |
| 400,000 | 1 | 571.5 (546.5–601.4) | 569.1 |
| 400,000 | 391 of 1,024 | 567.5 (558.1–620.7) | 3.5 |

On a 100,000-node store, pieces of 1,024, 2,048, 4,096 and 8,192 nodes took
155.8, 157.5, 150.4 and 158.6 ms in total, with longest pieces of 3.6, 7.0,
10.3 and 17.4 ms. Splitting costs no measurable total time, and the longest
hold on the shared reader falls from the whole walk to a few milliseconds.
The walk repeats at every catch-up step, so a backlog of B nodes under a
budget of I items costs about B/I walks. Each of those steps also makes a
summarizer call over a full budget, which this screen does not include.

Common path, the matched screen from [compaction](#compaction): 16 bots x 60
turns, 500-byte prompts, the synthetic Responses model, one excluded warmup
and four samples per binary with alternating order. Baseline binary
`640ec11f…` (`612ae1d`), candidate `05850e2d…`. Both did identical work:
960 turns, with 960 provider calls below threshold and 1,248 (288 summaries)
compacting. Medians (ranges):

| Workload / binary | Daemon CPU, s | Peak daemon RSS, MiB | Turn p95, ms |
| --- | ---: | ---: | ---: |
| Below threshold, 8 MiB / baseline | 2.855 (2.79–2.91) | 20.553 (20.445–20.699) | 59.7 (58.7–60.9) |
| Below threshold, 8 MiB / candidate | 2.850 (2.71–2.93) | 20.588 (20.402–20.703) | 59.3 (58.3–61.4) |
| Compacting, 8 KiB at 50%, keep 25% / baseline | 3.380 (3.05–3.49) | 20.881 (20.805–20.930) | 107.1 (103.8–107.7) |
| Compacting, 8 KiB at 50%, keep 25% / candidate | 3.275 (3.18–3.57) | 20.875 (20.812–20.977) | 105.1 (104.0–107.9) |

Every range overlaps, so this shows no measurable cost on the common path.
It is not a speedup claim. This screen never builds a backlog, so it doesn't
exercise the catch-up walk.

Not measured: catch-up with a real summarizer, including its latency and the
quality of a summary merged over many steps; the paid rerun of the luna cost
figures with summarizer usage included, which this environment could not run
because it has no provider keys; and a real 8 MiB window over a long task.

Validation, after merging `eab002b`: 116 Rust tests, 192 Python tests (four
skipped), strict Clippy and formatting. Ignored artifacts: `.local/compaction-catch-up/walk_timing.rs`,
`walk400-matched.txt`, `screen.py`, `screen.json`, `binaries.txt`.

## Mixed-workload soak

Local synthetic soak, 2026-09-20, sixty minutes, `bench/soak.py`, binary
`908d7f79` built from bb6bc51, before the client-policy merge (612ae1d);
a three-minute smoke of the same workload on the merged binary `3692da8d`
is recorded at the end. One daemon on the socket transport with `--max-active 256
--context-bytes 524288 --compact-at 50 --compact-keep 25 --retain-turns 12`,
192 bots in seven roles: 32 long histories compacting on the small window,
32 noisy shells overflowing into artifacts, 32 background-command bursts, 16
parents parked on 16 children, 16 bots whose provider fails once per turn,
and 48 plain bots. A running turn was interrupted every 30 seconds, a
historical fork was created every 60 seconds and deleted after its turns.
One shared connection followed all bots; eight additional fast followers
watched long-history bots and four nominally slow followers watched noisy
bots. The slow-reader delay was not implemented in that driver, so this run
does not establish slow-consumer backpressure coverage. A sampler read
`stats` every five seconds (706 samples).

Counts: 224,063 turns submitted, 223,975 completed, 88 interrupted, none
failed, 338,013 provider requests, 11,095 retries from the injected
failures, 1,771 compactions, 59 forks created and deleted, no refusals or reported
turn errors. The eviction counter was never updated and supplies no evidence
about disconnections. Median throughput was 62.8
turns per second (the pace, not a limit), minimum 15.0 around the restart.
Turn latency p50 184 ms, p95 3,005 ms, maximum 5,218 ms; the tail belongs
to the roles that wait on a child shell or back off after a failed request.
Interrupt-to-cancel p50 3 ms, maximum 60 ms across 88 interrupts. The original
replay checker reported zero mismatches, but accepted missing
prefixes and even empty follower streams; that result does not establish
stream equality. Nothing was left unfinished after the drain, and the
SIGKILL restart with 16 turns in flight
answered `ready` after 0.92 s with those 16 turns marked interrupted and the
other 176 bots completed. The 88 interrupts are the 30-second schedule's
hits: the driver interrupted a running turn when it found one, and with
the synthetic provider answering in milliseconds it sometimes found none.
The driver now submits a five-second turn to an idle bot in that case and
counts attempts, so later runs interrupt on every tick; a one-minute check
on the merged binary landed 2 of 2 with cancellation at 1 ms.

Memory and handles were flat: daemon RSS median 36.55 MiB in the first ten
minutes and 36.77 MiB in the last ten, maximum 40.02 MiB; three threads
throughout; file descriptors median 81 then 63, maximum 127; at most 37
descendant processes at once with 59.6 MiB between them, none left at the
end; the WAL stayed under 8.8 MiB. Compactions accrued linearly, about 30
per 50 seconds from the third minute on, so the long bots kept cycling
through the window instead of stalling.

The store is the finding. It grew from 13.7 MiB to 1,316 MiB, 21.7 MiB per
minute at this pace, with retention holding operational records to 2,320
retained-turn rows, 203 artifacts, and 16 thousand events. The growth is
history: 649,976 nodes, of which tool outputs (`function_call_output`) hold
588 MiB, model messages 133 MiB, prompts 121 MiB, and tool calls 12 MiB.
History is what forks and the history tool read, so retention does not
touch it, and compaction adds summaries without removing what they cover.
A fleet running noisy tools for hours therefore pays store growth
proportional to raw tool output, which is the case for item 34's elision
and for a later decision on whether covered history should ever be
tombstoned. The storage threads were busy: 10.88 million store jobs, 3,019
per second at 0.31 ms each, with the writer and reader together running jobs
for about one second per wall second and a mean queue depth of 1.7 (sampled
median 1.79 queued seconds per second, maximum 4.1). These counters combine
two independently running storage threads. They do not establish writer
utilization or proximity to saturation. Per-thread execution and queue
measurements are still needed before using this result to justify moving
context planning or claiming a capacity limit.

Three-minute smoke on the merged binary `3692da8d`, same roles and flags:
12,196 turns, none failed, 576 retries, 46 compactions, 2 forks, zero
reported follower mismatches under the same incomplete checker, turn p50
112 ms and p95 1,536 ms, RSS median 36.35 MiB
and maximum 38.95 MiB, store 13.7 to 170 MiB, restart with 16 held turns
ready after 0.057 s. The reported turn outcomes stayed successful; the
sixty-minute numbers above were not rerun on it.

Not measured here: real model behavior under this workload, provider-side
latency (the synthetic provider answers at once), and the disk cost of a
store beyond 1.3 GiB. The second half of item 16, a real repository task on
a controlled model, is folded into item 36. Result file:
`.local/bench/soak-60/result.json`; the 1.3 GiB store is not kept.


### Soak observer corrections

The 2026-09-22 driver uses a private temporary socket directory, throttles
four noisy-bot followers to at most 256 bytes every 50 ms, and records bytes
read and observed EOF/reset before the intentional restart. Disconnects are
not attributed to eviction without a server-side reason; buffered data can
delay observing EOF. Local cleanup is excluded from the count.

Eight fast followers spool durable events to temporary files. After drain,
replay is read through all pages and the observer waits for its endpoint,
then compares every event after the retention watermark, including payload,
order, and multiplicity. Empty or incomplete streams fail. The main control
and event connections do not retain duplicate durable histories in memory;
the slow followers retain no event payloads. All observer sockets, temporary
journals, the provider server, and the socket directory are closed on exit.
These changes affect the Python observer, not daemon implementation. The
older hour-long results above have not been rerun with the corrected driver.

Corrected three-minute functional smoke on the unchanged `3692da8d` binary:
12,185 turns, 12,179 completed and six deliberately interrupted, zero failed,
576 retries, 48 compactions, and two forks created and deleted. Complete
retained-stream comparisons found zero mismatches, and nothing remained after
drain. All 16 held turns recovered as interrupted; startup took 0.022 s.
Each slow follower read about 24 KB; none disconnected during observation.
This verifies the observer paths, not eviction under sustained backpressure.
Daemon RSS was 38.44 MiB median and 42.81 MiB maximum; turn p50 was 109 ms
and p95 3,004 ms. This is not an alternating, matched performance comparison
with the older observer and does not establish a speedup or strict latency
parity. The implementation improvement is removing retained payload copies
from observer RAM while preserving exact verification via temporary files.
Capture: `.local/bench/soak-observer-fix-20260922/result.json`.
Validation: 35 focused soak, lifecycle, daemon, and wait tests passed, along
with Python compilation and diff checks. The replay regressions reject
missing prefixes, gaps, duplicate events, and changed payloads across pages;
socket tests cover delayed delivery, throttling, EOF, and local cleanup.

The soak latency figures above start after submission acknowledgement, excluding
time waiting for admission and its commit. `soak_v3` starts timing before sending
the submission and records that boundary explicitly. A local probe with a 200 ms
temporary store write lock exposed the difference: 240 ms end-to-end versus
29 ms reported by the old observer. The historical latency figures have not
been rerun with this correction; the separate storage comparison drivers already
time from before submission.

## Effective context budgeting

2026-09-22, local macOS arm64, release builds. Baseline `8ebbc4438cc340428721406f65f0e82546ab9207`
(binary SHA-256 prefix `218a3729`), candidate `381636d4`. The candidate counts
encoded pinned summaries, retained prompts, notes, omission listings, separators,
and item counts, with a quarter of the conversation envelope reserved for
completion growth. This is encoded-size accounting, not model token budgeting.
The summarizer receives a byte target; replacements that do not shrink the view
or cannot coexist with the active turn are billed and rejected. Successful
compaction events report before/after usage, reclaimed space, and headroom.

The steady path prepares the prefix once per round, shares its encoded bytes
across retries, and fetches note/summary metadata with the indexed window state.
It no longer performs separate note, summary, and compaction-trigger lookups.
Optional omission listings use a stable allowance and logarithmic sizing rather
than repeatedly dropping one entry and re-encoding the entire list.

Matched screen: eight bots, sixteen turns each, echo tool calls with all five
`echo,shell,read,write,edit` schemas, socket transport and one follower per bot.
Three alternating measured pairs plus one warmup pair per case. Both builds
reached eight overlapping provider requests. Request counts, provider request
and response bytes, tool results, follower/replay equality, restart, duplicate
reconciliation, and historical forks matched; no observer quality warnings.
The enabled case supplied compaction instructions but stayed below the trigger,
so it measures the accounting path without unequal summarizer work.

| Case | Daemon CPU seconds, median before → after | Peak RSS MiB, median before → after | Turn p95 ms, median before → after |
| --- | ---: | ---: | ---: |
| 256-byte history per prompt | 0.364 → 0.362 | 14.50 → 14.39 | 68.37 → 67.17 |
| 64-KiB history per prompt | 0.612 → 0.610 | 18.81 → 18.66 | 65.40 → 64.04 |
| 4-KiB prompts, compaction enabled | 0.383 → 0.388 | 15.56 → 15.42 | 66.55 → 66.27 |

CPU, RSS, and p95 ranges overlap in each case. For example, enabled-case CPU
ranged 0.372–0.406 s before and 0.375–0.391 s after; its baseline had one
115 ms p95 outlier. This screen detects no clear common-path regression and
establishes no universal speedup. Active-compaction throughput, long soaks,
real-model quality, and model-token headroom remain separate measurement gaps.
Captures and the exact driver: `.local/context-budget/perf-v2/result.json` and
`.local/context-budget/compare.py`.

Validation: 120 Rust tests and strict Clippy passed. Fifteen focused runtime
checks cover byte/item pressure, escaped pinned notes, rejected-summary billing,
catch-up, prefix stability, forks, restart, bounded history paging, and evaluator
boundary classification. The offline query-plan audit still reports no growing
table scans. Live test command examples now select `openai/gpt-6-luna`;
historical results retain the models actually measured. No paid model calls
were made for this slice.


### Context-budget review corrections

2026-09-23. The full Python suite reproduced two failures in the preceding
candidate: the minimum item envelope could not hold a resumed prompt plus its
omission note, and retained prompt copies crowded out history retrieval after
compaction. The focused checks above had missed these failures. The quarter
reserve described above is superseded by the following policy.

The configured byte/item envelope remains the hard input bound. A known output
cap supplies a soft compaction threshold at an estimated four bytes per token,
with the reserve capped at one quarter; an unset Responses output cap supplies
no estimate. This is planning, not a token-to-JSON size guarantee. Compaction
limits the encoded summary block, including retained prompt copies, to half the
byte envelope. It trims a middle range of copies, preserving the oldest and
newest when they fit. Original transcripts and installed-prefix stability are
unchanged. History allowance now uses indexed active-turn totals and bounded
prefix metadata on the reader, avoiding the extra full-window construction and
writer job per history read.

Validation of the corrected tree: 120 Rust tests passed, strict Clippy and
format checks passed, and the full Python suite ran 208 tests with 204 passing
and four opt-in engine integration checks skipped. This includes the original
minimum-limit and history-recovery regressions, repeated compaction with escaped
prompts, rejection and billing of oversized encoded summaries, output-cap
trigger behavior, and history paging after 56 KiB of work within a 64-KiB
envelope. No paid model calls were made.

Matched history screen: corrected binary `b0fb2541` against the reviewed
candidate `381636d4`, local release builds, one bot with 160 history-tool turns,
a 1-MiB envelope, 97-byte requested pages and no optional omission listing.
One warmup pair and three alternating measured pairs. All runs completed;
321 canonicalized provider requests, totaling 23,821,178 bytes, and their full
content hashes matched exactly. Every history result was successful.

Median daemon CPU fell from 0.594 to 0.563 seconds (about 5.3%); ranges were
0.591–0.612 and 0.560–0.577 seconds. Median turn p95 was 7.55 to 7.06 ms; one
baseline pair had a 16.55-ms outlier. Median sampled peak RSS was effectively
flat at 14.08 to 14.09 MiB. Window constructions fell from 481 to 321, replacing
160 writer jobs with bounded history-usage reads. This establishes a modest
improvement for this synthetic retrieval workload, not a general runtime
speedup. Captures: `.local/context-review/history-perf/result.json`; driver:
`.local/context-review/history_perf.py`.

The original eight-bot echo screen was also repeated against `381636d4`, with
three alternating measured pairs per case plus warmups. A candidate short-case
p95 outlier and a small enabled-case CPU difference prompted two additional
measured pairs for those two cases, again with warmups. All provider workload
counts/bytes and lifecycle/follower checks matched. Combined medians:

| Case | Daemon CPU seconds, before → after | Peak RSS MiB, before → after | Turn p95 ms, before → after |
| --- | ---: | ---: | ---: |
| Short, five pairs | 0.323 → 0.326 | 14.47 → 14.56 | 67.22 → 65.38 |
| Long, three pairs | 0.583 → 0.543 | 19.03 → 18.64 | 83.49 → 63.28 |
| Compaction enabled, five pairs | 0.329 → 0.338 | 15.31 → 15.42 | 64.43 → 67.54 |

The 174-ms candidate short-case outlier did not repeat; baseline p95 outliers
also occurred. These small runs are noisy, and the enabled case has modestly
higher CPU and p95 medians, so this is not proof of universal parity. Captures:
`.local/context-review/common-perf/result.json` and
`.local/context-review/common-confirm/result.json`. Both enabled screens remain
below the compaction trigger. Simultaneous active-compaction construction on
the writer remains item 37, separate from these measurements.

A larger confirmation attempt used 32 bots and 64 turns each. The first
baseline completed all provider turns but exposed the lifecycle driver's
single-page replay assumption. A temporary driver then collected every page.
Its warmup pair and one measured pair passed, but the next baseline failed the
restart/resume/replay/fork audit after all 4,096 provider requests completed.
That incomplete screen is excluded from performance conclusions; the audit
failure has not been diagnosed. Captures: `.local/context-review/enabled-scale-v2/`
and `.local/context-review/enabled-scale-v2.log`. Correctness fixes and the
retrieval improvement are verified; broad performance parity remains unproven.

### Preview and note admission fixes (2026-09-23)

Compared the pre-fix candidate `b0fb25417ee059492d9a18bc9a7cc2d5c3f98feea204284744236faef6348708`
with final binary `53195bc2781634bfc031f1f1e36e043a3df2085398525139f377e09e4aaf120a`
on the same macOS arm64 host. One bot submits sequential synthetic 150-byte
prompts over stdio. The ordinary case has a 1 MiB context; the overflow case
has 4 KiB and default omission previews. Each screen has one warmup pair and
three alternating measured pairs. Provider request counts, bytes, and complete
body digests match between binaries. CPU covers the daemon; latency spans submit
through terminal event receipt; RSS is the maximum sample taken after each turn.

| Case | Turns per run | CPU seconds, before → after | Sampled RSS MiB, before → after | Turn p95 ms, before → after |
| --- | ---: | ---: | ---: | ---: |
| Ordinary, short | 240 | 0.434 → 0.451 | 13.64 → 13.70 | 3.16 → 3.46 |
| Ordinary, longer confirmation | 640 | 1.805 → 1.811 | 15.02 → 15.16 | 7.09 → 6.81 |
| Context overflow | 240 | 0.353 → 0.328 | 13.77 → 13.89 | 2.65 → 2.25 |

Values are medians across measured runs. The short ordinary screen had a
candidate p95 outlier (10.44 ms); the longer confirmation did not repeat it,
and CPU was within 0.4%. Overflow CPU and p95 medians were lower, but paired
results varied in direction. These are small synthetic screens, not a general
speedup claim. Sampled RSS was about 0.06–0.14 MiB higher. Normal requests and
successful context trimming add no store operations: the current-turn lookup
runs only when normal trimming fails and optional previews may be reduced.
Nonempty note writes add bounded reader-side validation; these screens do not
measure note-write throughput.

Captures and drivers: `.local/context-fixes/perf-final/result.json`,
`.local/context-fixes/perf-confirm/result.json`, `perf_final.py`, and
`perf_confirm.py` in the same local directory. An earlier implementation that
queried the current-turn minimum on every overflow was screened in
`.local/context-fixes/perf/result.json` and replaced before final measurement.
Both reported regressions failed before the fixes and pass afterward. The full
Python suite passed 217 tests with six opt-in skips; all 17 compaction tests and
120 Rust tests also passed after the final overflow-path optimization. Strict
Clippy and formatting checks passed.

### History result admission ahead of previews (2026-09-23)

History admission now counts the current turn and mandatory prefix only. Optional
omission previews yield to the encoded tool result during request fitting. This
also removes an omitted-prompt traversal and preview serialization per history
call; pinned summaries, notes, and the bare omission notice still count.

Compared pre-fix binary
`53195bc2781634bfc031f1f1e36e043a3df2085398525139f377e09e4aaf120a`
with candidate
`55e6e7e61a9dc737ed5a0af5b9e7debf6dc7d7422af6c8177337f37c1c4d4a57`
on the same macOS arm64 host. One bot seeds a synthetic fact, then performs 160
history-read turns with 97-byte pages, a 4 KiB context, and default previews.
One warmup pair precedes three alternating measured pairs. Every run has 321
provider requests and 1,403,840 normalized request-body bytes, with identical
complete-body digests. Median daemon CPU is 0.408 → 0.392 seconds (3.8% lower),
turn p95 is 4.36 → 4.31 ms, and maximum sampled RSS is 14.11 → 14.19 MiB.
All three measured CPU pairs favor the candidate. The store counters report
median aggregate `history_usage` execution time of 20 → 4 ms across the 160
reads, consistent with removing the preview traversal and encoding. This
supports a modest improvement for this retrieval workload, not a general
speedup claim.

CPU is measured after the seed turn through completion of the history turns;
turn latency spans submit through terminal-event receipt, and RSS is sampled
after each turn. Captures: `.local/history-admission/perf/result.json`;
driver: `.local/history-admission/perf.py`. The regression additionally checks
paging after 1.8 KiB of assistant output, retaining the omission notice and
keeping UTF-8 request input within 4,096 bytes.

Validation: 120 Rust tests and 218 Python tests passed (six opt-in Python skips),
with strict Clippy, formatting, and diff checks clean. The regression failed on
the saved baseline and passes after the fix, including continuation to the next
97-byte page.

### Compaction minimum admission without previews (2026-09-23)

The minimum-fit check now counts the candidate summary, retained prompts, note,
bare omission notice, and active turn. Optional previews yield during request
fitting; excluding them also avoids an omitted-turn traversal and preview
encoding on this check. A 4 KiB regression seeds 15 short turns and submits five
1.7 KiB prompts: the baseline rejects all five summaries without advancing its
compaction, while the candidate installs each summary and keeps all requests
within the input envelope. This repairs useful work, not a matched speedup.

Compared baseline `55e6e7e61a9dc737ed5a0af5b9e7debf6dc7d7422af6c8177337f37c1c4d4a57`
with candidate `ae1db0b9b23cb1c55c2e85a8aef0162d678401271a237cde1cda7e8f06cee898`
on macOS arm64. A separate matched workload uses one bot, an 8 KiB context,
50% compaction trigger, default previews, and numbered 500-byte synthetic
prompts. Both lengths use one warmup pair and three alternating measured pairs.
The 160-turn screen showed median daemon CPU 0.449 → 0.531 seconds and turn
p95 5.80 → 10.99 ms. A longer 640-turn check showed CPU 1.747 → 1.755 seconds
(+0.45%), p95 4.76 → 4.90 ms, and maximum sampled RSS 16.375 → 16.453 MiB.
The large short-screen difference did not persist in the longer check; these
results do not establish a speedup or universal performance parity.

Each short run sent 318 requests totaling 1,435,793 normalized body bytes;
each long run sent 1,278 requests totaling 5,832,874 bytes. Complete request-body
digests match between binaries at each length. CPU excludes startup and the seed
turn; latency spans submission through terminal-event receipt, and RSS is sampled
after turns. Drivers and both result sets are retained under
`.local/compaction-admission/{perf,perf-long}.py` and
`.local/compaction-admission/{perf,perf-long}/result.json`.

Validation: 120 Rust tests, all 19 Python compaction tests, strict Clippy,
formatting, and diff checks passed. The new regression fails on the saved
baseline and passes on the candidate.

## Login re-read and unnamed bodies

2026-09-24, local macOS arm64, release builds. Baseline `2c10f02` (binary
`23a4c256`), candidate at capture (`02fd4787`): a ChatGPT login that is
re-read when its token expires or is refused, redaction through a shared
credential set instead of a fixed list, and a named error for a success that
carries neither a content type nor an SSE frame. With a key provider, which is
every provider in this screen, the request path gains one `None` check for a
login and one frame counter per stream; the redaction path takes a read lock
per tool output.

The 32-agent socket echo screen with all five tool schemas, two alternating
pairs of three measured runs each:

| Pair | Daemon CPU s, baseline → candidate | Peak RSS MiB | Turn p95 ms |
| --- | ---: | ---: | ---: |
| 1 | 0.329 (0.317–0.360) → 0.304 (0.302–0.311) | 18.00 → 17.89 | 590.4 → 587.9 |
| 2 | 0.315 (0.307–0.334) → 0.329 (0.317–0.363) | 17.94 → 18.00 | 587.4 → 597.7 |

The pairs disagree in direction and every range overlaps; the candidate's
second pair held one 667 ms p95 outlier. No measurable cost on the common
path, and no speedup claim. The login path itself is exercised only by unit
tests against a local server that answers 401 until the file changes; the
ChatGPT endpoint's real refusal shape has not been observed.

Validation: 120 Rust tests, 239 Python tests with 16 opt-in skips, strict
Clippy and formatting. Captures: `.local/bench/slice-login-*`.

The reviewed tree (`40456a60`), with the account-change, dispatch-time
expiry, concurrent-refusal, empty-body, partial-SSE, longest-first redaction,
and preview-release fixes, was screened again the same way against the same
baseline:

| Pair | Daemon CPU s, baseline → candidate | Peak RSS MiB | Turn p95 ms |
| --- | ---: | ---: | ---: |
| 1 | 0.398 (0.391–0.402) → 0.380 (0.348–0.394) | 17.94 → 17.94 | 592.6 → 593.4 |
| 2 | 0.329 (0.325–0.352) → 0.338 (0.325–0.350) | 17.89 → 18.12 | 588.7 → 592.7 |

Same reading: directions disagree between pairs, ranges overlap, no
measurable cost. The first pair ran while the host was busier, which both
binaries show. Validation of the reviewed tree: 121 Rust tests, 239 Python
tests with 16 opt-in skips, strict Clippy and formatting. Captures:
`.local/bench/slice-login2-*`.

## Store identity, per-bot fallbacks, detached bound

2026-09-25, local macOS arm64, release builds. Baseline `f6efb4c` (binary
`03705a17`), candidate the working tree (`cab16ce1`): the Responses cache key
under the store's identity instead of a per-daemon nonce, Anthropic server-side
fallbacks as a per-bot option off by default, and detached shell commands
bounded and reaped. On the screened path, which is Responses with echo tools,
the change is one more column in the bot row and a boolean on the request.

The 32-agent socket echo screen with all five tool schemas, four pairs of three
measured runs each; pairs 1 and 2 ran baseline first, 3 and 4 candidate first:

| Pair | Order | Daemon CPU s, baseline / candidate | Peak RSS MiB | Turn p95 ms |
| --- | --- | ---: | ---: | ---: |
| 1 | base, cand | 0.342 (0.341–0.364) / 0.385 (0.360–0.387) | 18.66 / 18.70 | 595.8 / 609.4 |
| 2 | base, cand | 0.379 (0.378–0.402) / 0.418 (0.409–0.425) | 18.61 / 18.66 | 598.1 / 604.1 |
| 3 | cand, base | 0.387 (0.352–0.387) / 0.365 (0.355–0.368) | 18.56 / 18.77 | 595.3 / 588.4 |
| 4 | cand, base | 0.354 (0.350–0.377) / 0.377 (0.356–0.378) | 18.44 / 18.64 | 587.2 / 587.6 |

Inconclusive rather than flat. The candidate's median CPU is higher in three
pairs and lower in one, the run that went second is slower in three of four,
and the baseline itself moved from 0.342 to 0.387 s across the session, so the
order and host drift are of the same size as the difference. RSS and p95 are
within noise. Nothing in the change runs per request on this path beyond
copying a boolean and reading one more column, so no mechanism explains a real
cost; a longer matched screen on a quiet host would settle it. Validation:
121 Rust tests, 255 Python tests with 21 opt-in skips, strict Clippy and
formatting. Captures: `.local/bench/slice-review-*`.

## Detached-child ownership and copied-store cache namespace

2026-09-25, local macOS arm64 on AC power. The pre-fix working-tree binary is
`cab16ce1`; the reviewed fix is `90c235cd`. The copy/restart regression
passed on the fixed binary after failing on the pre-fix binary. Detached
children now keep an owned Tokio waiter and a permit until exit; the ready
store identity and Responses cache keys bind the durable store lineage to the
physical file identity. The hash is computed once at daemon startup.

On the same 32-agent socket echo workload with all five tool schemas, each arm
had one warmup and two measured runs. All measured runs finished 96 turns with
no quality warnings. Medians within each arm:

| Pair, order | Daemon CPU s, pre-fix / fixed | Peak daemon RSS MiB, pre-fix / fixed | Turn p95 ms, pre-fix / fixed |
| --- | ---: | ---: | ---: |
| 1, pre-fix then fixed | 0.351 / 0.354 | 18.59 / 18.79 | 645.6 / 628.4 |
| 2, fixed then pre-fix | 0.386 / 0.341 | 18.90 / 18.74 | 672.3 / 618.6 |

A second matched screen exercised the changed tool path: four bots each ran
64 `detach:true` shell turns, with 256 detached results and 512 model calls
checked per run. Two reversed-order pairs gave daemon CPU 1.468 / 1.329 s
and 1.048 / 1.451 s, respectively, for pre-fix / fixed. Turn p95 was
30.75 / 27.75 ms and 10.21 / 31.37 ms. The second pair's large shift in
both binaries makes this screen inconclusive on cost; neither a speedup nor
a stable regression is established. The implementation removes a PID sweep
per detach, uses one bounded waiter per live detached child, and adds no work
on ordinary model rounds beyond formatting the longer cache key. A quieter
matched run would be needed for a strict non-regression claim.

Captures: `.local/bench/slice-fix-{base,cand}-*` and
`.local/bench/slice-fix-detach-results-long.json`; the synthetic detached
screen is `.local/bench/slice-fix-detach-screen.py`. Validation: full Rust
suite, relevant CLI/runtime/compaction/Harbor Python suite, strict Clippy,
formatting, and diff checks. Cabal Linux amd64 also passed the focused
detached-child test (job `cabal/01M3BF841C18Y5MNFTD9D30G65`, source
`f6efb4c95415-dirty`, exit 0).

After merging `02e79eb` (keep-warm refreshes and sticky routing), the same
screen ran once more against that base (binary `78aa535b`) with the merged
tree (`36a44102`), two pairs: daemon CPU 0.361 / 0.334 s and 0.344 / 0.315 s,
peak RSS 18.72 / 18.89 and 18.80 / 18.81 MiB, turn p95 760.0 / 615.6 and
609.4 / 606.3 ms, baseline first in both. The baseline's first pair held a
noisy p95; nothing here is a speedup claim. A keep-warm refresh renders the
bot's fallback choice exactly as its call did, so the cache it refreshes is
the one the call wrote. Validation of the merged tree: 121 Rust tests, 264
Python tests with 22 opt-in skips, strict Clippy and formatting.


## Group commit

Observed 2026-09-26 on a Linux x86_64 cloud VM (4 vCPUs, ext4 on a virtio
disk), Rust 1.98.0, bundled SQLite. Baseline is main `2d03ac2` (binary
`181c783b…`); the candidate commits the storage worker's queued jobs in
groups (binary `2a88e7b3…`). Every store job used to be its own transaction
and its own sync, so the one worker allowed at most one job per sync. This
VM syncs in about 0.15 ms, so a delay was injected into every `fsync` and
`fdatasync` with an `LD_PRELOAD` shim (sleep, then the real call) to stand
in for slower storage. Treating 2 and 10 ms as the range of SD cards and
network block devices is an assumption, not a measurement of either.

The screen: 64 bots each resubmit the moment their turn finishes (open
loop, requests pipelined on stdio), a synthetic model holding each reply
200 ms, text turns, `--context-items 8`, 3 s warmup then 10 s measured.
Alternating pairs, baseline first:

| Injected sync | Baseline turns/s | Group turns/s | Baseline p50 / p95 ms | Group p50 / p95 ms | Jobs per commit |
| --- | ---: | ---: | ---: | ---: | ---: |
| none | 307.3, 307.6 | 304.3, 308.4 | 208 / 214, 207 / 213 | 209 / 221, 207 / 212 | 2.36, 2.25 |
| 2 ms | 95.1, 94.1 | 128.3, 127.2 | 667 / 860, 683 / 803 | 490 / 603, 504 / 577 | 3.28, 3.29 |
| 10 ms | 26.2, 25.9 | 37.8, 36.9 | 2,402 / 3,005, 2,432 / 2,808 | 1,663 / 1,884, 1,724 / 1,890 | 3.12, 3.09 |

The model's own ceiling is 320 turns/s (64 bots, 200 ms per reply). With no
injected delay neither binary is storage-bound. With a delay, the baseline
runs about five syncs per turn back to back, and grouping lifts throughput
by about 35 to 45 percent and cuts the median by about a third. It does not
reach the model's ceiling: about three jobs share a commit, because the
service loop awaits `begin` for each submission and `finish` for each
completion before handling the next request, so those commits never share a
sync with each other. That loop is now the limit on slow storage (NEXT item
44).

The 32-agent socket echo screen (`bench.lifecycle --agents 32 --mode echo
--tools echo,shell,read,write,edit --transport socket --repeat 3`), three
alternating pairs, medians of three measured runs each:

| Pair | CPU s, baseline / group | Peak RSS MiB | Turn p95 ms |
| --- | ---: | ---: | ---: |
| 1 | 1.10 / 1.12 | 21.81 / 22.10 | 595.4 / 582.9 |
| 2 | 1.10 / 1.12 | 21.95 / 22.25 | 587.7 / 581.1 |
| 3 | 1.10 / 1.15 | 22.04 / 22.17 | 584.7 / 584.0 |

p95 is level or slightly lower. RSS is up about 0.2 MiB in every pair. CPU
medians are higher in every pair with overlapping ranges (baseline 1.08 to
1.16, group 1.06 to 1.16), so the difference is unresolved, not shown to be
noise. SQLite's share should fall rather than rise: a separate probe with
sync off (Python's SQLite, three jobs of one insert and one update each) spent
14 to 15 µs of CPU per job as separate transactions and 10 to 12.5 µs as
savepoints in one group. The candidate's own additions per job are one
`try_recv` and holding the answer until the commit; this screen does not
attribute the difference.

Not established: real slow hardware, the daemon on macOS, tool turns, or
more than 64 bots.

**macOS flush.** The same change turns on `PRAGMA fullfsync` and
`checkpoint_fullfsync`. Without them the bundled SQLite syncs with a plain
`fsync`, which on macOS does not flush the drive cache, so `synchronous=FULL`
was not power-loss durable there. A microbenchmark on an M1 Max (internal
APFS SSD, AC power, rusqlite 0.40.2 with bundled SQLite 3.53.2, WAL, one job
= a 600-byte insert and a counter update, median of three rotated 3 s
rounds; observed 2026-09-26, source and table in the project's shared
files) gives the price:

| Strategy | Jobs/s |
| --- | ---: |
| Plain fsync, one job per commit (before) | 11,600 |
| F_FULLFSYNC, one job per commit | 184 |
| F_FULLFSYNC, savepoint groups of 8 / 32 | 1,280 / 5,090 |

A flush is about 5.4 ms, so an idle Mac pays that per commit: a shell turn of
about six commits takes about 30 ms longer. Under load the flush is shared by
the group. Linux ignores both pragmas; the Linux screens above are unchanged.
A store test checks that the writer carries both pragmas and the reader,
which can run the checkpoint when the last connection closes, carries
`checkpoint_fullfsync`. The per-operation `ran` times in `stats` no
longer include the sync; the new `commit` operation carries it.
Validation: 182 Rust tests, including two for grouped commits that fail
when grouping or the rollback answer is removed; 264
Python tests with 22 opt-in skips; strict Clippy and formatting.

## Completion scheduling and macOS flush attribution

Observed 2026-09-26 on macOS arm64. Baseline: `15d629c`, binary
`c34bb494…`. Candidate: task-owned durable completion, binary `3e4da31b…`.
Both use `synchronous=FULL`, `fullfsync=ON`, and `checkpoint_fullfsync=ON`.
The worker still answers and publishes only after commit. Each finished turn
now submits its own completion job before its task is reaped, so independent
finishes can share a group; previously the service loop awaited them one at a
time. Admission and bot creation still await their commits serially.

### Attribution

A separate diagnostic build changed only the two full-flush pragmas to OFF.
This is a weaker durability contract, not an optimization candidate or a runtime
mode. In a 32-bot, three-turn text probe (64 KiB new input, 5 KiB output, 500 ms
scripted streaming per turn), median commit-operation time was 1,702 ms in the
baseline versus 194 ms in the control. `begin` took 107 versus 94 ms;
context-window work took 6 versus 5 ms, with item reads 13 versus 12 ms. These
are medians of three measured runs after one warmup, with turn-phase wall times of 2.568 versus
1.811 seconds. This identifies disk flushing as the largest cost in this short
workload; it does not isolate every CPU increase since the older README build
or establish the cost of long-history compaction.

The streaming driver's `ready_seconds` includes creation of all benchmark bots.
It is not daemon startup alone. Direct protocol readiness in this probe was a
median 24 ms with full flushing and 15 ms in the weaker control. The earlier
26-to-189-ms historical comparison measured benchmark readiness, including 32
bot creations, not just process startup.

### Completion burst

`bench.completion_burst` reuses the existing synthetic provider and psutil.
It holds all 32 model replies, then releases them together. Timings start at
release and end at terminal-event receipt; CPU is the daemon's user plus system
time over that interval. Creation and admission are outside the interval;
response processing, append, completion, and publication are inside it. It
checks successful completion but does not validate conversation contents.

Two sequential pairs, with reversed order on the second pair; each cell is the
median of three measured fresh-store runs after one excluded warmup:

| Pair | Baseline release-to-terminal p99 | Candidate p99 | Baseline CPU | Candidate CPU | Baseline / candidate commit-operation time |
| --- | ---: | ---: | ---: | ---: | ---: |
| baseline then candidate | 216.2 ms | 15.0 ms | 25.1 ms | 6.5 ms | 180 / 10 ms |
| candidate then baseline | 238.1 ms | 18.0 ms | 27.4 ms | 6.7 ms | 209 / 13 ms |

Every run completed all 32 turns. Commit-operation counts fell from median
45–50 to 15–16; these counts include read-only transactions, so they are not
physical-flush counts. The result establishes a benefit when replies finish
together, not a steady-state throughput multiplier.

### Ordinary streaming

The same two-order comparison with the text workload above was less decisive:

| Pair | Baseline / candidate wall time | Baseline / candidate terminal tail | Baseline / candidate daemon CPU |
| --- | ---: | ---: | ---: |
| baseline then candidate | 2.899 / 2.611 s | 1,031 / 881 ms | 0.518 / 0.491 s |
| candidate then baseline | 2.609 / 2.659 s | 889 / 917 ms | 0.515 / 0.496 s |

Here the terminal tail is the maximum of 96 turns (the nearest-rank p99).
CPU includes startup and creation; wall time includes only the turn phase.
The provider validates exact conversation history and every run completed all
96 turns. This supports the mechanism and a small observed CPU reduction, but
not a blanket latency improvement. End-of-work RSS was 22.2 / 24.4 MiB in the
first pair and 24.8 / 24.6 MiB in the reversed pair; these are not peak-memory
measurements or proof of memory parity.

Next isolate serialized admission and creation, preserving capacity, same-bot
ordering, durable acknowledgements, and provider execution only after commit.
Measure long-history context construction separately before introducing caches.

### Mixed workload and verification

A sequential baseline/candidate pair used the existing 192-bot mixed soak for
two minutes each, seed 7, with the first cancellation deliberately targeting a
slow turn. The original random first attempt could select a turn that had
already finished and fail to exercise cancellation at all. Later attempts stay
randomized. The streaming-budget test also now counts the actual UTF-8 JSON,
not an ASCII-escaped re-encoding.

| Measurement | Baseline | Candidate |
| --- | ---: | ---: |
| Turns drained | 5,504 | 4,939 |
| Unexpected failures | 0 | 0 |
| Deliberate interruptions | 4 | 4 |
| Turn p50 / p95 | 289 / 3,069 ms | 274 / 1,827 ms |
| Sampled peak daemon RSS | 25.42 MiB | 27.16 MiB |
| Final database / WAL | 36.01 / 4.12 MiB | 33.40 / 4.12 MiB |
| Historical forks run and deleted | 1 | 2 |
| Replay mismatches / unfinished after drain | 0 / 0 | 0 / 0 |

Both recovered all 16 turns interrupted by a daemon kill. Neither reached the
compaction threshold. Work selection depends on completion timing and pacing,
so the different turn and fork counts make this an operational check, not an
equivalent-workload efficiency comparison. In particular, the candidate's lower
turn count does not establish throughput parity, and its smaller store does
not establish better storage efficiency. A longer compaction/storage soak is
still needed.

Verification: 186 Rust tests, strict Clippy, formatting, and diff checks passed.
The full Python run passed 246 tests, skipped 22, and failed the first-cancellation
race above; the corrected real-daemon soak test passed on a focused rerun.

### Fixed-work shell lifecycle

The mixed run was followed by two reversed-order pairs of the socket lifecycle
screen: 32 bots, three turns each, shell tools, all five tool schemas registered,
one warmup and three measured runs per build. Each run validates 96 shell-tool
results and workspace artifacts, exact replay, restart, idempotent submission,
and historical forks. No quality warnings occurred.

The observer now allows the configured shell workload's 65 processes (one
daemon, 32 shells, 32 sleep children). Its former 48-process guard stopped a
valid candidate warmup; that incomplete run and the preceding comparison were
excluded. Both measured builds used the same corrected observer and guard.

| Pair | Baseline / candidate turn p99 | Baseline / candidate observed tree CPU | Baseline / candidate peak daemon RSS | Baseline / candidate peak tree RSS |
| --- | ---: | ---: | ---: | ---: |
| baseline then candidate | 936.2 / 940.9 ms | 1.207 / 1.216 s | 18.78 / 18.86 MiB | 101.19 / 99.95 MiB |
| candidate then baseline | 1019.0 / 945.7 ms | 1.229 / 1.223 s | 18.75 / 18.83 MiB | 92.08 / 98.81 MiB |

Medians above include shell descendants in tree CPU and RSS; daemon RSS is
reported separately. These short results are approximately flat, not a general
speedup or proof of sustained-load parity. They support retaining the focused
completion improvement while admission and long-run costs remain open.

### Retention publication boundary

The review found a same-bot race in the completion candidate: with
`--retain-turns 1`, finishing A and cancelling queued B could share a commit.
B's retention then removed A's terminal event before publication read it. A
32-bot gated-provider probe reproduced missing completion events in two of ten
runs on `3e4da31b…`, and none in ten on the fix, `d196f434…`.

Completion, queued cancellation, explicit pruning, and deletion now identify
which bot's events they may remove. The worker closes and publishes a group
before taking a second such job for the same bot, preserving FIFO order.
Different bots still batch. The check scans at most the existing 32-job group;
it adds no SQL reads or event-payload copies. It adds a bot-name allocation to
these jobs and can require another commit when same-bot retention conflicts.
Durability and the retention policy are unchanged.

A deterministic storage-worker regression first failed with terminal events
`[3, 2]` instead of `[1, 3, 2]`. It now verifies all three events in order,
subsequent pruning of the first result, and a shared commit for independent
bots. All 187 Rust tests and 84 Python delivery/runtime/wait tests passed,
as did strict Clippy, formatting, and diff checks.

Matched 32-bot completion bursts on the same macOS arm64 host, with full
flushing in both builds, one warmup and three measured runs per cell:

| Order | Before / fixed p99 | Before / fixed daemon CPU |
| --- | ---: | ---: |
| before then fixed | 19.14 / 18.32 ms | 7.18 / 6.77 ms |
| fixed then before | 34.19 / 15.89 ms | 7.04 / 6.58 ms |

All runs completed 32 turns. These short samples show the cross-bot batching
benefit survives the fix, with no observed CPU or tail regression in this
probe. The varying baseline tails preclude attributing a further speedup to
the boundary check. This does not resolve the mixed-soak or long-history gaps
above, or measure the added commit cost when retention conflicts for one bot.

### Fixed-work mixed-load diagnosis

On 2026-09-26, a diagnostic held the mixed soak's logical work fixed to
investigate its lower candidate turn count. The saved baseline was `15d629c`
(binary SHA-256 `c34bb494356faa3a9f6b66299eef95df60270557f1b06591b69af609f5e57e3b`);
the candidate included the completion and retention-publication changes in
`a7746cd` (binary SHA-256
`d196f434ba7255990ffb478970b6d10c29dd04b7f1145060f36688da881e1e83`).
Both kept full flushing. These saved binaries isolate the completion change;
they do not measure the subsequent rebase onto the routing-token change.

The same macOS arm64 host ran baseline/candidate, then candidate/baseline,
sequentially after one smaller warmup per build. Each run used 192 bots with
the soak's seven roles, 512 KiB context and 12-turn operational retention.
Per-bot quotas were 32 long, 8 noisy, 24 background, 24 parent, 24 child,
16 flaky and 32 plain turns. Prompt choices were seeded per bot and turn,
and child delays cycled through 500, 1,500 and 3,000 ms. One in-flight turn
per bot was refilled immediately, rather than selecting work by elapsed time.

All four runs completed 4,608 primary turns plus six turns on three historical
forks. The normalized prompt digest, per-bot turn/model-round/retry totals,
7,046 provider requests and 256 retries matched exactly; there were no failures
or refusals. Fork, two follow-up turns and deletion ran after the primary work,
taking 0.17–0.34 seconds per fork across the four runs. This separates their
cost from sustained throughput, but does not test concurrent fork/deletion
contention. Cancellation, slow readers, crash recovery and compaction are also
outside this diagnostic. Its synthetic provider does not validate exact
conversation contents.

| Order | Baseline / candidate primary-work wall | Baseline / candidate daemon CPU | Baseline / candidate sampled peak daemon RSS |
| --- | ---: | ---: | ---: |
| baseline then candidate | 94.62 / 78.57 s | 21.50 / 19.52 s | 25.08 / 28.78 MiB |
| candidate then baseline | 104.96 / 83.88 s | 20.56 / 18.69 s | 26.64 / 26.50 MiB |

Wall and CPU exclude bot creation and the later fork phase. CPU covers only
the daemon, not shell descendants or the provider/observer. RSS was sampled
approximately every five seconds and is not an allocation high-water mark.
Across primary and fork turns, p99 was 3,196 / 3,115 ms in the first pair and
3,335 / 3,139 ms in the reversed pair; the 3-second child delay contributes
to these tails. Two samples per build support a workload-specific improvement,
not a general speedup or memory-parity claim.

Store-operation counters identify the remaining cost:

| Order | Baseline / candidate commit groups | Baseline / candidate commit execution | Baseline / candidate total store execution |
| --- | ---: | ---: | ---: |
| baseline then candidate | 16,121 / 12,213 | 77.14 / 60.44 s | 92.46 / 74.10 s |
| candidate then baseline | 15,985 / 12,049 | 82.59 / 63.99 s | 100.97 / 78.27 s |

Commit groups include read-only groups, so their count is not a count of disk
flushes. The candidate used 24–25% fewer groups, took 17–20% less wall time,
and used about 9% less daemon CPU. Commit execution remained about 82% of
measured candidate store execution. Candidate `begin` execution cost
5.22–5.76 seconds, versus 0.30–0.37 seconds for context preparation and
1.22–1.23 seconds for window selection. Those job timings exclude the shared
commit and queue wait. Admission/creation batching remains the next hypothesis
to test, preserving capacity reservation, same-bot ordering and durable
acknowledgements; these results do not justify a context cache first.

Re-reading the original timed soak also found unequal noisy-tool output:
97.44 MB on the baseline versus 108.10 MB on the candidate, including 69
versus 87 one-megabyte outputs. The candidate nevertheless ran more turns
in its first minute and slowed later. Neither fixed-work candidate reproduced
that abrupt slowdown. Work-mix differences invalidate the original raw turn
comparison, but do not establish the cause of its late slowdown. The original
capture lacks per-operation time samples; concurrent fork contention and host
flush variability remain unproven explanations. Keep an instrumented mixed
operational soak as a separate follow-up before claiming sustained parity.

Ignored evidence: `.local/throughput-diagnosis/` contains the diagnostic driver,
per-operation samples, exact-work checks and comparison summary.
Separately, the rebased `7120b48` passed all 187 Rust tests, a release build,
and three focused Python checks for routing-token persistence and compaction
budgets. These are post-rebase correctness checks, not performance measurements.

### Instrumented operational follow-up

On 2026-09-26, the same host ran the timed 192-bot soak for three minutes
per build, baseline first. The baseline was the saved `15d629c` binary above;
the current build was `7120b48`, SHA-256
`438c5734a02f86b42f260c8d2c278494d8768d6d1b280c590d42eebe30ccfbdc`.
An isolated observer recorded the existing per-operation store counters,
daemon CPU, command latencies and driver phases. Neither runtime was
instrumented or given a weaker durability policy. Unlike the fixed-work
diagnostic, this exercised concurrent historical forks/deletion, retention,
slow readers, compaction, cancellation, replay and crash recovery.

| Observation | Baseline | Current |
| --- | ---: | ---: |
| Turns drained | 8,380 | 11,314 |
| Unexpected failures / replay mismatches / unfinished turns | 0 / 0 / 0 | 0 / 0 / 0 |
| Deliberate interruptions | 6 | 6 |
| Compactions | 21 | 43 |
| Historical forks / deletions | 2 / 2 | 2 / 2 |
| Turn p50 / p95 | 298 / 3,062 ms | 176 / 3,040 ms |
| Sampled peak daemon RSS | 32.08 MiB | 29.03 MiB |
| Final database / WAL | 53.12 / 4.15 MiB | 62.34 / 4.16 MiB |
| Restarted interrupted turns | 16 | 16 |

Both recovered with 176 completed and 16 interrupted bot states. No follower
disconnect was observed; this does not establish whether buffered slow readers
had already been disconnected server-side. The larger current turn count and
smaller peak RSS are operational observations, not matched-work throughput or
memory claims. There was only one timed run per build, in one order, and work
selection still depends on completion timing. Three minutes does not establish
long-term memory or store-growth bounds.

Neither run reproduced the earlier abrupt slowdown. Baseline fork commands
took 6–9 ms and deletion 37–50 ms; current fork commands took 6–13 ms and
deletion 21–25 ms. These calls did not produce a multi-second stall in these
runs. Commits accounted for about 83% / 80% of cumulative store execution.
The driver spent 178.7 / 158.3 seconds waiting on submit replies: it submits
sequentially over one connection. Batching concurrent admissions therefore
needs its own measurement; this soak alone cannot validate that benefit.

That separate diagnostic opened 32 client connections, submitted simultaneously,
and gated provider completions until every admission was acknowledged. On
`7120b48`, a captured repeat series completed one warmup and eleven measured
runs. Median time to all admission replies was 284.8 ms, median daemon CPU
was 72.8 ms, and every run recorded 32 `begin` jobs and 34 commit groups in
the admission interval. Median commit execution was 239 ms. These counters
also include preparation jobs for the admitted turns; commit groups include
read-only groups. The steadily increasing reply latencies agree with the
service's serial dispatch path. This identifies a batching opportunity, not
the size of an implemented improvement.

**Unresolved diagnostic failure:** an earlier admission attempt lost a
connection after its warmup. Its database contains three terminal
`storage_error` outcomes and two running turns. That attempt discarded daemon
stderr, and that build mapped SQLite failures to `storage_error` without
retaining their underlying codes. Sixteen subsequent runs with stderr capture
completed successfully and emitted no daemon error. The failure is excluded
from timing summaries, but remains open as a correctness concern. After the
runs, the host reported only about 2.7 GiB available; no space measurement or
SQLite error code was captured at the failure, so disk pressure is a possible
factor, not an established cause.

Before changing admission concurrency, preserve safe SQLite error codes at
the failure boundary and resolve or reproduce this failure. Then measure a
bounded batch of queued admissions, with no batching delay for a lone caller,
per-request rollback, capacity reserved only for fresh running turns, and
same-bot ordering. Acknowledgements and provider execution must remain after
commit. Keep the original unexplained soak slowdown distinct from this new
storage failure.

Ignored evidence is under `.local/throughput-diagnosis/`: `operational-before`,
`operational-current`, their command/phase traces and summary, and the
`admission-current`, `admission-captured` and `admission-repeat` probe series.

### SQLite failure diagnostics

On 2026-09-26, the diagnostic change on `7120b48` retained numeric SQLite
primary and extended codes in `storage_error` details. It excludes engine
messages, SQL, paths and parameter names. Begin/commit failures preserve their
codes for every affected caller; a statement that rolls back its whole group
supplies the original code instead of the subsequent missing-transaction error.
Successful jobs retain the same grouping and publication boundaries, with no
new queries or diagnostic allocations on success. Admission batching is unchanged.

Regression tests exercised deferred foreign-key failure at commit, automatic
transaction rollback, group recovery and removal of sensitive diagnostics.
The accounting fixture verified identical error details in live and replayed
terminal events. All 189 Rust tests passed; the final diagnostic cases, strict
all-targets Clippy and the focused Python fixture also passed.

The 32-client gated admission probe now captures free space at startup,
creation, admission and completion, plus failure state and daemon stderr.
All 97 fresh-daemon runs passed: 16 baseline runs and 81 diagnostic-build runs,
3,104 completed turns total. No storage error or daemon stderr was observed.
Available space ranged from about 2.2 to 1.9 GiB during these probes; there is
still no space measurement from the original failure. It remains unresolved,
not fixed by adding diagnostics.

Two reversed-order admission pairs used eight runs per build per pair, dropping
each cell's first run as warmup. Median admission wall time was 250.5 to 290.2 ms
in pair A and 273.1 to 277.2 ms in pair B. Median daemon CPU was 65.4 to 72.8 ms
and 70.1 to 71.7 ms respectively. These short results do not establish parity.
The additional 65 candidate runs, including one warmup, had medians of 279.7 ms
wall and 71.2 ms daemon CPU.

A larger fixed-work check then ran baseline/candidate/candidate/baseline,
sequentially with no concurrent builds or tests. Each run performed 1,152
primary turns across 192 bots plus six historical-fork turns, 1,766 provider
requests and 64 expected retries. Every run completed without errors and had
identical per-bot work counts and logical digest
`2005388ee4e14cfe90e303dac0f4483964db84ed043a6e79c350a5075317ab20`.
Wall and daemon CPU below cover primary work, excluding creation and the later
fork phase. RSS is the peak of periodic samples.

| Fixed-work metric | Baseline A | Diagnostic A | Baseline B | Diagnostic B |
| --- | ---: | ---: | ---: | ---: |
| Elapsed | 19.85 s | 19.93 s | 19.27 s | 19.16 s |
| Daemon CPU | 4.034 s | 4.091 s | 4.210 s | 4.167 s |
| Sampled peak RSS | 25.88 MiB | 24.70 MiB | 25.38 MiB | 24.53 MiB |
| Commit execution | 14.74 s | 15.61 s | 13.96 s | 13.87 s |

Elapsed differences were +0.4% and -0.6%; CPU differences were +1.4% and -1.0%.
This screen found no material regression; two pairs do not prove a speedup or
sustained parity. It exercised no compaction, cancellation, crash recovery or
slow-reader pressure. The original operational slowdown and intermittent
storage failure remain separate open investigations.

The baseline binary SHA-256 is
`438c5734a02f86b42f260c8d2c278494d8768d6d1b280c590d42eebe30ccfbdc`;
the diagnostic binary is
`05d13ff8b530e4325e2810dbed19f0bb7b642581f5a50342b67cf0dc2b74f8ad`.
Ignored evidence under `.local/sqlite-diagnostics/` includes both binaries,
test logs, the admission driver and captures, and all four `mixed-*` results.

### Disk-full cause and containment

On 2026-09-26 a read-only inspection of a copy of the failed attempt's store,
the probe's own records and the host's system log on the same macOS arm64
host found the likely cause: the volume ran out of space. Nothing was
changed on that host. The facts, with times relative to the attempt's first
run directory:

- The volume had been in macOS's very-low-disk state for about ten minutes,
  with roughly 140–250 MB free. At +2.165 s the system log records another
  process's SQLite WAL write failing with `ENOSPC`, the only one that hour,
  22 ms after the commit that recorded the three `storage_error` completions.
  Free space was back near 3 GB by +11.7 s; what freed it was not identified.
  The captured and repeat series ran later, with about 2.9 GB free.
- The store copy passed `integrity_check`. Its WAL holds 39 valid commits,
  six uncommitted frames of the next group (the fifth turn's completion), and
  the 24-byte header of a seventh frame whose data was never written. That
  cut matches SQLite's frame append failing on the data write as the WAL grew.
  A process kill would have to land in the same microsecond window to leave it.
- Commit 38 admitted the fifth turn and recorded the other three turns'
  failures. Commit 39 changed nothing logically but rewrote 16 pages that
  admissions touch with unchanged contents, consistent with admission jobs
  rolling back to their savepoints inside a group that still committed. That
  is an inference from page contents.
- Only one measured run's store was affected; 27 of its 32 submissions never
  committed. The probe saw `runtime exited before expected response`. On that
  build a completion that failed to commit returned an error through the
  service loop, and the daemon exited, closing every connection. Stderr went
  to `/dev/null` and the exit status was not recorded.

Two code paths turn a full disk into that pattern. Both were reproduced on a
Linux container, on this branch before the changes below (`8724f22`), with an
`LD_PRELOAD` shim that makes writes fail with `ENOSPC` while a flag file
exists. One mode refuses only writes that would grow a regular file, like a
full volume; the other refuses every regular-file write. Each run used 32
socket clients submitting at once to a gated synthetic provider.

- Group commit runs each job in a savepoint. SQLite keeps the pages a
  savepoint changes so it can roll back alone, and past 64 KiB it spills them
  to a temporary file. Every admission that starts a turn crossed that line.
  Traced opens counted one temporary file per such admission, 8 of 8, and none
  on the build before group commit, `2d03ac2`. With growth refused, all 32
  admissions failed with `storage_error: sqlite_primary=13 sqlite_extended=13`
  (`SQLITE_FULL`), and every refused write was to those temporary files.
- With every write refused for 0.5 s as the held replies were released, the
  daemon exited with status 1 and `agent: storage_error: sqlite_primary=13
  sqlite_extended=13`. All 32 turns were left `running`, and the next start
  ended them `interrupted` with `process_interrupted`, discarding replies the
  provider had already sent. The store passed `integrity_check`.

Changes:

- The writer sets `temp_store=MEMORY`. A job's savepoint journal stays in
  memory and is freed when the job ends, bounded by the pages one job
  changes. Refused growth now fails admissions only at the COMMIT's WAL
  append, still as `SQLITE_FULL`. No temporary files were opened.
- A completion that fails to commit is submitted again, with backoff from
  10 ms to one second, while its bot stays durably busy. Only shutdown stops
  the retries; the turn is then left for the next start, and the daemon exits
  with the error after draining the other completions.
- `stats` counts jobs answered `storage_error` per operation and in total,
  so a refusal is visible without the daemon's stderr.

On the changed build, the same 0.5 s refusal left the daemon running. All 32
turns ended `failed` with `storage_error` and `sqlite_primary=13`, delivered
as terminal events. Each completion was refused six times in that half
second, counted as 192 `finish` storage errors in `stats`, and committed on
the first retry after writes were accepted; three runs gave the same counts. Their replies were still lost: the jobs that append a reply are
not retried, so a turn whose reply cannot be stored fails. Integrity checks
passed.

Sequential admission cost, 256 bots each receiving one held turn, eight
alternating runs per build, native container sync:

| Median per admission | `8724f22` | `temp_store=MEMORY` |
| --- | ---: | ---: |
| `begin` job execution | 451 µs | 164 µs |
| Submit round trip | 1.45 ms | 1.26 ms |
| Daemon CPU | 1.09 ms | 0.98 ms |
| Daemon RSS after admission | 27.2 MiB | 27.2 MiB |

These come from one Linux container. Creating a temporary file costs more on
some filesystems, so macOS needs its own measurement. The per-group counter
change measured flat in a 320,000-job no-op microbenchmark: median
2.66 versus 2.57 µs per job, with overlapping ranges.

A review asked whether an in-memory journal lets one retention job hold a
turn's artifacts in memory. It does not hold them: a freed large value's
overflow pages are not journaled. One completion pruning 16, 64 and 160
turns of one near-1 MiB artifact each (14, 57 and 142 MiB stored) raised the
daemon's peak RSS by 3.0–3.3, 3.3–3.7 and 3.3–4.0 MiB, with or without the
journal in memory (one or two runs each, same container). What the journal holds is every table and
index page the job rewrites: pruning 5,000 extra 300-byte event rows raised
the peak by 4.8 MiB in memory against 3.0 MiB spilling to a file, one run
each. Since
completion retention used to prune a whole backlog in one job, it now removes
one piece of at most four turns per completion, like explicit pruning. A
build that raised SQLite's spill threshold to 1 MiB instead answered that
5,000-row prune with `SQLITE_FULL` on a disk with 26 GB free, cause not
found, so it was dropped.

Still unproven: the SQLite codes of the original failure, since that build
kept none; which operation failed first for the three failed turns; and
whether the host's disk was at zero at exactly those instants. The
reproduction injects the error. A run on a nearly full disk image on macOS
would settle the platform question.

### Admission window

Observed 2026-09-26 on a Linux x86_64 container (4 vCPUs), Rust 1.98.0,
bundled SQLite. Baseline is `4260673`, this branch before the window (binary
`a7e5561e…`); the candidate queues up to 32 admissions at once (binary
`738cba82…`). The window-size builds are the candidate with only the
constant changed. Slow storage uses the same `fsync` delay shim as
[group commit](#group-commit).

Before, the service awaited each `create` and `submit` commit before it read
the next request, so admissions never shared a sync with each other; the
[concurrent probe](#instrumented-operational-follow-up) saw replies arrive one
commit apart. `bench.admission_burst` with 32 bots, builds alternating,
medians of ten measured runs at native sync and eight at 2 ms:

| Per phase | Serial, native | Window, native | Serial, 2 ms | Window, 2 ms |
| --- | ---: | ---: | ---: | ---: |
| 32 submissions at once, last reply | 37.3 ms | 5.2 ms | 113.8 ms | 7.5 ms |
| Median reply | 13.0 ms | 4.8 ms | 49.1 ms | 7.2 ms |
| Daemon CPU | 27.6 ms | 5.2 ms | 32.3 ms | 6.0 ms |
| Write groups | 37.5 | 4 | 35 | 4 |
| 32 creations at once, last reply | 15.8 ms | 4.1 ms | 90.2 ms | 6.5 ms |
| Daemon CPU | 10.4 ms | 3.7 ms | 15.2 ms | 4.0 ms |
| One submission alone, median / p99 | 1.25 / 2.34 ms | 1.26 / 2.75 ms | 5.69 / 11.37 ms | 5.72 / 11.23 ms |

The burst rows' ranges do not overlap between builds. A submission that
arrives alone is not delayed: its median and daemon CPU match, and its p99,
the slowest of 32 samples per run, varies within the same range in both.
Groups count every write job of the phase: 128 for 32 submissions, each
admission and its turn's three start-up jobs. They were recounted, six runs
per build at native sync and three at 2 ms, after the bench stopped counting
its own closing `stats` request, which is a storage job too. That fix changes
no other row.

Window sizes, same bench, eight runs at native sync and six at 2 ms, with
groups recounted over three runs at native sync as above:

| Window | Native: last reply / median | Groups | 2 ms: last reply / median | Daemon CPU at 2 ms |
| --- | ---: | ---: | ---: | ---: |
| Serial | 41.2 / 13.9 ms | 37 | 114.4 / 48.6 ms | 30.8 ms |
| 1 | 38.1 / 13.1 ms | 37 | 112.3 / 49.2 ms | 32.9 ms |
| 4 | 12.3 / 5.2 ms | 12 | 30.7 / 15.0 ms | 16.4 ms |
| 8 | 8.5 / 4.2 ms | 9 | 18.7 / 8.6 ms | 12.2 ms |
| 16 | 5.9 / 3.1 ms | 5 | 12.2 / 5.3 ms | 7.1 ms |
| 32 | 5.2 / 4.9 ms | 4 | 7.3 / 7.0 ms | 5.9 ms |

A window of 16 answers half the burst after the first commit, so its median
reply is the lowest; 32 answers the whole burst soonest with the least CPU,
and its lead grows as syncs slow down. It also equals the storage worker's
group limit and queue, so one full window fills one group. The default is 32.

Sustained load, the group-commit screen above (64 bots resubmitting as each
turn finishes, 200 ms model replies, 10 s measured), alternating pairs:

| Injected sync | Serial turns/s | Window turns/s | Serial p50 / p95 ms | Window p50 / p95 ms | Jobs per commit |
| --- | ---: | ---: | ---: | ---: | ---: |
| none | 305.6, 306.9 | 304.5, 306.0 | 208 / 219, 208 / 214 | 210 / 219, 209 / 216 | 2.8 → 3.9 |
| 2 ms | 182.1, 185.1 | 266.0, 266.4 | 352 / 390, 342 / 386 | 237 / 267, 237 / 268 | 5.4 → 8.0 |
| 10 ms | 56.9, 55.7 | 132.9, 129.7 | 1,113 / 1,252, 1,126 / 1,401 | 465 / 665, 493 / 674 | 5.2 → 12.4 |

The serial baseline here runs 56 turns/s at 10 ms, not group commit's 37,
because completions have since moved to the turn tasks. With no injected
delay both stay at the model's ceiling of 320.

Memory: idle RSS was 17.8 MiB for both, and after 32 one-at-a-time
submissions 19.2 and 19.3 MiB with overlapping ranges. After the burst
phases RSS was 20.5 MiB serial and 20.8 MiB windowed, ranges not
overlapping. Up to 32 admissions and their replies now live at once; what
holds the extra 0.3 MiB was not isolated.

A creation's reply repeats its instructions and compaction instructions,
64 KiB each at most. In a unit test without the check below, 32 such
replies sent back to back overflowed a session's 2 MiB output queue
(`output_lagged`), which closes a socket session after its bots were
created. An admission now waits when its session's queue could not take
its reply and event with those already promised to that session. With both
texts at 64 KiB, 11 share a window. On the burst above, where replies are small, the check changed
nothing measurable: five runs each, last reply 5.11 ms before and 5.14 ms
after for submissions, 4.41 and 4.29 ms for creations.

Ordering tests cover a shared commit answered in request order, a retry and
busy work queued behind the admission they depend on, the active limit with
a promised slot that goes unused, a lost group commit that starts nothing
and frees its slots, an interrupt behind the admission it names, large
creations that must fit their session's output queue, and four clients
sending the same submission at once. Shutdown with queued
admissions is covered by reading the code, not by a test. All of this is one
Linux container with an injected sync delay; macOS, where a flush costs
about 5.4 ms, is not measured. The burst uses one connection; many clients
arrive interleaved, which the Python test exercises but no timing does.
