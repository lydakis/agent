# Storage growth and lossless payload storage

Observed 2026-09-22 on macOS arm64, AC power. The initial component experiment
below left the runtime unchanged. The follow-up implements targeted artifact
compression and shared large prompts in schema 26. Original history stays intact;
transcript nodes and provider request construction retain their native bytes.

## Where the bytes go

The original hour-long soak reported 1,316 MiB after 224,063 turns; that store
was deleted, so its detailed attribution cannot be independently repeated.
The surviving corrected three-minute soak is 180.46 MiB. Its stopped-store
profile finds only 20 KiB on the freelist and about 95% of allocated pages
occupied by payload. VACUUM or removing indexes cannot explain away that size.

| Object | Allocated MiB |
| --- | ---: |
| artifacts | 115.85 |
| nodes | 48.65 |
| turns | 10.21 |
| events | 1.82 |
| compactions | 0.75 |

The raw node payload is 44.21 MiB, including 30.00 MiB of tool results.
All 5.76 MiB of `turns.prompt` text also appears exactly in user nodes. That
is a candidate for removing a redundant copy, not permission to discard the
original prompt. These payload amounts exclude row and index overhead.

A separate deterministic probe uses the unchanged daemon with sixteen bots:
eight emit a 1 MiB varied diagnostic fixture per turn, eight receive roughly
4 KiB text prompts. Retention keeps four turns per bot. Every turn completes;
the daemon stops at each boundary for attribution and resumes on the same
store. This is a growth probe, not an uninterrupted latency benchmark.

| Completed turns | Database MiB | Node payload MiB | Prompt copy MiB | Artifact payload MiB |
| ---: | ---: | ---: | ---: | ---: |
| 64 | 34.73 | 2.22 | 0.13 | 32.00 |
| 256 | 49.40 | 8.89 | 0.54 | 32.00 |
| 1,024 | 79.23 | 35.57 | 2.14 | 32.00 |

Artifacts plateau while transcript bytes grow approximately in proportion to
turn count. Large tool results leave a bounded preview in nodes and full bytes
in retained artifacts. Pruning artifacts does not remove that transcript.
Context compaction and elision change requests; they do not shrink stored
original history. Lossless compression can reduce the slope, not make an
ever-growing durable conversation occupy fixed space.

## Native component experiment

`bench/storage_codec.rs` compares raw SQLite BLOBs with independently compressed
16 KiB blocks using miniz_oxide 0.8.9, zlib level 1. A 4 KiB prefix sample skips
compression when savings are below 12.5%. Each block can remain raw; the whole
object stays inline unless savings including estimated block overhead exceed
12.5%. An incompressible prefix can miss later compressible content. This loses
an optimization opportunity, never bytes. There is no dictionary training.

This is an isolated payload schema, not the runtime's node/turn/index schema.
Both variants use the same bytes, WAL/NORMAL, and transactions of 32 objects.
Writes include corpus reads, encoding, and inserts. Final checkpoint wall time
is reported separately; write CPU excludes that final checkpoint. WAL size is
sampled at the end, not at its peak. CPU is process user plus system time.

Each corpus gets one excluded warmup pair and five measured pairs, alternating
which variant goes first. After reopening, each trial measures 4,096 identical
pseudorandom ranges: seven eighths request 4 KiB and one eighth request 64 KiB.
Selection is uniform over objects, not weighted by bytes or real access rates.
The OS cache is uncontrolled. These are warm-cache component observations,
not cold-disk claims or daemon turn latency. Every original byte is compared
after reopening, outside the timed windows. Decoding is capped at 16 KiB per
block; returned pages are capped at 64 KiB.

Each synthetic corpus contains 128 objects cycling through 256 B, 4 KiB,
64 KiB, and 1 MiB. Repeated bytes are a favorable bound; varied generated
code/diagnostics test a less repetitive shape; seeded random bytes test the
incompressible path. None establish compression ratios for real coding traces.
The soak corpus exports all node, prompt, and artifact payloads from the
surviving three-minute synthetic run: 47,527 objects, 173,590,681 raw bytes.

Medians of five trials, raw → compressed:

| Corpus | Database MiB | Write CPU ms | Read CPU ms | Read p50 µs | Read p95 µs |
| --- | ---: | ---: | ---: | ---: | ---: |
| Repeated | 34.22 → 0.29 | 126.8 → 36.5 | 213.1 → 56.8 | 7.58 → 12.00 | 196.00 → 30.92 |
| Varied | 34.22 → 9.73 | 121.7 → 128.8 | 205.6 → 127.6 | 7.71 → 27.75 | 193.00 → 71.17 |
| Random bytes | 34.22 → 34.22 | 120.9 → 124.1 | 204.3 → 205.4 | 7.04 → 7.17 | 192.12 → 193.08 |
| Synthetic soak | 173.25 → 25.26 | 666.5 → 337.4 | 16.9 → 16.6 | 3.25 → 3.25 | 6.00 → 6.88 |

