# Five-harness screen

Measured 2026-10-28 UTC with `bench.matrix` plus separate runs of its last
case. **This is an exploratory screen, not a ranking.** The engines differ in
features, storage, and deployment arrangement (listed below), so the numbers
describe what each tested arrangement cost for the same synthetic
conversation work. They do not isolate implementation efficiency. See the
[comparison contract](COMPARISON_CONTRACT.md) and
[benchmark tools](BENCHMARKS.md).

## Conditions

- Host: a 4-vCPU Intel Xeon (2.8 GHz) Linux 6.18 cloud VM with 16 GiB RAM,
  shared and not otherwise controlled. Earlier screens in these docs ran on a
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
| Agent | `agent-runtime 0.4.1`, binary `8dbe6a2f…`, daemon source identical to main `3a9f7df` | one native daemon, stdio JSONL protocol | SQLite, `synchronous=FULL` |
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
| Agent | 28.7 | 0.69 | 82 ms | 608 ms | 32 of 32 |
| Pi | 158.8 | 1.94 | 104 ms | 777 ms | 32 of 32 |
| Codex | 245.5 | 40.06 | 2,391 ms | 10,211 ms | 7–9 of 32 |
| opencode | 957.6 | 24.69 | 3,408 ms | 6,152 ms | 32 of 32 |
| Claude Code | 6,490.2 | 42.43 | 514 ms | 3,489 ms | 24–29 of 32 |

Codex and Claude Code did not reach the configured concurrency at 32 agents
on this host, in either history size, so their 32-agent rows describe a
smaller overlap than the others and `compare` refuses them. Each of those
runs carries the runner's "configured concurrency was not achieved" warning;
no other run in this screen has a quality warning. In the 2026-09-23 screen
Claude Code reached 32 of 32 and Codex 10 to 18.

Median peak memory in MiB by case:

| Agents | New text per turn | Agent | Pi | Codex | opencode | Claude Code |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 4 KiB | 20.2 | 101.8 | 158.9 | 650.0 | 270.9 |
| 1 | 64 KiB | 21.5 | 99.7 | 160.6 | 654.3 | 271.0 |
| 8 | 4 KiB | 21.2 | 108.4 | 180.0 | 724.9 | 1,708.0 |
| 8 | 64 KiB | 25.2 | 119.2 | 191.0 | 727.0 | 1,711.6 |
| 32 | 4 KiB | 23.9 | 123.8 | 215.1* | 800.4 | 6,528.5* |
| 32 | 64 KiB | 28.7 | 158.8 | 245.5* | 957.6 | 6,490.2* |

Median observed CPU seconds by case:

| Agents | New text per turn | Agent | Pi | Codex | opencode | Claude Code |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 4 KiB | 0.43 | 0.93 | 0.52 | 9.86 | 1.15 |
| 1 | 64 KiB | 0.37 | 0.96 | 0.50 | 10.27 | 1.24 |
| 8 | 4 KiB | 0.38 | 1.27 | 7.70 | 14.26 | 9.41 |
| 8 | 64 KiB | 0.41 | 1.18 | 9.30 | 13.12 | 9.32 |
| 32 | 4 KiB | 0.67 | 1.64 | 38.76* | 19.53 | 40.49* |
| 32 | 64 KiB | 0.69 | 1.94 | 40.06* | 24.69 | 42.43* |

\* Codex reached 11–14 (4 KiB) and 7–9 (64 KiB) of 32 concurrent streams,
Claude Code 21–32 and 24–29.

Every measured run completed all its turns with no invalid provider requests.
Full ranges, ready times, connection counts and process counts are in the
ignored captures under `.local/bench/harness-oct28/`.

## Observations

- Agent's daemon grew 7.2 MiB from 1 to 32 agents at 64 KiB, while also
  committing every turn to SQLite with full durability, which no other engine
  here does at that level. Against the 2026-09-23 screen at `8ebbc44` (22.1
  MiB, 0.63 s) it used 6.6 MiB more at the top case, about 3 MiB of it
  already present with one agent, and about 10% more CPU on a host where the
  other engines needed 40 to 80% more.
- Pi, a library rather than a coding agent, stayed closest: about 100 MiB of
  Node baseline plus 1 to 2 MiB per agent.
- opencode starts at about 650 MiB and used about 10 CPU seconds for a
  one-agent run; its first turn includes a lazy start-up of about 2.5 to 3 s.
- Claude Code's cost is dominated by its arrangement, one CLI process per
  agent: about 200 MiB of summed RSS per process (shared pages counted each
  time) and about 6 s to get 32 processes ready.
- Codex's CPU rose from 0.5 s at one agent to 8 to 9 s at eight on this host,
  where the macOS screen in [RUST_MEASUREMENTS.md](RUST_MEASUREMENTS.md)
  recorded 3.2 s at 32 agents. That difference is not investigated here.

## Not measured

Tools, durable resume, historical forks, compaction, cancellation, slow
consumers, TLS, live provider authentication, and model quality. These results
do not show coding-agent capacity at any scale.
