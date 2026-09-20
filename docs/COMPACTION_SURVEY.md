# Compaction: what harnesses and providers do, and what the research says

2026-09-19. A survey before any design. Sources are the harnesses' own docs
and source, the providers' API docs, and papers from 2025 and 2026; secondary
write-ups are used only where they read source we could not. Where a claim
is a number, its source is named. Nothing here is a proposal.

## The problem as the field states it

Every harness meets the same wall: the context window fills, and before it
fills the model's behavior degrades. Anthropic's own guidance to Claude Code
users opens with it: "LLM performance degrades as context fills. When the
context window is getting full, Claude may start 'forgetting' earlier
instructions or making more mistakes." The papers call the same thing
"context rot". Compaction is the field's word for replacing older history
with something shorter, and it has two halves that are often conflated:
deciding *when* and deciding *what survives*.

## What the coding harnesses do

| Harness | Who triggers, and when | What the summary is | What stays verbatim | Can the model trigger it? |
| --- | --- | --- | --- | --- |
| Claude Code | Harness, at ~89–98% of the window (sources differ by version); `/compact [instructions]` by hand; partial summarize from a checkpoint | Same model writes a structured summary: accomplished, in progress, files, next steps, user constraints. `CLAUDE.md` can carry standing compaction instructions. Also cheaper layers first: tool-result clearing, "microcompact" | Session metadata; the rest is summarized | No |
| Codex CLI | Harness, at 90% of the resolved window (cannot be raised); `/compact` by hand; fires before a new user message and mid-turn | `/responses/compact`: an encrypted, opaque "compaction item" the server mints, holding "key prior state and reasoning"; rewritten, not appended, on each compaction (median blob size change −168 chars over 949 samples) | User messages, newest first, up to ~20k tokens (64k budget in one measurement) | No |
| Gemini CLI | Harness, at a configurable fraction of the window (70% default in one version, ~50% in another); `/compress` by hand | Summarizer persona prompt; older tool outputs truncated first | A recent tail (about 30% in one description) | No |
| OpenCode | Harness, when the window would overflow (~96–99%); `/compact` by hand | Prune old tool outputs, then LLM summary with the same section list as Claude Code | Last 40k tokens of tool output protected | No |
| Pi | Harness, when `contextTokens > window − 16,384 reserve`, checked after tool results land and before a new prompt; `/compact` by hand | Markdown template: Goal, Constraints & Preferences, Progress (done / in progress / blocked), Key Decisions, Next Steps, Critical Context, `<read-files>`, `<modified-files>`. Repeated compactions *update* the previous summary rather than restart | 20k tokens of recent messages, cut only at user or assistant messages, never at a tool result; file operations tracked cumulatively across compactions | No; extensions can cancel or replace the summary |
| Cline | Harness, at 90% utilization (`max(window − 40k, 0.8·window)`); `/smol` or `/compact` by hand | Two tiers: deterministic "basic compaction" (merge turns, strip attachments) then "agentic compaction" by a summarizer model | Recent turns past a computed boundary | Listed in one taxonomy as the one agent with a model-callable condense tool; its own docs describe harness triggering, and an early version drew complaints of condensing "roughly every 10 messages" and burning tokens |
| OpenHands | Event-count threshold (100 events default) | "Condensers": `recent` (drop old events, no LLM), `observation_masking` (hide old tool outputs, keep actions), LLM summarizing condenser; the log is append-only with suppression markers, so full history stays replayable | Depends on condenser | No |
| Amp | Nobody automatically; a manual "handoff" extracts what matters into a fresh thread | Secondary model extracts | n/a | No; philosophy is short threads |
| Aider | Harness, at a token threshold | Recursive hierarchical summarization | Recent messages | No |
| SWE-agent, mini-swe-agent | Composable history processors: keep first and last N observations, elide the rest; mini-swe-agent has none and crashes at the limit | None | First and last N observations | No |
| Microsoft Agent Framework | Configurable triggers (tokens, messages, turns, groups) and a target that stops it | A pipeline: collapse old tool results, summarize with a separate cheaper model, sliding window, truncation as backstop; tool call and result kept as an atomic group | The most recent N groups | No |
| Letta (MemGPT) | The model, continuously | Not a summary of the transcript: size-limited "memory blocks" in context that the model edits with tools, plus archival storage it searches; a "sleep-time" agent can rewrite the blocks between turns | The blocks | Yes, by design; no controlled evidence published on how well models manage them |

Three things are common to all of them. The trigger is a harness threshold
on tokens, never a model choice, with Letta as the one architecture built the
other way round and Cline as the one coding agent that may have tried it. A
recent tail is kept verbatim, sized in tokens, and the cut is placed at a
message boundary rather than inside a tool exchange. The summary is
structured by a template that names the same things everywhere: goal,
constraints the user stated, what is done and what is not, files touched,
next steps. Claude Code and Codex additionally let the user steer the summary
with instructions, and Pi and Codex keep one rolling summary rather than a
chain of them.

## What the API providers now offer

