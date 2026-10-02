# Current evidence

Snapshot, 2026-09-26, at `afdd633` plus the change that added this page,
updated at `095ff68` for admission batching and disk-full containment, at
`e707632` and `6a81bd6` for the realistic-budget long-task runs, at
`973be14`, the change that built tool approval, for its cost and a store
lock fix, at `dd95047` for the per-summary choice between a copy and a
request of its own, at `cd1d45f` for the fix to run 5's lost steers, at
`7061fab` and `7c1904d` for the sustained task's runs, at `ea82f7a` for
serving a gate tag to one approver, at `74f726b` and `aed1669` for
the automatic approver's own cost, at `81458d2` for the judges
compared, at `7a66687` for the finish cost of per-bot settings, and at
the change that runs summaries beside the turn, on `0a6f2b2`.
This is the one place that says what is currently
known. The documents it links to
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
- **macOS durability cost, reversed.** A full flush (`F_FULLFSYNC`) costs
  about 5.4 ms, so an idle shell turn took about 30 ms longer than with a
  plain `fsync`; grouping recovers most of it at load (1,280 jobs a second
  in groups of 8, against 184 one at a time). M1 Max, APFS, 2026-09-26.
  [Record](DAEMON_MEASUREMENTS.md#group-commit). On the 32-agent socket echo
  screen the per-commit flush doubled daemon CPU (0.343 to 0.757 s) and
  added 70 ms to turn p95 in the commit that turned it on, and HEAD with
  the flush off returned to 0.364 s and 614 ms, so per-commit flushing is
  off since 2026-09-27. Checkpoints still flush to preserve WAL/database
  write ordering and consistency, but later acknowledged commits can be
  lost after an OS crash or power loss. Daemon-crash survival is retained;
  workspace writes have independent durability. macOS arm64, 2026-09-27.
  [Record](DAEMON_MEASUREMENTS.md#full-flush-bisect).
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
  (11 and 6.24 a turn), p50 13.3 against 13.4 ms, and 266 against 260
  turns a second over 32 bots. A call answered within the hold adds no
  durable commit and about 0.7 ms at the median (14.0 ms, the screen's
  answering round trip included); one answered after its turn parks adds
  three durable commits and about 2.6 ms (9.32 fsyncs a turn, 16.0 ms).
  With one screen answering 32 bots, the screen becomes the wait: 391 turns
  a second ungated (main 381), 292 held, 192 parked. Linux x86_64
  container, `973be14` against main at `ddf3f8b`, with within-turn
  compaction, 2026-09-26; an earlier run at `7238c6c` found the same.
  Serving a tag (`ea82f7a` against `0b295d2`, 2026-09-27, same method and
  host, three rotated runs): held p50 15.9 against 15.7 ms, the same
  commits and event and store bytes, and the plan commit's job 0.61
  against 0.57 ms, inside the run-to-run spread.
  The automatic approver's own cost (`74f726b`, 2026-09-27, same host and
  workload over a Unix socket, three rotated runs, judges that answer at
  once): against a client that answers directly (p50 14.8 ms, 9.8 ms of
  daemon CPU and 12 commits a turn), `agent approver` with a stand-in for
  Jev adds 2.7 ms at the median (17.5 ms), 1.1 ms of daemon CPU for the
  `prompts` read, and 1.1 ms of its own CPU, with no added commit; a
  general model through the daemon adds 11.5 ms (26.3 ms), 8.6 ms of daemon
  CPU, and 11 commits a round for the fork, its turn, and its deletion.
  Rechecked after the review fixes (`aed1669` against `74f726b`, two
  interleaved runs of each, six rounds per arm): Jev adds 2.2 against
  2.3 ms and a general model 8.8 against 9.7 ms over the direct client,
  with the same commits, so the fixes cost nothing measurable.
  Neither includes the judge's own time, measured below.
  macOS is not measured.
  [Record](APPROVALS.md#measure-before-building).
- **Judges compared, benign calls.** George's Mac (macOS 27.0, M1 Max),
  2026-09-27, at `81458d2` (the approver of `aed1669`): 338 rounds holding
  341 labeled calls that should all run, replayed through the daemon and
  `agent approver`, the same rounds, questions, thresholds (0.35, 0.70)
  and state for both judges, two passes in opposite order. Jev allowed
  275 and 276 and denied the rest (59 and 60 as unclear), with a judge
  time of 161 to 170 ms p50 and 305 to 345 ms p99, about 516k input tokens
  and $0.0217 a pass. `gpt-6-luna` at low reasoning on a ChatGPT plan
  allowed 303 and 309, denying the rest as risky, with 2.8 to 3.0 s p50,
  about 12 s p99 and 25.4 s at the worst, against its 30 s deadline; about
  438k input and 33k output tokens a pass. No check failed in either.
  Because every call should run, this says nothing about false allows.
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
- **A group takes the write lock when it begins.** Groups began deferred,
  so a job that read before it wrote asked for SQLite's write lock inside
  the transaction, where SQLite answers `SQLITE_BUSY` at once rather than
  wait; the daemon's reader holds that lock for a moment when it catches
  the WAL header mid-update. With 32 bots at 25 turns under strace, 6 of
  11 runs failed a turn with `storage_error`, on main at `3bfb0b0` and on
  the approval branch alike; beginning each group `IMMEDIATE`, none of 9
  did. The untraced runs never hit it. Linux x86_64 container, `a72a2ec`,
  2026-09-26; the store test
  `a_group_waits_for_a_write_lock_another_connection_holds` reproduces the
  error without strace.
- **Retention.** A race that could lose a completion event under
  `--retain-turns 1` (2 of 10 runs) is fixed (0 of 10). Retention halves
  per-turn store growth (1.4 against 2.8 KB). Reading each bot's own
  retention and context settings inside its finishing commit, instead of
  daemon flags, left 32 turns finishing together unchanged within noise:
  median p99 12.24 ms at `7a66687` against 12.21 ms at `2df257c`, ranges
  11.1–19.1 and 11.0–22.0 ms, 15 runs a build, bots on the default
  settings. Linux x86_64 container, 2026-09-27.
  [Race](DAEMON_MEASUREMENTS.md#retention-publication-boundary),
  [growth](LIVE_FLEET.md),
  [finish cost](DAEMON_MEASUREMENTS.md#per-bot-settings-finish-cost).
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
  excluded (2026-09-19). [Record](LONG_HISTORY.md).
- **Within-turn compaction.** One synthetic repository task, three bots
  per condition, on the ChatGPT plan's `gpt-6-sol`, macOS arm64,
  2026-09-26. With a 20 KiB budget forcing two or three summaries and two stub
  passes per bot, 3 of 3 finished correctly and kept every fact that lived
  only in tool results and the steered correction, as did 3 of 3 with full
  context. Summary requests, sent as copies of the bot's last call, served
  34% of their input from cache (0% when built fresh). Counting the
  summarizer, compacting sent twice the uncached input of full context per
  correct task (25,637 against 12,669 tokens) on a task this short.
  `51d8744`. [Record](LONG_TASK_EVAL.md#live-run-2). On a larger version of
  the task at a 256 KiB budget, five bots per arm, 20 of 20 finished
  correctly, but only 2 of the 15 bots at 256 KiB reached the compaction
  trigger: the model kept its context to 25,127 to 82,585 tokens.
  `e707632`, 2026-09-26. [Record](LONG_TASK_EVAL.md#live-run-3). At
  128 KiB, ten bots per arm, 40 of 40 finished correctly; compacting cost
  more than full context on this task (79,273 input token-equivalents per
  bot with stubs against 73,503, counting cached input at a tenth), since
  full context never passed 38,256 tokens. `6a81bd6`, 2026-09-26.
  [Record](LONG_TASK_EVAL.md#live-run-4). With each summary choosing
  between copying the bot's call and a request of its own by estimated
  cost, ten bots per arm at 128 KiB: summaries cost 0.093 input
  token-equivalents per byte summarized, against 0.330 always copying the
  whole call and 0.337 never copying, and 9 of 10 bots finished correctly
  in both the choice arm and the arm with no copy. The two misses were
  a steer left waiting for room, which a scripted task reproduces on the
  build before the choice. `dd95047` against `d9ecbcf`, 2026-09-26.
  [Record](LONG_TASK_EVAL.md#live-run-5). With such a steer admitted
  against the whole budget, 20 bots a side: 20 of 20 steers went in and
  20 of 20 bots were correct, against 18 of 20 on main, whose two misses
  were that steer. `cd1d45f` against `0b295d2`, 2026-09-27; later
  commits only narrow that admission and send such a steer to the model
  before any summary, which has not run live.
  [Record](LONG_TASK_EVAL.md#live-run-7). On a sustained task that settles
  six closes in one turn, about 508 KB of required output, 40 bots in
  four arms at once: none compacted, since the model sent long outputs to
  files and read their tails (peak 29,638 tokens, 11% of a 272k window),
  and 25 of 40 finished before the correction was sent, which alone
  decided who was correct. `7061fab`, 2026-09-27.
  [Record](LONG_TASK_EVAL.md#live-run-8). With each step read whole and
  the correction sent after two successful settlements, 38 of 40 were
  correct, and compacting took 39 to 45% less input per correct task than
  full context (416k input token-equivalents with stubs at 128 KiB, 438k
  with summaries only, 457k with stubs at 256 KiB, against 754k). Bots
  with stubs matched full context's median time (p50 347 against 374 s)
  but not its tail (max 530 against 383 s); bots with summaries only
  took 541 s at the median, and their summarizer wrote 113k
  output tokens that the token-equivalents leave out. `7c1904d`,
  2026-09-27. [Record](LONG_TASK_EVAL.md#live-run-9).
- **Summaries beside the turn.** A summary due inside a turn runs beside
  its calls and tools and is installed at a later boundary. On the
  synthetic fixture (work calls 1 s, summaries 4 s, 150 rounds, 64 KiB),
  the turn took 152.5 s against 168.3 s before, all four summaries hidden,
  for 12% more work-call input bytes; at 24 KiB, where each round fills
  the room left, it saved about one round per summary (8.96 against
  9.76 s). Linux cloud container, 2026-10-02, against `0a6f2b2`.
  [Record](DAEMON_MEASUREMENTS.md#summaries-beside-the-turn).
- **Reconnects.** HTTP is the default transport. Live fleets saw transport
  failures (54 turns lost to connection failures in one 256-bot run, clean on
  rerun), retried per [the retry policy](RUST_PROTOTYPE.md). The WebSocket
  transport is a prototype with no live measurement; its
  [plan](WEBSOCKET.md#measurement-plan) includes a synchronized loss of every
  connection.

Approval consolidation validation (2026-09-27, based on `3d725b4`):
308 Rust runtime/client tests passed; the full release-binary Python suite
ran 363 tests with 23 optional tests skipped. Clippy with warnings denied,
formatting, and diff checks passed. Lease regressions cover duplicate
registration, publication preservation, cancellation with simultaneously
ready output, storage waits, failed listing cleanup, and reply ordering.
The bounded macOS comparison is recorded in APPROVALS.md; it is a small
non-regression screen, not an efficiency ranking.

Catalog consolidation validation (2026-09-27, based on `f15854f`, with
explicit output-modality handling from the approach in `985179c`): all
318 Rust runtime/client tests and 38 release-binary CLI integration tests
passed, along with Clippy, formatting, and diff checks. A synthetic
provider with only an invalid model ID reproduced successful discovery
of a comments-only file before the renderer became fallible; it now
returns `models_none_listed` and leaves no file. Provider tests cover the
shared listing budget, refused credentials, and the bounded shared AWS
startup lookup. These are synthetic checks, not fresh paid-provider
validation or a catalog performance comparison.

The later `e0f5067` approval follow-up was checked against this
consolidation. Its reply-boundary refinement is retained without
reintroducing lease replacement: a valid answer holds its lease through
reply queueing, and only its owning session can release that hold. The
62 server tests and 24 release-binary approval integration tests passed;
the 10 focused hub tests, Clippy, formatting, and diff checks also passed.

## Not established

- Any Terminal-Bench score: five tasks are a screen.
- The five-harness screen at the current build.
- Compaction quality and cost on real coding tasks, beyond two synthetic
  ones.
- Whether choosing between a copy of the bot's call and a request of its
  own saves anything beyond one task, one model and one budget: run 5's
  0.093 against 0.337 token-equivalents per byte summarized is 11
  summaries a side, with the backend's cache reading 9 of the 10 copies
  (`dd95047`, macOS arm64). Its estimate does not model a cache miss.
- Whether compacting pays beyond one synthetic task and one model: it
  did on the sustained task read whole (run 9), but full context cost
  less on the shorter task (runs 4 and 5), and when the prompt let it,
  the model kept its own context under the budget (run 8). Neither
  prices the summarizer's output.
- Whether summaries beside the turn save time on a live model, and what
  the larger requests sent meanwhile cost there: the screen has synthetic
  delays and no prompt cache.
- Admission batching on macOS.
- Enqueue-to-answer latency for small control operations: `stats` reports
  it per operation, but no run has recorded it.
- Whether the WebSocket transport pays for itself, and a fleet-wide bound on
  its full-send memory.
- Tool approval at fleet scale on macOS.
- How often either judge allows a risky call: the labeled set holds only
  calls that should run, so it measures false denials, not false allows.
- Multi-daemon operation and concurrent tool calls: designs only.
