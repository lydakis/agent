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
