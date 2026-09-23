# Performance measurement tools

Status, 2026-09-07: the measurement kit runs Pi's real agent core and Responses
transport, and one shared native Codex app-server, against a local synthetic
Responses SSE endpoint. The original binary transport client remains a calibration
target. The new Rust core runs the same workload through its shared provider
transport and history implementation. These experiments measure
ephemeral text conversations, not durable agent capacity or model quality.

## Reuse decision

Use [psutil](https://pypi.org/project/psutil/7.2.2/) for process counters, and
existing profilers for CPU stacks and allocations. The local code provides the
synthetic workload and correlates its events, provider counters, and resource
samples. See [reuse assessment](REUSE.md) before expanding this tooling.

Python is the external test driver, not a choice for the Agent execution core.
The Python measurement dependency is pinned in `bench/requirements.txt`. Pi's
adapter dependencies are pinned in `bench/adapters/package.json` and its pnpm
lockfile. These are benchmark dependencies, not a selection of the product runtime.
No profiler is installed globally and no model credentials are needed.

## Run and compare

Run from the repository root on macOS or Linux with Python 3.11 or later:

```sh
uv --cache-dir .local/uv-cache venv .local/venv --python python3
uv --cache-dir .local/uv-cache pip install --python .local/venv/bin/python --only-binary :all: -r bench/requirements.txt
.local/venv/bin/python -m unittest discover -s tests -v
.local/venv/bin/python -m bench run --out .local/bench/baseline
.local/venv/bin/python -m bench run --out .local/bench/candidate
.local/venv/bin/python -m bench compare .local/bench/baseline/result.json .local/bench/candidate/result.json
```

Output directories must be new and under ignored `.local/`. Each contains the
workload, per-run sampled counters, provider counters, and a versioned result.
Raw target stdout/stderr and command arguments are not retained. Target stdout
is reserved for the benchmark event protocol; malformed output fails the run.
The fixture uses only synthetic history and response data over loopback HTTP.
Custom target commands run with the caller's environment and existing permissions;
the driver neither inspects nor records credential values.

The default is one excluded warmup and three measured runs. Each run starts a
fresh target and a separate fixture provider. Existing output is never overwritten.
Failed runs return nonzero and are retained as failures, not compared as wins.
Comparisons require matching workload, host/platform, observer code and version,
sampling settings, limits, warmup count, and observed external-power state.
Target labels and revisions may differ. Feature profiles and engine must match
for regression percentages. Use `--exploratory` for unmatched observations; it
lists gaps and omits rankings. Missing historical profiles fail closed by default.
Configured provider concurrency must be achieved even in exploratory mode.
See [COMPARISON_CONTRACT.md](COMPARISON_CONTRACT.md). CPU load and temperature are uncontrolled:
medians and ranges describe these runs, not statistical confidence or significance.

```sh
.local/venv/bin/python -m bench run --out .local/bench/adapter \
  --label example-adapter --revision immutable-target-revision \
  --workload bench/workloads/smoke.json --repeat 5 \
  --timeout 30 --rss-limit-mib 1024 --process-limit 64 \
  -- executable-and-adapter-arguments
```

The custom command must implement the binary fixture contract below.
For standalone startup timing use Hyperfine instead. Profiling should be a
separate run, since profilers change execution cost and latency.

## Real-engine adapters

Install Pi's pinned packages with Node.js 22.19 or later and pnpm. The Codex adapter
uses an installed native executable; the validated version is `codex-cli 0.153.1`.
For npm installations the runner resolves the native executable inside that
package, bypassing the launcher. Ambiguous layouts fail explicitly.

```sh
pnpm --dir bench/adapters install --frozen-lockfile --ignore-scripts
CARGO_HOME=.local/cargo cargo build --release --locked
AGENT_BENCH_TEST_ENGINES=1 AGENT_TEST_RUNTIME=1 .local/venv/bin/python -m unittest discover -s tests -v
.local/venv/bin/python -m bench run --engine pi --out .local/bench/pi
.local/venv/bin/python -m bench run --engine codex --out .local/bench/codex
.local/venv/bin/python -m bench run --engine rust --out .local/bench/rust
.local/venv/bin/python -m bench compare --exploratory .local/bench/pi/result.json .local/bench/codex/result.json
.local/venv/bin/python -m bench.matrix --out .local/bench/engine-matrix
```

`bench.matrix` defaults to Pi, Codex, and Rust (72 runs, 54 measured). Use
`--engines pi rust` to select a pair. Rust-only runs need no Node installation.
It is a fixed screening experiment: 1, 8, and 32 agents; 4 KiB and
64 KiB of new user text per turn; three turns; twenty 256-byte output deltas per
turn, spaced 25 ms apart. Histories grow across turns. It runs engines sequentially,
alternates their order across cases, and takes three measured fresh-process runs
after one excluded run for each engine/case. Each run has a 30-second timeout,
512 MiB sampled RSS guard per tree, and 16-process guard. Counters are sampled
every 100 ms and trees discovered every 500 ms. A failed run stops the matrix. Cross-engine output is exploratory and omits
percentage rankings because the loaded capabilities and execution boundaries differ.

These bounds are an initial screen, not final product acceptance criteria. A
candidate must complete every turn and retain every prior user and assistant
message. Achieved provider concurrency and sampling warnings accompany comparisons.
Reject a performance conclusion if fewer streams overlap than claimed. No result
from this screen alone decides historical forks, recovery, or thousand-agent capacity.

The Pi adapter creates multiple `Agent` instances in one Node process using the
published `@earendil-works/pi-agent-core` and `pi-ai` 0.85.1 packages. It supplies
Pi's actual `openai-responses` stream function, not a replacement transport or
model/tool loop. The Codex adapter is a Node JSON-RPC client controlling multiple
threads on one native app-server. **Target RSS/CPU includes the adapter process**:
one process for Pi or Rust, two for Codex in validated runs. This is the cost of the tested
deployment arrangement, not an isolated per-engine or per-agent allocation count.

All three use ephemeral state, the same short base instruction and synthetic prompts,
HTTP/1.1 over loopback, and no tool calls. Pi and Rust declare no tools; Codex still has
native protocol, context, and harness machinery. Its adapter disables shell tools,
plugins, hooks, remote model discovery, request compression, and WebSockets, but
does not claim to make its internal work or advertised tool schemas identical to
Pi's. Request bytes include each candidate's native serialization and overhead.
This is comparable conversation work, not complete feature parity or default CLI
performance. Full access is configured inside the caller's existing permissions.

Engine subprocesses get a small environment allowlist and fresh private HOME,
CODEX_HOME, and workspace folders under the ignored capture. Personal credentials,
proxies, Node injection options, and user configuration are not forwarded. Native
synthetic state may remain in these folders. Raw target output remains suppressed;
adapter failures record only static diagnostic categories. Binary custom commands
retain the separate caller-environment behavior described above.

Provenance records Node version/hash, Pi versions/lock hash or Codex version/native
binary hash, and a fingerprint of benchmark Python, adapter sources, and lockfiles.
Rust records its version, release binary SHA-256, and Cargo.lock SHA-256. The
Rust engine has no benchmark entry point: the runner starts `agent serve` on a
fresh store bound to the synthetic provider and drives its stdio JSONL protocol
from the observer (`bench/daemon_driver.py`), creating one bot per agent and
submitting the workload's turns. Only the daemon is charged to the target; the
observer translates its responses and `text_delta` events into the fixed-size
chunk protocol. Rust captures therefore include SQLite FULL durability
(`durability: sqlite_full` in the profile), so `compare` against the ephemeral
Pi, Codex, and FX profiles is exploratory only. Rust captures before 2026-09-15
used a removed `agent benchmark` subcommand that bypassed the protocol and
SQLite; they are not comparable with later ones.
Change the output directory for every run. The source audit revisions in RUNTIMES
are separate evidence and must not be assumed identical to an installed binary.

## FX native embedded core

Added 2026-09-12. `libfx` is pinned to 0.0.8 in the benchmark dependency lockfile.
Install the locked benchmark dependencies and run:

```sh
pnpm --dir bench/adapters install --frozen-lockfile --ignore-scripts
AGENT_BENCH_TEST_FX=1 .local/venv/bin/python -m unittest discover -s tests -v
.local/venv/bin/python -m bench run --engine fx --out .local/bench/fx
.local/venv/bin/python -m bench.matrix --engines rust fx --out .local/bench/fx-matrix
```

This explicitly selects FX's native addon and fails if it is unavailable; there
is no automatic WebAssembly fallback. Node, the native addon, bridge threads,
and their allocations are inside the target boundary. The CLI is not launched
and built-in tools are unavailable. Every agent has its own in-memory conversation, the same
short instructions and empty tool list as Rust, and full history is verified
on every turn. No checkpoint export, disk persistence, compaction, tool calls,
or cancellation-latency measurement is part of this screen.

FX's real serialization/parser/loop use Gateway SSE. The adapter's host-fetch
callback permits only the synthetic chat endpoint and redirects the known
model-catalog request to the fixture. It rejects all other destinations. It does
not translate the model request into Responses or replace FX's model loop.
The fixture accepts Gateway's text-only request shape, then uses the existing
Responses transcript ledger to reject missing history, cross-agent messages,
retries, and overlapping turns. Output text and per-delta delays are identical;
Gateway and Responses framing, metadata, and terminal payloads differ.

Catalog GETs have separate request and response-byte counters. Their response
bytes and connections are also included in the overall provider counters; the
`requests` and `completed_requests` counters count inference POSTs only. These
body counts exclude HTTP headers, framing, TLS, and real provider traffic.
FX's available pre-output transport retry remains an unexercised difference;
the transcript ledger rejects repeated accepted inference requests.

`bench compare --exploratory` permits the specific Responses/Gateway protocol
difference and lists it as a gap. All host, workload, observer, and sampling
checks still apply, and the report never emits efficiency percentages across
these protocols. The matrix remains 1/8/32 simultaneous streams, two history
sizes, one excluded warmup and three measured fresh processes per engine/case.
The native addon hash, SDK JavaScript hashes, Node hash/version, package version,
release-source reference, and dependency-lock hash are retained as provenance.
The release-source reference identifies the upstream tag, not a reproducible
build attestation for the downloaded npm artifact.

See [FX measurements](FX_MEASUREMENTS.md) for observations and contribution ideas.

## Live fleet check

`bench.live_fleet` is a paid, explicitly invoked check against a real
provider, never part of the test suite. It starts one daemon, submits N
detached turns at once on fresh bots, waits on all handles, and reports
achieved concurrency, per-turn latency, outcomes, tokens, and daemon RSS and
threads sampled from outside. The provider key comes from the caller's
environment and is never printed or stored.

```sh
.local/venv/bin/python -m bench.live_fleet --bots 32 --model anthropic/claude-sonnet-5 \
  --out .local/bench/fleet-sonnet-32
```

Results are `live_fleet_v1` records with the binary hash. See
[LIVE_FLEET.md](LIVE_FLEET.md) for the recorded runs and their limits.

## The 10,000-bot shape

`bench.fleet_screen` runs the shape the goal is about: many bots exist, a
bounded set is active, most wait. Everything goes through the daemon's stdio
protocol, never CLI processes. Phases: create N bots; submit every bot once
with `--max-active` bounding the live set, retrying the daemon's
`active_agent_limit` refusals as turns finish; park P bots on one anchor turn
the synthetic model holds open, then release it; submit a bounded wave held
open by the provider, kill the daemon, confirm exit, and time recovery. Daemon
RSS, threads, and open files are sampled from outside throughout. Synthetic by default with `--delay-ms` making replies
slow enough that turns actually overlap at the bound; `--model` runs the burst
against a real provider with the key from the environment. `--no-restart`
keeps the daemon's exit clean for a heap profile. Synthetic parking requires
`--max-active` of at least two, or zero for unbounded admission: the held
anchor occupies one slot. A one-slot parking configuration is rejected before
creating the workspace or starting the daemon.

With `--max-active 0`, the driver submits up to the whole fleet before waiting
for completions. Positive values cap outstanding submissions at that value.
Earlier screens using zero incorrectly serialized submissions; discard those
zero-limit runs as concurrency evidence. This is a driver correction.

Results use `fleet_screen_v4`: `completed` counts only successful turns,
`failed` counts all other terminal outcomes, and `finished` is their sum.
`turns_per_second` measures successful work; `finished_per_second` includes
failures. Latency runs from immediately before an accepted submission to the
controller reader's receipt of its terminal event, including submission
acknowledgment time and excluding later batch-processing delay. Percentiles
include all finished outcomes. Earlier v1/v2 latency numbers used batch
processing time and are not comparable to this boundary. The v4 restart phase
holds `min(bots, max-active)` turns open (all bots when the limit is zero),
waits for process exit before reopening the store, and verifies that every
submitted bot recovered as interrupted. It works with a single bot and is
independent of reply latency. Its controlled recovery workload differs from
v1–v3, which tried to kill after half the fleet finished; do not compare their
restart timings as matched work. Earlier v1 captures named all finished turns
`completed`; their failure counts remain in `refusals` and must be subtracted when interpreting successful throughput.

```sh
.local/venv/bin/python -m bench.fleet_screen --bots 10000 --max-active 1024 \
  --parked 5000 --delay-ms 500 --out .local/bench/fleet-screen-10k
```

`bench.heap_profile` reads the dhat JSON written by a daemon built with the
`heap-profile` feature and `AGENT_HEAP_PROFILE=path`, and prints live bytes at
the global peak by owning frame in this crate and by innermost allocation
site. The profiling build keeps debug info and lives in its own target
directory so the measured binary is untouched:

```sh
CARGO_PROFILE_RELEASE_STRIP=none CARGO_PROFILE_RELEASE_DEBUG=1 \
  cargo build --release --features heap-profile --target-dir .local/target-dhat
AGENT_HEAP_PROFILE=$PWD/.local/bench/heap.json .local/venv/bin/python -m bench.fleet_screen \
  --binary .local/target-dhat/release/agent --bots 10000 --max-active 1024 \
  --parked 5000 --delay-ms 5000 --no-restart --out .local/bench/fleet-screen-heap
.local/venv/bin/python -m bench.heap_profile .local/bench/heap.json
```

Allocation tracing slows the daemon by an order of magnitude, so the profiled
run uses a long reply delay to reach the same overlap; its timings mean nothing.

## Sustained live load

`bench.sustained` keeps N bots taking turns back to back for M minutes through
one daemon against a real provider, resubmitting each bot as soon as its turn
finishes whatever the outcome. It records completed and failed turns by error
code per 30 s window with latency percentiles, daemon RSS, threads, and open
files every 10 s, store growth, and token totals from the turn records.
`--context-items` (default 8) keeps requests the same size as histories grow.
Paid and explicitly invoked; results are `sustained_v1` records with the
binary hash. See [LIVE_FLEET.md](LIVE_FLEET.md#sustained-load).

```sh
.local/venv/bin/python -m bench.sustained --bots 64 --minutes 5 \
  --model openai/gpt-5.6-luna --out .local/bench/sustained-luna-64
```

## Query plan audit

`bench.query_plans STORE` runs `EXPLAIN QUERY PLAN` for runtime SQL extracted
from `src/store/db.rs`, expanding both deletion templates for artifacts,
processes, and tools with their actual predicates. It uses the source's column
and history-view expressions and enables foreign-key checks as the daemon does.
Against a store at the current schema, it exits nonzero when
any statement scans a growing table without an index. Structural flags
(`EXISTS` constant rows, named recursive walks, per-turn sorts, singleton
configuration, and schema metadata) are counted separately; scan exemptions
match exact plan steps. One-time migrations are excluded. The summary reports
the Python interpreter's SQLite version, which can differ from the daemon's
bundled SQLite. This is a plan check, not a runtime timing benchmark. Open a
copy of a store once with the current binary to migrate it before auditing.

## Long history probe

`bench.long_history` seeds one bot with N stored items through a stdio daemon
and a synthetic Responses provider, then measures what a caller pays as stored
history grows while the context window stays fixed: daemon startup to readiness
and the subsequent `resume` operation measured separately, one turn's request
bytes, item count, and latency, a fork from the head and from the first checkpoint, a turn on the fork, a `history` read of
turn 1, and daemon RSS sampled from outside. Seeding time is reported but is
setup, not a claim. Results are `long_history_v2` records with the binary hash.
`startup_ms` ends at readiness; `resume_op_ms` measures only the following request. Older v1 captures
misnamed startup alone as `restart_and_resume_ms`; those historical numbers
exclude resume.

```sh
.local/venv/bin/python -m bench.long_history --items 100000   --context-bytes 65536 --context-items 256 --out .local/bench/history-100k
```

## What is measured

| Field | Meaning and limits |
| --- | --- |
| Target/provider peak RSS | Maximum sampled sum across each owned process tree. Shared pages can be counted more than once; this is neither private memory nor PSS. |
| Observed CPU seconds | Sum of the last observed user+system CPU counters for each process lifetime. Exited processes retain their last observation, but work after the last sample and unseen short-lived children is missed. |
| Processes | Peak sampled live count; configured agent concurrency is separate. |
| Threads | Peak sampled thread count summed across the target tree. Includes Node/native bridge workers for FX; this is not an allocation or stack-memory measurement. |
| Provider peak active requests | Simultaneously streaming fixture requests actually observed by the provider. This is not proof of active agent capacity. |
| Ready and turn/first-chunk timings | Observed at the driver's stdout reader using a monotonic clock. Includes event transport and observer scheduling. Adapters normalize engine deltas to logical fixture chunks; first-chunk time is time to a full logical chunk, not necessarily the first token. |
| Provider bytes and connections | HTTP request/response body bytes and connections that served requests. Excludes HTTP headers, TCP/TLS overhead, retransmits, unrelated target networking, and model token counts. |
| Observer overhead | Driver CPU, sampled driver RSS, and time spent sampling. Provider resources are reported separately. These cannot simply be subtracted to undo observer-induced contention. |

Missing metrics are explicit: private/PSS memory, exact process-tree CPU,
allocation counts, wire bytes, admission latency, and cancellation latency are
not measured in this first slice. Timeout cleanup is tested, but is not an
engine cancellation-latency measurement.

Resource counters are sampled every 100 ms by default. Tree discovery is more
expensive and defaults to once a second plus startup/exit. Observed descendants
remain tracked after reparenting; unseen children that spawn, detach, and exit
between discovery passes can be missed. `--interval` and `--discovery-interval`
control this tradeoff. Resource limits are sampled guards, not OS-enforced hard
limits. A run can transiently exceed them between observations. CPU sampling
of very short commands is unsuitable; fewer than two live samples fails the run.

Both target and provider have the declared time/resource guard context: the
timeout covers target execution; provider startup has a separate five-second
deadline. RSS/process guards apply separately to the target and provider trees
during execution. Cleanup sends termination, then kill, to owned groups and
observed descendants that changed groups. It cannot promise cleanup of an
unobserved daemon that escaped the ancestry and process group before discovery.

The report flags sampler wall time above 10% of target wall time. Before using
small differences to guide optimization, inspect observer overhead and provider
load, use longer runs, and calibrate at multiple sample intervals. The fixture
provider can itself become a bottleneck. This host's macOS process APIs and
loopback networking require execution outside the Codex sandbox; permission
failures must be resolved explicitly, not converted into zero resource use.

## Fixture and event contract

Workloads set concurrency, turns per agent, chunks per turn, bytes per chunk,
delay before each chunk, and history bytes per request. Responses and history
are deterministic ASCII `x` bytes. TCP packet boundaries are not chunk boundaries.
The v1 workload caps total turns at 100,000, chunks at 1,000,000, and request
history/response payload totals at 512 MiB each. Explicit workload edits are
required for larger tests. No fork, tool, reconnect, or slow-follower scenario is
claimed yet.

The driver supplies `AGENT_BENCH_PORT` and `AGENT_BENCH_WORKLOAD` (JSON). A target
posts `{"history":"..."}` to `http://127.0.0.1:PORT/stream`, with a Content-Length
header. The body must contain exactly the declared synthetic history. Responses
have a Content-Length header and streamed binary body; persistent connections
are supported. This remains the binary calibration endpoint.

The real-engine mode instead accepts `POST /v1/responses` with a Content-Length
request and returns HTTP chunked SSE: response creation, message/content creation,
text deltas, finalized text/item, and a completed response. It is a text-only
protocol subset, not a general OpenAI emulator. It rejects tool-call histories,
retries, hidden previous-response references, missing history, and cross-agent
messages. Native system/developer and environment context may accompany the
validated synthetic conversation. Token usage is a fixed placeholder and must
not be interpreted as measured token usage or cost.

Model workloads additionally cap output text at 1 MiB per turn, estimated full
history at 8 MiB per request, and cumulative history resend volume at 512 MiB.
The server rejects request bodies above 16 MiB. Response body counts include SSE
envelopes and repeated final text; `output_text_bytes` counts generated text once.
Both modes validate that text total against the adapter's delivered byte count.

Target stdout emits newline-delimited JSON, at most 64 KiB per line:

```json
{"event":"ready"}
{"event":"turn_start","agent":"bob","turn":"0"}
{"event":"chunk","agent":"bob","turn":"0","seq":0,"bytes":256}
{"event":"turn_end","agent":"bob","turn":"0"}
```

Emit every declared chunk with a contiguous zero-based sequence before ending
the turn. Agent and turn IDs are nonempty strings up to 128 characters. Each
agent must complete its declared number of turns, without overlapping turns on
the same identity. Completion validates event totals against the provider's
completed requests and delivered text count. Report validation failures and
nonzero target exits even if timing or memory appears better.

## Next useful extensions

Add fixed tool-call fixtures, durable fork/resume cases, cancellation, and slow
readers as distinct contracts. Retained text history is now checked on every
model request; durable history and fork sharing remain unmeasured.
Use the existing profiler appropriate to a demonstrated hotspot. A new engine
requires a measured advantage or a feature gap after that reuse assessment.


## Rust lifecycle and feature costs

```sh
.local/venv/bin/python -m bench.lifecycle --agents 32 --mode text --tools echo --out .local/bench/durable-text
.local/venv/bin/python -m bench.lifecycle --agents 32 --mode echo --tools echo --out .local/bench/durable-echo
.local/venv/bin/python -m bench.lifecycle --agents 32 --mode shell --tools echo,shell --out .local/bench/durable-shell
```

By default these drive the stdio service (`agent serve` without `--socket`, bound to the
synthetic Responses provider through `--provider openai=responses,URL`) and
exercise SQLite FULL durability, tool round trips,
kill/restart, exact resume/replay/item retrieval, idempotent submission, and
historical forks. Shell mode validates an actual workspace artifact; forked
workspaces must remain untouched. The separate fixture validates every prior
message and tool result. Tests separately cover independent fork continuation
and cancellation; this performance workload does not time those operations.

Pass `--transport socket` to measure the Unix-socket daemon with one independent
follower connection per bot. Every follower's durable stream is checked against
database replay, before and after restart. The controller and follower readers
run in the Python observer; these measurements do not include Rust CLI processes.
For all built-in tool schemas, use `--tools echo,shell,read,write,edit`. Shell mode
executes shell tools; read/write/edit are registered but are not exercised here.
Results use `rust_lifecycle_v2` and record transport, follower count, peak sampled
daemon RSS separately from its descendant tree, and tree thread counts.

```sh
.local/venv/bin/python -m bench.lifecycle --transport socket --agents 32 \
  --mode echo --tools echo,shell,read,write,edit --out .local/bench/socket-32
```

This is the per-slice regression screen, and it runs in echo mode: shell
mode at 32 agents puts a shell and its child under the daemon for every
turn, 65 processes at once, which exceeds the observer's 48-process limit
and fails the run before the turns complete. Use `--agents 8` for shell
mode, or `--mode echo` at 32.

The observer/controller and provider are separate from the charged native
process plus its descendants. Sampling is every 200 ms, including recursive child
discovery, with a wider group scan every 500 ms. Each idle observation lasts 450 ms. Short-lived processes can still
be missed and observed CPU is a lower bound. Limits are 30 seconds, 512 MiB per
target/provider tree, 48 target processes. Each case has one excluded warmup and
three measured runs. A full service startup happens before its first sample;
reported peak RSS cannot exclude earlier transient peaks. Idle phase samples
show retained RSS, not live heap allocation.

`--binary PATH` selects a preserved Rust executable in both streaming and
lifecycle modes. Its binary hash identifies the artifact. In streaming override
mode the original Cargo.lock hash is unknown and recorded as null. Never attach
the current lockfile to an older executable as build provenance.

Lifecycle results have a separate schema and cannot be passed to streaming
`bench compare`. Different modes are feature-cost observations, not like-for-like
speedups. Compare lifecycle revisions only with the same mode, toolset, complete
workload, transport/follower count, host/power, observer fingerprint, bounds, successful runs, and achieved
concurrency. See [the recorded measurements](LIFECYCLE_MEASUREMENTS.md).

Benchmark failures from Rust can now include a strictly whitelisted stage/code
and numeric OS error. URLs, error messages, stderr, prompts, and credentials are
not retained as diagnostic data. The new observer fingerprint means older and
newer captures must not be silently combined.

## Store scale

```sh
.local/venv/bin/python -m bench.store_scale --sizes-gb 1 10 --out .local/bench/store-scale
```

This separate screen grows one store through the stdio service with synthetic
text and shell turns. A quarter of the turns go to eight heavy bots; both
heavy and light groups receive equal numbers of text and shell turns. Each
checkpoint measures 32 turns per shape, with at most eight concurrent heavy
turns or 32 light turns. These are submission bounds, not measured stream
concurrency. The result records the growth mix, binary hash, mean request body
bytes, operation costs, and sampled daemon RSS and WAL size.

The crash probe waits for 32 new held requests to arrive at the provider,
fails on timeout, and checks every interrupted turn after restart. Paging
results are first/repeated reads with uncontrolled cache state. The provider
does not validate prior conversation content; provider, observer, and child
process memory are not included in daemon RSS. This is an exploratory screen,
not a matched regression comparison or a capacity claim. The temporary store
is removed afterwards unless `--keep` is given. `--bots` must be at least 32.
See [the results and limitations](DAEMON_MEASUREMENTS.md#store-scale).

## Mixed-workload soak

```sh
.local/venv/bin/python -m bench.soak --minutes 60 --out .local/bench/soak-60
```

One daemon on the socket transport and the synthetic provider, no spend, with
192 bots in seven roles running at once for the whole run: long histories
compacting on a small window, shell output overflowing into artifacts,
background-command bursts, parents parked on children, one-time provider
failures, historical forks run and deleted, slow socket followers, and a
running turn interrupted every 30 seconds (a slow turn is submitted for it
when nothing interruptible is running), with retention pruning every bot as
it goes. Every five seconds it samples daemon RSS, threads, file
descriptors, descendant processes and their memory, store and WAL size, the
store's queue and run time, active, waiting, paced, and queued turns,
background work, and provider pools. It counts turns by outcome, compactions,
forks, deletes, retries, cancellation latency, and observed slow-follower
EOF/resets before restart. Four noisy-bot followers read at most 256 bytes
every 50 ms; eight long-history followers spool durable events to temporary
files. The observer does not infer eviction from a disconnect, and buffered
data can delay observing EOF. At the end it drains, pages through replay,
waits for the fast followers to reach its endpoint, compares the complete
retained interval, then kills the daemon with turns in flight and restarts
it on the same store. The
result records the binary hash, roles, counts, latency percentiles, the
restart outcome, and every sample. In `soak_v3`, turn latency runs from before
sending `submit` through receipt of the terminal event, including acknowledgement
delay. Earlier captures started after acknowledgement and are not directly
comparable. Turn latency includes roles that wait on
a child shell or retry a failed request, so it is not a provider-only
number; the provider does not validate conversation content, and the
workload is paced, so throughput is the pace, not a capacity claim. See
[the results](DAEMON_MEASUREMENTS.md#mixed-workload-soak).

## Storage attribution and codec screen

These are synthetic, local storage experiments with no paid calls. Keep stores,
corpora, and captures under `.local/`. Profile a stopped store or consistent
snapshot; the profiler does not checkpoint, vacuum, or alter its source.
Outputs must be new paths. `dbstat` attribution is optional when the installed
Python SQLite lacks that module; payload accounting remains available.
The corpus exporter appends `.json` to its complete output filename for the
report (for example, `varied.sqlite.json`) and refuses existing corpus or
report paths before creating either output.

```sh
.local/venv/bin/python -m bench.storage_profile \
  --store .local/bench/soak-60/state.sqlite --out .local/storage/profile.json
.local/venv/bin/python -m bench.storage_growth \
  --turns 64 256 1024 --out .local/storage/growth
```

The growth probe stops at each boundary, profiles, then resumes the same
sixteen identities with four-turn retention. It is not a latency soak.
The isolated codec crate does not add a daemon dependency or modify its schema:

```sh
CARGO_TARGET_DIR=.local/target/storage-codec cargo build --release --locked --manifest-path bench/storage_codec/Cargo.toml
for profile in repeated varied entropy; do
  .local/venv/bin/python -m bench.storage_corpus \
    --profile "$profile" --out ".local/storage/$profile.sqlite"
  .local/target/storage-codec/release/storage-codec-screen \
    --corpus ".local/storage/$profile.sqlite" \
    --out ".local/storage/$profile-results" --repeats 5
done
```

Add `--codec lz4` to compare LZ4 instead of zlib in the isolated screen.
For a synthetic store containing raw artifacts, replace `--profile` with
`--store PATH` to export its node, inline prompt, and artifact bytes into a
separate corpus. The exporter rejects compressed artifacts instead of silently
benchmarking their encoded representation; use decoded protocol output for
that corpus. The profiler supports both formats and distinguishes logical
artifact bytes from `stored_artifact_bytes`.
The native screen verifies every byte after reopening for both raw and
compressed storage. It reports repeated write CPU/wall time, final checkpoint
time, disk bytes, and identical bounded partial reads; it deletes its temporary
trial databases after verification. This does not test daemon lifecycle,
forks, production migration, or turn latency. See [results and measurement
boundaries](STORAGE_GROWTH.md) before interpreting compression savings.

Focused checks:

```sh
.local/venv/bin/python -m unittest tests.test_storage_profile
cargo test --locked --manifest-path bench/storage_codec/Cargo.toml
cargo clippy --locked --manifest-path bench/storage_codec/Cargo.toml --all-targets -- -D warnings
```

Matched runtime screens:

```sh
.local/venv/bin/python -m bench.storage_compare \
  --before PATH_TO_BASELINE --after PATH_TO_CANDIDATE --out .local/storage/lifecycle
.local/venv/bin/python -m bench.storage_artifacts \
  --before PATH_TO_BASELINE --after PATH_TO_CANDIDATE --out .local/storage/artifacts
```

The lifecycle comparison alternates one warmup pair and five measured pairs at
256-byte and 64-KiB history prompts, reusing exact provider-history validation,
resume/replay, duplicate submission, and historical forks. The artifact screen
alternates one warmup and three measured pairs per shape: varied diagnostics
and seeded random bytes encoded as ASCII. Eight shell bots produce 1 MiB per
turn while eight ordinary text bots run, for sixteen rounds with four-turn
retention. It measures daemon CPU (excluding shell/provider/observer CPU), RSS
every 10 ms, separate shell/text turn latency, and 128 late-offset 4 KiB artifact
pages including protocol overhead. It reconstructs eight full artifacts,
compares exact transcript hashes per bot, stops for storage attribution, and
checks inherited artifact reads after restart and fork. The two binaries use
the same fixture bytes and durability. OS caches are uncontrolled, and these
bounded screens do not establish sustained capacity or real-task compression.

## Active steering

```sh
.local/venv/bin/python -m bench.active_steering \
  --before PATH_TO_BASELINE --after PATH_TO_CANDIDATE \
  --out .local/bench/active-steering
```

This matched stdio screen holds eight concurrent synthetic provider requests,
queues 40 strict steers per bot, then releases the responses. It repeats for
20 boundaries of the same active turns, crossing the 32-steer storage batch
on every boundary. Two shapes emit either one or 48 assistant items per
response, exercising increasingly long current-turn accounting walks. All
work fits the default context in both binaries. Every steer must finish as
`steered` into its original turn; every subsequent provider request must contain
the exact expected conversation. Request bytes, call counts, absorbed counts,
and history hashes must match before comparing results.

Each shape excludes one full warmup per binary, then runs two
before/after/after/before blocks (four samples per binary). It records daemon
CPU, daemon RSS sampled every 5 ms, total wall time, and each storage operation's
count, execution time, and queue time. Boundary latency runs from releasing a
provider response to receiving the next complete request, including provider
and observer scheduling, response storage, absorption, and context construction.
Wall time also includes sequential steer submission and validation. Provider,
observer, and client memory/CPU are outside daemon accounting. This screen has
no tool processes, overload, context eviction, real provider latency, or capacity
claim. Captures are local; a failed contract assertion stops the comparison.

## Optional detailed memory counters

`bench.lifecycle --memory-detail` adds `pss_bytes` and `private_bytes` to each
target-tree sample and records the option in the result. PSS apportions shared
resident pages among processes; private bytes are USS. Unsupported, denied, or
racing reads produce null rather than zero or a partial total. RSS remains
available independently. These counters require more observer work and are off
by default. Do not compare timing directly across different sampling modes.

## Context-quality evaluation

This opt-in paid screen checks a workspace rule across long conversations.
It reports file outcomes separately for turns with the rule retained, omitted,
or crossing the window boundary, plus unknown results for failed turns.
It counts history calls across all event pages and includes complete usage.
The window snapshots bracket each turn; they do not locate the action within
a transitional turn. Visible examples and workspace files remain possible
sources of the rule. This is not a CPU, memory, or latency benchmark.

```sh
(set -a; . ./.env.local; set +a; .local/venv/bin/python -m bench.context_eval --model openai/gpt-5.6-luna --out .local/context-eval/luna.json)
```

Synthetic regression checks require no paid calls:

```sh
AGENT_TEST_RUNTIME=1 .local/venv/bin/python -m unittest tests.test_context_eval
```

The runtime test requires the release binary and local loopback access.
See [the exploratory results and their limits](DAEMON_MEASUREMENTS.md#context-quality-before-compaction).
