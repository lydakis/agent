# Memory evaluation

Does memory carry what one task learned to a later task, and does a later
task catch a remembered fact that the code now contradicts? This is plan
decision 7: a run on real models on George's Mac, which happens only with
his go-ahead. Written 2026-10-10. It has not run on a real model yet. The
script, its no-model checks and the cost estimate below are ready.

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
  `~/.agent/memory` script, and the app's `memory` skill is installed. A
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

In `stale`, `fact_corrected` records whether the agent removed the wrong
fact or rewrote it to say prices round half up, which the `memory` skill
asks it to do; `fact_after` keeps the fact's text for reading.

The research's third condition, the skill with no index composed, is left
out. M2 already composes the indexes, and the runtime has one behavior, so
that path no longer exists to measure.

## Run it

These steps run on George's Mac. The app must be installed (the script
uses its `agent-app --memory`), and Codex must be logged in to ChatGPT:

```sh
cd ~/Developer/agent
git pull
cargo build --release --locked
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
- **App path.** When the app isn't where `~/.agent/memory` points, pass
  `--memory-app /path/to/agent-app`.
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
(`cargo build` in `app/src-tauri`, or `AGENT_TEST_APP`).

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
expected run costs about $1.50 to $6. The 7.2-million-token ceiling,
billed at the high rate with no cache, would cost under $40.

## Reading the result

What the run can show:

- **Recall:** whether `memory` beats `none` on `decision` and `preference`.
- **Verification:** `stale` checks whether agents test a fact that names
  code before acting on it. A `memory` result below `none` there means the
  wrong fact misled the agent.
- **Cost:** what memory adds, in rounds and tokens.

With three trials per cell, these are observations, not rates.
