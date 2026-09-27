# Long-task evaluation

Does one long task survive several compactions inside its own turn, keep
what it learned, and finish correctly? This page has two parts: acceptance
cases that combine compaction with the runtime's other guarantees, run
against scripted providers, and an evaluation of a real model on one
synthetic repository task (roadmap [item 36](NEXT.md)). Written 2026-09-26.
The acceptance cases pass. The evaluation has run twice live on the small
task, recorded [below](#live-run-1), and twice on the large task at
realistic budgets ([live runs 3 and 4](#live-run-3)).

## Acceptance cases

Each case names its test and states the outcome that must hold. All run the
release daemon against the scripted providers in `tests/test_runtime.py`
(`AGENT_TEST_RUNTIME=1`). The scripted long task makes one shell call a
round, and each call appends its round number to `rounds.log` in the
workspace, so a replayed side effect shows as an extra line.

1. **Compact, reconnect and replay, then fork from inside the split turn.**
   `tests/test_turn_acceptance.py`,
   `test_compaction_then_reconnect_and_replay_then_a_historical_fork`. A
   40-round turn in a 24 KiB budget compacts while a client follows it over
   the socket. The client disconnects, the turn compacts again with nobody
   attached, and the client reconnects from the last cursor it saw.
   Expected:
   - the events the client received before and after reconnecting are every
     stored event exactly once, in order, the missed compaction included,
     ending in one `turn_finished` with status `completed`;
   - every round ran once: `tool_completed` for `long-0` to `long-39` in
     order, and `rounds.log` holds 0 to 39;
   - every `compacted` event is pinned to the turn's prompt and covers turn 1
     only.

   Then a fork at a checkpoint two rounds after the second cut, into another
   workspace. Expected:
   - the fork binds the second compaction's version;
   - its first request carries that summary (`covering the start of turn
     1`), the turn's prompt whole, and the rounds from the cut to the
     checkpoint, every call paired with its result, and nothing after the
     checkpoint;
   - creating and running the fork replays no tool call: its only
     `tool_started` is its own new call, and neither workspace gains a
     `rounds.log` line;
   - the source bot's events are unchanged.
2. **Restart after a cut inside the turn.**
   `test_a_restart_after_a_cut_inside_the_turn_neither_reruns_nor_forgets`.
   The daemon is killed mid-turn after at least one cut. Expected:
   - on resume the bot is `interrupted`, with no model request and no tool
     rerun, and its compaction is the last recorded version;
   - completed calls are `long-0` onward in order; a call the kill cut short
     ran at most once and is not run again;
   - the next turn's request starts with the stored summary, carries the
     interrupted turn's prompt whole, and pairs every call with its result.
3. **A round that overflows before compaction is due.**
   `tests/test_turn_compaction.py`,
   `test_a_round_that_overflows_before_compaction_is_due_forces_a_summary`.
   Four small rounds, then one large one, with `--compact-at 99` and no
   `read` tool, so nothing can be elided. Expected: exactly one summary,
   made after the overflowing round; the next request carries the summary,
   the prompt whole, and only the newest exchange; the turn completes and
   no request exceeds the budget. The same turn on a bot without summarizer
   instructions fails with `context_limit`. Before this change the first
   bot failed too.
4. **Catch-up through a turn larger than the budget.** Store contract tests
   `catch_up_through_a_turn_larger_than_the_budget_cuts_at_its_rounds` and
   `catch_up_cuts_a_finished_turn_larger_than_the_budget_at_its_rounds`.
   Expected: each step ends at a completed exchange, keeps that turn's
   prompt pinned, and merges the previous summary; the walk's piece size
   does not change a step; once the rest fits, an ordinary summary follows
   (at the newest round in a running turn, at the newest prompt after a
   finished one); the transcript keeps every item.
5. **The existing single-feature cases.** One turn crossing several cuts
   (`test_one_long_turn_crosses_several_compactions_and_finishes`), thinking
   kept bound across cuts on the Anthropic family with prefix enforcement
   (`test_thinking_stays_bound_across_cuts_inside_the_turn`), and the
   elision cases in `tests/test_elision.py`.
6. **A steer the turn has no room for.** `tests/test_elision.py`,
   `test_a_steer_the_turn_had_no_room_for_goes_in_once_elision_makes_some`.
   A strict steer arrives while the turn holds two whole results of about
   11 KiB in a 24 KiB budget, more than the three quarters a steer may join.
   Expected: it goes in at the next boundary, after elision stubs the older
   result and before that boundary's model call, following a result the
   model has not answered yet; its turn ends `steered` into the task. Before
   the fix it stayed queued and failed with `stale_turn` when the task
   ended, which is what the live run hit. In
   `test_a_steer_goes_in_at_the_whole_budget_when_the_newest_result_fills_the_turn`
   one result of about 15 KiB, the newest round, leaves no room within
   three quarters and nothing a stub or summary can take. Expected: the
   steer goes in against the whole budget, since the bot has a
   summarizer, and the task finishes with it; before that fix it failed
   with `stale_turn`, which is what run 5 hit.
7. **The evaluation runner itself.** `tests/test_long_task_eval.py`: the
   scorer reads each fact from a workspace where it was kept and where it
   was lost, and the runner drives a scripted agent through the task below
   to a correct finish, sending the correction as a steer, crossing a
   compaction, and counting the summarizer's calls apart from the model's.

## The task

`bench/long_task_eval.py` builds one small repository per bot and submits
one prompt: implement `convert(rows)` in `ledger/convert.py`, run
`tools/env-check` first, create the account map with `tools/migrate`, pass
`make check`, then run `make bench` and report the throughput. Each fact
the model needs later lives only in a tool result, as Astra Pro's review
asked:

| Fact | Where it appears | What losing it looks like |
| --- | --- | --- |
| A restriction: `vendor/` is checksummed and must not change | line 17 of 30 in `tools/env-check`'s output | `vendor/money.py` edited; `make check` fails its checksum |
| A failed approach: `make quick` was removed | `make quick`'s error, which the prompt still names | `make quick` run again, counted in `.quick-attempts` |
| An operation with an unknown outcome | `tools/migrate` applies, then reports the outcome unknown and says to check `--status` | a second run, which corrupts the account map |
| A measured number | `make bench`'s last line; only `tools/.seed` holds it | the final answer lacks it |
| A later correction supersedes the prompt | a steer after the sixth completed tool call: round half to even, not truncate | hidden tests fail on half-cent amounts |

The visible tests pass with truncation. The hidden cases include
half-cent amounts where truncation and half to even differ, and map
accounts through the migration's output, so a doubled migration fails them
too. The seed sets each bot's throughput, so trials differ in the number to
report but not in the task.

### The large task

The small task's outputs are a few KiB, so only a 20 KiB budget makes it
compact. The large task is the same repository with the outputs a real
one prints, so it outgrows a realistic budget without padding: the same
facts, steer, hidden tests, and number to report for a given seed.

- `tools/env-check` runs 820 probes and states the policy after probe 477.
- `tools/migrate` lists each of the 940 accounts it renames before the
  unknown outcome.
- `make check` also runs 560 fixture batches from past closes. Every
  fixture amount has whole cents, so the suite passes under either
  rounding rule, and none reaches 20,000 cents, so no file but the
  benchmark's seed holds the number to report.
- `make bench` prints 860 warmup lines, and the README carries a close
  history.

Each output that carries a fact stays under the shell tool's 64 KiB
preview, so no fact falls in an omitted middle. A failing `make check`
prints about 490 KB and is cut to its head and tail, as a real suite's
would be. Measured on the seed-7 workspace, the required steps print
env-check 54,734 bytes, migrate 55,601, a failing check 64 KiB as shown,
a passing check 60,954 before and again after the correction, and the
benchmark 59,380: about 357 KB before any read the model chooses, against
a 256 KiB budget, about 64k tokens.

## Conditions and scores

Each condition runs from fresh starts in its own daemon, with `--trials`
bots at once. All use the CLI's default preamble and compaction
instructions, as the context evaluation does.

| Condition | Task | Budget | Tools | What makes room |
| --- | --- | --- | --- | --- |
| `compact` | small | 20 KiB | `shell, read, write, edit, history` | stubs, then summaries |
| `full` | small | 4 MiB | the same | nothing needed |
| `large-compact` | large | 256 KiB | the same | stubs |
| `large-summary` | large | 256 KiB | the same without `read` | summaries only |
| `large-full` | large | 4 MiB | the same as `large-compact` | nothing needed |

At 256 KiB, stubbing everything the model has answered below the 64 KiB
verbatim tail takes the large task's view far below the 75% trigger, so
`large-compact` runs no summary: the scripted run in
`tests/test_long_task_eval.py` elides once and summarizes never. A bot
without `read` stores no stubs, so in `large-summary` summaries make all
the room (twice in the scripted run). That is the condition the summary
requests are measured on, against the build before them and one that sends
only requests of their own; see [running it](#running-it).

Scores come from the workspace and the event log, not the model's account
of itself: hidden tests passed, every file under `vendor/` unchanged with
none added or removed, `make quick` runs in total and after the first
compaction, migrations applied, whether the correction reached the task
(its steer turn's final status, not the submit reply), whether the answer
carries the measured number and the benchmark ran, whether the bot ran
`tools/env-check` before changing anything, a passing `make check` on its
final files, and the benchmark after that check, commands repeated after
the first compaction, retrieval calls (`history`, or `read` of a `result/` reference),
compactions, elisions, the view each model call was made under (its
summary version and cut, and its elision floor), input, cached input, and output tokens for the model and the
summarizer separately, summarizer latency from its send to the send of
the model call it held back, and for each installed summary whether it was
a catch-up step, the bytes it summarized and the view they came from, how
it was sent (a copy of the bot's call, with the window items copied, or a
request of its own) with the runtime's estimate of each way, and its
summarizer calls and tokens. Failed summaries are live-only events, so the
runner collects them as they arrive. The workspace's own tools record
each run of `tools/env-check`, `make check`, and the benchmark with its
exit status and a digest of the workspace's files, dotfiles and bytecode
caches aside, so the order is scored from what ran and how it ended, not
from command text: a command that only names a step, or a step that
fails, does not count. The hidden tests import the code the
model wrote, so they run in a child process that keeps only `PATH`,
`TMPDIR`, and the locale from the runner's environment, and no credentials.

## Running it

Real model, real spend. Per the project's rule, run it on the ChatGPT plan
with Codex's login, never on a paid Anthropic key without asking first:

```sh
.local/venv/bin/python -m bench.long_task_eval --model chatgpt/MODEL \
    --trials 3 --out .local/long-task-eval/MODEL.json
```

`MODEL` is the id Codex's `/model` picker shows. Workspaces and the store go
under `.local/long-task-eval/run/`, which git ignores. Each condition
prints a one-line summary; the JSON file keeps every bot's scores and
answer.

The realistic-budget comparison runs its arms at once, so every arm sees
the backend at the same time. The runtime has one behavior, so an arm with
another way of sending summaries is a measurement build, never committed:
the commit before the per-summary choice (`d9ecbcf`, which copies the
bot's call whenever it fits), and the same commit as the other arms with
the copy turned off in `compact()`, which sends every summary as a request
of its own.

```sh
git worktree add .local/copy-off HEAD
perl -0pi -e 's/sent\.filter\(\|sent\| sent\.model == reference\)/sent.filter(|_| false)/' \
    .local/copy-off/src/server/turn.rs
git -C .local/copy-off diff --stat    # 1 file changed, 1 insertion(+), 1 deletion(-)
cargo build --release --bin agent --manifest-path .local/copy-off/Cargo.toml \
    --target-dir .local/copy-off/target
git worktree add .local/always-copy d9ecbcf
cargo build --release --bin agent --manifest-path .local/always-copy/Cargo.toml \
    --target-dir .local/always-copy/target

run() { .local/venv/bin/python -m bench.long_task_eval --model chatgpt/MODEL --trials 10 \
    --context-bytes 131072 "$@"; }
run --conditions large-compact --out .local/long-task-eval/large-compact.json &
run --conditions large-full --context-bytes 4194304 --out .local/long-task-eval/large-full.json &
run --conditions large-summary --out .local/long-task-eval/large-summary-choice.json &
run --conditions large-summary --binary .local/always-copy/target/release/agent \
    --out .local/long-task-eval/large-summary-copy.json &
run --conditions large-summary --binary .local/copy-off/target/release/agent \
    --out .local/long-task-eval/large-summary-own.json &
wait
```

Each JSON file records its binary's digest, which tells the `large-summary`
arms apart. `--context-bytes N` gives every condition the
run names the budget N instead of its own, for example to run the
`large-` arms at 128 KiB beside a `large-full` run without it.

## Live run 1

2026-09-26, commit `b9541c7`, `chatgpt/gpt-6-sol` on the ChatGPT plan with
Codex's login, seed 7, three bots per condition. No API key was used. The
numbers below are from the run's JSON, kept in the ignored `.local/`
directory of the machine that ran it.

| | compact (20 KiB) | full (4 MiB) |
| --- | --- | --- |
| Completed / correct | 3/3 / 2/3 | 3/3 / 3/3 |
| Vendor intact, migrated once, number reported | 3/3 each | 3/3 each |
| `make quick` runs | 0, 0, 0 | 0, 0, 0 |
| Compactions / elisions per bot | 4/3, 4/3, 3/3 | none |
| Retrieval calls per bot | 0, 1, 1 | 0, 0, 0 |
| Model input / cached / output tokens | 103,365 / 28,928 / 3,444 | 156,819 / 117,120 / 3,443 |
| Summarizer input / cached / output tokens | 17,606 / 0 / 7,185 | none |
| Model input served from cache | 28% (12 of 38 calls read any) | 75% |
| Summary time holding the model back | 147.8 s over 11 summaries | none |
| Condition wall time | 121.6 s | 48.1 s |

Every bot kept all four facts from tool results, and compact-0 and
compact-1 kept the steered correction across two or three later cuts. The
one wrong answer, compact-2, never received the correction. It was queued
after the sixth tool call, one compaction into the turn. At that boundary
the turn held 15,833 bytes against the 15,360 a steer may join, measured
before that round's elision, so the steer stayed queued. Nothing retried
it once elision and two cuts made room. At the turn's end the strict steer
failed with `stale_turn`, and the bot truncated as the prompt said
(hidden tests 5/9). That is fixed: acceptance case 6.

Two scorer faults were found and fixed. `steer` recorded the submit reply,
so compact-2 read "steered". A `migrate_calls` count matched command text,
counting reads of the script and missing runs chained with `--status`.
`migrations_applied`, from the workspace, was right: no bot applied the
migration twice.

Compaction's cost in this run:
- Uncached model input was 74,437 tokens against 39,699, plus 17,606
  summarizer input tokens, none cached.
- Summary requests are built fresh, with the compaction instructions, no
  tools, and their own cache key, so nothing they send starts with a prefix
  the provider holds.
- Each cut and each stub pass rewrites the view near its front. Stub passes
  fell on other rounds than cuts, so their misses stacked; one bot made
  three uncached calls in a row.
- After a cut only the instructions and tools stay ahead of the summary,
  which here is under OpenAI's 1,024-token minimum cacheable prefix.
- The 20 KiB budget forces a cut about every three calls, far more often
  than a real budget would, so this overstates the cost per task.

Claude Code and Codex send the summary request as a copy of the call just
made, with the compaction instruction appended, so most of it reads from
cache. Pi builds a fresh summary prompt, as this runtime does. Sources,
read 2026-09-26:

- Claude Code: Thariq Shihipar, [Lessons from building Claude Code: Prompt
  caching is everything](https://claude.com/blog/lessons-from-building-claude-code-prompt-caching-is-everything),
  published 2026-04-30. Compaction uses "the exact same system prompt,
  user context, system context, and tool definitions as the parent
  conversation", then the parent's messages, then the compaction prompt as
  a new user message.
- Codex: `codex-rs/core/src/compact_remote_v2_attempt.rs` on `openai/codex`
  main at `b334d5b` (2026-09-26T15:48:27Z); the newest commit touching the
  file is `20f4d12` (2026-09-16). It builds the input from the turn's
  history, appends a `CompactionTrigger` item, and sends it with the
  turn's base instructions and model-visible tools through the turn's
  client session; the reply is one encrypted compaction item, and the
  trigger is popped afterwards. It sets a reasoning effort of its own for
  compaction.
- Pi: npm `@mariozechner/pi-coding-agent` 0.73.1, published 2026-05-07,
  the newest under that name. `generateSummary` in
  `dist/core/compaction/compaction.js` sends its own system prompt and one
  user message holding the conversation serialized as text, with no tools.

George chose the copy on 2026-09-26, and summary requests on the bot's own
model are now built that way: the bot's last call as it was sent, the
items since, then the compaction request carrying the client's
instructions ([details](RUST_PROTOTYPE.md#compaction)). A separate
summarizer model, and a catch-up step over history larger than the budget,
keep the request of their own. This run predates it;
[live run 2](#live-run-2) measures it.

## Live run 2

2026-09-26, commit `51d8744`: summary requests on the bot's own model are
copies of its last call, and the steer fix of acceptance case 6 is in.
Same model, plan, seed and three bots per condition as run 1, with no API
key. The JSON is kept in the same ignored directory.

| | compact (20 KiB) | full (4 MiB) |
| --- | --- | --- |
| Completed / correct | 3/3 / 3/3 | 3/3 / 3/3 |
| Steered, vendor intact, migrated once, number reported | 3/3 each | 3/3 each |
| `make quick` runs | 0, 0, 0 | 0, 0, 0 |
| Compactions / elisions per bot | 3/2, 3/2, 2/2 | none |
| Retrieval calls per bot | 0, 0, 0 | 0, 0, 0 |
| Model calls | 32 | 34 |
| Model input / cached / output tokens | 83,831 / 28,032 / 3,008 | 160,247 / 122,240 / 3,360 |
| Summarizer input / cached / output tokens | 32,119 / 11,008 / 5,444 | none |
| Model input served from cache | 33% | 76% |
| Summarizer input served from cache | 34% (5 of 8 summaries read any) | none |
| Summary time holding the model back | 117.5 s over 8 summaries | none |
| Condition wall time | 89.0 s | 52.2 s |

Every bot passed the hidden tests (9/9), kept the four facts and the
steered correction, and repeated no command after its first compaction.
Against run 1's compact condition:

- Summaries read the bot's cache: 11,008 of 32,119 input tokens, against
  none. The five that read any reused 1,664 to 2,432 tokens each; one read
  2,304 of the 2,412 tokens its preceding call sent. The calls just before
  the eight summaries sent about 22,300 tokens between them, so summaries
  read about half of what they could. Three read nothing, and ordinary
  calls missed the same way: 4 of the 15 calls whose prefix had not
  changed read nothing. That these misses are the provider's is inferred.
- Each summary request is larger, since it carries the bot's instructions,
  tools and every item since its last call: 4,015 input tokens on average
  against 1,601. Uncached summarizer input rose from 17,606 to 21,111
  tokens over three fewer summaries, and a summary took about 14.7 s
  against 13.4 s. Summarizer output fell from 7,185 to 5,444 tokens.
- Uncached input, model and summarizer together, fell from 92,043 to
  76,910 tokens and summary time from 147.8 to 117.5 s, because there were
  fewer compactions (8 against 11) and model calls (32 against 38). With
  three bots per condition, that drop and the drop in retrievals cannot be
  attributed to the copy.
- The first call after every summary read no cache (8 of 8). Only the
  instructions and tools stay ahead of the summary, about 1,023 tokens
  here, one under OpenAI's minimum; a realistic preamble would cache that
  part. Claude Code and Codex also replace the history with the summary,
  so their next call can reuse no more than that either. Stub passes
  still rewrite the view near its front on rounds of their own.

Per correct task, compact sent 25,637 uncached input tokens (model and
summarizer) against full's 12,669. The task is short enough that full
context never grows large, so here compacting costs more than it saves;
the 20 KiB budget exists to force boundaries, not to save tokens.

## Live run 3

2026-09-26, 19:35 to 19:39 UTC, commit `e707632`, `chatgpt/gpt-6-sol` on
the ChatGPT plan with Codex's login, seed 7, the large task, five bots per
arm, all four arms at once, each in its own daemon on macOS arm64. The fresh
arm ran the copy-off build (binary digest `de6b3569…`), the other three the
commit's own (`ed2e4ece…`). No API key was used. The JSON files are kept in
the ignored `.local/` directory of the machine that ran them.

| | large-compact | large-summary, copy | large-summary, fresh | large-full |
| --- | --- | --- | --- | --- |
| Budget | 256 KiB | 256 KiB | 256 KiB | 4 MiB |
| Completed, correct, steered, vendor intact, migrated once | 5/5 each | 5/5 each | 5/5 each | 5/5 each |
| Number reported | 4/5 | 5/5 | 5/5 | 5/5 |
| `make quick` runs | 0, 0, 1, 0, 0 | none | 0, 0, 1, 1, 0 | 0, 0, 0, 1, 0 |
| Bots that compacted / stub passes | 0 / 0 | 1 / 0 | 1 / 0 | 0 / 0 |
| Peak input tokens per bot | 25,394 to 37,289 | 25,127 to 55,574 | 37,026 to 63,207 | 25,271 to 82,585 |
| Model calls | 69 | 69 | 68 | 71 |
| Model input / cached / output tokens | 1,800,945 / 1,610,624 / 8,090 | 2,063,297 / 1,799,808 / 7,041 | 2,485,180 / 2,189,824 / 7,006 | 2,478,429 / 2,220,544 / 14,574 |
| Summarizer input / cached / output tokens | none | 73,668 / 54,400 / 729 | 54,079 / 0 / 571 | none |
| Summary time holding the model back | none | 16.5 s | 12.4 s | none |
| Condition wall time | 90.2 s | 93.9 s | 106.5 s | 218.6 s |

- The task rarely reached the budget. Its required steps print about
  357 KB, but no bot's context passed 82,585 tokens, and 13 of the 15 bots
  at 256 KiB never reached the 75% trigger, so neither stubs nor summaries
  ran. The peaks imply that the model read the long outputs filtered or in
  part rather than whole; the runner does not collect commands, so that is
  inferred.
- Without a compaction, `large-compact` and `large-full` ran the same
  path, and their totals still differ: full context sent 38% more input,
  mostly from one bot that peaked at 82,585 tokens. Five bots per arm do
  not separate differences smaller than that.
- One summary per summary arm. The copy sent 73,668 input tokens and read
  54,400 (74%) from cache; the fresh request sent 54,079 and read none. The
  copy is the larger request because it carries the bot's instructions,
  tools, and whole view, including the result that crossed the trigger.
  Counting cached input at a tenth of the price, the copy cost 24,708
  token-equivalents against 54,079, 54% less. It took 16.5 s against
  12.4 s and wrote 729 output tokens against 571. With one summary each,
  this is a direction, not a result.
- The one answer without the benchmark's number came from a bot that ran
  the benchmark after its passing check and never compacted, so no
  compaction lost it.

The copy needs more summaries than this task makes at 256 KiB, so
[live run 4](#live-run-4) repeats the three 256 KiB arms at 128 KiB.

## Live run 4

2026-09-26, 19:47 to 19:50 UTC, commit `6a81bd6`, which changes only the
eval and docs since run 3, so the same two binaries ran (`ed2e4ece…`, and
`de6b3569…` for the fresh arm). Same model, plan, seed and host as run 3;
ten bots per arm, all four arms at once. The three `large-` arms of run 3
ran at 128 KiB (`--context-bytes 131072`), `large-full` at its own 4 MiB.

| | large-compact | large-summary, copy | large-summary, fresh | large-full |
| --- | --- | --- | --- | --- |
| Budget | 128 KiB | 128 KiB | 128 KiB | 4 MiB |
| Completed, correct, steered, vendor intact, migrated once | 10/10 each | 10/10 each | 10/10 each | 10/10 each |
| Number reported | 10/10 | 10/10 | 9/10 | 10/10 |
| Bots that ran `make quick` (none twice) | 3 | 0 | 1 | 2 |
| Bots that stubbed / summarized | 7 / 1 | 0 / 5 | 0 / 3 | 0 / 0 |
| Summaries | 1 | 10 | 5 | none |
| Peak input tokens per bot | 21,382 to 34,927 | 21,246 to 34,646 | 21,580 to 34,510 | 24,878 to 38,256 |
| Model calls | 135 | 142 | 139 | 142 |
| Model input / cached / output tokens | 2,483,189 / 1,917,056 / 13,631 | 3,080,929 / 2,571,264 / 15,495 | 2,931,991 / 2,520,576 / 14,724 | 3,891,280 / 3,506,944 / 15,690 |
| Model input served from cache | 77% | 83% | 86% | 90% |
| Summarizer input / cached / output tokens | 34,891 / 0 / 736 | 319,029 / 99,456 / 7,167 | 110,854 / 0 / 3,343 | none |
| Input token-equivalents per bot, cached at a tenth | 79,273 | 99,631 | 77,433 | 73,503 |
| Summary time holding the model back | 47.7 s | 323.6 s | 265.7 s | none |
| Condition wall time | 147.1 s | 196.4 s | 174.7 s | 148.9 s |

Summaries that could be copies, being no catch-up step and taken from a
view within the input limit, one row each (token-equivalents count cached
input at a tenth):

| Arm | Span bytes | View bytes | Input / cached tokens | Token-equivalents |
| --- | --- | --- | --- | --- |
| copy | 63,317 | 100,650 | 34,742 / 21,504 | 15,388 |
| copy | 63,337 | 102,952 | 35,168 / 21,504 | 15,814 |
| copy | 62,066 | 111,740 | 37,752 / 21,120 | 18,744 |
| copy | 62,414 | 130,154 | 46,945 / 20,736 | 28,283 |
| copy | 37,324 | 101,349 | 33,775 / 14,592 | 20,642 |
| fresh | 63,279 | 101,153 | 21,032 / 0 | 21,032 |
| fresh | 61,853 | 99,706 | 21,034 / 0 | 21,034 |
| fresh | 37,845 | 101,297 | 13,965 / 0 | 13,965 |

- Every bot finished correctly with the correction, 40 of 40; none ran
  `make quick` twice or applied the migration twice. The one answer
  without the benchmark's number came from a bot whose only summary came
  three calls into the task, before the benchmark ran, so no compaction
  removed it (inferred from its views).
- Per byte summarized, the copy and the fresh request cost the same at
  this budget: 0.343 and 0.344 token-equivalents (98,871 over 288,458
  bytes, against 56,031 over 162,977). A copy pays full price for what is
  new since the bot's last call, here mostly the result that crossed the
  trigger, and a tenth for the rest; a fresh request pays for the span.
  So the copies were cheaper for spans of about 62 KB taken from views of
  about 100 KB, and dearer for a 37 KB span and for a copy of a view at the
  limit. Copies read 53% of their input from cache.
- Half the copy arm's summaries could not be copies. In two bots one
  result took the view to about 168 KB, past the budget, and the two
  catch-up steps that followed each sent a request of its own; one more
  summary's view (134,849 bytes) was over the limit for a copy. The fresh
  arm met the same overflow in one bot. Claude Code and Codex trim the copy
  instead in that case (Claude Code 2.1.283's bundle and `openai/codex`
  main at `7f6c0f9`, read 2026-09-26).
- The default-tools arm's one summary was a copy of a 111 KB view for a
  5,837-byte span and read no cache: 34,891 tokens, where a request of its
  own would have sent a few thousand (inferred from the fresh rows).
- Compacting did not pay on this task. Full context peaked at 24,878 to
  38,256 tokens, and cost 73,503 token-equivalents per bot, against 79,273
  with stubs, which cut the model's cache hits from 90% to 77%. The two
  summary arms' totals differ mostly in how many bots crossed the trigger
  (5 against 3), not in the copy.
- Summaries held the model back 32 s each on average in the copy arm, 53 s
  in the fresh arm and 47.7 s for the one in `large-compact`, against 12
  to 17 s in run 3. Forty bots ran at once here against twenty; that this
  is the cause is inferred.

Across budgets, per summary against a fresh request, the copy cost 73%
more at 20 KiB (runs 1 and 2, different commits), the same per byte at
128 KiB, and 54% less at 256 KiB (one summary each). It pays when the span
is large beside what is new since the last call.

## Live run 5

2026-09-26, 22:03 to 22:08 UTC, the per-summary choice at commit
`dd95047` (binary `8a88f4b0…`, also used for `large-compact` and
`large-full`), against the commit before it, which copies whenever the
copy fits (`d9ecbcf`, `663831db…`), and `dd95047` with the copy turned off
(`eecdf4a3…`). Same model, plan, seed and host as runs 3 and 4; ten bots
per arm, all five arms at once, at 128 KiB except `large-full`.

| | large-compact | summary, choice | summary, always copy | summary, own | large-full |
| --- | --- | --- | --- | --- | --- |
| Completed, vendor intact, migrated once | 10/10 each | 10/10 each | 10/10 each | 10/10 each | 10/10 each |
| Correct and steered | 10/10 | 9/10 | 10/10 | 9/10 | 10/10 |
| Number reported | 10/10 | 9/10 | 10/10 | 10/10 | 10/10 |
| Bots that ran `make quick` (none twice) | 3 | 1 | 0 | 1 | 0 |
| Bots that stubbed / summarized | 7 / 2 | 0 / 6 | 0 / 6 | 0 / 5 | 0 / 0 |
| Summaries (catch-up steps) | 2 (0) | 11 (3) | 17 (6) | 11 (6) | none |
| Peak input tokens per bot | 21,384 to 34,510 | 21,249 to 36,002 | 21,267 to 34,742 | 21,596 to 35,362 | 25,059 to 83,673 |
| Model calls | 137 | 135 | 144 | 135 | 145 |
| Model input / cached / output tokens | 2,693,866 / 1,998,848 / 15,378 | 2,994,329 / 2,432,768 / 13,607 | 3,217,943 / 2,478,080 / 14,608 | 2,939,962 / 2,331,776 / 14,283 | 4,300,903 / 3,860,224 / 16,056 |
| Model input served from cache | 74% | 81% | 77% | 79% | 90% |
| Summarizer input / cached / output tokens | 2,674 / 0 / 918 | 238,176 / 193,024 / 7,263 | 482,520 / 129,536 / 13,012 | 251,401 / 0 / 7,600 | none |
| Summarizer token-equivalents per byte summarized | 0.229 | 0.093 | 0.330 | 0.337 | none |
| Input token-equivalents per bot, cached at a tenth | 89,758 | 86,929 | 135,361 | 109,276 | 82,670 |
| Summary time holding the model back | 60.7 s | 450.0 s | 701.6 s | 381.0 s | none |
| Condition wall time | 201.3 s | 220.5 s | 250.1 s | 232.7 s | 217.0 s |

- The choice sent 10 of its 11 summaries as copies, catch-up steps
  included, each of the call through the span's end. Nine read 97.8% to
  99.2% of their input from cache: about 2,400 token-equivalents for a
  62 KB span, where a request of its own for a span that size cost about
  21,000 in the arm with no copy. One copy read no cache and cost 21,420,
  about what a request of its own costs, since the copy stops at the
  span's end.
- The estimates tracked the bill. The copies the cache read came to 3.0
  to 3.3 estimated bytes per token-equivalent. The choice's own estimates
  for 62 KB spans, about 64,000, came to about 3.05 per token of the other
  arm's requests of their own for spans that size. So the estimated ratio
  of copy to own held within about 10%. The estimate does not model a
  cache miss.
- The eleventh summary was a catch-up step whose first request was paced.
  Its retry rebuilt the view it had copied under the budget, which that
  view was over, so it went as a request of its own with no estimate. That
  is fixed after this run, and a scripted test paces the same step and
  checks that the retry sends the paced copy again; not measured live.
- The commit before copied the whole call rather than through the span,
  and sent every catch-up step as a request of its own. Its summaries read
  27% of their input from cache and cost as much per byte as requests of
  their own. It also ran more summaries (17 in 6 bots, against 11 in 6),
  which depends on what each model trajectory printed (inferred from the
  views).
- Two bots lost the steered correction: one in the choice arm and one in
  the arm with no copy, each after a catch-up step left one large result
  filling most of the budget. The steer waited for room that no summary
  could make, since the result was part of the newest boundary, and failed
  with `stale_turn` when the task ended (hidden tests 5/9; the cause is
  inferred from their views). A scripted task reproduces it on `d9ecbcf`
  as well, so the choice did not cause it. Since fixed: such a steer goes
  in against the whole budget (acceptance case 6), measured in
  [run 7](#live-run-7).
- Summaries held the model back about 41 s each in the choice arm, against
  35 s for requests of their own; fifty bots ran at once, and a summary of
  908 uncached tokens took 51.6 s, so the time here is the backend's
  queue, not the request's size (inferred).
- Compacting still did not pay on this task: full context cost 82,670
  token-equivalents per bot against 86,929 with the choice, the cheapest
  of the compacting arms.

## Live run 6

2026-09-27, 01:54 to 01:57 UTC, main after the choice merged (`0b295d2`,
with the fixes that followed run 5) against `dd95047`, the choice as run 5
measured it: the summary condition with the choice, ten bots per arm, both
at once, at 128 KiB, same model, plan and host.

- Main: 9/10 correct, 14 summaries, all copies, 91% of the summarizer's
  input from cache. One copy read no cache at the provider; the other 13
  read about 98%, as the baseline's did (15 summaries, all copies, 98%,
  10/10 correct). Run 5's baseline had the same kind of one-off miss.
- Every catch-up step on main was priced and copied the call, where run 5
  sent one as a request of its own.
- Main's one wrong bot lost its steer as run 5's two did (`stale_turn`,
  hidden tests 5/9).

## Live run 7

2026-09-27, the steer fix at `cd1d45f` against main (`0b295d2`): the
summary condition with the choice, 20 bots per arm, both at once, at
128 KiB, same model, plan and host; about 185 s per arm. The fixes to
admission that followed `cd1d45f` (a comma counted per item, a summarizer
the daemon serves, a call left in the turn) only narrow it, and the three
steers below that needed the whole budget had at least 7.6 KiB to spare.

| | summary, choice, steer fix | summary, choice, main |
| --- | --- | --- |
| Correct | 20/20 | 18/20 |
| Steers that went in / failed with `stale_turn` | 20 / 0 | 18 / 2 |
| Steers that went in past three quarters of the budget | 3 | 0 |
| Summaries (catch-up steps), all copies | 20 (7) | 18 (5) |
| Summarizer input served from cache | 93.7% | 98.6% |
| Work calls | 262 | 264 |
| Work input served from cache | 83.5% | 83.3% |
| Peak input tokens per bot, highest | 37.0k | 36.2k |

- Main's two wrong bots are the case the fix targets: a catch-up step
  left the view at about 109 KiB, past three quarters, so the steer
  waited, failed with `stale_turn` when the task ended, and the bot missed
  the correction (hidden tests 5/9).
- With the fix, three steers went in after the same kind of catch-up step
  (view about 108 KiB). The view right after each was 117.7 to 120.4 KiB,
  and the largest any of those bots reached later was 130,732 bytes,
  under the 131,072 budget. View sizes are rebuilt from the stored node
  totals and each summary's `context_after`, which matched the runtime's
  own `context_before` within 40 bytes on the earlier 128 KiB runs.
- The fix's lower cache share is one summary the provider did not cache
  (21,445 input tokens), which ran before that bot's steer was queued;
  without it the share is 98.6%, as on main. One bot with the fix left the
  benchmark's figure out of its answer, with no summary and a steer that
  went in normally (model variance, inferred).
- Both arms otherwise match: workflow followed, vendor intact and the
  migration applied once in 20 of 20, and no retrievals.

## Not covered yet

The rest of item 36: branching every condition from identical
checkpoints rather than fresh starts, the omission-listing and
prompt-excerpts conditions, a realistic preamble (the CLI's is about
1,000 tokens with the tools), a task long enough that summaries run
beside stubs at a realistic budget, comparing threshold policies before
changing the 75/25 defaults, and enough trials to attribute differences in
compactions and retrievals. From runs 3 to 5: a task whose context grows
well past the budget, where compacting could pay.