Checkpoint medians range from 0.20–4.01 ms for compressed and 0.24–0.83 ms
for raw. The soak's compressed checkpoint is 4.01 ms versus 0.83 ms raw.
Each read CPU number covers all 4,096 reads. The soak's many small objects
dominate that unweighted selection; its stable median does not establish cheap
reads of its largest artifacts. Its database figures describe exported payloads,
not a measured reduction of the original 180.46 MiB daemon database.

An initial 64 KiB-block prototype without sampling compressed the varied corpus
to 4.75 MiB, but read CPU rose from 214.4 to 261.0 ms and p50 from 8.71 to
39.67 µs. Random-byte write CPU roughly doubled (123.2 to 258.9 ms). Smaller
blocks and sampling substantially improve those costs, at the expense of disk
savings. Varied-data write CPU remains about 5.8% higher and median reads are
still slower. These results do not meet a blanket performance-parity claim.

## Initial decision

The initial screen ruled out compressing all runtime nodes:
history/context readers currently use SQL JSON extraction and bounded `substr`,
so changing their representation has costs beyond this component screen.

That led to the targeted LZ4 and prompt-sharing follow-up below. Cold transcript
compression remains outside this slice. Another unchanged hour-long soak would
not resolve these representation and partial-read tradeoffs.

## Targeted runtime follow-up

