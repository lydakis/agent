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