Both large providers moved compaction to the server in 2026, and both say to
prefer it over client code.

- Anthropic, `compact_20260112`: triggers on input tokens (default 150,000,
  minimum 50,000). The same model writes a summary inside a `compaction`
  block; the default instruction is "Write down anything that would be
  helpful, including the state, next steps, learnings etc." and a client
  instruction *replaces* it. Everything before the block is dropped on later
  requests. The summary is plaintext the client can read, must be passed
  back, and can carry `cache_control` so the system prompt's cache survives
  the rewrite. Thinking blocks before the compaction are not carried forward.
  Alongside it, context editing clears old tool results (default trigger
  100,000 tokens, keep the last 3 pairs, with a `clear_at_least` floor so a
  cache invalidation is worth it) and old thinking blocks; Anthropic reports
  using tool-result clearing in Claude Code with a 29–39% improvement on its
  own measure. The cookbook's recommended stack is clearing first, compaction
  second, and a model-driven memory tool whose system prompt says "ALWAYS
  VIEW YOUR MEMORY DIRECTORY BEFORE DOING ANYTHING ELSE... Your context
  window might be reset at any moment."
- OpenAI, `/responses/compact` and in-stream `context_management` with a
  `compact_threshold`: the server returns an encrypted compaction item
  "not intended to be human-interpretable" that "preserves the model's
  latent understanding"; user messages are kept in plaintext. Codex is built
  on it. One independent measurement over 1,087 real compactions: median
  blob 15.8 KB, a session going from 226,661 to 19,537 tokens, compaction
  turns taking 6 to 34 seconds, and the blobs decrypting for any account,
  so they behave as long-lived bearer tokens for the assistant-side content.
- xAI documents the same shape.

Opacity limits inspection of the summary's meaning, not storage of its
bytes. OpenAI's [compaction guide](https://developers.openai.com/api/docs/guides/compaction)
documents stateless reuse of the returned compacted window, passed intact
to the next request. For our storage design, this means a saved window can
be versioned and reused by separate continuations with a compatible provider
and model. Historical forks before that window still need the original
history and the context view valid at the requested checkpoint. A plaintext
summary can also be inspected and edited; the semantic contents of an opaque
item cannot be audited locally. Cross-provider portability and the duration
of provider support are separate questions, not guarantees of either format.

## What the research says

