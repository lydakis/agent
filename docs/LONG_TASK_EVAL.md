# Long-task evaluation

Does one long task survive several compactions inside its own turn, keep
what it learned, and finish correctly? This page has two parts: acceptance
cases that combine compaction with the runtime's other guarantees, run
against scripted providers, and an evaluation of a real model on one
synthetic repository task (roadmap [item 36](NEXT.md)). Written 2026-09-26.
The acceptance cases pass. The evaluation has run once live, recorded
[below](#live-run-1).

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
   ended, which is what the live run hit.
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

## Conditions and scores

Two conditions run the same task from fresh starts, each in its own
daemon, with `--trials` bots at once: `compact` with a 20 KiB context
budget, which forces several compactions, and `full` with 4 MiB as the
control. Both use the CLI's default preamble and compaction instructions,
as the context evaluation does, and the tools `shell, read, write, edit,
history`.

Scores come from the workspace and the event log, not the model's account
of itself: hidden tests passed, every file under `vendor/` unchanged with
none added or removed, `make quick` runs in total and after the first
compaction, migrations applied, whether the correction reached the task
(its steer turn's final status, not the submit reply), whether the answer
carries the measured number and `make bench` ran, whether the bot ran
`tools/env-check` before its first edit, `make check` after its last, and
`make bench` after that check, commands repeated after the first
compaction, retrieval calls (`history`, or `read` of a `result/` reference),
compactions, elisions, the context-view version each model call was made
under, input, cached input, and output tokens for the model and the
summarizer separately, and summarizer latency from its send to the send of
the model call it held back. Failed summaries are live-only events, so the
runner collects them as they arrive. An edit is a `write` or `edit` call,
`tools/migrate`, or a shell command that writes a file by redirect, `tee`,
in-place `sed` or `perl`, a patch, or `cp`, `mv`, `rm`, `touch`, or
`mkdir`, matched by pattern; a change made another way, such as a Python
one-liner, is not seen. The hidden tests import the code the
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
- Codex: the request shape (the turn's request with a `compaction_trigger`
  item appended, answered by one encrypted compaction item) was read in
  `core/src/compact_remote_v2_attempt.rs` on `openai/codex` main at a
  revision that was not recorded, so it is an unpinned source observation.
  The Codex CLI 0.157.1 release of the same day (npm `@openai/codex`,
  linux-x64 binary) contains that source file and the `compaction_trigger`
  item type; the binary does not show the request's shape.
- Pi: npm `@mariozechner/pi-coding-agent` 0.73.1, published 2026-05-07,
  the newest under that name. `generateSummary` in
  `dist/core/compaction/compaction.js` sends its own system prompt and one
  user message holding the conversation serialized as text, with no tools.

## Not covered yet

The rest of item 36: branching every condition from identical
checkpoints rather than fresh starts, the omission-listing, elision-only,
and prompt-excerpts conditions, a condition with a realistic budget and
preamble, comparing threshold policies before changing the 75/25 defaults,
and a rerun after the steer fix.
