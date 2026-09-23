# Five-harness screen

Measured 2026-09-23 UTC with `bench.matrix` plus separate runs of its last
case. **This is an exploratory screen, not a ranking.** The engines differ in
features, storage, and deployment arrangement (listed below), so the numbers
describe what each tested arrangement cost for the same synthetic
conversation work. They do not isolate implementation efficiency. See the
[comparison contract](COMPARISON_CONTRACT.md) and
[benchmark tools](BENCHMARKS.md).

## Conditions

- Host: a 4-vCPU Intel Xeon (2.8 GHz) Linux 6.18 cloud VM with 16 GiB RAM,
  shared and not otherwise controlled. Earlier screens in these docs ran on a
  10-core macOS machine and are not comparable with this one.
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
| Agent | `agent-runtime 0.1.0`, binary `f2d47286…`, source identical to main `8ebbc44` | one native daemon, stdio JSONL protocol | SQLite, `synchronous=FULL` |
| Pi | `pi-agent-core` / `pi-ai` 0.85.1 | one Node process, one `Agent` per agent | in memory |
| Codex | `codex-cli 0.153.1`, binary `b9315df6…` | one native app-server plus a Node JSON-RPC adapter | ephemeral threads |
| opencode | 1.18.32, binary `513f500a…` | one native `opencode serve` plus a Node HTTP adapter | SQLite, WAL, `synchronous=NORMAL` |
| Claude Code | 2.1.267, binary `0399c793…` | one native CLI process per agent plus a Node adapter | no transcript saved |

Node v22.22.2 for every adapter. Each adapter's disabled features and known
differences are in [BENCHMARKS.md](BENCHMARKS.md).

## Results

32 agents, 64 KiB per turn:

| Engine | Peak memory, MiB | CPU s | First chunk p50 | Turn p99 | Streams reached |
| --- | ---: | ---: | ---: | ---: | ---: |
| Agent | 22.1 | 0.63 | 39 ms | 616 ms | 32 of 32 |
| Pi | 164.2 | 1.30 | 70 ms | 696 ms | 32 of 32 |
| Codex | 243.7 | 24.86 | 1,448 ms | 5,909 ms | 10–18 of 32 |
| opencode | 926.7 | 14.37 | 1,745 ms | 3,330 ms | 32 of 32 |
| Claude Code | 6,493.9 | 23.84 | 222 ms | 1,827 ms | 32 of 32 |

Codex did not reach the configured concurrency at 32 agents on this host, in
either history size, so its 32-agent rows describe a smaller overlap than the
others and `compare` refuses them. It reached 32 of 32 on the macOS host in
[RUST_MEASUREMENTS.md](RUST_MEASUREMENTS.md). Each of those runs carries the
runner's "configured concurrency was not achieved" warning; no other run in
this screen has a quality warning.

Median peak memory in MiB by case:

| Agents | New text per turn | Agent | Pi | Codex | opencode | Claude Code |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 4 KiB | 17.3 | 98.1 | 159.1 | 658.5 | 254.2 |
| 1 | 64 KiB | 18.6 | 104.3 | 160.6 | 589.0 | 278.0 |
| 8 | 4 KiB | 17.6 | 111.4 | 180.4 | 706.4 | 1,720.8 |
| 8 | 64 KiB | 21.0 | 118.5 | 192.1 | 707.8 | 1,736.3 |
| 32 | 4 KiB | 19.9 | 125.9 | 220.3* | 885.4 | 6,548.2 |
| 32 | 64 KiB | 22.1 | 164.2 | 243.7* | 926.7 | 6,493.9 |

Median observed CPU seconds by case:

| Agents | New text per turn | Agent | Pi | Codex | opencode | Claude Code |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 4 KiB | 0.30 | 0.66 | 0.36 | 6.90 | 0.72 |
| 1 | 64 KiB | 0.31 | 0.70 | 0.41 | 6.95 | 0.86 |
| 8 | 4 KiB | 0.37 | 0.84 | 6.11 | 8.95 | 5.58 |
| 8 | 64 KiB | 0.38 | 0.84 | 5.94 | 9.79 | 5.70 |
| 32 | 4 KiB | 0.59 | 1.18 | 25.18* | 13.78 | 24.61 |
| 32 | 64 KiB | 0.63 | 1.30 | 24.86* | 14.37 | 23.84 |

\* Codex reached 12–19 (4 KiB) and 10–18 (64 KiB) of 32 concurrent streams.

Every measured run completed all its turns with no invalid provider requests.
Full ranges, ready times, connection counts and process counts are in the
ignored captures under `.local/bench/five-engines-2026-09-23/`.

## Observations

- Agent's daemon grew 3.5 MiB from 1 to 32 agents at 64 KiB, while also
  committing every turn to SQLite with full durability, which no other engine
  here does at that level.
- Pi, a library rather than a coding agent, stayed closest: about 100 MiB of
  Node baseline plus 1 to 2 MiB per agent.
- opencode starts at about 600 MiB and used about 7 CPU seconds for a
  one-agent run; its first turn includes a lazy start-up of about 1.5 to 2 s.
- Claude Code's cost is dominated by its arrangement, one CLI process per
  agent: about 200 MiB of summed RSS per process (shared pages counted each
  time) and 3.6 to 3.8 s to get 32 processes ready.
- Codex's CPU rose from 0.4 s at one agent to about 6 s at eight on this host,
  where the macOS screen recorded 3.2 s at 32 agents. That difference is not
  investigated here.

## Not measured

Tools, durable resume, historical forks, compaction, cancellation, slow
consumers, TLS, live provider authentication, and model quality. These results
do not show coding-agent capacity at any scale.
