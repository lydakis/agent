# Memory evaluation

Does memory carry what one task learned to a later task, and does a later
task catch a remembered fact that the code now contradicts? This is plan
decision 7: a run on real models on George's Mac, which happens only with
his go-ahead. Written 2026-10-10. It ran once on 2026-10-10; the
[first run](#first-run-2026-10-10) has the results.

## What it runs

`bench/memory_eval.py` gives each bot one task in a fresh git worktree of
a small synthetic project, `shop`, the way a coordinator's task starts.
Each bot gets its own HOME, and with it its own daemon and its own
`~/.agents/memory`, so no trial sees another trial's saves. The real `agent`
CLI makes each bot with `run --new --agents`, so its instructions are what
the composer gives today, memory indexes included
([CLIENT.md](CLIENT.md#layers)).

There are two conditions:

- **`none`**: no facts and no `memory` skill. This is the control.
- **`memory`**: three facts are saved through the app's own
  `~/.agent/memory` script, and the app's `memory` skill is installed, both
  from this checkout so they match. A
  new agent's instructions then carry the person's index and the project's.

There are three scenarios. Each pairs a fact the code does not show with a
task that needs it:

| Scenario | Fact in memory | Task | Correct means |
| --- | --- | --- | --- |
| `decision` | project: API timestamps end in `Z`, never `+00:00` | add `created_at` to `Order.to_json()` | `2026-01-02T03:04:05Z` for a time with microseconds, where `isoformat()` gives `.678901+00:00` |
| `preference` | person: error messages start with a bracketed code | make `add_item` reject a quantity below 1 | `ValueError("[E_...] ...")` for 0 and -1, while 1 still works |
| `stale` | project: prices round half to even with `round_price` in `shop/money.py` (wrong: the file is gone, and `shop/pricing.py` rounds half up) | add a `discount` to `Order.total()`, rounded as the project rounds | 10.05 at 50% off is 5.03 (half up, not 5.02), and no `shop/money.py` |

Hidden checks score the worktree after the turn; the agent's own account of
its work isn't used. For each bot, the script also records:

- the turn's status, model rounds, input, cached input and output tokens,
  all from the daemon's totals;
- the shell commands that touched memory;
- the facts saved or removed;
- whether the visible tests pass;
- the final reply and the time taken.

In `stale`, `fact_changed` records whether the agent touched the wrong
fact, and `fact_after` keeps its text, or null if removed. Whether the
change is a correction, as the `memory` skill asks, is read from that
text: no keyword test tells a fix from a reworded mistake.

The research's third condition, the skill with no index composed, is left
out. M2 already composes the indexes, and the runtime has one behavior, so
that path no longer exists to measure.

## Run it

These steps run on George's Mac, with Codex logged in to ChatGPT. The
build makes the daemon and the app from the same checkout, since the memory
script is `agent-app --memory` and the skill comes from the checkout:

```sh
cd ~/Developer/agent
git pull
cargo build --release --locked -p agent-runtime -p agent-app
.local/venv/bin/python -m bench.memory_eval --model chatgpt/MODEL --out .local/memory-eval/MODEL.json
```

- **Model.** Name it as Codex's `/model` picker shows it.
- **Login.** The daemons read Codex's login from `CODEX_HOME`, or from
  `~/.codex` when that is unset.
- **Run size.** 18 bots start at once: 2 conditions × 3 scenarios × 3
  trials (`--trials`). Each bot's turn is capped at 400,000 tokens
  (`--turn-budget-tokens`) and 20 minutes (`--timeout`).
- **Output.** The summary prints per condition and scenario. Every bot's
  record goes to `--out`. Its folder, worktree and memory stay in a new
  `agent-memory-eval-*` folder under the system's temporary directory,
  named in the output as `bots_dir`. That is outside this checkout so a
  bot's instructions don't pick up this repository's `AGENTS.md`.
- **Provenance.** The result records when it ran, the checkout's
  revision, and the hashes of the `agent` and `agent-app` it used.
- **API key instead.** An API key works as well: `--model openai/MODEL`
  with `OPENAI_API_KEY` set, or `anthropic/MODEL` with `ANTHROPIC_API_KEY`.
  Only the selected provider's key reaches the daemons, which keep it out
  of the bots' shells.

Checks with no model call, which ran in this PR:

```sh
python3 -m bench.memory_eval --self-check
AGENT_TEST_RUNTIME=1 python3 -m unittest tests.test_memory_eval
```

The first applies a right and a wrong reference answer to each scenario.
The right one must pass and the wrong one fail, the untouched project
fails, and the visible tests still pass.

The second runs the whole path against the free synthetic model. That
covers the worktree, the per-bot HOME, the facts saved through the real
script, `run --new --agents`, the daemon's totals and the scoring. It
checks that the memory condition's instructions carry both indexes and the
control's carry none. It needs the release `agent` and a built `agent-app`
(the build above, or `AGENT_TEST_APP`).

## Cost estimate

What a model was told was measured on the synthetic run: about 1,300
characters of instructions for `none` and about 2,100 for `memory`. That
covers the preamble, the project's AGENTS.md, the skills index and the two
memory indexes. Tool definitions come on top of that. Everything below is
an estimate, not a measurement:

- **One task:** about 6 to 15 model rounds, averaging about 8,000 input
  tokens per round as the files it reads pile up. That is about 50,000 to
  120,000 input tokens per bot, most of it served from the prompt cache
  after the first round, plus 2,000 to 10,000 output tokens (reasoning
  included).
- **A full run of 18 bots:** about 1 to 2 million input tokens, roughly
  70% cached, and about 50,000 to 180,000 output tokens.
- **The ceiling:** 18 × 400,000 = 7.2 million tokens, if every task hit its
  cap.

**On the ChatGPT plan** the run costs no API money; it uses plan quota.
How much quota isn't published, so that part is unknown.

**On an API key**, the price depends on the model's rates. As an
illustration only: at $1 to $5 per million uncached input tokens, a tenth
of that for cached input, and $10 to $25 per million output tokens, the
expected run costs about $1.50 to $6. The 7.2-million-token ceiling
counts input and output together; billed at the output rate with no
cache, it would cost up to $180.

## Reading the result

What the run can show:

- **Recall:** whether `memory` beats `none` on `decision` and `preference`.
- **Verification:** `stale` checks whether agents test a fact that names
  code before acting on it. A `memory` result below `none` there means the
  wrong fact misled the agent.
- **Cost:** what memory adds, in rounds and tokens.

With three trials per cell, these are observations, not rates.

## First run, 2026-10-10

Measured, not estimated. Observed 2026-10-10 from 13:34Z on George's Mac,
at revision fefef85 with the defaults (3 trials, 400,000-token turn cap,
20-minute timeout), on `chatgpt/gpt-6.1-sol` through Codex's ChatGPT
login. All 18 bots completed, with no errors or timeouts, in 21 to 59
seconds each. Every bot left the visible tests passing, and none created
`shop/money.py`.

Rounds are per trial. Tokens are the daemon's totals summed over the three
trials. The last column counts shell commands that touched memory.

| Condition | Scenario | Correct | Rounds | Input | Cached | Output | Memory commands |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `memory` | `decision` | 3/3 | 9, 8, 9 | 104,648 | 81,408 | 2,446 | 1, 0, 1 |
| `memory` | `preference` | 3/3 | 7, 7, 7 | 77,507 | 47,104 | 1,758 | 1, 0, 0 |
| `memory` | `stale` | 3/3 | 7, 8, 8 | 90,096 | 63,488 | 3,071 | 1, 1, 1 |
| `none` | `decision` | 0/3 | 7, 7, 7 | 37,211 | 21,504 | 1,129 | 0, 0, 0 |
| `none` | `preference` | 0/3 | 6, 7, 6 | 30,825 | 19,584 | 1,110 | 0, 0, 0 |
| `none` | `stale` | 3/3 | 6, 6, 6 | 39,506 | 22,016 | 2,220 | 0, 0, 0 |

The whole run used 379,793 input tokens (255,104 cached) and 11,734
output tokens. The input is below the estimate above, at about 38% of
its low end.

What it shows, as observations from three trials per cell:

- **Recall.** Memory carried both facts every time. Without memory, every
  bot used `isoformat()`, giving `.678901+00:00`, and wrote error messages
  with no code. With memory, every bot wrote `...05Z` and a bracketed code.
- **Verification.** The wrong fact misled no bot: `memory` matched `none`
  on `stale`. All three `memory` bots then rewrote the fact in place. Each
  now says prices round half up with `quantize_price` in `shop/pricing.py`
  and that `shop/money.py` is gone, and each cites the source and a
  verified date. Because the control also gets `stale` right from the code
  alone, this scenario can show harm from a wrong fact but not a benefit
  from memory. A wrong fact that is more tempting would test more.
- **Cost.** Memory took about 2.5 times the input tokens and zero to two
  more rounds per task, 1.3 on average. Its roughly 800 extra characters
  of instructions don't account for that. A likely cause, which this run
  doesn't establish, is the rounds spent reading memory and saving facts.
  Three `memory` bots re-saved a fact they had been given, with the same
  text, which left the file unchanged and spent a call.

The raw result stays in George's local `.local/memory-eval/`. The bots'
folders and transcripts stay in the temporary folder it names as
`bots_dir`.
