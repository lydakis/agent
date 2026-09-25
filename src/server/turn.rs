//! One turn: model rounds, committed tool plans, tool execution, and the
//! terminal event. Tool failures are results the model sees; only the runtime
//! failing ends a turn early. A `wait` parks the turn: the task ends, the
//! store holds the state (including any calls still to run), and a resumed
//! task records the results and carries on.
//!
//! No transcript lives in memory. Each model call streams the bot's bounded
//! context window from the store in batches; items appended during the turn
//! are committed before the next call and read back like any other.
use super::{
    handles::{Completion, Handle, Handles, Waiter, now_ms, wait_result},
    hub::Hub,
};
use agent_runtime::{
    Error, Result,
    codec::split_model,
    fail,
    provider::{Chain, Delta, Items, Provider, Report, Request as ModelRequest, ToolCall},
    store::{ContextPrefix, ContextUsage, Store, Window},
    tools::{Outcome, Prepared, ReadSource, Registry},
};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
};
use tokio::sync::{mpsc, oneshot, watch};

pub const MAX_ROUNDS: usize = 200;
/// Attempts per model call before its failure is the turn's failure, and the
/// wall-clock budget those attempts may span. A model call has no side
/// effects, so retrying one is always safe; a tool is never rerun.
const MAX_ATTEMPTS: u32 = 8;
/// Refusals for pace are spaced by the pool and are not the request's fault,
/// so they get many more attempts inside the same time budget.
const MAX_PACED_ATTEMPTS: u32 = 64;
use agent_runtime::provider::pace::MIN_PARK;
const RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(300);
/// Nodes per piece of a compaction catch-up walk: a few milliseconds of
/// metadata reads, after which other bots' reads may run.
const CATCH_UP_PIECE_NODES: i64 = 1024;
/// Read-ahead while a body streams: a batch stops at either bound, so the
/// memory held per in-flight request is a number, not a function of item
/// sizes. An item larger than the byte bound travels alone.
const WINDOW_BATCH: usize = 64;
const WINDOW_BATCH_BYTES: u64 = 256 * 1024;

/// Stored items to stream: ids with their sizes, and the bytes each loses
/// without thinking.
#[derive(Clone, Copy)]
struct Span<'a> {
    ids: &'a [i64],
    sizes: &'a [u32],
    thinking: &'a [u32],
}
impl Span<'_> {
    const EMPTY: Span<'static> = Span {
        ids: &[],
        sizes: &[],
        thinking: &[],
    };
}

/// A stable fingerprint of the context in front of a window: its encoded
/// prefix and first item. FNV-1a, so it survives restarts and upgrades.
fn fingerprint(prefix: &[u8], first: i64) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in prefix.iter().chain(&first.to_le_bytes()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash as i64
}

/// Split window items into read-ahead batches by count and by bytes.
fn batches(ids: &[i64], sizes: &[u32]) -> Vec<Vec<i64>> {
    let mut out: Vec<Vec<i64>> = Vec::with_capacity(ids.len().div_ceil(WINDOW_BATCH));
    let mut bytes = 0u64;
    for (index, id) in ids.iter().enumerate() {
        let size = u64::from(sizes.get(index).copied().unwrap_or(0));
        let full = out
            .last()
            .is_some_and(|b| b.len() >= WINDOW_BATCH || bytes + size > WINDOW_BATCH_BYTES);
        if out.is_empty() || full {
            out.push(Vec::new());
            bytes = 0;
        }
        out.last_mut().unwrap().push(*id);
        bytes += size;
    }
    out
}

/// Stored items read batch by batch and joined by commas. A batch can come
/// back empty when every item in it held only replayed thinking, so the
/// separator goes only between batches that carry something.
fn item_chunks(
    store: Store,
    chunks: Arc<[Vec<i64>]>,
    floor: i64,
) -> impl futures_util::Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    let mut started = false;
    stream::iter(0..chunks.len())
        .then(move |index| {
            let (store, chunks) = (store.clone(), chunks.clone());
            async move {
                store
                    .read("items_by_ids", move |db| {
                        db.items_by_ids(&chunks[index], floor)
                    })
                    .await
                    .map_err(|error| std::io::Error::other(error.code))
            }
        })
        .map(move |bytes| {
            let mut bytes = bytes?;
            if started && !bytes.is_empty() {
                bytes.insert(0, b',');
            }
            started |= !bytes.is_empty();
            Ok(Bytes::from(bytes))
        })
}

/// The Responses prompt-cache key for a bot's calls: the store id of the bot
/// whose cache it shares (its own, or a fork's source) under a nonce drawn
/// once per daemon, so bots of different stores (every Harbor
/// container's first bot is id 1) never share a key. A restart costs each bot
/// one cache miss. Summaries have their own prefix, so their own key.
fn cache_key(bot: i64, summary: bool) -> String {
    use std::hash::BuildHasher;
    static NONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let nonce = *NONCE.get_or_init(|| {
        std::collections::hash_map::RandomState::new().hash_one(std::process::id())
    });
    let suffix = if summary { "-summary" } else { "" };
    format!("{nonce:016x}-{bot}{suffix}")
}

pub struct Turn {
    pub bot: String,
    pub turn: i64,
    pub store: Store,
    pub providers: Arc<HashMap<String, Provider>>,
    pub registry: Registry,
    pub hub: Hub,
    pub handles: Handles,
    pub background_failures: mpsc::UnboundedSender<Error>,
    pub context_bytes: usize,
    pub context_items: usize,
    /// Omitted turns the context note lists; zero for the bare count.
    pub note_turns: usize,
    /// Compaction threshold and verbatim tail, as percentages of the budget.
    pub compact_at: usize,
    pub compact_keep: usize,
    /// Continue a parked turn: record its wait results, then keep going.
    pub resume: bool,
    /// A steer for this bot may be queued. Set by the service, cleared by
    /// the boundary before it reads, so unrelated bots never pay for one
    /// bot's pending steer.
    pub steers: Arc<AtomicBool>,
    pub tokens: Arc<TokenTotals>,
}

/// Provider-reported tokens across every turn since the daemon started:
/// three relaxed atomics, no lock, no storage read for `stats`.
#[derive(Default)]
pub struct TokenTotals {
    input: std::sync::atomic::AtomicU64,
    cached_input: std::sync::atomic::AtomicU64,
    output: std::sync::atomic::AtomicU64,
}
impl TokenTotals {
    pub fn add(&self, usage: &agent_runtime::provider::Usage) {
        self.input.fetch_add(usage.input_tokens, Relaxed);
        self.cached_input
            .fetch_add(usage.cached_input_tokens, Relaxed);
        self.output.fetch_add(usage.output_tokens, Relaxed);
    }
    pub fn snapshot(&self) -> serde_json::Value {
        let (input, cached, output) = (
            self.input.load(Relaxed),
            self.cached_input.load(Relaxed),
            self.output.load(Relaxed),
        );
        json!({"input_tokens":input,"cached_input_tokens":cached,"output_tokens":output,
            "cache_hit":agent_runtime::store::cache_hit(cached as i64, input as i64)})
    }
}

/// The last model call of a turn, kept so its prompt cache can be refreshed
/// while the calls it planned run: see [`Turn::run_tool`].
struct Warm<'a> {
    provider: &'a Provider,
    model: &'a str,
    instructions: &'a str,
    reasoning: Option<&'a str>,
    tools: &'a serde_json::value::RawValue,
    context: &'a Context,
    after: std::time::Duration,
    /// When the cache was last read, by the call or a refresh.
    read_at: tokio::time::Instant,
    stopped: bool,
    /// Tokens the refreshes billed, for the bot's budget.
    tokens: u64,
}

/// Lives outside the cancellable rounds future. No allocation or per-attempt
/// storage write: each execution segment flushes once, including when parked.
#[derive(Default)]
struct Accounting {
    retries: u64,
    paced_ms: u64,
    retrying: bool,
    report: Report,
    /// Attempts and non-pacing time spent by the unfinished model call.
    call_attempts: u32,
    call_spent_ms: u64,
    /// Set when a call parked the turn on a closed pool.
    parked_until: u64,
    /// Persist the call's phase with its existing park transaction.
    compaction: bool,
}
impl Accounting {
    fn totals(&self) -> (u64, u64) {
        (
            self.retries + u64::from(self.retrying && self.report.dispatched),
            self.paced_ms + self.report.paced_ms,
        )
    }
    fn begin(&mut self, retrying: bool) {
        (self.retries, self.paced_ms) = self.totals();
        self.report = Report::default();
        self.retrying = retrying;
    }
}

