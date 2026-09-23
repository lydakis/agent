# Harbor task benchmarks

Status, 2026-09-23. The runtime runs under [Harbor](https://github.com/laude-institute/harbor),
the evaluation framework from the Terminal-Bench authors, through
`bench/harbor_agent.py`. It has passed a local Harbor trial against a scripted
model. It has not been run against a real model or a published dataset, so
there is no pass rate or cost to report yet.

These are task-quality benchmarks. They measure whether the model, driven by
this harness, finishes real tasks and what the tokens cost. They are not the
runtime-overhead measurements in [BENCHMARKS.md](BENCHMARKS.md), and a result
from one does not stand in for the other.

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
   as it happens. After the turn the adapter saves `agent turns` and `agent stats`,
   shuts the daemon down and copies the store into the trial's logs. The turn's
   exit status becomes the trial's.
3. **Account.** Token totals come from the daemon-wide `stats`, which include any
   bots the task bot delegated to. Cost is computed from LiteLLM's price table, as
   Harbor's own adapters do, or left empty for models the table does not know.
   Provider failures map to Harbor's retryable error types, for example
   `provider_http_429` to `ApiRateLimitError` and `provider_http_401` to
   `AgentAuthenticationError`.

Validation on 2026-09-23 used Harbor 0.23.0 (commit `15da91c`) with Docker. It ran
the Harbor `hello-world` task, rebuilt so it needed no network, against a
scripted Responses endpoint that issues one shell call and then answers. Reward
was 1.0, with 2,000 input, 1,200 cached and 100 output tokens recorded. A second
endpoint answering 401 produced reward 0 and `AgentAuthenticationError`.
`tests/test_harbor_agent.py` covers argument quoting, provider key forwarding and
token accounting. It runs under Harbor's Python and skips without Harbor.

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
name the model `chatgpt/MODEL`, using the id Codex's `/model` picker shows. The
adapter copies Codex's `auth.json` into each task container, readable only by the
agent user, and adds `--provider chatgpt`. The token is not refreshed during a
run, so run any `codex` command just before starting. Plan usage windows cap how
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

- **Anthropic cost is a lower bound.** Turn records fold cache writes into input
  tokens, and Anthropic bills cache writes above the base input rate. Recording
  cache-creation tokens separately would close this.
- **Timeouts lose bookkeeping.** Harbor kills the command at the task's agent
  timeout. Tokens then come from the streamed usage events, which cover only the
  task bot, and the store copy is skipped.
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
