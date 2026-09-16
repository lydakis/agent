# Long conversations with bounded working memory

Requirement recorded 2026-09-07. Extremely long conversation histories are a
first-class design target. Since 2026-09-15 stored history is unbounded and each
request carries a bounded context window of whole turns with an explicit
omission note and a `history` tool for retrieval; see
[RUST_PROTOTYPE.md](RUST_PROTOTYPE.md#long-history-and-context-windows) and the
[measurements](DAEMON_MEASUREMENTS.md#long-history). Compaction with summaries,
versioned context views, and checkpoint indexes for old-node lookups remain
future work described below.

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
