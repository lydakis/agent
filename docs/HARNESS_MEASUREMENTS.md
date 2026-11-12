# Five-harness screen

Measured 2026-11-12 UTC (04:29–04:37) at main `987a276`, the 0.9.4
candidate, with every compared harness on the same pinned release and the
same binary as the 2026-11-01 screen. Agent was the only change. The cases
ran in the matrix's order, interleaving engines and reversing that order on
alternate cases, with a runner that does not stop at a case `compare` would
refuse. Earlier screens are kept
[below](#previous-screens). **This is an exploratory screen, not a ranking.**
The engines differ in features, storage, and deployment arrangement (listed
below), so the numbers describe what each tested arrangement cost for the
same synthetic conversation work. They do not isolate implementation
efficiency. See the [comparison contract](COMPARISON_CONTRACT.md) and
[benchmark tools](BENCHMARKS.md).

## Conditions

- Host: a 4-vCPU Intel Xeon (2.1 GHz) Linux 6.18 cloud VM with 16 GiB RAM,
  shared and not otherwise controlled, the same kind as the 2026-10-28 to
  2026-11-01 screens (those reported a 2.8 GHz part). Earlier screens in
  these docs ran on a 10-core macOS machine and are not comparable with this one. Run-to-run drift on this
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
| Agent | main `987a276`, `agent-runtime 0.9.3 (987a27619)`, binary `2db51db1…` | one native daemon, stdio JSONL protocol | SQLite, `synchronous=FULL` |
| Pi | `pi-agent-core` / `pi-ai` 1.4.0 | one Node process, one `Agent` per agent | in memory |
| Codex | `codex-cli 0.166.0`, binary `b8a11fed…` | one native app-server plus a Node JSON-RPC adapter | ephemeral threads |
| opencode | 1.19.5, tag commit `810c0eba`, binary `7df4e413…` | one native `opencode serve` plus a Node HTTP adapter | SQLite, WAL, `synchronous=NORMAL` |
| Claude Code | 2.1.316, npm `latest`, binary `fd13c700…` | one native CLI process per agent plus a Node adapter | no transcript saved |

Node v22.23.4 for every adapter (v22.22.0 before 2026-11-01). Pi's
coding agent 1.4.0 runs on the same `pi-agent-core` 1.4.0 the adapter
drives. Claude Code is pinned to npm `latest`, the channel the CLI updates on
by default; `stable` was 2.1.309 on 2026-11-01. Each adapter's disabled features and
known differences are in [BENCHMARKS.md](BENCHMARKS.md). Two of them changed
with these versions, against the 2026-10-30 pins: Claude Code 2.1.316 no longer sends its `HEAD /api/hello`
warm-up, so it uses one connection per agent instead of two, and it runs one
`git` probe per process at startup instead of three.

## Results

32 agents, 64 KiB per turn:

| Engine | Peak memory, MiB | CPU s | First chunk p50 | Turn p99 | Streams reached |
| --- | ---: | ---: | ---: | ---: | ---: |
| Agent | 26.6 | 0.24 | 58 ms | 575 ms | 32 of 32 |
| Pi | 156.0 | 1.14 | 77 ms | 669 ms | 32 of 32 |
| Codex | 314.8 | 6.75 | 255 ms | 1,200 ms | 32 of 32 |
| opencode | 854.4 | 9.43 | 589 ms | 2,464 ms | 32 of 32 |
| Claude Code | 5,225.5 | 23.82 | 192 ms | 1,655 ms | 32 of 32 |

Agent is the only engine whose binary changed since 2026-11-01. Its top case
fell from 30.4 to 26.6 MiB and from 0.75 to 0.24 CPU seconds, and its first
chunk and turn p99 fell too. An interleaved A/B on the same host the same day
(Agent only, medians of three) separates the releases: 0.9.0 25.5 MiB and
0.34 s, 0.9.3 27.2 MiB and 0.30 s, main 27.6 MiB and 0.28 s. With the daemon
idle, 0.9.0 to main is +0.8 MiB of clean code pages (the binary grew from
20.8 to 22.1 MB) and +0.1 MiB of heap. Every engine reached every case's
concurrency this time, Codex and Claude Code included, and no run carries a
quality warning.

Median peak memory in MiB by case:

| Agents | New text per turn | Agent | Pi | Codex | opencode | Claude Code |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 4 KiB | 18.4 | 98.3 | 190.9 | 564.7 | 234.9 |
| 1 | 64 KiB | 19.7 | 94.0 | 193.2 | 562.7 | 237.4 |
| 8 | 4 KiB | 20.4 | 107.4 | 224.3 | 640.5 | 1,487.2 |
| 8 | 64 KiB | 24.5 | 114.6 | 235.9 | 649.6 | 1,428.4 |
| 32 | 4 KiB | 23.8 | 123.1 | 287.0 | 756.2 | 5,192.5 |
| 32 | 64 KiB | 26.6 | 156.0 | 314.8 | 854.4 | 5,225.5 |

Median observed CPU seconds by case:

| Agents | New text per turn | Agent | Pi | Codex | opencode | Claude Code |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 4 KiB | 0.02 | 0.58 | 0.36 | 4.71 | 0.72 |
| 1 | 64 KiB | 0.03 | 0.53 | 0.33 | 4.65 | 0.76 |
| 8 | 4 KiB | 0.06 | 0.75 | 1.96 | 5.88 | 5.13 |
| 8 | 64 KiB | 0.10 | 0.71 | 1.72 | 6.01 | 5.36 |
| 32 | 4 KiB | 0.19 | 0.98 | 7.27 | 9.08 | 23.55 |
| 32 | 64 KiB | 0.24 | 1.14 | 6.75 | 9.43 | 23.82 |

Every measured run completed all its turns with no invalid provider requests.
Full ranges, ready times, connection counts and process counts are in the
ignored captures under `.local/bench/h5/`, and the A/B under `.local/bench/m1/`.

### Run-to-run drift on this host

A trial of the same screen with the same pinned engines ran on 2026-10-31
(22:3x UTC) on an older main, from before the 10-31 daemon work. Its top case
measured Agent 29.7 MiB and 0.66 s, Pi 153.0 MiB and 1.61 s, Codex 317.3 MiB and
8.79 s, opencode 810.8 MiB and 12.89 s, and Claude Code 5,207.2 MiB and 34.75 s.
The other engines' binaries were identical between the two runs, yet their
peak memory moved by 1 to 20%, while each run's three repeats agreed within a
few percent. Between 2026-11-01 and this screen the other engines' binaries
were again identical: Pi and opencode moved by 1 to 2%, Codex fell 14% and
Claude Code 16%, and their CPU fell by 28 to 42%. Agent's change in this
screen is larger than that drift and matches the same-day A/B.

## Observations

- Agent's daemon grew 6.9 MiB from 1 to 32 agents at 64 KiB, while also
  committing every turn to SQLite with full durability, which no other engine
  here does at that level. Its top case on this kind of host:
  22.1 MiB at `8ebbc44` (2026-09-23, before group commit), 28.7 MiB at
  `3a9f7df` (2026-10-28), 28.9 MiB at `2182b4a` (2026-10-30), 30.4 MiB at
  `70637f9` (2026-11-01) and 26.6 MiB now. The drop since 2026-11-01 came
  with 0.9.0; 0.9.1 to 0.9.3 added about 1.5 MiB back, nearly all of it
  code pages.
- Pi 1.4.0, a library rather than a coding agent, stayed closest: about
  95 MiB of Node baseline plus 1 to 2 MiB per agent.
- Codex 0.166.0 holds 32 streams at 32 agents in every case here, with three
  provider connections per agent (96 at the top case). Why its overlap and
  CPU changed since 0.153.1 is not investigated.
- opencode 1.19.5 starts at about 565 MiB (1.18.32: about 610) and used about
  4.7 CPU seconds for a one-agent run.
- Claude Code's cost is still dominated by its arrangement, one CLI process
  per agent: about 160 MiB of summed RSS per agent (shared pages counted each
  time). It reached all 32 streams at 32 agents in this screen (2026-11-01:
  26 to 28 at 64 KiB).

## Previous screens

### 2026-11-01

At `afafeec` (main `70637f9` plus the pins above), `agent-runtime 0.7.0`,
binary `a84decc9…`, with the same pinned engines and binaries as the current
screen, on a host reporting a 2.8 GHz Xeon. 32 agents, 64 KiB per turn:

| Engine | Peak memory, MiB | CPU s | First chunk p50 | Turn p99 | Streams reached |
| --- | ---: | ---: | ---: | ---: | ---: |
| Agent | 30.4 | 0.75 | 83 ms | 628 ms | 32 of 32 |
| Pi | 155.0 | 1.97 | 120 ms | 746 ms | 32 of 32 |
| Codex | 368.0 | 9.34 | 380 ms | 1,358 ms | 30–32 of 32 |
| opencode | 869.3 | 13.81 | 895 ms | 3,580 ms | 32 of 32 |
| Claude Code | 6,257.0 | 36.78 | 544 ms | 2,427 ms | 26–28 of 32 |

Codex 0.166.0 was that screen's main change: 0.153.1 had reached only 8 to 10
of 32 streams, at about 35 s of CPU and a 9 s turn p99. Claude Code reached
30 to 32 streams at 4 KiB and 26 to 28 at 64 KiB. Its captures are under
`.local/bench/harness-nov01-afafeec/`.

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