enum Round {
    Finished,
    /// The turn is parked or was ended elsewhere; nothing to finish here.
    Parked,
    /// Parked on a closed pool until the given time.
    Paced(u64),
}

pub enum Exit {
    Finished(Option<Error>),
    Parked,
    /// Parked on a rate-limited pool; the service resumes it at this time.
    Paced(u64),
}

/// Completion as one storage job: the terminal event, the outcome its
/// waiters get, and retention, in that order. The worker publishes all of
/// it after the job commits.
pub struct Finished;

impl Finished {
    pub fn record(
        db: &mut agent_runtime::store::Database,
        bot: &str,
        turn: i64,
        error: Option<&Error>,
        keep: Option<usize>,
    ) -> Result<()> {
        db.finish(turn, error)?;
        let outcome = db
            .turn_outcome(bot, turn)?
            .ok_or_else(|| Error::new("stale_turn"))?;
        db.announce(bot, turn, outcome);
        // A later steer may already be terminal, placing this completion
        // outside retention: the outcome is captured above, and the turn's
        // own records are kept so its terminal event is published.
        if let Some(keep) = keep {
            db.prune_except(bot, keep, Some(turn))?;
        }
        Ok(())
    }
}

impl Turn {
    pub async fn execute(&self, mut cancelled: watch::Receiver<bool>) -> Exit {
        // Only an explicit interrupt cancels. A dropped sender (the service
        // replacing this task's slot) must not end the turn.
        let interrupt = async {
            if cancelled.wait_for(|c| *c).await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        let mut accounting = Accounting::default();
        // Cancelling mid-job loses nothing: a job the worker has taken runs
        // to its commit, and the worker publishes whatever committed.
        let mut result = tokio::select! {
            biased;
            _ = interrupt => fail("cancelled"),
            result = self.rounds(&mut accounting) => result,
        };
        // The rounds future is gone, so cancellation cannot discard this flush.
        let (retries, paced_ms) = accounting.totals();
        let turn = self.turn;
        let flushed = if let Ok(Round::Paced(at)) = &result {
            let (at, attempts, spent) = (*at, accounting.call_attempts, accounting.call_spent_ms);
            let compaction = accounting.compaction;
            // Commit the park and its accounting together, outside cancellation.
            self.store
                .op("suspend_paced", move |db| {
                    db.suspend_paced(turn, at, attempts, spent, retries, paced_ms, compaction)
                        .map(|_| ())
                })
                .await
        } else if retries > 0 || paced_ms > 0 {
            self.store
                .op("note_pacing", move |db| {
                    db.note_pacing(turn, retries, paced_ms)
                })
                .await
        } else {
            Ok(())
        };
        if let Err(error) = flushed {
            result = Err(error);
        }
        // An interrupt may arrive while the non-cancellable flush is pending.
        // Honor it before retiring a parked task whose receiver is still live.
        if result.is_ok() && *cancelled.borrow() {
            result = fail("cancelled");
        }
        let error = match result {
            Ok(Round::Parked) => return Exit::Parked,
            Ok(Round::Paced(resume_at_ms)) => return Exit::Paced(resume_at_ms),
            Ok(Round::Finished) => None,
            Err(error) => {
                self.handles.forget(Waiter::Turn(self.turn));
                Some(error)
            }
        };
        // Leave the bot durably busy until the service can commit completion
        // and publish it before dispatching a subsequent submission.
        Exit::Finished(error)
    }

    /// Prepare metadata and the encoded prefix once per model round. Retries
    /// stream the same immutable nodes without rebuilding the context view.
    async fn context(&self, bytes: usize, count: usize, prefix_budget: usize) -> Result<Context> {
        let bot = self.bot.clone();
        let window = self
            .store
            .op("window", move |db| {
                db.window(&bot, bytes as i64, count as i64)
            })
            .await?;
        let Some(window) = window else {
            return Ok(Context::empty());
        };
        let prefix = self.prefix(&window, prefix_budget).await?;
        Ok(Context {
            window: Some(window),
            prefix,
            thinking_floor: 0,
        })
    }

    async fn prefix(
        &self,
        window: &agent_runtime::store::Window,
        budget: usize,
    ) -> Result<ContextPrefix> {
        let listed = if window.omitted_items > 0 && self.note_turns > 0 {
            let (start, limit) = (window.ids[0], self.note_turns);
            self.store
                .read("omitted_turns", move |db| db.omitted_turns(start, limit))
                .await?
        } else {
            Vec::new()
        };
        window.prefix(&listed, budget)
    }

    /// The configured envelope bounds input. Completion headroom is a soft
    /// compaction trigger, not a tax on every request's usable history.
    fn input_limit(&self) -> ContextUsage {
        ContextUsage {
            bytes: self.context_bytes,
            items: self.context_items,
        }
    }

    async fn fit_context(&self, mut context: Context, limit: ContextUsage) -> Result<Context> {
        let mut raw = ContextUsage {
            bytes: self.context_bytes,
            items: self.context_items,
        };
        let mut prefix_budget = limit.bytes * 2 / 3;
        while !context.usage().fits(limit) {
            let next = ContextUsage {
                bytes: raw
                    .bytes
                    .min(limit.bytes.saturating_sub(context.prefix.bytes.len())),
                items: raw
                    .items
                    .min(limit.items.saturating_sub(context.prefix.items)),
            };
            if next.bytes == 0 || next.items == 0 {
                return agent_runtime::fail_with(
                    "context_limit",
                    "pinned context and the current turn exceed the input budget",
                );
            }
            match self.context(next.bytes, next.items, prefix_budget).await {
                Ok(resized) => {
                    raw = next;
                    context = resized;
                }
                Err(error) if error.code == "context_limit" && self.note_turns > 0 => {
                    // Normal trimming needs no extra lookup. Only a current
                    // turn that cannot fit beside the previews needs its
                    // indexed minimum and a smaller optional listing.
                    let (bot, turn) = (self.bot.clone(), self.turn);
                    let (_, bytes, items) = self
                        .store
                        .read("turn_usage", move |db| db.turn_usage(&bot, turn))
                        .await?;
                    let smaller = prefix_budget
                        .min(limit.bytes.saturating_sub(bytes + items.saturating_sub(1)));
                    if smaller == prefix_budget {
                        return Err(error);
                    }
                    prefix_budget = smaller;
                    if let Some(window) = &context.window {
                        context.prefix = self.prefix(window, prefix_budget).await?;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(context)
    }

    fn items(&self, context: &Context) -> Items {
        match &context.window {
            Some(w) => self.window_items(
                context.prefix.bytes.clone(),
                Span {
                    ids: &w.ids,
                    sizes: &w.sizes,
                    thinking: &w.thinking,
                },
                context.thinking_floor,
            ),
            None => self.window_items(context.prefix.bytes.clone(), Span::EMPTY, 0),
        }
    }

    /// Anthropic thinking replay. Each thinking block is bound to the exact
    /// context before it, so once the context in front of the window changes
    /// (the window slides, a compaction or note lands, or a fork starts from
    /// other instructions) the blocks written under the old one are sent
    /// without thinking; blocks written after the change keep theirs. Other
    /// families send items unchanged.
    async fn thinking_floor(
        &self,
        record: &mut agent_runtime::store::Bot,
        context: &mut Context,
    ) -> Result<()> {
        let Some(window) = context
            .window
            .as_ref()
            .filter(|w| w.family == agent_runtime::codec::Family::Anthropic && !w.ids.is_empty())
        else {
            return Ok(());
        };
        let prefix = fingerprint(&context.prefix.bytes, window.ids[0]);
        if record.thinking_prefix != Some(prefix) {
            let floor = window.ids[window.ids.len() - 1] + 1;
            let bot = self.bot.clone();
            self.store
                .op("set_thinking", move |db| {
                    db.set_thinking(&bot, prefix, floor)
                })
                .await?;
            record.thinking_prefix = Some(prefix);
            record.thinking_floor = floor;
        }
        context.thinking_floor = record.thinking_floor;
        Ok(())
    }

    /// The context prefix, then the items of `ids` read from the store in
    /// batches as the request streams; items below `floor` without thinking.
    fn window_items(&self, prefix: Bytes, span: Span<'_>, floor: i64) -> Items {
        let Span {
            ids,
            sizes,
            thinking,
        } = span;
        let stripped: usize = ids
            .iter()
            .zip(thinking)
            .filter(|(id, _)| **id < floor)
            .map(|(_, &bytes)| bytes as usize)
            .sum();
        let total = (prefix.len()
            + sizes.iter().map(|&size| size as usize).sum::<usize>()
            + ids.len().saturating_sub(1))
        .saturating_sub(stripped);
        let (store, chunks): (_, Arc<[_]>) = (self.store.clone(), batches(ids, sizes).into());
        Items::new(total, move || {
            stream::iter([Ok(prefix.clone())])
                .chain(item_chunks(store.clone(), chunks.clone(), floor))
                .boxed()
        })
    }

    /// Compaction at a round boundary: once the turns since the last summary
    /// hold `compact_at` percent of the budget, summarize everything older
    /// than the newest `compact_keep` percent with the client's instructions
    /// and summarizer, and record the result as the new context start. A
    /// backlog larger than the budget is summarized oldest first, one bounded
    /// span per round boundary, until it fits. A failed summary
    /// leaves the context view unchanged and is reported live; the turn goes on
    /// with the window as it is. Returns a park time if the summarizer's
    /// call parked the turn.
    #[allow(clippy::too_many_arguments)]
    async fn compact_if_due(
        &self,
        record: &mut agent_runtime::store::Bot,
        model_rounds: &mut usize,
        turn: i64,
        accounting: &mut Accounting,
        tools: &serde_json::value::RawValue,
        context: &mut Context,
        output_bytes: Option<usize>,
    ) -> Result<Option<u64>> {
        if record.compaction_instructions.is_none() {
            return Ok(None);
        }
        let pressure = context.pressure();
        let limit = self.input_limit();
        let reserve = output_bytes.unwrap_or(0).min(limit.bytes / 4);
        if pressure.bytes < (self.context_bytes / 100 * self.compact_at).min(limit.bytes - reserve)
            && pressure.items < (self.context_items * self.compact_at / 100).min(limit.items)
        {
            return Ok(None);
        }
        let keep = (self.context_bytes / 100 * self.compact_keep)
            .min(
                limit.bytes.saturating_sub(context.prefix.bytes.len()) * self.compact_keep
                    / self.compact_at,
            )
            .max(1) as i64;
        let keep_items = (self.context_items * self.compact_keep / 100)
            .min(
                limit.items.saturating_sub(context.prefix.items) * self.compact_keep
                    / self.compact_at,
            )
            .max(1) as i64;
        let (max_bytes, max_items) = (limit.bytes as i64, limit.items as i64);
        // Nodes are immutable and only this turn moves the bot's head, so
        // the reader's snapshot plans what the worker would. A catch-up walk
        // over a long backlog goes in pieces, so neither other bots' commits
        // nor their context reads wait behind all of it.
        let plan = match self
            .plan_compaction(keep, keep_items, max_bytes, max_items)
            .await
        {
            Ok(Some(plan)) => plan,
            Ok(None) => return Ok(None),
            Err(error) if error.code == "compaction_span_limit" => {
                self.compaction_failed(turn, &error).await?;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let reference = summarizer(record);
        let (name, model) = split_model(&reference)?;
        let Some(summarizer) = self.providers.get(name) else {
            self.hub
                .live(
                    &self.bot,
                    json!({"event":"compaction_failed","bot":self.bot,"turn":turn,"durable":false,
                        "error":"provider_unavailable","detail":name}),
                )
                .await?;
            return Ok(None);
        };
        let instructions = record.compaction_instructions.clone().unwrap();
        // Messages requires definitions for historical tool blocks. Reuse the
        // already encoded bot selection; tool_choice disables new calls.
        // Responses accepts historical calls without definitions.
        let empty;
        let summary_tools = match summarizer.family() {
            agent_runtime::codec::Family::Anthropic => tools,
            agent_runtime::codec::Family::Responses => {
                empty = self.registry.encoded(summarizer.family(), &[])?;
                &empty
            }
        };
        let completion = match self
            .call_with(
                summarizer,
                model,
                &instructions,
                summary_tools,
                Body::Span(&plan),
                record,
                model_rounds,
                turn,
                accounting,
            )
            .await
        {
            Ok(Some(completion)) => completion,
            Ok(None) => return Ok(Some(accounting.parked_until)),
            Err(error) => {
                self.hub
                    .live(
                        &self.bot,
                        json!({"event":"compaction_failed","bot":self.bot,"turn":turn,"durable":false,
                            "error":error.code,"detail":error.detail}),
                    )
                    .await?;
                return Ok(None);
            }
        };
        // Success is billable even if its text is empty or too large to use.
        *model_rounds += 1;
        if let Some(usage) = &completion.usage {
            record.tokens_used = record
                .tokens_used
                .saturating_add(usage.input_tokens)
                .saturating_add(usage.output_tokens);
        }
        let usage = completion
            .usage
            .map(|usage| summarizer_usage(usage, name, model));
        let summary = completion_text(&completion.items);
        let invalid = if summary.len() > plan.summary_bytes {
            Some(Error::new("compaction_summary_limit"))
        } else if summary.trim().is_empty() {
            Some(Error::new("empty_summary"))
        } else {
            None
        };
        if let Some(error) = invalid {
            self.store
                .op("compaction_usage", move |db| {
                    db.compaction_usage(turn, usage.as_ref())
                })
                .await?;
            self.compaction_failed(turn, &error).await?;
            return Ok(None);
        }
        let bot = self.bot.clone();
        let billed = usage.clone();
        let note_turns = self.note_turns;
        let input_limit = self.input_limit();
        if let Err(error) = self
            .store
            .op("compact", move |db| {
                db.compact(
                    &bot,
                    &plan,
                    &summary,
                    usage.as_ref(),
                    note_turns,
                    input_limit,
                )
            })
            .await
        {
            self.store
                .op("compaction_usage", move |db| {
                    db.compaction_usage(turn, billed.as_ref())
                })
                .await?;
            if matches!(
                error.code.as_str(),
                "compaction_not_smaller" | "compaction_context_limit"
            ) {
                self.compaction_failed(turn, &error).await?;
                return Ok(None);
            }
            return Err(error);
        }
        *context = self
            .context(
                self.context_bytes,
                self.context_items,
                self.context_bytes * 2 / 3,
            )
            .await?;
        Ok(None)
    }

    async fn plan_compaction(
        &self,
        keep: i64,
        keep_items: i64,
        max_bytes: i64,
        max_items: i64,
    ) -> Result<Option<agent_runtime::store::CompactionPlan>> {
        use agent_runtime::store::Planning;
        let bot = self.bot.clone();
        let planning = self
            .store
            .read("compaction_plan", move |db| {
                db.compaction_plan(&bot, keep, keep_items, max_bytes, max_items)
            })
            .await?;
        let mut walk = match planning {
            None => return Ok(None),
            Some(Planning::Plan(plan)) => return Ok(Some(plan)),
            Some(Planning::CatchUp(walk)) => walk,
        };
        while !walk.done() {
            walk = self
                .store
                .read("catch_up_piece", move |db| {
                    db.catch_up_piece(&mut walk, CATCH_UP_PIECE_NODES)?;
                    Ok(walk)
                })
                .await?;
        }
        let bot = self.bot.clone();
        self.store
            .read("catch_up_plan", move |db| db.catch_up_plan(&bot, walk))
            .await
    }

    async fn compaction_failed(&self, turn: i64, error: &Error) -> Result<()> {
        self.hub
            .live(
                &self.bot,
                json!({"event":"compaction_failed","bot":self.bot,
            "turn":turn,"durable":false,"error":error.code,"detail":error.detail}),
            )
            .await
    }

    /// The summarizer's request body: the previous summary, if any, then
    /// the span's items in store-read batches, then the request to write.
    async fn span_items(&self, plan: &agent_runtime::store::CompactionPlan) -> Result<Items> {
        let bot = self.bot.clone();
        let family = self
            .store
            .op("inspect", move |db| db.inspect(&bot)?.family())
            .await?;
        let (head, tail) = agent_runtime::store::CompactionPlan::frame(
            family,
            plan.previous_summary.as_deref(),
            plan.summary_bytes,
        )?;
        let total = head.len()
            + plan.sizes.iter().map(|s| *s as usize).sum::<usize>()
            + plan.ids.len().saturating_sub(1)
            + tail.len();
        if total > self.input_limit().bytes
            || plan.ids.len() + 1 + usize::from(plan.previous_summary.is_some())
                > self.input_limit().items
        {
            return fail("compaction_input_limit");
        }
        // The summarizer's instructions differ from the bot's, so no
        // thinking block in the span is bound to this request.
        let stripped: usize = if family == agent_runtime::codec::Family::Anthropic {
            let ids = plan.ids.clone();
            self.store
                .read("thinking_of", move |db| db.thinking_of(&ids))
                .await?
                .iter()
                .map(|&bytes| bytes as usize)
                .sum()
        } else {
            0
        };
        let total = total.saturating_sub(stripped);
        let (head, tail) = (Bytes::from(head), Bytes::from(tail));
        let (store, chunks): (_, Arc<[_]>) =
            (self.store.clone(), batches(&plan.ids, &plan.sizes).into());
        Ok(Items::new(total, move || {
            stream::iter([Ok(head.clone())])
                .chain(item_chunks(store.clone(), chunks.clone(), i64::MAX))
                .chain(stream::iter([Ok(tail.clone())]))
                .boxed()
        }))
    }

    async fn rounds(&self, accounting: &mut Accounting) -> Result<Round> {
        let (bot, turn) = (self.bot.clone(), self.turn);
        let mut record = self.store.op("inspect", move |db| db.inspect(&bot)).await?;
        let context = self.store.op("context", move |db| db.context(turn)).await?;
        let (provider, model) = split_model(&context.model)?;
        let provider = self
            .providers
            .get(provider)
            .ok_or(Error::with("provider_unavailable", provider))?;
        // The daemon runs whatever provider set started it; the one thing a
        // conversation needs is that its provider speaks the bot's encoding.
        if provider.family() != record.family()? {
            return Err(Error::with(
                "provider_family_mismatch",
                context.model.as_str(),
            ));
        }
        let workspace = PathBuf::from(&context.workspace);
        // What this turn's children inherit: the CLI a bot runs to delegate
        // needs a model for the peer (its own by default), the bot's own name
        // so a created peer records who created it, and its creator so a peer
        // can address the bot that spawned it.
        let mut environment = vec![
            ("AGENT_MODEL".to_owned(), context.model.clone()),
            ("AGENT_BOT".to_owned(), context.bot.clone()),
            ("AGENT_BOT_ID".to_owned(), context.bot_id.to_string()),
        ];
        if let (Some(parent), Some(id)) = (&context.created_by, context.created_by_id) {
            environment.push(("AGENT_PARENT".to_owned(), parent.clone()));
            environment.push(("AGENT_PARENT_ID".to_owned(), id.to_string()));
        }
        let mut resume_window = false;
        if self.resume {
            let (waiting, _, steers) =
                match self.store.op("resume", move |db| db.resume(turn)).await {
                    Ok(resumed) => resumed,
                    Err(error) if error.code == "turn_not_waiting" => return Ok(Round::Parked),
                    Err(error) => return Err(error),
                };
            if steers {
                // Queued while parked: absorbed at the first boundary below.
                self.steers.store(true, Relaxed);
            }
            // Only a pool park continues the same model call's retry budget.
            if waiting.paced_since_ms.is_some() {
                accounting.call_attempts = waiting.call_attempts;
                accounting.call_spent_ms = waiting.call_spent_ms;
                resume_window = !waiting.compaction;
            } else {
                let outcome = Outcome::text(wait_result(self.handles.take(turn)).to_string());
                let id = waiting.call_id;
                self.store
                    .op("tool_finish", move |db| db.tool_finish(turn, &id, &outcome))
                    .await?;
                // Calls that followed the wait in the same model response.
                if self
                    .execute_calls(
                        waiting.pending,
                        &workspace,
                        &environment,
                        &record.tools,
                        None,
                    )
                    .await?
                {
                    return Ok(Round::Parked);
                }
            }
        }
        let mut model_rounds = context.model_rounds;
        // This bot's tools, encoded once per distinct selection and shared.
        let tools = self.registry.encoded(provider.family(), &record.tools)?;
        // Steers submitted since the last boundary go in before this call.
        self.absorb().await?;
        while model_rounds < MAX_ROUNDS {
            // The budget is checked before each call, so one call may overshoot.
            if let Some(error) = budget_error(record.budget_tokens, record.tokens_used) {
                return Err(error);
            }
            // Past the threshold, the older turns are summarized before this
            // call, with the summarizer's own call paced and billed like any.
            // Resume the parked call, not the whole boundary. In particular,
            // an exhausted summary must not start over when the ordinary call
            // parks on the same pool. A later model round may compact again.
            let mut context = self
                .context(
                    self.context_bytes,
                    self.context_items,
                    self.context_bytes * 2 / 3,
                )
                .await?;
            if !std::mem::take(&mut resume_window)
                && let Some(parked) = self
                    .compact_if_due(
                        &mut record,
                        &mut model_rounds,
                        turn,
                        accounting,
                        &tools,
                        &mut context,
                        provider.output_byte_estimate(model),
                    )
                    .await?
            {
                return Ok(Round::Paced(parked));
            }
            if let Some(error) = budget_error(record.budget_tokens, record.tokens_used) {
                return Err(error);
            }
            if model_rounds >= MAX_ROUNDS {
                return fail("tool_round_limit");
            }
            let mut context = self.fit_context(context, self.input_limit()).await?;
            self.thinking_floor(&mut record, &mut context).await?;
            // No earlier than the call's cache read; pacing only moves it later.
            let read_at = tokio::time::Instant::now();
            let Some(response) = self
                .call(
                    provider,
                    model,
                    &tools,
                    &context,
                    &mut record,
                    &mut model_rounds,
                    turn,
                    accounting,
                )
                .await?
            else {
                return Ok(Round::Paced(accounting.parked_until));
            };
            model_rounds += 1;
            if let Some(usage) = &response.usage {
                record.tokens_used = record
                    .tokens_used
                    .saturating_add(usage.input_tokens)
                    .saturating_add(usage.output_tokens);
            }
            let items = response.items;
            let calls: Vec<ToolCall> = response.calls.clone();
            let usage = response.usage.clone();
            match self
                .store
                .op("append", move |db| {
                    db.append(turn, items, &calls, usage.as_ref())
                })
                .await
            {
                Ok(entries) => {
                    let nodes: Vec<i64> = entries
                        .iter()
                        .filter(|entry| entry["event"] == "message")
                        .filter_map(|entry| entry["data"]["node"].as_i64())
                        .collect();
                    provider.recorded(&self.bot, &nodes);
                }
                Err(error) => {
                    self.failed_usage(response.usage.clone()).await?;
                    return Err(error);
                }
            }
            if response.calls.is_empty() {
                // A steer that arrived during the final call keeps the turn
                // going for one more round rather than ending it unheard.
                if self.absorb().await? {
                    continue;
                }
                return Ok(Round::Finished);
            }
            let mut warm = provider
                .keep_warm_after(model, record.reasoning.as_deref())
                .map(|after| Warm {
                    provider,
                    model,
                    instructions: &record.instructions,
                    reasoning: record.reasoning.as_deref(),
                    tools: &tools,
                    context: &context,
                    after,
                    read_at,
                    stopped: false,
                    tokens: 0,
                });
            let parked = self
                .execute_calls(
                    response.calls,
                    &workspace,
                    &environment,
                    &record.tools,
                    warm.as_mut(),
                )
                .await?;
            let refreshed = warm.map_or(0, |warm| warm.tokens);
            record.tokens_used = record.tokens_used.saturating_add(refreshed);
            if parked {
                return Ok(Round::Parked);
            }
            self.absorb().await?;
        }
        fail("tool_round_limit")
    }

    /// The round boundary: queued steers become user items after everything
    /// recorded so far. The worker publishes each batch and answers the
    /// steers' waiters. One atomic read when nothing is waiting; the flag
    /// clears before the read, so a steer landing during it is seen next.
    async fn absorb(&self) -> Result<bool> {
        if !self.steers.swap(false, Relaxed) {
            return Ok(false);
        }
        let (turn, bytes, items) = (self.turn, self.context_bytes, self.context_items);
        let mut through = None;
        let mut steered = false;
        loop {
            let absorbed = self
                .store
                .op("absorb", move |db| db.absorb(turn, through, bytes, items))
                .await?;
            through = absorbed.next_through;
            steered |= !absorbed.outcomes.is_empty();
            // Release the batch before loading another; new arrivals beyond
            // the initial snapshot wait for the next model-round boundary.
            if through.is_none() {
                break;
            }
        }
        Ok(steered)
    }

    /// One model call with retries. Each attempt streams the same immutable
    /// nodes and shares its prepared prefix; a
    /// refusal for pace holds the provider's pool rather than this turn.
    #[allow(clippy::too_many_arguments)]
    async fn call(
        &self,
        provider: &Provider,
        model: &str,
        tools: &serde_json::value::RawValue,
        context: &Context,
        record: &mut agent_runtime::store::Bot,
        model_rounds: &mut usize,
        turn: i64,
        accounting: &mut Accounting,
    ) -> Result<Option<agent_runtime::provider::Completion>> {
        let instructions = record.instructions.clone();
        self.call_with(
            provider,
            model,
            &instructions,
            tools,
            Body::Window(context),
            record,
            model_rounds,
            turn,
            accounting,
        )
        .await
    }

    /// One model call with retries, pacing, and accounting: the turn's own
    /// request, or a compaction's request over a span of history.
    #[allow(clippy::too_many_arguments)]
    async fn call_with(
        &self,
        provider: &Provider,
        model: &str,
        instructions: &str,
        tools: &serde_json::value::RawValue,
        body: Body<'_>,
        record: &mut agent_runtime::store::Bot,
        model_rounds: &mut usize,
        turn: i64,
        accounting: &mut Accounting,
    ) -> Result<Option<agent_runtime::provider::Completion>> {
        accounting.compaction = matches!(body, Body::Span(_));
        let started = std::time::Instant::now();
        let mut attempt = std::mem::take(&mut accounting.call_attempts);
        let prior_spent =
            std::time::Duration::from_millis(std::mem::take(&mut accounting.call_spent_ms));
        let paced_before = accounting.totals().1;
        let cache_key = cache_key(record.cache_bot(), matches!(body, Body::Span(_)));
        loop {
            let items = match body {
                Body::Window(context) => self.items(context),
                Body::Span(plan) => self.span_items(plan).await?,
            };
            accounting.begin(attempt > 0);
            let result = provider
                .complete_accounted(
                    ModelRequest {
                        model,
                        instructions,
                        reasoning: record.reasoning.as_deref(),
                        tools,
                        allow_tool_calls: matches!(body, Body::Window(_)),
                        cache_key: Some(&cache_key),
                        items,
                        chain: Some(self.chain(body)),
                    },
                    |delta| {
                        let (kind, text) = match delta {
                            Delta::Text(text) => (if matches!(body, Body::Span(_)) { "compaction_text_delta" } else { "text_delta" }, text),
                            Delta::Thinking(text) => (if matches!(body, Body::Span(_)) { "compaction_thinking_delta" } else { "thinking_delta" }, text),
                        };
                        self.hub.live(
                            &self.bot,
                            json!({"event":kind,"bot":self.bot,"turn":turn,"durable":false,"text":text}),
                        )
                    },
                    &mut accounting.report,
                )
                .await;
            let error = match result {
                Ok(completion) => {
                    if let Some(usage) = &completion.usage {
                        self.tokens.add(usage);
                    }
                    for (from, to) in &completion.fallbacks {
                        // A classifier declined and the provider continued
                        // on another model; the usage event prices each.
                        self.hub
                            .live(
                                &self.bot,
                                json!({"event":"model_fallback","bot":self.bot,"turn":turn,
                                    "durable":false,"from":from,"to":to}),
                            )
                            .await?;
                    }
                    if completion.thinking_dropped > 0 {
                        // The provider found history edited under replayed
                        // thinking; the runtime should never cause this.
                        self.hub
                            .live(
                                &self.bot,
                                json!({"event":"thinking_dropped","bot":self.bot,"turn":turn,
                                    "durable":false,"count":completion.thinking_dropped}),
                            )
                            .await?;
                    }
                    return Ok(Some(completion));
                }
                Err(error) => error,
            };
            let paced_ms = accounting.totals().1 - paced_before;
            let spent = prior_spent
                + started
                    .elapsed()
                    .saturating_sub(std::time::Duration::from_millis(paced_ms));
            if let Some(block) = accounting.report.park_for {
                // No HTTP call took place. Keep its retry state without
                // incrementing attempts or cumulative dispatched retries.
                accounting.call_attempts = attempt;
                accounting.call_spent_ms = spent.as_millis() as u64;
                accounting.parked_until = now_ms() + block.as_millis() as u64;
                return Ok(None);
            }
            // Whatever the provider billed for a failed attempt is still spent.
            if let Some(usage) = &accounting.report.usage {
                self.tokens.add(usage);
                *model_rounds += 1;
                record.tokens_used = record
                    .tokens_used
                    .saturating_add(usage.input_tokens)
                    .saturating_add(usage.output_tokens);
            }
            let usage = accounting.report.usage.take();
            if matches!(body, Body::Span(_)) {
                if let Some(usage) = usage {
                    let reference = summarizer(record);
                    let usage = summarizer_usage(usage, split_model(&reference)?.0, model);
                    self.store
                        .op("compaction_usage", move |db| {
                            db.compaction_usage(turn, Some(&usage))
                        })
                        .await?;
                }
            } else {
                self.failed_usage(usage).await?;
            }
            // A retry is another billable call. Preserve final provider errors,
            // but stop retrying once failed usage has spent the bot's budget.
            let error = if retryable(&error.code) {
                budget_error(record.budget_tokens, record.tokens_used)
                    .or_else(|| {
                        (*model_rounds >= MAX_ROUNDS).then(|| Error::new("tool_round_limit"))
                    })
                    .unwrap_or(error)
            } else {
                error
            };
            attempt += 1;
            let paced = error.code == "provider_rate_limited" || error.code == "provider_http_429";
            let cap = if paced {
                MAX_PACED_ATTEMPTS
            } else {
                MAX_ATTEMPTS
            };
            // Time spent waiting fairly in the pool is the fleet's, not this
            // call's; the budget counts only the attempts and their backoff.
            if !retryable(&error.code) || attempt >= cap || spent >= RETRY_BUDGET {
                return Err(error);
            }
            // A pool closed by a rate limit for a while is not this turn's to
            // wait out with a live task and an active slot: park durably
            // and let the service retry when the block lifts. The retry is
            // announced now and counted only when dispatched. Its call-local
            // budget survives the park independently of cumulative totals.
            let park = paced
                .then(|| provider.blocked_for(model))
                .flatten()
                .filter(|block| *block >= MIN_PARK);
            // Rate limits already hold the pool; other transient failures
            // back off exponentially with a little spread.
            let delay = match park {
                Some(block) => block,
                None if paced => std::time::Duration::ZERO,
                None if error.code == "provider_login_refreshed" => std::time::Duration::ZERO,
                None => backoff(attempt, turn as u64),
            };
            self.hub
                .live(
                    &self.bot,
                    json!({"event":"retry","bot":self.bot,"turn":turn,"durable":false,
                        "attempt":attempt,"error":error.code,"detail":error.detail,
                        "delay_ms":delay.as_millis() as u64,"parked":park.is_some()}),
                )
                .await?;
            if let Some(block) = park {
                accounting.call_attempts = attempt;
                accounting.call_spent_ms = spent.as_millis() as u64;
                accounting.parked_until = now_ms() + block.as_millis() as u64;
                return Ok(None);
            }
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
    }

    /// Where this request sits in the bot's history, for a provider that can
    /// continue from the bot's previous response. A span has no window.
    fn chain<'a>(&'a self, body: Body<'a>) -> Chain<'a> {
        match body {
            Body::Window(Context {
                window: Some(window),
                prefix,
                thinking_floor,
            }) => Chain {
                bot: &self.bot,
                window: Some((&prefix.bytes[..], &window.ids[..])),
                tail: Box::new(move |skip| {
                    let span = Span {
                        ids: &window.ids[skip..],
                        sizes: &window.sizes[skip..],
                        thinking: &window.thinking[skip..],
                    };
                    self.window_items(Bytes::new(), span, *thinking_floor)
                }),
            },
            _ => Chain {
                bot: &self.bot,
                window: None,
                tail: Box::new(|_| Items::empty()),
            },
        }
    }

    async fn failed_usage(&self, usage: Option<agent_runtime::provider::Usage>) -> Result<()> {
        if let Some(usage) = usage {
            let turn = self.turn;
            self.store
                .op("failed_usage", move |db| db.failed_usage(turn, &usage))
                .await?;
        }
        Ok(())
    }

    /// Run a prepared tool, refreshing the last model call's prompt cache
    /// each time it has sat unread for `after`. The outer error is the
    /// runtime's; the inner one is the tool's result.
    async fn run_tool(
        &self,
        prepared: Prepared,
        workspace: &std::path::Path,
        environment: &[(String, String)],
        warm: Option<&mut Warm<'_>>,
    ) -> Result<Result<Outcome>> {
        let run = self.registry.execute(prepared, workspace, environment);
        let Some(warm) = warm.filter(|warm| !warm.stopped) else {
            return Ok(run.await);
        };
        tokio::pin!(run);
        let (provider, context) = (warm.provider, warm.context);
        let (model, instructions, reasoning, tools) =
            (warm.model, warm.instructions, warm.reasoning, warm.tools);
        loop {
            tokio::select! {
                biased;
                result = &mut run => return Ok(result),
                () = tokio::time::sleep_until(warm.read_at + warm.after) => {}
            }
            let sent = tokio::time::Instant::now();
            let dispatched = std::sync::atomic::AtomicBool::new(false);
            let refresh = provider.keep_warm(
                ModelRequest {
                    model,
                    instructions,
                    reasoning,
                    tools,
                    allow_tool_calls: true,
                    cache_key: None,
                    items: self.items(context),
                    chain: None,
                },
                &dispatched,
            );
            tokio::pin!(refresh);
            tokio::select! {
                biased;
                result = &mut run => {
                    // A refresh already sent is billed: let it answer, so
                    // what it cost is recorded. An unsent one costs nothing.
                    if dispatched.load(Relaxed) {
                        let refreshed = refresh.await;
                        self.refreshed(warm, sent, refreshed).await?;
                    }
                    return Ok(result);
                }
                refreshed = &mut refresh => self.refreshed(warm, sent, refreshed).await?,
            }
            if warm.stopped {
                return Ok(run.await);
            }
        }
    }

    /// Record a refresh: billed like any call but not a model round. A
    /// refused one ends the refreshes until the next model call, which
    /// rebuilds the cache as it would have without them.
    async fn refreshed(
        &self,
        warm: &mut Warm<'_>,
        sent: tokio::time::Instant,
        refreshed: Result<agent_runtime::provider::Usage>,
    ) -> Result<()> {
        let usage = match refreshed {
            Ok(usage) => usage,
            Err(error) => {
                warm.stopped = true;
                return self
                    .hub
                    .live(
                        &self.bot,
                        json!({"event":"keep_warm_failed","bot":self.bot,"turn":self.turn,
                            "durable":false,"error":error.code,"detail":error.detail}),
                    )
                    .await;
            }
        };
        self.tokens.add(&usage);
        warm.tokens = warm
            .tokens
            .saturating_add(usage.input_tokens)
            .saturating_add(usage.output_tokens);
        warm.read_at = sent;
        let turn = self.turn;
        self.store
            .op("keep_warm_usage", move |db| {
                db.keep_warm_usage(turn, &usage)
            })
            .await
    }

    /// Run planned calls in order. Returns true when a wait parked the turn;
    /// the calls after it are stored with the parked state.
    async fn execute_calls(
        &self,
        calls: Vec<ToolCall>,
        workspace: &std::path::Path,
        environment: &[(String, String)],
        allowed: &[String],
        mut warm: Option<&mut Warm<'_>>,
    ) -> Result<bool> {
        let turn = self.turn;
        let mut calls = calls.into_iter();
        while let Some(call) = calls.next() {
            let started = call.clone();
            self.store
                .op("tool_start", move |db| db.tool_start(turn, &started))
                .await?;
            // A tool failure is a result the model can act on. Only the
            // scheduler closing is a runtime failure. The bot's selection is
            // enforced here, not only by what the model was shown.
            let prepared = if allowed.iter().any(|name| name == &call.name) {
                self.registry.prepare(&call.name, &call.arguments)
            } else {
                Err(Error::with("tool_not_available", call.name.as_str()))
            };
            let outcome = match prepared {
                Ok(Prepared::Wait {
                    handles,
                    timeout_ms,
                    any,
                }) => {
                    match self
                        .park(&call.call_id, handles, timeout_ms, any, &mut calls)
                        .await?
                    {
                        Some(outcome) => outcome,
                        None => return Ok(true),
                    }
                }
                Ok(Prepared::Shell {
                    command,
                    timeout_ms,
                    background: true,
                }) => {
                    // A full line behind the process bound is a tool result
                    // the model acts on, not a runtime failure.
                    match self
                        .background(
                            &call.call_id,
                            command,
                            workspace.to_path_buf(),
                            timeout_ms,
                            environment.to_vec(),
                        )
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(error) if error.code == "capacity_exhausted" => failure(error),
                        Err(error) => return Err(error),
                    }
                }
                Ok(Prepared::Read {
                    source:
                        ReadSource::Artifact {
                            turn: owner,
                            call_id: ref_call,
                            stream,
                        },
                    offset,
                    limit,
                }) => {
                    let bot = self.bot.clone();
                    let text = self
                        .store
                        .op("artifact_lines", move |db| {
                            db.artifact_lines(&bot, owner, &ref_call, &stream, offset, limit)
                        })
                        .await;
                    match text {
                        Ok(page) => Outcome::text(self.registry.redact_text(page)),
                        Err(error) => failure(error),
                    }
                }
                Ok(Prepared::History {
                    turn: wanted,
                    offset,
                    limit,
                }) => match Box::pin(self.history(&call.call_id, wanted, offset, limit)).await {
                    Ok(outcome) => outcome,
                    Err(error) => failure(error),
                },
                // Stored with the result: one commit records the note, its
                // version node, and the tool outcome together.
                Ok(Prepared::Note { text }) => {
                    let outcome = Outcome {
                        output: json!({"bytes":text.len(),"cleared":text.is_empty()}).to_string(),
                        artifacts: Vec::new(),
                        note: Some(text),
                    };
                    // Clearing is always permitted. Nonempty notes must fit
                    // before they become a mandatory prefix on future turns.
                    if outcome.note.as_ref().is_some_and(|text| text.is_empty()) {
                        outcome
                    } else {
                        let (bot, id, limit) =
                            (self.bot.clone(), call.call_id.clone(), self.input_limit());
                        match self
                            .store
                            .read("validate_note", move |db| {
                                db.validate_note(&bot, turn, &id, &outcome, limit)?;
                                Ok(outcome)
                            })
                            .await
                        {
                            Ok(outcome) => outcome,
                            Err(error) => failure(error),
                        }
                    }
                }
                Ok(prepared) => match self
                    .run_tool(prepared, workspace, environment, warm.as_deref_mut())
                    .await?
                {
                    Ok(outcome) => annotate(outcome, turn, &call.call_id),
                    Err(error) if error.code == "tool_scheduler_closed" => return Err(error),
                    Err(error) => failure(error),
                },
                Err(error) => failure(error),
            };
            let id = call.call_id;
            self.store
                .op("tool_finish", move |db| db.tool_finish(turn, &id, &outcome))
                .await?;
        }
        Ok(false)
    }

    async fn history(
        &self,
        call_id: &str,
        wanted: i64,
        offset: u64,
        limit: usize,
    ) -> Result<Outcome> {
        // Old turns can leave the window; the current turn and pinned items
        // cannot. Indexed accounting avoids rebuilding a full window per read.
        let allowance = self.input_limit();
        let (bot, turn) = (self.bot.clone(), self.turn);
        let (family, used) = self
            .store
            .read("history_usage", move |db| db.history_usage(&bot, turn))
            .await?;
        let budget = allowance.bytes.saturating_sub(used.bytes + 1) / 2;
        if used.items >= allowance.items || budget < 256 {
            return fail("history_context_exhausted");
        }
        let bot = self.bot.clone();
        let mut page = self
            .store
            .read("history_read", move |db| {
                db.history_read(&bot, wanted, offset, limit.min(budget))
            })
            .await?;
        loop {
            let output = self.registry.redact_text(page.to_string());
            // Page metadata, JSON escaping, redaction, and the provider's tool
            // result envelope all count. Shrink in memory, without rereading
            // history, until the actual encoded item fits the reserved budget.
            if family.tool_result_item(call_id, &output)?.len() <= budget {
                return Ok(Outcome::text(output));
            }
            let text = page["text"].as_str().unwrap();
            let mut end = text.len() / 2;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end == 0 {
                return fail("history_context_exhausted");
            }
            let kept = text[..end].to_owned();
            page["items"] = json!(kept.bytes().filter(|b| *b == b'\n').count());
            page["text"] = json!(kept);
            page["next_offset"] = json!(offset + end as u64);
            page["truncated"] = json!(true);
            page["done"] = json!(false);
        }
    }

    /// Make the turn durable as waiting, then register its handles. Returns
    /// an immediate result only for arguments that can never resolve.
    async fn park(
        &self,
        call_id: &str,
        handles: Vec<String>,
        timeout_ms: Option<u64>,
        any: bool,
        calls: &mut std::vec::IntoIter<ToolCall>,
    ) -> Result<Option<Outcome>> {
        for text in &handles {
            match Handle::parse(text) {
                Err(error) => return Ok(Some(failure(error))),
                Ok(Handle::Turn { bot, turn }) if bot == self.bot && turn == self.turn => {
                    return Ok(Some(failure(Error::with(
                        "invalid_handle",
                        "a turn cannot wait on itself",
                    ))));
                }
                Ok(_) => {}
            }
        }
        // Only move the remaining calls once this wait can actually park.
        let pending: Vec<ToolCall> = calls.collect();
        let deadline_ms = timeout_ms.map(|t| now_ms() + t);
        let (turn, id, list) = (self.turn, call_id.to_owned(), handles.clone());
        self.store
            .op("suspend", move |db| {
                db.suspend(turn, &id, &list, deadline_ms, any, &pending)
            })
            .await?;
        self.handles
            .attach(
                &self.store,
                Waiter::Turn(self.turn),
                &handles,
                deadline_ms,
                any,
                Completion::Resume {
                    bot: self.bot.clone(),
                    turn: self.turn,
                },
            )
            .await;
        Ok(None)
    }

    /// Start a command now; its result is retrievable through a proc handle
    /// that the store keeps unique and durable.
    async fn background(
        &self,
        call_id: &str,
        command: String,
        workspace: PathBuf,
        timeout_ms: u64,
        environment: Vec<(String, String)>,
    ) -> Result<Outcome> {
        // The place in line is taken before anything durable is written.
        let queued = self.registry.queue()?;
        let (turn, started) = (self.turn, call_id.to_owned());
        let id = self
            .store
            .op("process_start", move |db| db.process_start(turn, &started))
            .await?;
        let (sender, receiver) = oneshot::channel();
        self.registry
            .background(command, workspace, timeout_ms, environment, queued, sender);
        let (store, handles, failures, call_id_for_refs) = (
            self.store.clone(),
            self.handles.clone(),
            self.background_failures.clone(),
            call_id.to_owned(),
        );
        tokio::spawn(async move {
            let result = receiver.await.unwrap_or_else(|_| fail("process_lost"));
            let (value, artifacts) = match result {
                Ok(outcome) => {
                    let outcome = annotate(outcome, turn, &call_id_for_refs);
                    (
                        serde_json::from_str(&outcome.output)
                            .unwrap_or(json!({"output":outcome.output})),
                        outcome.artifacts,
                    )
                }
                Err(error) => (
                    json!({"error":error.code,"detail":error.detail}),
                    Vec::new(),
                ),
            };
            // Commit the result and overflow together before waking waiters.
            // Report storage failures to the service instead of stranding waiters.
            let recorded = value.clone();
            match store
                .op("process_finish", move |db| {
                    db.process_finish(id, &recorded, &artifacts)
                })
                .await
            {
                Ok(()) => handles.process_finished(id, value),
                Err(error) => {
                    // A closed receiver means the service has already exited.
                    let _ = failures.send(error);
                }
            }
        });
        Ok(Outcome::text(
            json!({"handle":format!("proc:{id}"),"background":true}).to_string(),
        ))
    }
}

fn budget_error(budget: Option<u64>, used: u64) -> Option<Error> {
    budget
        .filter(|&cap| used >= cap)
        .map(|cap| Error::with("budget_exhausted", format!("{used} of {cap} tokens used")))
}

/// Failures of the provider's pace, capacity, or transport, none of which say
/// anything about the request. Model-level outcomes (`provider_incomplete`)
/// and client errors are final.
fn retryable(code: &str) -> bool {
    matches!(
        code,
        "provider_rate_limited"
            | "provider_http_429"
            | "provider_http_500"
            | "provider_http_502"
            | "provider_http_503"
            | "provider_http_504"
            | "provider_http_529"
            | "provider_stream_failed"
            | "provider_stream_stalled"
            | "provider_login_refreshed"
            | "truncated_sse_frame"
            | "provider_admission_timeout"
            | "provider_socket_expired"
    ) || code.starts_with("provider_connection_")
}

/// 250 ms doubling to 30 s, spread by up to a fifth either way so several
/// daemons on one key do not retry in step. Deterministic per turn and
/// attempt, so it costs a few integer operations.
fn backoff(attempt: u32, seed: u64) -> std::time::Duration {
    let base = 250u64.saturating_mul(1u64 << (attempt.saturating_sub(1)).min(7));
    let base = base.min(30_000);
    let mut x = seed ^ (u64::from(attempt) << 32) ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    let spread = (x % 41) as i64 - 20; // -20..=20 percent
    std::time::Duration::from_millis((base as i64 + base as i64 * spread / 100).max(1) as u64)
}

/// Name retained streams in the model-facing result so the model can read
/// them back: `TURN/CALL_ID/STREAM`.
fn annotate(mut outcome: Outcome, turn: i64, call_id: &str) -> Outcome {
    if outcome.artifacts.is_empty() {
        return outcome;
    }
    let refs: Vec<String> = outcome
        .artifacts
        .iter()
        .map(|(stream, _)| format!("{turn}/{call_id}/{stream}"))
        .collect();
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&outcome.output)
        && value.is_object()
    {
        value["artifacts"] = json!(refs);
        outcome.output = value.to_string();
    }
    outcome
}

#[cfg(test)]
use agent_runtime::store::pinned_item;

struct Context {
    window: Option<Window>,
    prefix: ContextPrefix,
    /// Items with ids below this go without their thinking blocks.
    thinking_floor: i64,
}
impl Context {
    fn empty() -> Self {
        Self {
            window: None,
            prefix: ContextPrefix {
                bytes: Bytes::new(),
                items: 0,
            },
            thinking_floor: 0,
        }
    }
    fn usage(&self) -> ContextUsage {
        self.window.as_ref().map_or(ContextUsage::default(), |w| {
            ContextUsage {
                bytes: w.item_bytes as usize,
                items: w.ids.len(),
            }
            .with_prefix(&self.prefix)
        })
    }
    fn pressure(&self) -> ContextUsage {
        self.window.as_ref().map_or(ContextUsage::default(), |w| {
            w.unsummarized.with_prefix(&self.prefix)
        })
    }
}

/// What a model call sends: the bot's window, or a compaction's span.
#[derive(Clone, Copy)]
enum Body<'a> {
    Window(&'a Context),
    Span(&'a agent_runtime::store::CompactionPlan),
}

/// The assistant text of a completion's items, in either family encoding.
fn completion_text(items: &[Bytes]) -> String {
    let mut text = String::new();
    for item in items {
        let Ok(value) = serde_json::from_slice::<Value>(item) else {
            continue;
        };
        if let Some(content) = value["content"].as_array() {
            for part in content {
                if let Some(piece) = part["text"].as_str()
                    && matches!(part["type"].as_str(), Some("output_text" | "text"))
                {
                    text.push_str(piece);
                }
            }
        }
    }
    text
}

fn failure(error: Error) -> Outcome {
    Outcome {
        output: json!({"error":error.code,"detail":error.detail}).to_string(),
        artifacts: Vec::new(),
        note: None,
    }
}

/// The summarizer: the bot's own model unless the client named one of the
/// same family, checked at creation.
fn summarizer(record: &agent_runtime::store::Bot) -> String {
    record
        .compaction_model
        .clone()
        .unwrap_or_else(|| format!("{}/{}", record.provider, record.model))
}

/// A summary is charged to the turn that needed it, but the summarizer may
/// be another model on another provider; name both, as a fallback names its
/// attempts, so the call is priced at that model's rates.
fn summarizer_usage(
    mut usage: agent_runtime::provider::Usage,
    provider: &str,
    model: &str,
) -> agent_runtime::provider::Usage {
    if usage.models.is_empty() {
        usage.models.push(agent_runtime::provider::ModelTokens {
            model: model.to_owned(),
            provider: None,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_write_tokens: usage.cache_write_tokens,
        });
    }
    for attempt in &mut usage.models {
        attempt.provider = Some(provider.to_owned());
    }
    usage
}

#[cfg(test)]
mod tests {
    #[test]
    fn pinned_blocks_cache_only_with_their_provider_format() {
        use agent_runtime::codec::Family;
        let anthropic: serde_json::Value =
            serde_json::from_slice(&super::pinned_item(Family::Anthropic, "stable").unwrap())
                .unwrap();
        assert_eq!(
            anthropic["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(
            super::pinned_item(Family::Responses, "stable").unwrap(),
            Family::Responses.user_item("stable").unwrap()
        );
    }
    use super::*;
    use serde_json::Value;

    #[tokio::test]
    async fn interrupt_finishes_a_committed_steer_batch_and_wakes_waiters() {
        use agent_runtime::{
            codec::Family,
            output::Output,
            provider::Transport,
            store::{Binding, Delivery, TurnOptions},
        };
        use tokio::io::AsyncReadExt;
        let dir =
            std::env::temp_dir().join(format!("agent-steer-interrupt-{}", std::process::id()));
        let (store, publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let (turn, steers) =
            store
                .op("create", |db| {
                    db.create(
                        "Bob",
                        Some("/synthetic"),
                        Binding {
                            provider: "openai",
                            family: Family::Responses,
                            model: "synthetic",
                            instructions: "",
                            reasoning: None,
                            budget_tokens: None,
                            tools: &[],
                            created_by: None,
                            created_by_id: None,
                            compaction_instructions: None,
                            compaction_model: None,
                        },
                    )?;
                    let turn = db
                        .begin(
                            "Bob",
                            "first",
                            "work",
                            true,
                            &TurnOptions::default(),
                            |_, _| Ok(()),
                        )?
                        .turn;
                    let options = TurnOptions {
                        delivery: Delivery::Steer,
                        ..TurnOptions::default()
                    };
                    let steers =
                        (0..512)
                            .map(|n| {
                                db.begin("Bob", &n.to_string(), "steer", true, &options, |_, _| {
                                    Ok(())
                                })
                                .map(|s| s.turn)
                            })
                            .collect::<Result<Vec<_>>>()?;
                    Ok((turn, steers))
                })
                .await
                .unwrap();
        // A slow firehose no longer holds the task: publication is the
        // worker's. Cancellation lands between two of the sixteen batches.
        let (writer, mut reader) = tokio::io::duplex(1);
        let output = Output::writer(writer);
        let hub = Hub::default();
        hub.add_firehose(0, output.clone());
        let handles = Handles::new(mpsc::unbounded_channel().0);
        let publisher = tokio::spawn(crate::server::publish(
            publications,
            hub.clone(),
            handles.clone(),
        ));
        let (reply_writer, mut reply_reader) = tokio::io::duplex(65536);
        handles
            .attach(
                &store,
                handles.request_waiter(),
                &steers[..32]
                    .iter()
                    .map(|id| format!("turn:Bob/{id}"))
                    .collect::<Vec<_>>(),
                None,
                false,
                Completion::Respond {
                    session: 1,
                    output: Output::writer(reply_writer),
                    request: json!(1),
                },
            )
            .await;
        let (cancel, cancelled) = watch::channel(false);
        let task = Turn {
            bot: "Bob".into(),
            turn,
            store: store.clone(),
            providers: Arc::new(HashMap::from([(
                "openai".into(),
                Provider::new(
                    Transport::new(1, 1).unwrap(),
                    Family::Responses,
                    "http://127.0.0.1:9/v1",
                    None,
                )
                .unwrap(),
            )])),
            registry: Registry::new("echo").unwrap(),
            hub,
            handles: handles.clone(),
            background_failures: mpsc::unbounded_channel().0,
            context_bytes: 8 << 20,
            context_items: 4096,
            note_turns: 48,
            compact_at: 75,
            compact_keep: 25,
            resume: false,
            steers: Arc::new(AtomicBool::new(true)),
            tokens: Arc::default(),
        };
        let running = tokio::spawn(async move { task.execute(cancelled).await });
        let last = steers[31];
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while store
                .op("turn_status", move |db| db.turn_status("Bob", last))
                .await
                .unwrap()
                != "steered"
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancel.send(true).unwrap();
        drop(output);
        let drain = tokio::spawn(async move {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        let exit = tokio::time::timeout(std::time::Duration::from_secs(2), running)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Finished(Some(error)) if error.code == "cancelled"));
        // The worker publishes every committed batch regardless of the task;
        // ending the store ends the stream once all of it is delivered.
        drop(store);
        tokio::time::timeout(std::time::Duration::from_secs(2), publisher)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            handles.stats(),
            (0, 0),
            "committed steer waiters must resolve although the task was cancelled"
        );
        let mut reply = Vec::new();
        reply_reader.read_to_end(&mut reply).await.unwrap();
        let reply: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(reply["result"]["results"].as_object().unwrap().len(), 32);
        for outcome in reply["result"]["results"].as_object().unwrap().values() {
            assert_eq!(outcome["status"], "steered");
            assert_eq!(outcome["into"], turn);
        }
        let bytes = drain.await.unwrap();
        let events: Vec<Value> = bytes
            .split(|b| *b == b'\n')
            .filter(|s| !s.is_empty())
            .map(|s| serde_json::from_slice(s).unwrap())
            .collect();
        // Exactly what committed was published, no more and no less, and
        // the batches the cancellation stopped stay queued.
        let conn = rusqlite::Connection::open(dir.join("state.sqlite")).unwrap();
        let (steered, queued): (i64, i64) = conn
            .query_row(
                "SELECT SUM(status='steered'),SUM(status='queued') FROM turns WHERE delivery='steer'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            steered >= 32 && queued > 0,
            "steered {steered}, queued {queued}"
        );
        assert_eq!(steered + queued, 512);
        assert_eq!(
            events
                .iter()
                .filter(|e| e["event"] == "turn_finished")
                .count() as i64,
            steered
        );
        assert_eq!(
            events.iter().filter(|e| e["event"] == "steered").count() as i64,
            steered
        );
        let mut cursors: Vec<i64> = events.iter().filter_map(|e| e["cursor"].as_i64()).collect();
        let sorted = {
            let mut s = cursors.clone();
            s.sort_unstable();
            s
        };
        assert_eq!(cursors, sorted, "durable events arrive in commit order");
        cursors.dedup();
        assert_eq!(cursors.len(), events.len(), "and exactly once");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn read_ahead_batches_stop_at_either_bound() {
        let ids: Vec<i64> = (1..=200).collect();
        let small = vec![10u32; 200];
        let by_count = batches(&ids, &small);
        assert_eq!(
            by_count.iter().map(Vec::len).collect::<Vec<_>>(),
            [64, 64, 64, 8]
        );
        // Three 100 KiB items fill a batch; a 1 MiB item travels alone.
        let sizes = [100 << 10, 100 << 10, 100 << 10, 1 << 20, 10, 10];
        let by_bytes = batches(&ids[..6], &sizes);
        assert_eq!(by_bytes, [vec![1, 2], vec![3], vec![4], vec![5, 6]]);
        assert!(batches(&[], &[]).is_empty());
    }
}