**Simple beats clever more often than expected.** On SWE-bench Verified with
five model configurations, masking old observations (hiding tool outputs,
keeping the agent's own actions) halved cost against the raw agent and
matched or slightly beat LLM summarization on solve rate; in four of five
settings it "paid less per problem and often performed better". LLM
summaries also "smooth over signs indicating that the agent should already
stop", making agents run 13–15% longer. A hybrid that masks first and
summarizes rarely saved a further 7–11%. (Complexity Trap, 2025; JetBrains
follow-up, Dec 2025.) OpenHands ships exactly this as `observation_masking`.

**The summary is performance-critical, and untrained models write uneven
ones.** Changing only the summarizer model moved SWE-bench Verified accuracy
from 49.0% to 55.5% with everything else fixed; training the policy to write
its own summaries under task reward added 3–7 points over the base model
(CompactionRL, Jul 2026). Learned compression guidelines (ACON, Oct 2025)
and treating retention as a learned action (Memory as Action, Oct 2025)
both beat fixed rules, and both needed training to do so. Nobody reports an
untrained model choosing *what* to keep as well as a trained one.

**Compression damages reliability before it damages accuracy.** On
AppWorld, summary-based compression underperformed plain FIFO truncation at
moderate budgets, and its main cost was instability: the gap between
solving a task at least once and solving it twice widened as compression
tightened, agents took extra blocked actions immediately after a
compaction, and compressing recent turns hurt more than compressing old
ones. The authors recommend evaluating compaction at the boundary, by
continuing identical states with and without it, rather than by end
results. (Execution Instability, Aug 2026.)

**Timing matters, and a model can judge it given a rubric.** A fixed
threshold "knows nothing about what the model is doing" and fires
mid-derivation. Asking the model periodically, with a short rubric, whether
a reasoning unit is closed and compaction is safe, then letting the same
model summarize, gained up to 18 points on competition math and 5–9 points
at 30–70% lower cost on agentic search, with no training. Two caveats from
the same paper: without the rubric, "some models call the tool at unhelpful
moments, others not at all", and the authors expect frontier models "may
detect context rot without a rubric", so the finding is strongest for
open-weight models. (SelfCompact, Jun 2026.)

**Recoverable beats lossy.** Replacing old observations with short
addressable stubs (an id, a head and tail preview) that the agent can recall
verbatim scored 99.4% on a needle test where the best summarizing baseline
scored 88.1%, with a smaller gain on real tasks "reflecting that reasoning
errors, not recall failures, dominate" (ARC, Jul 2026). Structured eviction
that keeps originals selectively rather than paraphrasing reports the same
direction (Beyond Compaction, Jun 2026). Validating a summary against the
trajectory it replaces catches what it dropped (Slipstream, May 2026).

**Compaction is a serving cost.** With a 16k threshold, compaction fired up
to 15 times a run and took 62% of wall time on one model; summaries do not
follow length instructions, so what is lost is non-deterministic across
runs. Partitioning history and summarizing blocks in parallel gave 1.4–2.1×
throughput and finer control over how much survives. (Parallel Context
Compaction, May 2026.)

**Anthropic's own guidance** orders the levers: clear tool results first
("once a tool has been called deep in the message history, why would the
agent need to see the raw result again?"), compact second with a prompt
tuned first for recall and then for precision, and keep structured notes
outside the window as the durable layer.

## Where our harness stands against this

What the daemon has today is closest to observation eviction with
addressable recall, at turn granularity: the window drops whole old turns,
the request opens with one note saying how many turns are hidden and that
the `history` tool reads them by number, and nothing is paraphrased. That is
the shape the research favors over lossy summaries, with two differences
from the systems that measured well. ARC's stubs carry an id *and a preview
of each evicted item*, so the agent sees what it could recall; our note
carries only a count. And ARC and SelfCompact both put a short structured
prompt in front of the model at the decision point, which is a different
thing from an instruction to act.

Our own [exploratory evaluation](DAEMON_MEASUREMENTS.md#context-quality-before-compaction)
found no `history` calls across the displayed cohorts: 19 conversations and
266 turns, including retained-context turns. The original captures record
the window after the final turn, so they do not establish whether the rule
was already omitted when each file-writing action was chosen. Copying visible
examples or workspace files is a possible explanation for some successes,
not an established cause. The corrected evaluator separates stable retained
and omitted turns from transitional and failed turns; a paid rerun is still
needed before treating those scores as a compaction comparison baseline.

## On the two questions asked before this survey

*Should the model control compaction, with the harness saying "compact now"
at a threshold?* The evidence splits the question in two. On *when*: the
field does not let models decide, Cline's attempt is the one data point and
it went badly at first, and the one careful study found models pick the
moment well only when asked a yes/no rubric at intervals, not when handed a
tool. On *what*: every harness has the model write the summary, so that
half already is model-controlled everywhere; the results say the summary's
quality swings task success by several points and that untrained models are
uneven at it, which is why harnesses constrain it with templates and let
users add standing instructions, and why the trained approaches exist.

*Should the history tool get a hint?* Nobody measured a bare hint. What was
measured is that a recall mechanism the agent can see the shape of (stubs
with previews) is used and works, and that first-party harnesses do not
hesitate to instruct: Anthropic's memory protocol is an all-caps order to
read memory before anything else. The bias concern is real and the field's
answer to it is to make the mechanism legible rather than to command its
use.

## Sources

- Anthropic: [Compaction](https://platform.claude.com/docs/en/build-with-claude/compaction), [Context editing](https://platform.claude.com/docs/en/build-with-claude/context-editing), [Context engineering cookbook](https://platform.claude.com/cookbook/tool-use-context-engineering-context-engineering-tools), [Effective context engineering for AI agents](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents), [Claude Code best practices](https://code.claude.com/docs/en/best-practices)
- OpenAI: [Compaction guide](https://developers.openai.com/api/docs/guides/compaction), [Shell + Skills + Compaction](https://developers.openai.com/blog/skills-shell-tips); measurements by [osolmaz](https://gist.github.com/osolmaz/a38acf6e522df67530e3ed47c80fdcd5)
- Harness sources: [Pi compaction.md](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/docs/compaction.md), [Cline auto compact](https://docs.cline.bot/features/auto-compact) and [issue #5616](https://github.com/cline/cline/issues/5616), [OpenHands condenser](https://docs.openhands.dev/sdk/arch/condenser), [Microsoft Agent Framework compaction](https://learn.microsoft.com/en-us/agent-framework/concepts/agents/conversations/compaction), [Letta memory blocks](https://www.letta.com/blog/memory-blocks/); comparative reading by [badlogic](https://gist.github.com/badlogic/cd2ef65b0697c4dbe2d13fbecb0a0a5f) and [danielvaughan](https://codex.danielvaughan.com/2026/04/10/context-compaction-showdown-coding-agents/)
- Papers: [The Complexity Trap](https://arxiv.org/abs/2508.21433) and the [JetBrains follow-up](https://blog.jetbrains.com/research/2025/12/efficient-context-management/); [CompactionRL](https://arxiv.org/abs/2607.05378); [ACON](https://arxiv.org/abs/2510.00615); [Memory as Action](https://arxiv.org/abs/2510.12635); [Execution instability under compression](https://arxiv.org/abs/2608.06503); [Self-Compacting Language Model Agents](https://arxiv.org/abs/2606.23525); [Addressable Recall Compaction](https://arxiv.org/abs/2607.25066); [Beyond Compaction: Structured Context Eviction](https://arxiv.org/abs/2606.11213); [Slipstream](https://arxiv.org/abs/2605.08580); [Parallel Context Compaction](https://arxiv.org/abs/2605.23296); [Inside the Scaffold](https://arxiv.org/abs/2604.03515)
