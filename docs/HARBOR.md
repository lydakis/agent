# Harbor task benchmarks

Status, 2026-09-26. The runtime runs under [Harbor](https://github.com/laude-institute/harbor),
the evaluation framework from the Terminal-Bench authors, through
`bench/harbor_agent.py`. Since 2026-09-24 it has run matched comparisons on
five Terminal-Bench 2.1 tasks with real models: on the ChatGPT plan against
Codex, and on Sonnet 5 against Claude Code. The [matched runs](#matched-runs)
below give each arm's pass rate, cost, served models and fallback policy, and
[EVIDENCE.md](EVIDENCE.md) summarizes them with the runtime measurements.
Five tasks make a screen, not a Terminal-Bench score.

These are task-quality benchmarks. They measure whether the model, driven by
this harness, finishes real tasks and what the tokens cost. They are not the
runtime-overhead measurements in [BENCHMARKS.md](BENCHMARKS.md), and a result
from one does not stand in for the other.

## Matched runs

Every number here was recomputed on 2026-09-26 from the job records and each
harness's own logs. The trial records themselves stay in the ignored local
directory.

All runs used Harbor 0.23.0 with Docker on an Apple silicon Mac, the
`terminal-bench/terminal-bench-2-1` dataset (digest `sha256:7d7bdc1c…`), and
the tasks kv-store-grpc, pypi-server, schemelike-metacircular-eval,
torch-tensor-parallelism and write-compressor. Two trials ran at a time, with
no Harbor retries, a 2,400-second agent timeout, and high reasoning on every
arm. The arms of each pair started within a second of each other. The
ChatGPT-plan arms of both harnesses used the same plan login, so their cost is
what the same tokens would cost on the API. Every arm is priced at the same
list rates, and each cost Harbor recorded reproduces from its token counts.

| Run | Arm | Requested model | Trials | Window, UTC |
| --- | --- | --- | ---: | --- |
| A | Agent `c585c16` | `chatgpt/gpt-6-sol` | 15 | 09-25 14:04–14:57 |
| A | Codex 0.156.1 | `openai/gpt-6-sol` | 15 | 09-25 14:04–15:23 |
| B | Agent `c585c16` | `anthropic/claude-sonnet-5` | 15 | 09-25 15:24–17:24 |
| B | Claude Code 2.1.282 | `anthropic/claude-sonnet-5` | 15 | 09-25 15:24–17:21 |
| C | Agent `15d629c` | `chatgpt/gpt-6-sol` | 10 | 09-26 05:02–05:32 |
| C | Codex 0.156.1 | `openai/gpt-6-sol` | 10 | 09-26 05:02–05:52 |
| – | Codex 0.156.1 | `openai/gpt-6-sol` | 10 | 09-26 04:18–05:01 |

The last row is unmatched: its Agent arm's outputs were lost before they were
read, so the pair was run again as C. It is kept for Codex's failure causes.

### Served models and fallbacks

| Run | Arm | Calls | Served, as recorded | Fallback configured | Fallback calls |
| --- | --- | ---: | --- | --- | ---: |
| A | Agent | 172 | gpt-6-sol | `--fallbacks`, inert on ChatGPT | 0 |
| A | Codex | 227 | gpt-6-sol, per turn | none | not recorded |
| B | Agent | 390 | claude-sonnet-5 | `--fallbacks` | 0 |
| B | Claude Code | 266 | claude-sonnet-5, per response | `--fallback-model` not set | 0 |
| C | Agent | 161 | gpt-6-sol | `--fallbacks`, inert on ChatGPT | 0 |
| C | Codex | 163 | gpt-6-sol, per turn | none | not recorded |
| – | Codex | 128 | gpt-6-sol, per turn | none | not recorded |

- Only Claude Code records the model the API reported for each response. No
  auxiliary model appears in its per-model usage. Codex records the model
  once per turn, so a server-side reroute within a turn would not show. Ours
  records the requested model, and names another only when Anthropic's
  fallback splits a call or a summarizer answers on another model; neither
  happened. See the gap in
  [COMPARISON_CONTRACT.md](COMPARISON_CONTRACT.md#task-comparisons).
- The adapter's `--fallbacks` asks Anthropic for its server-side fallback and
  does nothing on the ChatGPT provider, so it was live only in B. Harbor's
  Codex adapter has no fallback option and passed no `config.toml`. Harbor's
  Claude Code adapter passes `--fallback-model` only when asked, and it was
  not; whether Claude Code opts into Anthropic's fallback on its own does not
  show in its records.
- B's Agent calls include 9 cache refreshes during long tool calls. C's
  include 28 from four bots the task bot delegated to; those bots ran with no
  reasoning level set, so at the provider's default, while the task bot ran
  at high.

### Outcomes

| Run | Arm | Passed | Cost | A trial | Cached input | Uncached input a trial | Median agent time | Median trial time |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| A | Agent | 14/15 | $1.34 | $0.089 | 81.8% | 17.8k | 94 s | 162 s |
| A | Codex | 8/15 | $2.90 | $0.193 | 94.5% | 22.7k | 128 s | 440 s |
| B | Agent | 12/15 | $15.06 | $1.00 | 95.2% | 66.5k | 320 s | 589 s |
| B | Claude Code | 13/15 | $10.71 | $0.714 | 95.7% | 49.4k | 293 s | 539 s |
| C | Agent | 8/10 | $1.13 | $0.113 | 84.3% | 20.9k | 84 s | 196 s |
| C | Codex | 5/10 | $1.99 | $0.198 | 95.4% | 20.7k | 113 s | 492 s |
| – | Codex | 5/10 | $1.47 | $0.147 | 93.2% | 19.0k | 114 s | 491 s |

Harbor's figures match the harness records except for two timed-out trials in
B, and the table uses the harness records for both:

- One Agent trial reached the agent timeout, and about a minute later the
  runner's two-hour job limit stopped Harbor before grading, so Harbor
  recorded no cost for it. Its store puts it at about $3.23; Harbor's total
  without it is $11.84.
- Claude Code was stopped before writing its result in its timed-out trial,
  so Harbor rebuilt the trial from its trajectory at 4.37M input tokens and
  $3.21. Its transcripts hold 5.59M, about $3.56; Harbor's total is $10.36.

Nearly all uncached input on Sonnet is cache writes (99.9% on both sides).
Cached share, uncached input and times are over the trials Harbor recorded.

### Failures

A trial with two causes counts under harness. No failure came from grading
or the task environment.

| Run | Arm | Harness | Model | Timeout |
| --- | --- | ---: | ---: | ---: |
| A | Agent | 0 | 1 | 0 |
| A | Codex | 5 | 2 | 0 |
| B | Agent | 0 | 1 | 2 |
| B | Claude Code | 0 | 1 | 1 |
| C | Agent | 0 | 2 | 0 |
| C | Codex | 3 | 2 | 0 |
| – | Codex | 4 | 1 | 0 |

- **Harness.** Every one is a Codex trial on kv-store-grpc or pypi-server in
  which the model started the server the tests need and its own checks
  passed, but the server was gone when the tests ran. The trial logs show
  Codex ending a process a command put in the background when that command
  returned, and a persistent session's processes when `codex exec` exited.
  One trial lost its server even under `setsid`. In four of the Codex kv-store failures, and in A's one Agent
  failure, the model also named a proto field `val` where the tests require
  `value`.
- **Model.** Gradient or shape mismatches on the torch task, and schemelike
  runs in which four test programs failed.
- **Timeout.** All three were schemelike. The Agent's two saw no provider
  waits beyond 0.3-second connection retries.

On the ChatGPT plan, the Agent arms passed more trials than Codex at 46% and
57% of its cost a trial, but the pass-rate gap comes from Codex's process
lifetime on the two server tasks: counting model errors only, A is 1 against
2 and C is 2 against 2. On Sonnet 5, Claude Code passed one more trial at
about 29% less a trial, and the difference is on schemelike.

## What other harnesses report

Everything in this section is self-reported by the vendor and was not
reproduced here.

- **Strands Harness** (AWS, announced 2026-09-21). Ran on Harbor across
  ALFWorld, ContextBench, GAIA, WebShop, tau2-bench and Terminal-Bench 2.1, and
  claims 28% lower token cost at comparable accuracy
  ([MarkTechPost](https://www.marktechpost.com/2026/09/21/aws-strands-agents-team-releases-strands-harness/)).
  The Terminal-Bench 2.1 comparison used Claude Fable 5 with 89 trials per harness:

  | Harness | Cost | Accuracy |
  | --- | ---: | ---: |
  | Strands | $56.29 | 69.7% |
  | Oh-my-pi | $86.83 | 69.7% |
  | OpenCode | $73.42 | 66.3% |
  | Claude Code | $248.05 | 61.8% |
  | DeepSeek | $40.30 | 59.5% |

  A separate repository,
  [strands-labs/benchmark-harnesses](https://github.com/strands-labs/benchmark-harnesses),
  has its own Hydra-configured runner for SWE-bench Verified, SWE-bench Pro and
  Terminal-Bench 2 in Docker. It does not use Harbor.
- **Unreal Agent** (Unreal Labs, 2026-09-22). Reports Terminal-Bench, SWE-Atlas
  QnA, DeepSWE 1.1 and ALE-CLI with GPT-6 Astra, at about 40% lower cost than
  Codex. Which framework it used was not confirmed here. SWE-Atlas QnA and DeepSWE
  1.1 are both published on Harbor Hub.

Harbor Hub, observed 2026-09-23: `terminal-bench/terminal-bench-2-1` (89 tasks),
`swe-bench/swe-bench-verified` (500), `scale-ai/swe-bench-pro` (731),
`datacurve/deep-swe-1-1` (113), `scale-ai/swe-atlas-qna` (124),
`sierra-research/tau3-bench` (375), plus about 300 others. Harbor 0.23.0 ships
built-in adapters for `strands`, `claude-code`, `codex`, `opencode` and `pi`, so
the same model can drive each harness on the same tasks.

## The adapter

`bench/harbor_agent.py` is a Harbor installed agent that runs one bot per trial:

1. **Install.** Uploads a static x86-64 Linux `agent` to `/installed-agent/agent`
   and links it onto `PATH`, so the bot can delegate with `agent run` as it
   does locally. It installs `bash`, and `ca-certificates` when the image has no
   root store: the HTTP client loads the platform roots when the daemon starts,
   even for a plain-HTTP endpoint, so a stock `ubuntu:24.04` failed with
   `http_client_init` until the roots were installed.
2. **Run.** `agent run --new --bot task --agents --model PROVIDER/MODEL -- INSTRUCTION`
   runs in the task's working directory. Harbor model names already use the
   `PROVIDER/MODEL` form. The store lives on the container's disk under
   `/tmp/agent-harbor`, and the event stream is written to `/logs/agent/agent.jsonl`
   as it happens. The turn's exit status becomes the trial's.
3. **Finish.** After the turn, and also when Harbor's agent timeout cancels it,
   the adapter saves `agent stats` and shuts the daemon down. Shutdown cancels
   any turn still running, including bots the task delegated to, so nothing
   calls the model or runs tools past the deadline. It returns once the daemon
   has committed those turns and exited, and the store is then copied into the
   trial's logs.
4. **Account.** Tokens come from every bot's turn records in that final copy,
   so a delegated bot's last call, or a bot created at the very end, is counted.
   They are grouped by model, so a delegated bot on another model is priced at
   its own rates, and so is each call Anthropic's server-side fallback ran on
   another model or a summarizer ran on a `--compaction-model`, on that
   model's own provider (the `models`
   split of its `usage` event). Cost is
   computed from LiteLLM's price table, as Harbor's own adapters do. Cache
   reads are priced at the cache-read rate and Anthropic cache writes (the
   `cache_write_tokens` of each `usage` event) at the cache-write rate, as
   Harbor's Claude Code adapter prices its steps. The hour-long part
   (`cache_write_1h_tokens`, under `--cache-ttl 1h`) is priced at the table's
   one-hour write rate, or twice input when it has none. It is left
   empty when any model used is missing from the table, rather than reported low.
   The trial's metadata records the requested model, `served_calls` (billed
   calls per model, fallbacks and delegated bots included) and
   `fallbacks: true`, so a comparison can check that both arms ran the same
   model ([COMPARISON_CONTRACT.md](COMPARISON_CONTRACT.md#task-comparisons)).
   A call counts under the model it requested unless a fallback or summarizer
   answered on another.
   Provider failures map to Harbor's retryable error types, for example
   `provider_http_429` to `ApiRateLimitError`, `provider_stream_failed` to
   `NetworkConnectionError` and `provider_http_401` to `AgentAuthenticationError`.

Validation on 2026-09-23 used Harbor 0.23.0 (commit `15da91c`) with Docker. It ran
the Harbor `hello-world` task, rebuilt so it needed no network, against a
scripted Responses endpoint that issues one shell call and then answers. Reward
was 1.0, with 2,000 input, 1,200 cached and 100 output tokens recorded. A second
endpoint answering 401 produced reward 0 and `AgentAuthenticationError`. On
2026-09-24 a task with a 15-second agent timeout, whose model asked for
`sleep 120`, ended in `AgentTimeoutError` 15.4 seconds after the agent started.
Its copied store recorded the turn as `interrupted`, with the first round's
1,000 input and 50 output tokens.
`tests/test_harbor_agent.py` covers argument quoting, provider key forwarding,
when the ChatGPT login is uploaded and what it holds, per-model accounting, the
finishing command and cleanup after a timeout, and reads a store the runtime
wrote. It runs under Harbor's Python and skips without Harbor.

## Running it

Harbor needs Python 3.12 and Docker. Task images are x86-64, so on Apple silicon
they run under emulation.

```sh
# Static Linux binary. On Linux: rustup target add x86_64-unknown-linux-musl,
# plus musl-tools. On macOS, cargo-zigbuild builds the same target.
cargo build --release --locked --target x86_64-unknown-linux-musl
uv tool install harbor   # or: uv venv --python 3.12 .local/harbor && uv pip install harbor

export ANTHROPIC_API_KEY=...
# A handful of tasks first; -i/-x select task names, -n sets concurrency.
PYTHONPATH=. harbor run -d terminal-bench/terminal-bench-2-1 -a bench.harbor_agent:Agent \
  -m anthropic/claude-fable-5 -n 8 -l 5
# The whole set, then a baseline on the same model and tasks.
PYTHONPATH=. harbor run -d terminal-bench/terminal-bench-2-1 -a bench.harbor_agent:Agent \
  -m anthropic/claude-fable-5 -n 8
harbor run -d terminal-bench/terminal-bench-2-1 -a strands -m anthropic/claude-fable-5 -n 8
```

To run on a ChatGPT plan instead of an API key, sign in with `codex login` and
name the model `chatgpt/MODEL`, using the id Codex's `/model` picker shows. Every
task bot is created with `--fallbacks`, so a declined Anthropic request finishes
on the model Anthropic recommends instead of failing the task; a fleet opts in
per bot. The
adapter copies the access token and account id from Codex's `auth.json` into each
task container, readable only by the agent user, and adds `--provider chatgpt`.
The refresh and ID tokens stay on the host, and nothing is uploaded when a
`provider` spec points `chatgpt` at another endpoint, since the daemon then never
reads the login. The model's tools can still read the
access token file; the daemon redacts the token from tool output, but a task
that reads the file holds a login to the plan, so run only tasks you trust on
your own account. The token is not refreshed during a run: the daemon re-reads
the copied file when the token expires or is refused, and nothing in the
container writes a new one, so run any `codex` command just before starting.
Plan usage windows cap how
many tasks one run can finish, and whether a ChatGPT plan may drive a harness
other than Codex is a question for OpenAI's terms. Harbor's own Codex adapter
takes the same login with `CODEX_FORCE_AUTH_JSON=1`, so the Codex baseline can
run on the plan too.

```sh
PYTHONPATH=. harbor run -d terminal-bench/terminal-bench-2-1 -a bench.harbor_agent:Agent \
  -m chatgpt/MODEL -n 2 -l 5
CODEX_FORCE_AUTH_JSON=1 harbor run -d terminal-bench/terminal-bench-2-1 -a codex -m MODEL -n 2 -l 5
```

Pass `--ak KEY=VALUE` for adapter options: `reasoning`, `max_output_tokens`,
`stall_timeout`, `context_bytes`, `compact_at`, `binary` (another build),
`codex_auth` (another `auth.json` for `chatgpt/` models), and
`provider` (a `--provider` spec for a gateway such as Bedrock, whose named key
variable is forwarded from the host). Keep job outputs under the ignored
`.local/` directory, because trial logs contain full transcripts.

## Gaps before publishing a comparison

- **Served model.** The daemon records the model it requested, not the one
  the provider names in its response, so a reroute or snapshot change behind
  the same name would not show. Codex's records have the same limit within a
  turn; Claude Code's do not.

- **Timeouts miss the call in flight.** Shutdown cancels the model call in
  progress at the timeout before the provider reports its usage, so those
  tokens are not counted even if the provider bills them.
- **No trajectory.** The adapter does not emit Harbor's ATIF trajectory, so
  `harbor view` and `harbor analyze` show no steps. The copied store holds the
  full transcript.
- **Proxied sandboxes.** The provider client is built with `no_proxy()`, so an
  environment whose only egress is an HTTPS proxy cannot reach the provider.
- **ChatGPT-plan cost.** A `chatgpt/` run bills the plan, not tokens; the
  reported cost is what the same tokens would cost on the API.
- **Prompting.** The bot uses the `--agents` composed instructions, the same as
  the app, with no benchmark-specific prompting. Vendor numbers above may include
  tuned prompts.
