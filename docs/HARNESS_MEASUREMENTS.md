# Five-harness screen

Measured 2026-11-01 UTC (10:15–10:25) at `afafeec`, which is main `70637f9`
plus the benchmark pins below, after every daemon change of the 2026-10-31
performance review and the day-5 harness review had merged (the last, #575,
is `f047d85`). Each compared harness was moved to its newest npm release that
day. The whole matrix was run with its own settings and order, and cases it
would have stopped at (`compare` refuses an engine that misses its
concurrency) were run anyway. Earlier screens are kept
[below](#previous-screens). **This is an exploratory screen, not a ranking.**
The engines differ in features, storage, and deployment arrangement (listed
below), so the numbers describe what each tested arrangement cost for the
same synthetic conversation work. They do not isolate implementation
efficiency. See the [comparison contract](COMPARISON_CONTRACT.md) and
[benchmark tools](BENCHMARKS.md).

## Conditions

- Host: a 4-vCPU Intel Xeon (2.8 GHz) Linux 6.18 cloud VM with 16 GiB RAM,
  shared and not otherwise controlled, the same kind as the 2026-10-28 and
  2026-10-30 screens. Earlier screens in these docs ran on a 10-core macOS
  machine and are not comparable with this one. Run-to-run drift on this
  kind of host is up to about 20% for unchanged binaries
  ([below](#run-to-run-drift-on-this-host)), so compare engines within one
  screen, not across screens.
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
- Side traffic was retraced with `strace` on 2026-10-31 for Codex, opencode
  and Claude Code: every connection stays on loopback.

| Engine | Version | Arrangement | Store |
| --- | --- | --- | --- |
| Agent | `afafeec` (main `70637f9`), `agent-runtime 0.7.0`, binary `a84decc9…` | one native daemon, stdio JSONL protocol | SQLite, `synchronous=FULL` |
| Pi | `pi-agent-core` / `pi-ai` 1.4.0 (was 0.85.1) | one Node process, one `Agent` per agent | in memory |
| Codex | `codex-cli 0.166.0` (was 0.153.1), binary `b8a11fed…` | one native app-server plus a Node JSON-RPC adapter | ephemeral threads |
| opencode | 1.19.5 (was 1.18.32), tag commit `810c0eba`, binary `7df4e413…` | one native `opencode serve` plus a Node HTTP adapter | SQLite, WAL, `synchronous=NORMAL` |
| Claude Code | 2.1.316 (was 2.1.267), npm `latest`, binary `fd13c700…` | one native CLI process per agent plus a Node adapter | no transcript saved |

Node v22.23.4 for every adapter (v22.22.0 in the earlier screens). Pi's
coding agent 1.4.0 runs on the same `pi-agent-core` 1.4.0 the adapter
drives. Claude Code is pinned to npm `latest`, the channel the CLI updates on
by default; `stable` was 2.1.309 that day. Each adapter's disabled features and
known differences are in [BENCHMARKS.md](BENCHMARKS.md). Two of them changed
with these versions: Claude Code 2.1.316 no longer sends its `HEAD /api/hello`
warm-up, so it uses one connection per agent instead of two, and it runs one
`git` probe per process at startup instead of three.

## Results

32 agents, 64 KiB per turn:

| Engine | Peak memory, MiB | CPU s | First chunk p50 | Turn p99 | Streams reached |
| --- | ---: | ---: | ---: | ---: | ---: |
| Agent | 30.4 | 0.75 | 83 ms | 628 ms | 32 of 32 |
| Pi | 155.0 | 1.97 | 120 ms | 746 ms | 32 of 32 |
| Codex | 368.0 | 9.34 | 380 ms | 1,358 ms | 30–32 of 32 |
| opencode | 869.3 | 13.81 | 895 ms | 3,580 ms | 32 of 32 |
| Claude Code | 6,257.0 | 36.78 | 544 ms | 2,427 ms | 26–28 of 32 |

Codex 0.166.0 is the main change. 0.153.1 reached only 8 to 10 of 32 streams
here, at about 35 s of CPU and a 9 s turn p99. 0.166.0 reached all 32 at 4 KiB
and 30 to 32 at 64 KiB, at about 9.5 s of CPU and a 1.4 s turn p99, while its
memory rose (301 to 368 MiB at the top case). Claude Code again did not reach
32 streams at 32 agents (30 to 32 at 4 KiB and 26 to 28 at 64 KiB, against
30 to 32 and 26 to 31 before), and its summed RSS fell from about 7.3 to
6.3 GiB. Each run that missed its concurrency carries the runner's
"configured concurrency was not achieved" warning. No other run in this
screen has a quality warning, and every case reached its concurrency at one
and eight agents.

Median peak memory in MiB by case:

| Agents | New text per turn | Agent | Pi | Codex | opencode | Claude Code |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 4 KiB | 22.6 | 96.4 | 242.9 | 576.2 | 265.3 |
| 1 | 64 KiB | 24.3 | 99.7 | 245.5 | 584.5 | 263.7 |
| 8 | 4 KiB | 23.5 | 106.9 | 275.8 | 615.3 | 1,624.0 |
| 8 | 64 KiB | 28.6 | 113.2 | 286.4 | 636.2 | 1,631.4 |
| 32 | 4 KiB | 26.5 | 125.0 | 339.2 | 740.3 | 6,231.3* |
| 32 | 64 KiB | 30.4 | 155.0 | 368.0* | 869.3 | 6,257.0* |

Median observed CPU seconds by case:

| Agents | New text per turn | Agent | Pi | Codex | opencode | Claude Code |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 4 KiB | 0.36 | 0.78 | 0.45 | 6.42 | 1.15 |
| 1 | 64 KiB | 0.36 | 0.77 | 0.54 | 6.68 | 1.28 |
| 8 | 4 KiB | 0.38 | 0.94 | 2.86 | 8.52 | 8.18 |
| 8 | 64 KiB | 0.41 | 1.03 | 2.78 | 8.92 | 8.71 |
| 32 | 4 KiB | 0.85 | 1.67 | 9.58 | 13.62 | 32.95* |
| 32 | 64 KiB | 0.75 | 1.97 | 9.34* | 13.81 | 36.78* |

\* Codex reached 30–32 of 32 concurrent streams (32 agents, 64 KiB), Claude
Code 30–32 (4 KiB) and 26–28 (64 KiB).

Every measured run completed all its turns with no invalid provider requests.
Full ranges, ready times, connection counts and process counts are in the
ignored captures under `.local/bench/harness-nov01-afafeec/`.

### Run-to-run drift on this host

A trial of the same screen with the same pinned engines ran on 2026-10-31
(22:3x UTC) on an older main, from before the 10-31 daemon work. Its top case
measured Agent 29.7 MiB and 0.66 s, Pi 153.0 MiB and 1.61 s, Codex 317.3 MiB and
8.79 s, opencode 810.8 MiB and 12.89 s, and Claude Code 5,207.2 MiB and 34.75 s.
The other engines' binaries were identical between the two runs, yet their
peak memory moved by 1 to 20%, while each run's three repeats agreed within a
few percent. Agent's 30.4 MiB against 29.7 is inside that drift.

## Observations

- Agent's daemon grew 6.1 MiB from 1 to 32 agents at 64 KiB, while also
  committing every turn to SQLite with full durability, which no other engine
  here does at that level. Its top case is unchanged within host drift across
  the four screens on this kind of host: 22.1 MiB at `8ebbc44` (2026-09-23),
  28.7 MiB at `3a9f7df` (2026-10-28), 28.9 MiB at `2182b4a` (2026-10-30) and
  30.4 MiB now, after the reader pool, skip-pointer history, replay, parking,
  ingress credit and process-limit work of 2026-10-31.
- Pi 1.4.0, a library rather than a coding agent, stayed closest: about
  95 MiB of Node baseline plus 1 to 2 MiB per agent.
- Codex 0.166.0 holds 32 streams at 32 agents on this host for the first
  time in these screens, with three provider connections per agent (96 at
  the top case). Why its overlap and CPU changed is not investigated.
- opencode 1.19.5 starts at about 575 MiB (1.18.32: about 610) and used about
  6.5 CPU seconds for a one-agent run; it took about 1.8 s to become ready.
- Claude Code's cost is still dominated by its arrangement, one CLI process
  per agent: about 195 MiB of summed RSS per agent (shared pages counted each
  time) and about 6 s to get 32 processes ready.

## Previous screens

### 2026-10-30

At main `2182b4a` (0.7.0 code, version string `agent-runtime 0.6.0`), binary
`4685d0c1…`, with Pi 0.85.1, Codex 0.153.1, opencode 1.18.32 and Claude Code
2.1.267 on Node v22.22.0. Same host kind and workload. 32 agents, 64 KiB per
turn:

| Engine | Peak memory, MiB | CPU s | First chunk p50 | Turn p99 | Streams reached |
| --- | ---: | ---: | ---: | ---: | ---: |
| Agent | 28.9 | 0.65 | 74 ms | 599 ms | 32 of 32 |
| Pi | 171.7 | 1.76 | 94 ms | 773 ms | 32 of 32 |
| Codex | 301.2 | 35.50 | 2,009 ms | 8,959 ms | 8–10 of 32 |
| opencode | 990.5 | 19.83 | 2,514 ms | 4,694 ms | 32 of 32 |
| Claude Code | 7,347.5 | 32.66 | 415 ms | 2,751 ms | 26–31 of 32 |

Codex also reached only 4 to 8 of 8 streams at eight agents with 64 KiB, and
8 to 12 of 32 at 4 KiB; Claude Code 30 to 32 at 4 KiB. Its captures are under
`.local/bench/harness-oct30-2182b4a/`.

### 2026-10-28

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
