# Long conversations with bounded working memory

Requirement recorded 2026-09-07. Extremely long conversation histories are a
first-class design target. Since 2026-09-15 stored history is unbounded and each
request carries a bounded context window of whole turns with an explicit
omission note and a `history` tool for retrieval; see
[RUST_PROTOTYPE.md](RUST_PROTOTYPE.md#long-history-and-context-windows) and the
[measurements](DAEMON_MEASUREMENTS.md#long-history). Since 2026-09-19 the
daemon also compacts a conversation of many turns into a versioned summary
while keeping the original history and fork semantics
([RUST_PROTOTYPE.md](RUST_PROTOTYPE.md#compaction),
[measurements](DAEMON_MEASUREMENTS.md#compaction)). Still future work, as
described below: compaction inside one long turn, and checkpoint indexes for
old-node lookups. [EVIDENCE.md](EVIDENCE.md#operational-behavior) has the
current state.

## Separate the lifetimes

- **Durable conversation:** original messages, tools/results, branch ancestry,
  checkpoints, and compaction records on disk. Large tool outputs should become
  separately stored objects with bounded previews and explicit retrieval.
- **Model context:** a bounded selection of items and summaries constructed for
  a particular request, within that model's token budget.
- **Live runtime state:** current task, cursors, bounded streaming buffers, and
  bounded caches. An inactive bot must not keep its full transcript in RAM.

Compaction creates a versioned context view. Preserve the original conversation
and the exact source range, model/configuration, summary, and parent context view
used to construct it. A historical fork must select a consistent view at its
checkpoint, without summaries that reveal later turns or replaying old effects.
Record tool call/result boundaries and opaque provider reasoning requirements.
Summarization can lose information; retain a way to retrieve the originals.

## Storage and context work

Decouple stored-node count/bytes from per-request context limits. Add indexed
range/ancestry lookup and periodic checkpoint indexes so resuming a long bot or
forking an old checkpoint does not walk every prior node. Stream selected encoded
items from disk with bounded read-ahead. A provider token budget and a byte budget
are different constraints; neither can substitute for the other.

Keep the hot tail and shared context buffers under explicit cache budgets.
Persist a new compacted view atomically; a crash during compaction must retain a
usable prior view. Concurrent forks should share immutable history and completed
summaries; they must not all independently summarize the same prefix. Configure
summary work, tool concurrency, context reads, and model requests independently.
Any deletion/retention policy is separate from compaction and needs explicit
caller semantics. Do not silently drop transcript data to win a benchmark.

## Performance and behavior gates

The [Prime Intellect investigation](PRIME_INTELLECT.md) supplies a useful context
retrieval reference. Offer bounded history search/range reads and artifact
handles so a model can recover originals after compaction. Scope every read to
the selected branch and checkpoint, with explicit result byte limits and paging.
Persistent Python kernels and recursive model calls are optional capabilities,
not prerequisites for indexed retrieval. Their live state and inference budgets
must be accounted for separately if added.

First specify disk and run-duration budgets, then generate synthetic histories
at increasing lengths (for example 10,000, 100,000, and 1,000,000 stored items).
These are proposed scales, not measured capacity or a request to allocate them now.

At a fixed active model-context size, measure resume latency, historical-fork
latency, incremental RAM, context construction CPU, disk reads/writes, network
request bytes, and warm/cold cache behavior as total stored history grows.
Test a small number of active bots amid many stored bots. Separately scale
selected context size, concurrent active agents, and slow output consumers.

For compaction, measure summary calls/tokens/latency and additional storage, plus
behavioral tests for preserved constraints, required facts, tool boundaries,
recovery, and fork isolation. Cheap compaction that loses task-critical facts is
not an improvement. No paid summarization runs are authorized by this roadmap.

## Compaction plan

Recorded 2026-09-16. The window drops whole old turns, says how many are
missing, and offers the `history` tool: retrieval, not compaction. Compaction
is what the model sees once a task outgrows the window, and it has to be
efficient twice over: in tokens and cache hits per turn, and in preserving
what the task needs to finish correctly. Those pull in different directions,
so build it in layers, cheapest and most faithful first, and let the
task-quality evaluation decide how far down the list to go.

1. **Elide old tool results, keep everything else.** In a coding turn most
   bytes are tool output, not the model's words. Once a turn is older than N,
   the request carries each of its tool results as a one-line stub: the call,
   the exit status, the size, and the artifact reference the `read` tool
   already resolves. Prompts and replies stay verbatim. No model call,
   deterministic, cache-friendly, and lossless because the original is one
   tool call away. This should let the window hold several times as many
   turns and composes with the window's hysteresis, artifacts, and the
   history tool as they are. Built 2026-09-26 as [tool-result
   elision](RUST_PROTOTYPE.md#tool-result-elision), triggered by size
   rather than age, and within the running turn too: a versioned floor
   below the model's newest output, a stub with the size, head and tail
   excerpts, and a `result/NODE` read reference.
2. **A pinned, agent-owned note.** One durable item per bot, always first in
   context, and a tool that rewrites it. The agent records what it knows it
   will need: constraints, decisions, paths, what is left. The harness
   guarantees the slot; the model decides the content, being the only party
   that knows what matters for its task. Rewritten only when the agent
   chooses, so the cached prefix holds. This is the preserve-the-right-things
   mechanism, and it is a primitive, not a policy.
3. **Model summaries as versioned views**, only for what the first two leave
   uncovered. When turns leave the window, one bounded call summarizes only
   the departing turns against the previous summary, so cost per turn is
   constant rather than re-summarizing the transcript. The summary is a view
   stored beside the history, never a replacement, and a fork binds to the
   view valid at its checkpoint, as above. A structured shape (constraints,
   decisions, open items, facts with their turn numbers) preserves more than
   prose and lets the model fetch a source turn by ordinal. Concurrent forks
   share completed summaries rather than each paying for the same prefix.
   Since 2026-09-26 a summary can also [cut inside the running
   turn](RUST_PROTOTYPE.md#cuts-inside-a-turn), at a round after a
   completed tool exchange, keeping that turn's prompt whole ahead of the
   tail, so one long task can compact several times before it ends.

Prior art to read before building, with what to take from each: Prime
Agent's compaction (summary plus retained originals, rebuilding context from
the summary; see [PRIME_INTELLECT.md](PRIME_INTELLECT.md)), Pi's context
transformation hooks, Codex's native compaction, Claude Code's whole-transcript
summarization with pinned memory files, and FX's in-memory conversations
(see [RUNTIMES.md](RUNTIMES.md)). None of them is measured on cache hits under
compaction, which is where a fleet's cost actually lands.

Measure every layer on the same long conversation, live and synthetic:

- tokens per turn as the conversation grows, and where the curve flattens;
- cache-hit ratio per turn (recorded per turn and per bot since item 13 of
  [NEXT.md](NEXT.md)), since a compaction that rewrites the prefix every turn
  can cost more than the tokens it saves. The live baseline without
  compaction: 0.72 on luna and 0.85 on Sonnet over 48 turns with a
  12 KiB window, every window move a full-miss turn and the turns between
  at 0.91 to 0.95; a compaction that rewrites the prefix has to beat
  that;
- compaction cost itself: summary calls, tokens, latency, and storage;
- quality on the task-level evaluation (item 15): a constraint appears early,
  work pushes it out of the window, a later decision depends on it; measure
  whether the agent retrieves it and acts on it, and record what each layer
  lost when it fails;
- fork isolation: a fork's context never contains later or sibling-branch
  material.

If layers 1 and 2 pass that evaluation on real tasks, layer 3 may not be
worth its cost or its risk of summarizing away the wrong thing. No paid
summarization runs are authorized by this plan; the evaluation is built
first, then the layers, each measured against it.
