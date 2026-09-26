# Current evidence

Snapshot, 2026-09-26, at `afdd633` plus the change that added this page,
updated at `095ff68` for admission batching and disk-full containment, and
by the change that built tool approval for its cost. This
is the one place that says what is currently known. The documents it links to
keep the method, the raw tables and superseded runs. When a history document's
opening disagrees with this page, this page is current. A change that lands a
measurement updates this page with it.

Each line says what was measured, on which build and host, and when. Unless a
line says otherwise it is a measurement, and it holds only for that workload.
Vendor claims and inferences are labelled.

## Runtime efficiency

Fixed-work resource and latency measurements. A time-based soak that completes
more turns is not here, because its work changes with its speed; it is under
[operational behavior](#operational-behavior).

- **Group commit, fixed work.** 192 bots, 4,608 primary turns, the same
  requests and retries in both builds, full flushing in both. The build that
  batches turn completions, which also carried the retention-publication
  fix, took 17–20% less wall time and about 9% less daemon CPU
  (94.62→78.57 s and 104.96→83.88 s; 21.50→19.52 s and 20.56→18.69 s CPU), in
  two runs per build in reversed order. Commits are still about 82% of store
  execution. macOS arm64, `15d629c` against `a7746cd`, 2026-09-26.
  [Record](DAEMON_MEASUREMENTS.md#fixed-work-mixed-load-diagnosis).
- **Completion burst.** 32 turns finishing together: p99 216–238 ms before
  completions were batched, 16–18 ms after, with daemon CPU down from about
  25 to 7 ms. The retention-race fix kept it. Short samples; ordinary
  streaming turns did not get faster. macOS arm64, 2026-09-26.
  [Record](DAEMON_MEASUREMENTS.md#completion-burst).
- **Group commit, injected sync delay.** Linux VM, with an fsync delay injected
  under both builds: 26 to 38 turns a second at 10 ms per sync, 95 to 128 at
  2 ms, and no change with no delay. `2d03ac2` against the group build,
  2026-09-26. [Record](DAEMON_MEASUREMENTS.md#group-commit).
- **macOS durability cost.** A full flush (`F_FULLFSYNC`) costs about 5.4 ms,
  so an idle shell turn takes about 30 ms longer than with a plain `fsync`;
  grouping recovers most of it at load (1,280 jobs a second in groups of 8,
  against 184 one at a time). M1 Max, APFS, 2026-09-26. Accepted on purpose:
  a commit survives power loss. [Record](DAEMON_MEASUREMENTS.md#group-commit).
- **No regression from SQLite diagnostics.** 1,152 fixed turns in A/B/B/A
  order: 19.85 and 19.93 s against 19.27 and 19.16 s, CPU and RSS within run
  noise. `7120b48` against the diagnostic build, 2026-09-26.
  [Record](DAEMON_MEASUREMENTS.md#sqlite-failure-diagnostics).
- **Admission batching.** 32 submissions sent at once got their last reply
  in 5.3 ms instead of 47.1 ms at native sync, and in 7.9 ms instead of
  131.1 ms with 2 ms added to each sync, with daemon CPU 2.7 and 3.2 times
  lower (36.6→13.4 and 43.6→13.8 ms). A submission arriving alone is no
  slower (median 1.61 against 1.67 ms). Under sustained load, 64 bots at
  10 ms a sync went from about 56 to 131 turns a second, with no change at
  native sync. RSS after the bursts was 0.3 MiB higher, cause not isolated.
  Linux x86_64 container, `4260673` against `3b6dd53`, 2026-09-26.
  macOS is not measured: the earlier macOS probe of 32 clients (median
  284.8 ms to all replies, `7120b48`) has not been rerun.
  [Record](DAEMON_MEASUREMENTS.md#admission-window).
- **Savepoint journals in memory.** With the writer's journals in memory
  instead of temporary files, a submission's `begin` job ran in a median
  184 µs instead of 416 µs, ranges not overlapping. Its round trip (1.28
  against 1.43 ms) and daemon CPU moved within overlapping ranges, and RSS
  did not change. Linux x86_64 container, `8724f22` against the same source
  with only `temp_store=MEMORY` added (the line `4260673` landed),
  2026-09-26; macOS, where creating a file may cost more, is not measured.
  [Record](DAEMON_MEASUREMENTS.md#disk-full-cause-and-containment).
- **Tool approval.** One synthetic `shell` call per turn, 200 turns on one
  bot, medians of three rotated runs, with the screen itself answering
  `allow`. An ungated bot pays nothing: the same commits and fsyncs as main
  (11 and 6.24 a turn), p50 11.7 against 12.3 ms, and 247 against 245
  turns a second over 32 bots. A call answered within the hold adds no
  durable commit and about 1 ms at the median (13.2 ms, the screen's
  answering round trip included); one answered after its turn parks adds
  three durable commits and about 3 ms (9.32 fsyncs a turn, 15.0 ms). With
  one screen answering 32 bots, the screen becomes the wait: 309 turns a
  second ungated, 258 held, 196 parked. Linux x86_64 container, `7238c6c`
  (before the review fixes and the merge with admission batching) against
  `7e46c5f`, 2026-09-26; both binaries rebuild to the SHA-256 the runs
  recorded. macOS and the automatic approver's own cost are not measured.
  [Record](APPROVALS.md#measure-before-building).
- **Five harnesses, same synthetic work.** 32 agents, three turns each adding
  64 KiB: Agent 22 MiB peak and 0.6 s CPU, Pi 164 MiB and 1.3 s, Codex 244 MiB
  and 24.9 s, opencode 927 MiB and 14.4 s, Claude Code 6,494 MiB and 23.8 s.
  Exploratory, not a ranking: the harnesses do unequal work. Linux VM,
  `8ebbc44`, 2026-09-23, before group commit and before macOS full flushing;
  not rerun since. [Record](HARNESS_MEASUREMENTS.md).
- **Storage size.** A 1,024-turn conversation fell from 79.23 to 45.68 MiB with
  LZ4-compressed artifacts and prompts shared with their user node, node
  payloads unchanged. This reduces growth; retained history is still
  unbounded. macOS arm64, 2026-09-22. [Record](STORAGE_GROWTH.md).
- **Large stores and long histories.** A 9.6 GB store served light turns at
  p95 10.5 ms (2026-09-19). A 100,000-item conversation ran a turn in 3.1 ms
  and forked from its first checkpoint in 42.1 ms at 12.6 MiB RSS, single
  observations (2026-09-15). [Record](DAEMON_MEASUREMENTS.md#store-scale),
  [long history](DAEMON_MEASUREMENTS.md#long-history).
- **Live fleets.** Short-context turns on real providers: 1,024 overlapping
  turns in 12.5 s at 41 MiB, 64 bots for five minutes without drift, and
  10,000 bots through one key at the provider's rate with no failures
  (2026-09-15 and 16). They show a light runtime, not coding-agent capacity.
  [Record](LIVE_FLEET.md).

## Task effectiveness

Whether a model driven by this harness finishes real tasks, at what full cost,
and why it fails. Matched runs on five Terminal-Bench 2.1 tasks under Harbor
0.23.0, both arms of each pair started within a second of each other, with
the same concurrency and timeout and high reasoning for each task agent.
Each arm's served models and fallback policy are recorded as
[COMPARISON_CONTRACT.md](COMPARISON_CONTRACT.md#task-comparisons) requires;
the per-arm tables are in [HARBOR.md](HARBOR.md#matched-runs).

| Run | Arm | Passed | Cost a trial | Cached input | Failures: harness, model, timeout |
| --- | --- | ---: | ---: | ---: | --- |
| ChatGPT plan, gpt-6-sol, 2026-09-25 | Agent `c585c16` | 14/15 | $0.089 | 81.8% | 0, 1, 0 |
| | Codex 0.156.1 | 8/15 | $0.193 | 94.5% | 5, 2, 0 |
| Sonnet 5, 2026-09-25 | Agent `c585c16` | 12/15 | ≥ $1.00 | 95.2% | 0, 1, 2 |
| | Claude Code 2.1.282 | 13/15 | ≥ $0.714 | 95.7% | 0, 1, 1 |
| ChatGPT plan, gpt-6-sol, 2026-09-26 | Agent `15d629c` | 8/10 | $0.113 | 84.3% | 0, 2, 0 |
| | Codex 0.156.1 | 5/10 | $0.198 | 95.4% | 3, 2, 0 |

- **Served models.** Each arm's records show only the model it requested,
  and no fallback call, but only for calls that finished: the three Sonnet
  timeouts each cut off a call that left no record. What that shows differs
  by record: Claude Code's
  name the model the API reported for each response, so its arm ran that
  model; Codex's name the model once per turn, and ours, at these commits,
  name the requested model unless a fallback or summarizer answered, so a
  provider-side reroute in those arms would not show. Builds after `095ff68`
  keep the model each response names
  ([the gap](COMPARISON_CONTRACT.md#task-comparisons)). Our task bots' `--fallbacks` acts only on
  Anthropic requests, so it was live only in the Sonnet run; Claude Code ran
  without `--fallback-model`, and Harbor's Codex adapter has no fallback
  option.
- **Reasoning in the 2026-09-26 run.** Our task bot delegated 28 of the
  arm's 161 calls to four bots that set no reasoning level, so they ran at
  the provider's default; every other call on both sides ran at high. That
  run is matched on reasoning for the task agents only.
- **Codex's losses are mostly its process lifetime.** In 12 of Codex's 17
  failures across these runs and one unmatched run, the model started the
  server the task needs and its own checks passed, but the server was gone
  when the tests ran. That is a harness difference on server tasks, not model
  quality: counting only model errors, the ChatGPT arms are 1 against 2 and
  2 against 2.
- **Sonnet 5.** Claude Code passed one more task at about 29% less recorded
  cost a trial; the difference is on schemelike, where all three timeouts
  fell.
- **Cost is every recorded call.** Harbor's token counts and costs match
  each harness's own records except for two timed-out trials, corrected in
  the table from those records: ours on Sonnet, which Harbor never graded or
  priced, and Claude Code's, which Harbor rebuilt from a partial trajectory.
  A timeout also cuts off the call in flight before the provider reports
  its usage. Our record does not count that call, and Claude Code's
  transcripts, which log finished messages, most likely do not either
  (inferred), so the Sonnet arms, the only ones with timeouts, are lower
  bounds. ChatGPT-plan costs are what the same tokens cost on the API.
- **Cache on the ChatGPT plan.** Misses cost us 4.8k tokens a trial and
  Codex 5.0k, about $0.009 a trial at API rates. Our calls missed at the same
  rate as Codex's from the third call on (9.0% against 10.5%). Every partial
  miss on either side read exactly an earlier call's prompt, and misses
  cluster early in a conversation and after gaps under 10 seconds, not after
  idle time: an older cache copy served by OpenAI's routing, not expiry and
  not a change in our requests. The one pattern that is ours alone is the
  second call of a conversation (8 misses in 14 against 2 in 10), about 0.9k
  tokens a trial. `15d629c`, 5 tasks by 2 trials a side, 2026-09-26.
- **Cache on Sonnet 5.** Ordinary calls lost nothing: no misses, and each
  call's cache writes equal its new tokens. Of nine refreshes during long
  tool calls, six came after a reply that streamed for longer than the
  cache's five minutes, found it expired, and rewrote 400k tokens, about
  $0.92 or 6% of the arm. Claude Code, which does not refresh, lost 80k tokens to
  the same expiry. `c585c16`, 2026-09-25. `e6a2ac8` (#20) now also refreshes
  while a long reply streams; it has not been rerun on these tasks.
- **Cached share is the wrong measure across harnesses.** We send 3.4 to 4.2
  times less input a trial than Codex, so our cached share is lower (82 to
  84% against 94 to 95%) while our uncached input is at parity or lower.
  Tokens lost to misses a trial is the comparable number.
- **Cost structure.** Our fixed prefix is about 1.2k tokens on ChatGPT and
  2.2k on Anthropic, against 11.8k for Codex and 23.7k for Claude Code, which
  is most of our cost edge on short tasks. On Sonnet, output (mostly thinking)
  is about half the bill on both sides.

## Operational behavior

Reconnects, retention, overload, compaction and recovery.

- **Three-minute instrumented soak.** 192 bots with forks, deletion,
  retention, slow readers, compaction, cancellation, replay and crash
  recovery: no unexpected failures, replay mismatches or unfinished turns on
  either build, peak RSS 29–32 MiB. The current build drained 11,314 turns
  against 8,380, but work selection depends on speed, so that is not a
  throughput claim. One run per build. macOS arm64, 2026-09-26.
  [Record](DAEMON_MEASUREMENTS.md#instrumented-operational-follow-up).
- **Hour-long soak.** 224,063 turns with no failures, 1,771 compactions and
  59 forks; RSS flat at about 36.6 MiB; a SIGKILL restart was ready in 0.92 s.
  The store grew 21.7 MiB a minute. `bb6bc51`, 2026-09-20; it predates the
  corrected observer and has not been rerun.
  [Record](DAEMON_MEASUREMENTS.md#mixed-workload-soak).
- **Storage failure: most likely a full disk.** The admission probe that
  ended three turns with `storage_error` ran while the Mac's volume had about
  140–250 MB free, and the system log shows another process's SQLite write
  failing with `ENOSPC` 22 ms after the commit that recorded those failures.
  That cause is an inference: the build kept no SQLite codes. Injected
  `ENOSPC` on Linux reproduced both ways the failure could arise. The daemon
  now survives a full disk: journals stay in memory, and a completion, queued
  turn or parked turn the store refuses is tried again with backoff. A turn
  whose reply cannot be stored still fails. With every write refused for
  0.5 s, all 32 turns ended `failed` and the daemon kept running, where
  before it exited. Linux x86_64 container, `8724f22` against `4260673` and
  `095ff68`, 2026-09-26; no run on a nearly full macOS disk.
  [Record](DAEMON_MEASUREMENTS.md#disk-full-cause-and-containment).
- **Retention.** A race that could lose a completion event under
  `--retain-turns 1` (2 of 10 runs) is fixed (0 of 10). Retention halves
  per-turn store growth (1.4 against 2.8 KB).
  [Race](DAEMON_MEASUREMENTS.md#retention-publication-boundary),
  [growth](LIVE_FLEET.md).
- **Overload.** Paced turns release their slots, so a healthy provider's work
  goes through while another is rate limited. Live, 10,000 paced bots on one
  key all completed with no retries; the same load unpaced completed 1,961.
  [Record](LIVE_FLEET.md).
- **Interrupt and recovery.** At 1,024 bots, streaming turns stop in 193 ms
  and shell turns in 712 ms with their processes gone. Restart is ready in
  25.6 and 29.0 ms on stores of 1 and 9.6 GB, and in 154 ms with 10,000 bots.
  [Record](DAEMON_MEASUREMENTS.md#mass-interrupt).
- **Shutdown.** A graceful drain lets running turns finish for a set time.
  Admissions still waiting on their commit when a `shutdown` request or
  SIGTERM arrives are answered, and their turns end `interrupted` with
  `daemon_shutdown`. Behavior and tests only, not measured. Harbor trials end
  at their timeout within about half a second.
- **Compaction.** Long conversations of many turns are compacted with the
  original history kept. On luna, the evaluation's final file and every
  filler survived, and summaries restated the rule, with summarizer cost
  excluded (2026-09-19). Not built: compaction inside one long turn, which is
  what a long autonomous coding task needs. Not measured: whether summaries
  keep discoveries from tool results, failed approaches and later
  corrections. [Record](LONG_HISTORY.md).
- **Reconnects.** HTTP is the default transport. Live fleets saw transport
  failures (54 turns lost to connection failures in one 256-bot run, clean on
  rerun), retried per [the retry policy](RUST_PROTOTYPE.md). The WebSocket
  transport is a prototype with no live measurement; its
  [plan](WEBSOCKET.md#measurement-plan) includes a synchronized loss of every
  connection.

## Not established

- Any Terminal-Bench score: five tasks are a screen.
- The five-harness screen at the current build.
- Within-turn compaction, and compaction quality on real coding tasks with
  summarizer and retrieval costs included.
- Admission batching on macOS.
- Enqueue-to-answer latency for small control operations: `stats` reports
  it per operation, but no run has recorded it.
- Whether the WebSocket transport pays for itself, and a fleet-wide bound on
  its full-send memory.
- Tool approval on macOS, and at the current build.
- Multi-daemon operation, the automatic approver (rules and Jev), and
  concurrent tool calls: designs only.
