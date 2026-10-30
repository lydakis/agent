# Five-harness screen

Measured 2026-10-30 UTC at main `2182b4a`, after the daemon changes that go
into 0.7.0, with `bench.matrix` plus separate runs of its last two cases
(`compare` refuses an engine that misses its concurrency, which stops the
matrix; the cases were run by hand with the matrix's own settings and order).
The 2026-10-28 screen at `3a9f7df` is kept [below](#previous-screen-2026-10-28). **This is an exploratory screen, not a ranking.** The engines differ in
features, storage, and deployment arrangement (listed below), so the numbers
describe what each tested arrangement cost for the same synthetic
conversation work. They do not isolate implementation efficiency. See the
[comparison contract](COMPARISON_CONTRACT.md) and
[benchmark tools](BENCHMARKS.md).

## Conditions

- Host: a 4-vCPU Intel Xeon (2.8 GHz) Linux 6.18 cloud VM with 16 GiB RAM,
  shared and not otherwise controlled, the same kind as the 2026-10-28
  screen. Earlier screens in these docs ran on a
  10-core macOS machine and are not comparable with this one. The 2026-09-23
  screen ran on the same kind of VM, but the unchanged engines used 40 to 80%
  more CPU this time, so compare engines within one screen, not across them.
- Workload: the matrix's fixed screen. Each agent does three turns. Each turn
  adds 4 KiB or 64 KiB of user text and receives twenty 256-byte deltas spaced
  25 ms apart from a loopback synthetic provider, which validates the full
  prior history on every request. No tools are called. One excluded warmup and
  three measured fresh-process runs per engine and case; medians shown.
- Memory is **summed sampled RSS** of the target's process tree, sampled every
  100 ms. For a process-per-agent arrangement (Claude Code) this counts shared
  executable pages once per process, so it overstates unique memory; PSS is not
  available on this path. Brief peaks between samples can be missed.
- CPU is observed CPU seconds of the target tree, excluding some short-lived
  intervals. Turn p99 includes the provider's 500 ms of scripted streaming.
- Guards: 30 s per run, 16 processes, 2,048 MiB RSS per tree (raised from 512
  because opencode exceeds it with one agent); Claude Code gets 16 processes and
  2,048 MiB per agent.
- Workspaces are temporary directories outside the repository.

| Engine | Version | Arrangement | Store |
| --- | --- | --- | --- |
| Agent | main `2182b4a` (0.7.0 code; version string still `agent-runtime 0.6.0`), binary `4685d0c1…` | one native daemon, stdio JSONL protocol | SQLite, `synchronous=FULL` |
| Pi | `pi-agent-core` / `pi-ai` 0.85.1 | one Node process, one `Agent` per agent | in memory |
| Codex | `codex-cli 0.153.1`, binary `b9315df6…` | one native app-server plus a Node JSON-RPC adapter | ephemeral threads |
| opencode | 1.18.32, binary `513f500a…` | one native `opencode serve` plus a Node HTTP adapter | SQLite, WAL, `synchronous=NORMAL` |
| Claude Code | 2.1.267, binary `0399c793…` | one native CLI process per agent plus a Node adapter | no transcript saved |

Node v22.22.0 for every adapter. Each adapter's disabled features and known
differences are in [BENCHMARKS.md](BENCHMARKS.md).

## Results

32 agents, 64 KiB per turn:

| Engine | Peak memory, MiB | CPU s | First chunk p50 | Turn p99 | Streams reached |
| --- | ---: | ---: | ---: | ---: | ---: |
| Agent | 28.9 | 0.65 | 74 ms | 599 ms | 32 of 32 |
| Pi | 171.7 | 1.76 | 94 ms | 773 ms | 32 of 32 |
| Codex | 301.2 | 35.50 | 2,009 ms | 8,959 ms | 8–10 of 32 |
| opencode | 990.5 | 19.83 | 2,514 ms | 4,694 ms | 32 of 32 |
| Claude Code | 7,347.5 | 32.66 | 415 ms | 2,751 ms | 26–31 of 32 |

Codex and Claude Code again did not reach the configured concurrency at 32
agents, in either history size, so their 32-agent rows describe a smaller
overlap than the others and `compare` refuses them. Codex also reached only
4 to 8 of 8 streams at eight agents with 64 KiB, which stopped the matrix
there. Each of those runs carries the runner's "configured concurrency was
not achieved" warning; no other run in this screen has a quality warning.

Median peak memory in MiB by case:

| Agents | New text per turn | Agent | Pi | Codex | opencode | Claude Code |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 4 KiB | 20.7 | 117.4 | 215.7 | 611.6 | 313.8 |
| 1 | 64 KiB | 21.9 | 111.8 | 217.2 | 614.9 | 294.7 |
| 8 | 4 KiB | 21.8 | 121.0 | 237.9 | 738.1 | 1,926.6 |
| 8 | 64 KiB | 25.9 | 132.5 | 248.2* | 769.1 | 1,931.2 |
| 32 | 4 KiB | 24.5 | 137.5 | 276.6* | 843.7 | 7,327.7* |
| 32 | 64 KiB | 28.9 | 171.7 | 301.2* | 990.5 | 7,347.5* |

Median observed CPU seconds by case:

| Agents | New text per turn | Agent | Pi | Codex | opencode | Claude Code |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 4 KiB | 0.34 | 0.86 | 0.43 | 8.90 | 1.04 |
| 1 | 64 KiB | 0.28 | 0.84 | 0.48 | 9.28 | 0.97 |
| 8 | 4 KiB | 0.35 | 1.10 | 9.41 | 12.83 | 8.01 |
| 8 | 64 KiB | 0.44 | 1.05 | 8.83* | 12.95 | 7.56 |
| 32 | 4 KiB | 0.57 | 1.71 | 39.12* | 19.65 | 34.90* |
| 32 | 64 KiB | 0.65 | 1.76 | 35.50* | 19.83 | 32.66* |

\* Codex reached 4–8 of 8 (8 agents, 64 KiB), 8–12 (4 KiB) and 8–10 (64 KiB)
of 32 concurrent streams, Claude Code 30–32 and 26–31.

Every measured run completed all its turns with no invalid provider requests.
Full ranges, ready times, connection counts and process counts are in the
ignored captures under `.local/bench/harness-oct30-2182b4a/`.

### Run-to-run drift on this host

A trial of the same screen about 90 minutes earlier, at main `759c0db`
(before the last daemon changes) with the same pinned engines, measured the
top case at Agent 29.9 MiB and 0.67 s, Pi 155.4 MiB, Codex 247.2 MiB and
35.3 s, opencode 944.4 MiB and Claude Code 6,577.4 MiB. The other engines'
binaries did not change between the two runs, yet their peak memory moved by
5 to 22%, while each run's three repeats agreed within a few percent. Read
differences of that size between screens as host drift, not as a change in
any engine.

## Observations

- Agent's daemon grew 7.0 MiB from 1 to 32 agents at 64 KiB, while also
  committing every turn to SQLite with full durability, which no other engine
  here does at that level. Against the 2026-10-28 screen at `3a9f7df` (28.7
  MiB, 0.69 s, 608 ms turn p99) the top case is unchanged within this host's
  drift, after the 0.7.0 daemon work that touched the session queue, the
  service loop's fairness between sessions, waiting requests and approvals.
  The 2026-09-23 screen at `8ebbc44` measured 22.1 MiB and 0.63 s.
- Pi, a library rather than a coding agent, stayed closest: about 110 MiB of
  Node baseline plus 1 to 2 MiB per agent.
- opencode starts at about 610 MiB and used about 9 CPU seconds for a
  one-agent run; it took about 2.7 s to become ready.
- Claude Code's cost is dominated by its arrangement, one CLI process per
  agent: about 230 MiB of summed RSS per agent (shared pages counted each
  time) and about 5 s to get 32 processes ready.
- Codex's CPU rose from 0.5 s at one agent to about 9 s at eight on this
  host, where the macOS screen in [RUST_MEASUREMENTS.md](RUST_MEASUREMENTS.md)
  recorded 3.2 s at 32 agents. That difference is not investigated here.

## Previous screen (2026-10-28)

Same host kind, workload and pinned engines; Agent was `agent-runtime 0.4.1`,
binary `8dbe6a2f…`, daemon source identical to main `3a9f7df`. 32 agents,
64 KiB per turn:

| Engine | Peak memory, MiB | CPU s | First chunk p50 | Turn p99 | Streams reached |
| --- | ---: | ---: | ---: | ---: | ---: |
| Agent | 28.7 | 0.69 | 82 ms | 608 ms | 32 of 32 |
| Pi | 158.8 | 1.94 | 104 ms | 777 ms | 32 of 32 |
| Codex | 245.5 | 40.06 | 2,391 ms | 10,211 ms | 7–9 of 32 |
| opencode | 957.6 | 24.69 | 3,408 ms | 6,152 ms | 32 of 32 |
| Claude Code | 6,490.2 | 42.43 | 514 ms | 3,489 ms | 24–29 of 32 |

## Not measured

Tools, durable resume, historical forks, compaction, cancellation, slow
consumers, TLS, live provider authentication, and model quality. These results
do not show coding-agent capacity at any scale.