The same component screen with [lz4_flex 0.14.0 block encoding](https://docs.rs/lz4_flex/0.14.0/lz4_flex/block/index.html)
reduced varied-corpus write CPU from 120.7 to 70.6 ms and read CPU from 195.6
to 56.4 ms, but p50 still rose from 7.17 to 15.08 µs. That confirms the need
to keep small/hot transcript items raw. Selecting only the old soak's large
artifacts (206 objects, 121,200,000 bytes) reduced write CPU from 400.7 to
39.5 ms and p50 reads from 47.96 to 4.75 µs. These highly repetitive synthetic
artifacts are a favorable case; they do not predict real coding-task savings.

The runtime stores new compressible artifacts above 64 KiB as one BLOB with
an offset directory and independent 16 KiB LZ4 blocks. Unlike the component
prototype's per-block rows, this keeps retention/deletion topology unchanged.
A 4 KiB sample skips incompressible payloads, and encoding must save at least
12.5% after directory/block overhead. Individual blocks can stay raw. Decoding
has a fixed block bound; byte pages fetch just their directory and intersecting
encoded range. Both foreground and background command completions use it.

Started prompts of at least 4 KiB share the immutable user node via an indexed
foreign key. Pending work keeps its text until start, and absorbed steers share
their user node too. Tiny prompts remain inline to avoid paying index overhead
for little space saved. Fresh idempotency lookups keep a simple turn-row query;
only an actual retry of a shared prompt resolves its node. Node bytes, context
budgets, cache prefixes, and historical fork semantics are unchanged.

Migration shares exact indexed prompt matches once. Older steers without that
mapping keep their inline text. Existing artifacts stay raw, avoiding a bulk
startup rewrite. Freed pages are reusable, but an existing database file is
not vacuumed smaller. The original transcript still grows with work: artifact
compression reduces a retained working set, and prompt sharing reduces one
source of per-turn growth. Neither claims to bound lifetime transcript storage.

### Matched daemon results

The final candidate restores the simple submission path for fresh requests and
small prompts. Artifact statements retain their short lifetimes instead of
introducing a statement-cache change with compression. Earlier candidates
showed worse incompressible-workload latency; their captures remain available,
and their better isolated numbers are not substituted for the final results.

The mixed screen alternates one excluded warmup pair and three measured pairs
per shape. Each trial runs 256 turns, eight 1 MiB shell-output bots alongside
eight text bots, with four-turn retention and SQLite FULL durability. The
varied fixture contains generated diagnostics; the incompressible fixture is
seeded random bytes encoded as ASCII. Daemon CPU excludes shell, provider, and
observer CPU. RSS is sampled every 10 ms. Each trial verifies full decoded
artifacts, exact per-bot transcript hashes, partial pages, and inherited reads
after restart/fork. See the commands and full boundaries in BENCHMARKS.md.

Medians of three trials, baseline → final candidate:

| Mixed workload | Database MiB | Daemon CPU s | Peak RSS MiB | Text p50 / p95 ms | Shell p95 ms | Artifact page p50 / p95 ms |
| --- | ---: | ---: | ---: | --- | ---: | --- |
| Varied output | 48.91 → 17.61 | 1.173 → 0.932 | 47.77 → 46.50 | 26.23 / 51.85 → 16.66 / 31.17 | 73.02 → 46.81 | 0.256 / 0.508 → 0.129 / 0.220 |
| Incompressible ASCII | 48.49 → 48.50 | 1.184 → 1.174 | 52.34 → 51.70 | 23.39 / 87.97 → 14.73 / 52.49 | 115.12 → 67.95 | 0.242 / 0.445 → 0.263 / 0.401 |

The varied fixture's retained artifact bytes fall from 32.00 to 6.98 MiB;
incompressible data stays raw at 32.00 MiB. The latter's CPU difference is
small and its latency varied substantially between local runs. Do not read
its tail improvement as a promised speedup from trying compression. Its page
median was about 21 µs slower, including the protocol round trip.

A separate exact-history lifecycle screen alternates one warmup and five
measured pairs with eight bots and sixteen text turns per bot. Provider request
and response byte counts match; the fixture verifies the entire native input,
then checks restart, replay, duplicate submission, and historical forks.

| Prompt fixture | Daemon CPU s | Turn p50 ms | Turn p95 ms | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| 256 B | 0.196 → 0.192 | 3.17 → 3.37 | 5.814 → 5.820 | 13.64 → 13.77 |
| 64 KiB | 0.397 → 0.375 | 6.42 → 5.78 | 11.162 → 11.189 | 18.125 → 18.031 |

Large-prompt lifecycle storage including WAL/sidecars falls from 20.58 to
12.51 MiB; small-prompt storage is essentially unchanged. Tail latency is
essentially flat in this screen, but the small-prompt median rose about
0.20 ms and peak RSS by 0.13 MiB. These bounded local measurements support the
targeted change; they are not a claim that every metric improves or a sustained
capacity result. Normal provider inputs remain byte-identical by construction
and match the fixture's retained-history checks.

Repeating the fixed-turn growth probe with the final candidate:

| Completed turns | Baseline database MiB | Candidate database MiB | Exact node payload bytes, both |
| ---: | ---: | ---: | ---: |
| 64 | 34.73 | 9.54 | 2,331,174 |
| 256 | 49.40 | 17.94 | 9,325,083 |
| 1,024 | 79.23 | 45.68 | 37,301,060 |

At 1,024 turns, inline prompt copies fall from 2,246,570 to 13,260 bytes;
the remaining copies are small shell prompts. Artifact logical bytes stay
32 MiB; stored artifact bytes are 6.98 MiB. Total node bytes and all original
data are preserved. The original transcript remains the long-term growth
source; cold-history storage and a corrected hour-long soak remain follow-ups.

The per-slice regression screen, 32 agents on the socket transport in echo
mode with all built-in tool schemas, two alternating pairs of three measured
runs each, `69bd0dd` (binary `d333e1d4`) against the final tree (`218a3729`):
peak RSS 18.39 and 18.34 MiB baseline against 18.05 and 17.94 candidate,
daemon CPU 0.351 and 0.343 s against 0.337 and 0.339 s, turn p95 593.0 and
588.4 ms against 593.7 and 588.9 ms. Medians are within each other's ranges;
this shows no measurable cost on the common path, not a speedup.

Validation: 118 Rust tests, 66 focused Python tests, strict runtime Clippy,
formatting, and the SQL plan audit pass. New regressions exercise Unicode/NUL
prompts, idempotency, queue/steer/restart, deletion with a surviving fork,
old-store migration, foreground/background artifacts, cross-block UTF-8 pages,
pruning, malformed directories, and bounded decompression.

Final captures: `.local/storage-next/mixed-refined/result.json`,
`lifecycle-refined/result.json`, and `growth-refined/result.json`. The final
candidate binary SHA-256 is
`f803434d10fdc34a8638e7a922cf3c2e38cbbef00782837634d2421211c26f8f`;
the baseline is the unchanged daemon hash below. LZ4 component results are in
the same directory's `varied-lz4`, `entropy-lz4`, and `artifacts-lz4` directories.
No paid provider calls or commits were made for this slice.

## Reproduction and provenance

Commands are in [BENCHMARKS.md](BENCHMARKS.md#storage-attribution-and-codec-screen).
The standalone crate pins dependencies in `bench/storage_codec/Cargo.lock`;
it uses rusqlite 0.40.2 / SQLite 3.53.2 and rustc 1.98.0. The Python profiler
used SQLite 3.47.1. No paid provider calls occur.

Ignored captures live under `.local/storage-study/`: `soak-profile.json`,
`growth/result.json`, corpus manifests, and each `*-refined/result.json`.
Initial measurements are in `*-results/result.json`. The daemon binary hash is
`3692da8db4d6523f9c026c60065abc6a452cf657c619ba6c8dbd4910fd69daf4`.
The measured refined prototype binary hash is
`1e6f942906178478aee3f4f66f9ee4c4e932a809bb228fbdfe900ce7a3fa1799`.
Subsequent CLI help and error-wording changes do not change the timed paths.
Corpus manifests hash the framed kind and exact payload bytes:

| Corpus | SHA-256 |
| --- | --- |
| Repeated | `620f88511b8679bbe1723a2ca8fc0787e1bfe604cd02b7e7d2b070bef6ce9636` |
| Varied | `1c936c3d8f11d97577a4fb9f1ca44057192b09c52bc4c58377e08e9ed53355de` |
| Random bytes | `741071863e4e8760a3c1f75cb52818e49deb9a81ac25930e1daecce82fca02bd` |
| Synthetic soak | `ed5c44ef62f204a07d445ba02274c9b7c8d99896ffbdd75c9091dab91293a124` |
