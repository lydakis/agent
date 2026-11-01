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
    rules::Ruling,
    sandbox::Policy,
    store::{
        Bot, CallAnswer, ContextPrefix, ContextUsage, Database, ElisionPlan, Gated, Halt, Halted,
        Settings, Slot, Store, Strip, TURN_VIEW_ACCOUNTING, Waiting, Window, unserved,
    },
    tools::{Files, Outcome, POLL_YIELD_MS, Prepared, ReadSource, Registry},
};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    ops::ControlFlow,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    time::Duration,
};
use tokio::sync::{Notify, mpsc, oneshot, watch};

/// The error a cancelled turn ends with, sent on its cancel channel: a
/// client's interrupt, or the daemon shutting down around it.
pub const INTERRUPTED: &str = "cancelled";
/// The daemon stopping around a running turn, which parks it for the next
/// start rather than ending it.
pub const SHUTDOWN: &str = "daemon_shutdown";
/// The daemon stopping on a fatal error around a running turn, which ends
/// it as a crash would: only a planned stop parks a turn.
pub const DIED: &str = "process_interrupted";
/// A running turn's end when the daemon is paused.
pub const PAUSED: &str = "daemon_paused";
/// Attempts per model call before its failure is the turn's failure, and the
/// wall-clock budget those attempts may span. A model call has no side
/// effects, so retrying one is always safe; a tool is never rerun.
const MAX_ATTEMPTS: u32 = 8;
/// How long a command the load gate holds back waits live, a reading or
/// two of the gate, before its turn parks.
const LOAD_HOLD: std::time::Duration = std::time::Duration::from_millis(1500);
/// Refusals for pace are spaced by the pool and are not the request's fault,
/// so they get many more attempts inside the same time budget.
const MAX_PACED_ATTEMPTS: u32 = 64;
use agent_runtime::provider::pace::MIN_PARK;
const RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(300);
/// Nodes per piece of a compaction catch-up walk: a few milliseconds of
/// metadata reads, after which other bots' reads may run.
const CATCH_UP_PIECE_NODES: i64 = 1024;
/// A window budget no view reaches, for reading one whole from its saved
/// start.
const UNBOUNDED: i64 = i64::MAX / 4;
/// Read-ahead while a body streams: a batch stops at either bound, so the
/// memory held per in-flight request is a number, not a function of item
/// sizes. An item larger than the byte bound travels alone.
const WINDOW_BATCH: usize = 64;
const WINDOW_BATCH_BYTES: u64 = 256 * 1024;

/// Stored items to stream: ids with their weights as sent, the bytes each
/// loses without thinking, what each one's pictures add beyond their
/// weight, and the elision floor the sizes count stubs to.
#[derive(Clone, Copy)]
struct Span<'a> {
    ids: &'a [i64],
    sizes: &'a [u32],
    thinking: &'a [u32],
    pictured: &'a [i32],
    elided: i64,
}
impl Span<'_> {
    const EMPTY: Span<'static> = Span {
        ids: &[],
        sizes: &[],
        thinking: &[],
        pictured: &[],
        elided: 0,
    };
}

/// A stable fingerprint of what a window's thinking is bound to: the
/// encoded prefix in front of it, its first item, and the endpoint and model
/// that read it back. A signature is only valid for the model that wrote it,
/// on the endpoint that holds its key. FNV-1a, so it survives restarts and
/// upgrades.
fn fingerprint(prefix: &[u8], first: i64, url: &str, model: &str) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let reader = [url, model].map(|part| (part.len() as u64).to_le_bytes());
    for byte in prefix
        .iter()
        .chain(&first.to_le_bytes())
        .chain(&reader[0])
        .chain(url.as_bytes())
        .chain(&reader[1])
        .chain(model.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash as i64
}

/// Split window items into read-ahead batches by count and by bytes.
fn batches(ids: &[i64], sizes: &[u32], pictured: &[i32]) -> Vec<Vec<i64>> {
    let mut out: Vec<Vec<i64>> = Vec::with_capacity(ids.len().div_ceil(WINDOW_BATCH));
    let mut bytes = 0u64;
    for (index, id) in ids.iter().enumerate() {
        // Read as stored: with its pictures' bytes.
        let size = (i64::from(sizes.get(index).copied().unwrap_or(0))
            + i64::from(pictured.get(index).copied().unwrap_or(0)))
        .max(0) as u64;
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
    strip: Strip,
    elided: i64,
) -> impl futures_util::Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    let mut started = false;
    stream::iter(0..chunks.len())
        .then(move |index| {
            let (store, chunks) = (store.clone(), chunks.clone());
            async move {
                store
                    .read("items_by_ids", move |db| {
                        db.items_by_ids(&chunks[index], strip, elided)
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
/// whose cache it shares (its own, or a fork's source) under the store's
/// identity, so bots of different stores (every Harbor container's first
/// bot is id 1) never share a key, and a daemon restart keeps every bot's
/// cache affinity. A summary sent as a request of its own has its own
/// prefix, so its own key; one sent as a copy of the bot's call shares the
/// bot's.
fn cache_key(identity: u128, bot: i64, summary: bool) -> String {
    let suffix = if summary { "-summary" } else { "" };
    format!("{identity:032x}-{bot}{suffix}")
}

pub struct Turn {
    pub bot: String,
    pub turn: i64,
    pub store: Store,
    /// The store's identity, for provider cache keys.
    pub identity: u128,
    pub providers: Arc<HashMap<String, Provider>>,
    pub registry: Registry,
    pub hub: Hub,
    pub handles: Handles,
    pub background_failures: mpsc::UnboundedSender<Error>,
    /// The bot's context, compaction and approval settings, read with its
    /// record when the turn's rounds begin.
    pub settings: std::sync::OnceLock<Settings>,
    /// The bot's command rules, read with its record as the turn's rounds
    /// begin: every round rules its calls by them, so one that announces a
    /// call and the run that starts it agree.
    pub rules: std::sync::OnceLock<Arc<agent_runtime::rules::Rules>>,
    /// While a resumed park's calls run, what the rules made of them as
    /// their round was announced; otherwise the turn's own `rules`.
    pub ruled: std::sync::Mutex<Option<Arc<agent_runtime::rules::Rules>>>,
    /// The turn's model's context window, as the turn starts.
    pub window: std::sync::OnceLock<u64>,
    /// The park this task continues, already claimed by the service: record
    /// its wait results, then keep going.
    pub resumed: Option<Waiting>,
    /// A steer for this bot may be queued. Set by the service, cleared by
    /// the boundary before it reads, so unrelated bots never pay for one
    /// bot's pending steer.
    pub steers: Arc<AtomicBool>,
    /// Set by the service when a client asks for a summary; the next round
    /// boundary takes it. The bot's record holds the request too, so a
    /// turn that parks or a daemon that restarts still takes it.
    pub compact: Arc<AtomicBool>,
    /// A `steer` for this turn, which moves a foreground command it is
    /// running to the background.
    pub lift: Arc<Lift>,
    pub tokens: Arc<TokenTotals>,
    /// Results this turn's `read` found on the bot's lineage.
    pub read_results: std::sync::Mutex<Vec<i64>>,
    /// What summaries held and spent beside the turn's own accounting.
    pub side: SideCounts,
    /// Set by the service when the daemon stops taking work: the turn
    /// starts no model call or tool call after it and parks at its next
    /// boundary, for the next start to continue.
    pub stopping: Arc<AtomicBool>,
}

/// A steer waiting for the running turn's next boundary. Armed by `steer`
/// delivery alone and cleared where the boundary reads steers. A
/// foreground command running while it is armed moves to the background
/// and returns its handle, so no command holds a steer back; nothing is
/// killed, and the bot waits on the handle if it still wants the result.
#[derive(Default)]
pub struct Lift {
    armed: AtomicBool,
    notify: Notify,
}
impl Lift {
    pub fn arm(&self) {
        self.armed.store(true, Relaxed);
        self.notify.notify_waiters();
    }
    fn clear(&self) {
        self.armed.store(false, Relaxed);
    }
    fn is_armed(&self) -> bool {
        self.armed.load(Relaxed)
    }
    /// Once armed, and LIFT_GRACE later: a command that ends that soon
    /// returns as usual, so a quick one never becomes a handle.
    async fn lifted(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.armed.load(Relaxed) {
                break;
            }
            notified.await;
        }
        tokio::time::sleep(LIFT_GRACE).await;
    }
}
const LIFT_GRACE: Duration = Duration::from_millis(250);
/// The most of one round's calls a bot runs at once. Each command also
/// waits for the daemon's process budget, so this is a bot's share of it.
const PARALLEL_CALLS: usize = 4;

/// A tool's result, or the command a steer moved to the background.
enum Ran<F, T = Outcome> {
    Done(Result<T>),
    Lifted(std::pin::Pin<Box<F>>),
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

/// The model a refused request is sent to next: the bot's fallback.
struct Fallback {
    provider: Arc<Provider>,
    /// Its provider binding and model, which its usage is priced at.
    name: String,
    model: String,
    /// The refusal's credit, when the retry stays on the same provider.
    credit: Option<String>,
    /// Chosen, and not yet sent.
    pending: bool,
}

impl Fallback {
    /// As a call parked on its pool keeps it.
    fn parked(&self) -> agent_runtime::store::ParkedFallback {
        agent_runtime::store::ParkedFallback {
            model: format!("{}/{}", self.name, self.model),
            credit: self.credit.clone(),
        }
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
    /// The items the call sent without thinking: a refresh sends the same.
    thinking: Strip,
    /// Whether the call offered web search; a refresh offers the same.
    web_search: bool,
    after: std::time::Duration,
    /// When the cache was last read, by the call or a refresh.
    read_at: tokio::time::Instant,
    stopped: bool,
    /// Tokens the refreshes billed, for the bot's budget.
    tokens: u64,
    /// What the bot's and turn's caps leave the refreshes, when either is
    /// set: they stop once they have billed it.
    room: Option<u64>,
    /// The refresh in flight, held by [`Accounting`] so an interrupt that
    /// drops the turn's rounds still settles one already sent.
    pending: &'a mut Option<Pending>,
}

/// A refresh running as its own task, and whether it has been sent.
struct Pending {
    refresh: Arc<agent_runtime::provider::Refresh>,
    /// The usage, and when the refresh was sent.
    task: tokio::task::JoinHandle<Result<(agent_runtime::provider::Usage, tokio::time::Instant)>>,
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
    /// The fallback the unfinished call went to.
    call_fallback: Option<agent_runtime::store::ParkedFallback>,
    /// Set when a call parked the turn on a closed pool.
    parked_until: u64,
    /// Persist the call's phase with its existing park transaction.
    compaction: bool,
    /// What the summary call copies, kept in the park record so its retry
    /// sends the same request.
    copied: Option<agent_runtime::store::CopiedCall>,
    /// A prompt-cache refresh in flight while a reply streams or a tool runs.
    refresh: Option<Pending>,
    /// When the last call's cache was last read by a refresh sent while the
    /// reply streamed, and whether one was refused.
    warm_read_at: Option<tokio::time::Instant>,
    warm_stopped: bool,
    /// The provider's sticky-routing token for this turn's model calls,
    /// kept in the park record while the turn waits.
    route: std::sync::OnceLock<String>,
    /// A summary's own, running beside the turn's call.
    beside: bool,
    /// The tokens a summary beside the turn's call holds of its budget.
    summary_held: u64,
    /// Until when a summary that parked on its pool is not tried again
    /// while the view fits, kept in the park record.
    summary_retry_at: u64,
    /// The model has replied in this turn, also before a park it resumed
    /// from, so a reply with nothing in it, as after a reaction with nothing
    /// to add, ends the turn rather than failing it. An empty reply to a
    /// prompt fails; an empty summary is the summary's own failure.
    answered: bool,
    /// The cause a person can fix that parks the turn, when one does; and
    /// when the run of holds it belongs to began, kept across tries.
    blocked: Option<agent_runtime::store::Blocked>,
    held_since: Option<u64>,
}
impl Accounting {
    /// Park on a cause a person can fix: the call is tried again after the
    /// hold's wait, with a fresh retry budget. Returns when.
    fn hold(&mut self, error: &Error) -> u64 {
        let now = now_ms();
        let blocked = agent_runtime::store::Blocked {
            code: error.code.clone(),
            detail: error.detail.clone(),
            since_ms: *self.held_since.get_or_insert(now),
        };
        self.call_attempts = 0;
        self.call_spent_ms = 0;
        self.parked_until = now + blocked.wait_ms(now);
        self.blocked = Some(blocked);
        self.parked_until
    }
    fn totals(&self) -> (u64, u64) {
        (
            self.retries + u64::from(self.retrying && self.report.dispatched),
            self.paced_ms + self.report.paced_ms,
        )
    }
    /// Until when a summary is not tried again, if later than now.
    fn summary_retry(&self) -> Option<u64> {
        Some(self.summary_retry_at).filter(|at| now_ms() < *at)
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
    /// Parked on a verdict whose gate lapses at the given time.
    Lapses(u64),
    /// The daemon is stopping: park here, with nothing in flight.
    Drained,
}
/// How long an answered call holds its turn's slot for the answer before
/// it parks, at most: the bot's approval hold, when that is shorter.
const ANSWER_HOLD_MS: u64 = 250;
/// How many turns `history` lists of another agent.
const HISTORY_TURNS: usize = 20;
/// Why a round's calls stopped before the last one ran: the turn parked,
/// on handles or a verdict, and is resumed by the service.
enum Stop {
    Parked,
    /// Parked on a verdict or a wait; the service wakes it when a gate lapses.
    Lapses(u64),
    /// The daemon is stopping: the calls left in the round do not start.
    Drained,
}
impl Stop {
    fn round(self) -> Round {
        match self {
            Stop::Parked => Round::Parked,
            Stop::Lapses(at) => Round::Lapses(at),
            Stop::Drained => Round::Drained,
        }
    }
}
/// A gated call's fate once its verdict is in.
enum Approval {
    Run,
    Denied,
    Stop(Stop),
}

pub enum Exit {
    Finished(Option<Error>),
    Parked,
    /// Parked on a rate-limited pool, or on a verdict whose gate lapses;
    /// the service resumes it at this time unless something resumes it first.
    Paced(u64),
}

/// Completion as one storage job: the terminal event, the outcome its
/// waiters get, and retention, all or nothing, so a failed completion can
/// be submitted again. The worker publishes all of it after the job commits.
pub struct Finished;

impl Finished {
    pub fn record(
        db: &mut agent_runtime::store::Database,
        bot: &str,
        turn: i64,
        error: Option<&Error>,
    ) -> Result<()> {
        let outcome = db.atomic(|db| {
            db.finish(turn, error)?;
            let outcome = db
                .turn_outcome(bot, turn)?
                .ok_or_else(|| Error::new("stale_turn"))?;
            // A later steer may already be terminal, placing this completion
            // outside retention: the outcome is captured above, and the
            // turn's own records are kept so its terminal event is published.
            db.retain(bot, Some(turn))?;
            Ok(outcome)
        })?;
        db.announce(bot, turn, outcome);
        Ok(())
    }
}

/// The provider as a bot's settings shape its calls: its output bound,
/// cache lifetime, keep-warm and speed. The bot's defaults borrow it
/// unchanged.
pub fn shaped<'p>(
    provider: &'p Provider,
    settings: &Settings,
) -> Result<std::borrow::Cow<'p, Provider>> {
    provider.shaped(
        settings.max_output_tokens,
        settings.keep_warm(),
        settings.cache_hour(),
        settings.speed(),
    )
}

/// The provider as the bot's settings shape a call on another model than
/// its own, a summarizer or a fallback: at standard speed, which every
/// model takes.
fn shaped_other<'p>(
    provider: &'p Provider,
    settings: &Settings,
) -> Result<std::borrow::Cow<'p, Provider>> {
    provider.shaped(
        settings.max_output_tokens,
        settings.keep_warm(),
        settings.cache_hour(),
        None,
    )
}

impl Turn {
    pub async fn execute(&self, mut cancelled: watch::Receiver<Option<&'static str>>) -> Exit {
        // Only an explicit interrupt or shutdown cancels, naming its error. A
        // dropped sender (the service replacing this task's slot) must not
        // end the turn.
        let interrupt = async {
            // Release the channel's read guard before any await.
            let code = cancelled
                .wait_for(Option::is_some)
                .await
                .map(|code| code.unwrap_or(INTERRUPTED));
            match code {
                Ok(code) => code,
                Err(_) => std::future::pending().await,
            }
        };
        let mut accounting = Accounting::default();
        let mut landed = None;
        // Cancelling mid-job loses nothing: a job the worker has taken runs
        // to its commit, and the worker publishes whatever committed.
        // The pull cord parks the turn rather than ending it, and so does
        // the daemon stopping: the next start continues it.
        let mut stopped = None;
        let mut result = tokio::select! {
            biased;
            code = interrupt => {
                stopped = match code {
                    PAUSED => Some(Halt::Pause),
                    SHUTDOWN => Some(Halt::Shutdown),
                    _ => None,
                };
                fail(code)
            }
            result = self.rounds(&mut accounting, &mut landed) => result,
        };
        // Drained at a boundary, or before a model call's retry: parked as
        // a stop parks it, with nothing left uncertain.
        if matches!(&result, Ok(Round::Drained))
            || matches!(&result, Err(error) if error.code == SHUTDOWN)
        {
            (stopped, result) = (Some(Halt::Shutdown), fail(SHUTDOWN));
        }
        // The rounds future is gone, so cancellation cannot discard this flush.
        // A refresh it had already sent is billed: record what it cost.
        if let Some(Ok((usage, sent_at))) = settle(&mut accounting.refresh).await
            && let Err(error) = self.record_refresh(usage, sent_at).await
        {
            result = Err(error);
        }
        // So is a summary that landed beside a call the rounds ended in, and
        // it is installed as a summary before that call would have been; one
        // still running was cancelled with them, as a call is.
        if let Err(error) = self
            .join_summary(None, &mut landed, &mut 0, &mut 0, &mut accounting)
            .await
        {
            result = Err(error);
        }
        accounting.retries += self.side.retries.swap(0, Relaxed);
        accounting.paced_ms += self.side.paced_ms.swap(0, Relaxed);
        let summary_retry_ms = accounting.summary_retry();
        let (retries, paced_ms) = accounting.totals();
        let summary_ms = self.side.summary_ms.swap(0, Relaxed);
        let turn = self.turn;
        let flushed = if let Ok(Round::Paced(at)) = &result {
            let (at, attempts, spent) = (*at, accounting.call_attempts, accounting.call_spent_ms);
            let fallback = accounting.call_fallback.take();
            let compaction = accounting.compaction;
            let copied = accounting.copied.take().filter(|_| compaction);
            let route = accounting.route.get().cloned();
            let blocked = accounting.blocked.take();
            let held_since = accounting.held_since;
            // Commit the park and its accounting together, outside cancellation.
            self.store
                .op("suspend_paced", move |db| {
                    db.suspend_paced(
                        turn,
                        at,
                        attempts,
                        spent,
                        retries,
                        paced_ms,
                        summary_ms,
                        compaction,
                        copied,
                        route.as_deref(),
                        summary_retry_ms,
                        fallback,
                        blocked,
                        held_since,
                    )
                    .map(|_| ())
                })
                .await
        } else if let Some(stop) = stopped {
            let route = accounting.route.get().cloned();
            let parked = self
                .store
                .op("suspend_halted", move |db| {
                    db.suspend_halted(turn, stop, retries, paced_ms, summary_ms, route.as_deref())
                })
                .await;
            // Parked: nothing more to commit, and the service keeps a
            // wake-up that holds until `resume`, or the next start finds it.
            // A park of the turn's own that committed as the pull cancelled
            // it keeps its wake-ups, registered here as the rounds would
            // have. A park that could not be written ends the turn as the
            // pull or the stop used to.
            match parked {
                Ok(Halted::Parked(_)) => result = Ok(Round::Paced(0)),
                Ok(Halted::Already(waiting, lapse)) => {
                    result = Ok(self.rejoin(*waiting, lapse).await);
                }
                Err(_) => {}
            }
            Ok(())
        } else if retries > 0 || paced_ms > 0 || summary_ms > 0 {
            self.store
                .op("note_pacing", move |db| {
                    db.note_pacing(turn, retries, paced_ms, summary_ms)
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
        // A pull or a stop leaves a park as it is: the pause holds its
        // wake-up, and the next start finds it.
        if result.is_ok()
            && let Some(code) = *cancelled.borrow()
            && code != PAUSED
            && code != SHUTDOWN
        {
            result = fail(code);
        }
        let error = match result {
            Ok(Round::Parked) => return Exit::Parked,
            Ok(Round::Paced(resume_at_ms)) => return Exit::Paced(resume_at_ms),
            // The park is committed; the service only keeps its wake-up.
            Ok(Round::Lapses(at)) => return Exit::Paced(at),
            Ok(Round::Finished) => None,
            // Drained became a stop's park above; this is never reached.
            Ok(Round::Drained) => Some(Error::new(SHUTDOWN)),
            Err(error) => {
                self.handles.forget(Waiter::Turn(self.turn));
                Some(error)
            }
        };
        // The task wrapper commits completion before releasing the active slot.
        // Until then, the store keeps the bot durably busy.
        Exit::Finished(error)
    }

    /// Register the wake-ups of a park that committed as its task was
    /// cancelled, as a restart registers a parked turn's: a pool's time, a
    /// verdict or answer looked at again, or a wait's handles and its round's
    /// next lapse.
    async fn rejoin(&self, waiting: Waiting, lapse: Option<u64>) -> Round {
        if waiting.paced_since_ms.is_some() {
            return Round::Lapses(waiting.deadline_ms.unwrap_or(0));
        }
        if waiting.approval || waiting.load || waiting.answer.is_some() {
            self.handles.wake(self.bot.clone(), self.turn);
            return match waiting.deadline_ms.filter(|&at| at > now_ms()) {
                Some(at) => Round::Lapses(at),
                None => Round::Parked,
            };
        }
        self.handles
            .attach(
                &self.store,
                Waiter::Turn(self.turn),
                &waiting.handles,
                waiting.deadline_ms,
                waiting.any,
                Completion::Resume {
                    bot: self.bot.clone(),
                    turn: self.turn,
                },
            )
            .await;
        lapse.map_or(Round::Parked, Round::Lapses)
    }

    /// Prepare metadata and the encoded prefix once per model round. Retries
    /// stream the same immutable nodes without rebuilding the context view.
    async fn context(&self, bytes: usize, count: usize, prefix_budget: usize) -> Result<Context> {
        self.context_under(bytes, count, prefix_budget, None).await
    }

    /// `context`, under an earlier elision floor when one is given.
    async fn context_under(
        &self,
        bytes: usize,
        count: usize,
        prefix_budget: usize,
        floor: Option<i64>,
    ) -> Result<Context> {
        let bot = self.bot.clone();
        let window = self
            .store
            .op("window", move |db| {
                db.window_under(&bot, bytes as i64, count as i64, floor)
            })
            .await?;
        let Some(window) = window else {
            return Ok(Context::empty());
        };
        let prefix = self.prefix(&window, prefix_budget).await?;
        Ok(Context {
            window: Some(window),
            prefix,
            thinking: Strip::default(),
        })
    }

    async fn prefix(
        &self,
        window: &agent_runtime::store::Window,
        budget: usize,
    ) -> Result<ContextPrefix> {
        let listed = if window.omitted_items > 0 && self.settings().note_turns() > 0 {
            let (start, limit) = (window.ids[0], self.settings().note_turns());
            self.store
                .read("omitted_turns", move |db| db.omitted_turns(start, limit))
                .await?
        } else {
            Vec::new()
        };
        window.prefix(&listed, budget, self.settings().cache_hour())
    }

    /// The bot's settings, with one cache lifetime for every request that
    /// carries its pinned summary and note: the turn's model's, the
    /// summarizer's, which copies the turn's calls, and the fallback's,
    /// which answers them.
    fn with_one_lifetime(&self, record: &agent_runtime::store::Bot, model: &str) -> Settings {
        let summarizer = summarizer(record);
        one_lifetime(
            &self.providers,
            record.family().ok(),
            &record.settings,
            [
                Some(model),
                Some(summarizer.as_str()),
                record.fallback.as_deref(),
            ]
            .into_iter()
            .flatten(),
        )
    }

    /// The bot's settings. Every use is inside `rounds`, which reads them
    /// with the bot's record before anything else.
    fn settings(&self) -> &Settings {
        self.settings
            .get()
            .expect("rounds read the bot's settings first")
    }
    fn rules(&self) -> &agent_runtime::rules::Rules {
        self.rules.get().expect("rounds read the rules first")
    }
    /// The rulings of the calls being run ([`Turn::ruled`]).
    fn ruled(&self) -> Arc<agent_runtime::rules::Rules> {
        let ruled = self.ruled.lock().unwrap_or_else(|e| e.into_inner()).clone();
        ruled.unwrap_or_else(|| {
            self.rules
                .get()
                .expect("rounds read the rules first")
                .clone()
        })
    }
    fn set_ruled(&self, ruled: Option<Arc<agent_runtime::rules::Rules>>) {
        *self.ruled.lock().unwrap_or_else(|e| e.into_inner()) = ruled;
    }
    /// The configured envelope bounds input. Completion headroom is a soft
    /// compaction trigger, not a tax on every request's usable history.
    fn input_limit(&self) -> ContextUsage {
        ContextUsage {
            bytes: self.settings().context_bytes(),
            items: self.settings().context_items(),
        }
    }

    /// The view at the configured budget, fitted to the input limit.
    async fn fitted_context(&self) -> Result<Context> {
        let context = self
            .context(
                self.settings().context_bytes(),
                self.settings().context_items(),
                self.settings().context_bytes() * 2 / 3,
            )
            .await?;
        self.fit_context(context, self.input_limit()).await
    }

    async fn fit_context(&self, mut context: Context, limit: ContextUsage) -> Result<Context> {
        let mut raw = ContextUsage {
            bytes: self.settings().context_bytes(),
            items: self.settings().context_items(),
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
                Err(error) if error.code == "context_limit" && self.settings().note_turns() > 0 => {
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

    /// The window's items, those `strip` names without thinking.
    fn items(&self, context: &Context, strip: Strip) -> Items {
        match &context.window {
            Some(w) => self.window_items(
                context.prefix.bytes.clone(),
                Span {
                    ids: &w.ids,
                    sizes: &w.sizes,
                    thinking: &w.thinking,
                    pictured: &w.pictured,
                    elided: w.elided,
                },
                strip,
            ),
            None => self.window_items(context.prefix.bytes.clone(), Span::EMPTY, Strip::default()),
        }
    }

    /// Anthropic thinking replay. Each thinking block is bound to the exact
    /// context before it and to the model and endpoint that wrote it. Once
    /// the context in front of the window changes (the window slides, a
    /// compaction or note lands, or a fork starts from other instructions),
    /// or the request goes to another model or endpoint (a turn's own model,
    /// a fork on another model, a provider name bound elsewhere), every
    /// block written under the old one is sent without thinking. When the elision floor moves, only the blocks after
    /// the first newly stubbed result are: those before it still follow what
    /// they were written after, and the provider's cache still holds them.
    /// Blocks written after a change keep theirs. Other families send items
    /// unchanged.
    async fn bind_thinking(
        &self,
        record: &mut agent_runtime::store::Bot,
        context: &mut Context,
        (provider, model): (&Provider, &str),
    ) -> Result<()> {
        if let Some(window) = &context.window {
            context.thinking = self
                .bound(record, &context.prefix.bytes, window, (provider, model))
                .await?;
        }
        Ok(())
    }

    /// The strip for `window` sent behind `prefix` to `model` on `provider`,
    /// recorded on the bot as what its next request sends.
    async fn bound(
        &self,
        record: &mut agent_runtime::store::Bot,
        prefix: &[u8],
        window: &Window,
        (provider, model): (&Provider, &str),
    ) -> Result<Strip> {
        if window.family != agent_runtime::codec::Family::Anthropic || window.ids.is_empty() {
            return Ok(Strip::default());
        }
        let prefix = fingerprint(prefix, window.ids[0], provider.url().as_str(), model);
        let (next, elided) = (window.ids[window.ids.len() - 1] + 1, window.elided);
        let mut strip = record.thinking;
        if record.thinking_prefix != Some(prefix) || elided < record.thinking_elided {
            strip = Strip::from(next);
        } else if elided > record.thinking_elided {
            // Ids grow along a lineage, so the window's are ascending.
            let start = window
                .ids
                .partition_point(|id| *id <= record.thinking_elided);
            let end = window.ids.partition_point(|id| *id <= elided);
            let moved = window.ids[start..end].to_vec();
            let first = self
                .store
                .read("first_stub", move |db| db.first_stub(&moved))
                .await?;
            if let Some(first) = first {
                strip = strip.and(first, next);
            }
        }
        if (
            record.thinking_prefix,
            record.thinking,
            record.thinking_elided,
        ) != (Some(prefix), strip, elided)
        {
            let bot = self.bot.clone();
            self.store
                .op("set_thinking", move |db| {
                    db.set_thinking(&bot, prefix, strip, elided)
                })
                .await?;
            record.thinking_prefix = Some(prefix);
            record.thinking = strip;
            record.thinking_elided = elided;
        }
        Ok(strip)
    }

    /// The context prefix, then the items of `ids` read from the store in
    /// batches as the request streams; those `strip` names without thinking.
    fn window_items(&self, prefix: Bytes, span: Span<'_>, strip: Strip) -> Items {
        self.framed_items(prefix, span, strip, Bytes::new())
    }

    /// A copy of the bot's call with the compaction request after it.
    fn copied_items(&self, copy: Copied<'_>) -> Items {
        let (window, len) = (copy.window, copy.len);
        let span = Span {
            ids: &window.ids[..len],
            sizes: &window.sizes[..len],
            thinking: &window.thinking[..len],
            pictured: &window.pictured[..len],
            elided: window.elided,
        };
        self.framed_items(
            copy.prefix.clone(),
            span,
            copy.thinking,
            copy.request.clone(),
        )
    }

    /// Window items between `prefix` and `tail`, a trailing item with its
    /// leading comma, or nothing.
    fn framed_items(&self, prefix: Bytes, span: Span<'_>, strip: Strip, tail: Bytes) -> Items {
        let Span {
            ids,
            sizes,
            thinking,
            pictured,
            elided,
        } = span;
        let stripped: usize = ids
            .iter()
            .zip(thinking)
            .filter(|(id, _)| strip.strips(**id))
            .map(|(_, &bytes)| bytes as usize)
            .sum();
        let weight = (prefix.len()
            + sizes.iter().map(|&size| size as usize).sum::<usize>()
            + ids.len().saturating_sub(1)
            + tail.len())
        .saturating_sub(stripped);
        let pictures: i64 = pictured.iter().map(|&bytes| i64::from(bytes)).sum();
        let (store, chunks): (_, Arc<[_]>) =
            (self.store.clone(), batches(ids, sizes, pictured).into());
        Items::weighing((weight as i64 + pictures) as usize, weight, move || {
            let items = stream::iter([Ok(prefix.clone())]).chain(item_chunks(
                store.clone(),
                chunks.clone(),
                strip,
                elided,
            ));
            if tail.is_empty() {
                items.boxed()
            } else {
                items.chain(stream::iter([Ok(tail.clone())])).boxed()
            }
        })
    }

    /// The bytes `compact_at` and `compact_keep` are shares of: the
    /// configured budget, or less once the bot's token threshold comes
    /// first. The view's tokens are estimated from the bot's last call, the
    /// bytes it sent against the input tokens the provider counted, so the
    /// threshold falls where `compact_at` of this budget is the view that
    /// sends `compact_tokens`, `overhead` (instructions and tools) included.
    /// Before a first call there is no estimate and only bytes apply.
    fn soft_bytes(&self, record: &agent_runtime::store::Bot, overhead: usize) -> usize {
        let budget = self.settings().context_bytes();
        let window = self.window.get().copied().unwrap_or(200_000);
        let (Some(tokens), Some((bytes, counted))) =
            (self.settings().compact_tokens(window), record.sent)
        else {
            return budget;
        };
        let at = (u128::from(tokens) * u128::from(bytes) / u128::from(counted)).min(budget as u128)
            as usize;
        // Instructions and tools alone past the threshold: no summary
        // brings the view under it.
        match at.checked_sub(overhead) {
            Some(view) if view > 0 => (view * 100 / self.settings().compact_at()).min(budget),
            _ => budget,
        }
    }

    /// Whether the window has reached the compaction threshold, the point
    /// where answered tool results start going as stubs.
    fn elision_due(&self, context: &Context, output_bytes: Option<usize>, budget: usize) -> bool {
        let limit = self.input_limit();
        let reserve = output_bytes.unwrap_or(0).min(limit.bytes / 4);
        context.usage().bytes
            >= (budget / 100 * self.settings().compact_at()).min(limit.bytes - reserve)
    }

    /// Elision at a round boundary: tool results the model has answered,
    /// older than the newest `compact_keep` percent of the `budget`, go to it
    /// as stubs from this request on. No model call; each result stays whole
    /// in the store and the read tool returns it. A move must save a
    /// sixteenth of the budget, so a context of mostly other text does not
    /// rewrite its cached prefix each round for a little room. When the
    /// window cannot fit without it (`forced`), every answered result goes,
    /// whatever the keep target, and any saving counts. Returns whether the
    /// floor moved.
    async fn elide(&self, prefix: usize, forced: bool, budget: usize) -> Result<bool> {
        let limit = self.input_limit();
        let (keep, min_saving) = if forced {
            (0, 1)
        } else {
            let keep = (budget / 100 * self.settings().compact_keep())
                .min(
                    limit.bytes.saturating_sub(prefix) * self.settings().compact_keep()
                        / self.settings().compact_at(),
                )
                .max(1);
            (keep as i64, (budget / 16) as i64)
        };
        let bot = self.bot.clone();
        self.elide_as(move |db| db.elision_plan(&bot, keep, min_saving))
            .await
    }

    /// Elision for the pictures a request may take: past `MOST_PICTURES`
    /// or `MOST_PICTURE_BYTES`, the oldest go as their stubs until half of
    /// either remains, so the floor moves seldom. Like any elision, only
    /// what the model has answered goes. Returns whether the floor moved.
    async fn elide_pictures(&self) -> Result<bool> {
        let bot = self.bot.clone();
        self.elide_as(move |db| {
            db.picture_elision_plan(&bot, MOST_PICTURES / 2, MOST_PICTURE_BYTES / 2)
        })
        .await
    }

    /// Move the elision floor as `plan` says, if it says to.
    async fn elide_as(
        &self,
        plan: impl FnOnce(&Database) -> Result<Option<ElisionPlan>> + Send + 'static,
    ) -> Result<bool> {
        let Some(plan) = self.store.read("elision_plan", plan).await? else {
            return Ok(false);
        };
        let bot = self.bot.clone();
        match self
            .store
            .op("elide", move |db| db.elide(&bot, &plan))
            .await
        {
            Ok(_) => Ok(true),
            Err(error)
                if matches!(
                    error.code.as_str(),
                    "elision_not_forward" | "elision_version_shared"
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Compaction at a round boundary: once the turns since the last summary
    /// hold `compact_at` percent of the budget (see `soft_bytes`), or a
    /// client `requested` one, summarize everything older than the newest
    /// `compact_keep` percent, or than the newest boundary when requested, with the client's instructions
    /// and summarizer, and record the result as the new context start. The
    /// summary request copies `last`, the view this task's last call sent
    /// on the turn's `model`, when that is estimated cheaper; before this
    /// task's first call, the view itself when it is `inherited` from an
    /// earlier task's, as that call sent it through its newest boundary. A
    /// backlog larger than the budget is summarized oldest first, one bounded
    /// span per round boundary, until it fits. A failed summary
    /// leaves the context view unchanged and is reported live; the turn goes on
    /// with the window as it is. Returns a park time if the summarizer's
    /// call parked the turn.
    ///
    /// While the view and the reply still fit the input limit, with room in
    /// the round limit and the budget for both calls, the summary goes in
    /// `beside`, to run beside this boundary's call, which sends the view
    /// as it is; the round installs it once the call is recorded, before
    /// its tools run. Its plan and the view it copies are read here, as a
    /// summary before the call reads them. The time either way holds the
    /// turn counts toward its `summary_ms`.
    #[allow(clippy::too_many_arguments)]
    async fn compact_if_due<'a>(
        &'a self,
        record: &mut agent_runtime::store::Bot,
        model_rounds: &mut usize,
        turn: i64,
        accounting: &mut Accounting,
        tools: &Arc<serde_json::value::RawValue>,
        context: &mut Context,
        (last, inherited, model): (Option<&LastCall>, bool, &str),
        output_bytes: Option<usize>,
        beside: Option<&mut Option<Beside<'a>>>,
        requested: bool,
    ) -> Result<Compaction> {
        if record.compaction_instructions.is_none() {
            return Ok(Compaction::Skipped);
        }
        let pressure = context.pressure();
        let limit = self.input_limit();
        let reserve = output_bytes.unwrap_or(0).min(limit.bytes / 4);
        let budget = self.soft_bytes(record, record.instructions.len() + tools.get().len());
        if !requested
            && pressure.bytes
                < (budget / 100 * self.settings().compact_at()).min(limit.bytes - reserve)
            && pressure.items
                < (self.settings().context_items() * self.settings().compact_at() / 100)
                    .min(limit.items)
        {
            return Ok(Compaction::Skipped);
        }
        let (keep, keep_items) = if requested {
            (1, 1)
        } else {
            (
                (budget / 100 * self.settings().compact_keep())
                    .min(
                        limit.bytes.saturating_sub(context.prefix.bytes.len())
                            * self.settings().compact_keep()
                            / self.settings().compact_at(),
                    )
                    .max(1) as i64,
                (self.settings().context_items() * self.settings().compact_keep() / 100)
                    .min(
                        limit.items.saturating_sub(context.prefix.items)
                            * self.settings().compact_keep()
                            / self.settings().compact_at(),
                    )
                    .max(1) as i64,
            )
        };
        let ahead = Some(context.prefix.bytes.clone());
        let sent = match last {
            Some(last) => Some(Sent {
                view: SentView::Call(last),
                model,
                ahead,
            }),
            None if inherited => Some(Sent {
                view: SentView::Current(&*context),
                model,
                ahead,
            }),
            None => None,
        };
        // One that parked on its pool is not tried again before the pool
        // reopens, while the view and the reply still fit; one that
        // outgrows the limit summarizes first, and so does one a client
        // asked for, which parks on the pool rather than wait unasked.
        let fits = pressure.bytes + reserve < limit.bytes && pressure.items < limit.items;
        if fits && !requested && now_ms() < accounting.summary_retry_at {
            return Ok(Compaction::Skipped);
        }
        let _held = Held::from_now(&self.side.summary_ms);
        // Beside the call, a summary holds a copy's call and a request of
        // its own against the round limit. One a client asked for goes
        // before the call, as asked, and stays asked until it ends.
        let beside = beside
            .filter(|_| !requested && fits && *model_rounds + 2 < self.settings().max_rounds());
        let mut spent = Accounting {
            route: accounting.route.clone(),
            ..Accounting::default()
        };
        let planned = self
            .plan(
                record,
                turn,
                &mut spent,
                tools,
                (keep, keep_items),
                false,
                sent,
            )
            .await?;
        let compaction = match planned {
            None => Compaction::Skipped,
            Some(planned) => {
                // Under a budget it holds the most its calls may bill, read
                // from the larger of the view and the view it copies.
                let copied = planned
                    .copy
                    .as_ref()
                    .map_or(0, |(view, ..)| view.usage().bytes);
                let held = beside.as_ref().and_then(|_| {
                    self.summary_bound(record, context.usage().bytes.max(copied), tools)
                        .filter(|tokens| {
                            record.ceiling().is_none_or(|ceiling| {
                                record.tokens_used.saturating_add(*tokens) < ceiling
                            })
                        })
                });
                match (beside, held) {
                    (Some(slot), Some(tokens)) => {
                        // It spends only what it holds: its calls and their
                        // retries count from the rounds left after its
                        // share, and the bot's and turn's caps end at its share.
                        let (mut record, tools) = (record.clone(), tools.clone());
                        let share = record.tokens_used.saturating_add(tokens);
                        if let Some(budget) = &mut record.budget_tokens {
                            *budget = (*budget).min(share);
                        }
                        if let Some(cap) = &mut record.turn_budget {
                            cap.until = cap.until.min(share);
                        }
                        spent.beside = true;
                        accounting.summary_held = tokens;
                        *slot = Some(Box::pin(async move {
                            let (before, start) =
                                (record.tokens_used, self.settings().max_rounds() - 2);
                            let mut rounds = start;
                            let mut spent = Counted {
                                spent,
                                into: &self.side,
                            };
                            let result = self
                                .summarize(
                                    planned,
                                    &mut record,
                                    &mut rounds,
                                    turn,
                                    &mut spent.spent,
                                    &tools,
                                )
                                .await;
                            Landed {
                                result,
                                spent: Spent {
                                    tokens: record.tokens_used.saturating_sub(before),
                                    rounds: rounds - start,
                                },
                            }
                        }));
                        Compaction::Skipped
                    }
                    // Otherwise, or with no bound known, it goes before the
                    // call, its copy kept for a park record.
                    _ => {
                        accounting.copied = spent.copied.take();
                        self.summarize_now(planned, record, model_rounds, turn, accounting, tools)
                            .await?
                    }
                }
            }
        };
        if let Compaction::Done = compaction {
            *context = self
                .context(
                    self.settings().context_bytes(),
                    self.settings().context_items(),
                    self.settings().context_bytes() * 2 / 3,
                )
                .await?;
        }
        Ok(compaction)
    }

    /// The most a summary's two calls may bill toward a bot's or turn's cap:
    /// each sends at most `view` bytes, the tools and both instructions, a
    /// token to a byte at most, and generates at most the summarizer's
    /// output bound. Nothing without a budget; `None` when that bound is
    /// unknown, and then the summary goes before the call.
    fn summary_bound(
        &self,
        record: &agent_runtime::store::Bot,
        view: usize,
        tools: &serde_json::value::RawValue,
    ) -> Option<u64> {
        if record.ceiling().is_none() {
            return Some(0);
        }
        let reference = summarizer(record);
        let (name, model) = split_model(&reference).ok()?;
        let provider = match record.compaction_model {
            None => shaped(self.providers.get(name)?, self.settings()).ok()?,
            Some(_) => shaped_other(self.providers.get(name)?, self.settings()).ok()?,
        };
        let output = provider.output_byte_estimate(model)? / 4;
        let input = view
            + tools.get().len()
            + record.instructions.len()
            + record
                .compaction_instructions
                .as_deref()
                .map_or(0, str::len)
            + SUMMARY_REQUEST_BYTES;
        Some(2 * (input + output) as u64)
    }

    /// The current turn cannot fit the budget, alone or beside what goes
    /// ahead of it. Elide what the model has answered and look again; if it
    /// still cannot fit, summarize all but the newest boundary, a round
    /// inside the turn or its prompt, and look again. Each look fits the
    /// view to the input limit, prefix included, so `Fits` is a view that
    /// can be sent. Stuck when neither applies or the view still cannot fit.
    /// The first summary may copy `last`, the view the task's last call sent
    /// on `model`; one installed replaces that view, so `last` clears.
    #[allow(clippy::too_many_arguments)]
    async fn overflow(
        &self,
        record: &mut agent_runtime::store::Bot,
        model_rounds: &mut usize,
        turn: i64,
        accounting: &mut Accounting,
        tools: &serde_json::value::RawValue,
        elides: bool,
        (last, model): (&mut Option<LastCall>, &str),
    ) -> Result<Overflow> {
        let fits = |context: Result<Context>| match context {
            Ok(context) => Ok(Some(context)),
            Err(error) if error.code == "context_limit" => Ok(None),
            Err(error) => Err(error),
        };
        if elides
            && self.elide(0, true, 0).await?
            && let Some(context) = fits(self.fitted_context().await)?
        {
            return Ok(Overflow::Fits(Box::new(context)));
        }
        if record.compaction_instructions.is_none() {
            return Ok(Overflow::Stuck);
        }
        // A backlog larger than the summarizer's budget takes several
        // catch-up steps at this head; each must shrink the view, and each
        // counts as a model round, so the steps end.
        loop {
            let sent = last.as_ref().map(|last| Sent {
                view: SentView::Call(last),
                model,
                ahead: None,
            });
            match self
                .compact(
                    record,
                    model_rounds,
                    turn,
                    accounting,
                    tools,
                    (1, 1),
                    true,
                    sent,
                )
                .await?
            {
                Compaction::Parked(until) => return Ok(Overflow::Parked(until)),
                Compaction::Skipped => return Ok(Overflow::Stuck),
                Compaction::Done => {
                    *last = None;
                    if let Some(context) = fits(self.fitted_context().await)? {
                        return Ok(Overflow::Fits(Box::new(context)));
                    }
                    if *model_rounds >= self.settings().max_rounds() {
                        return Ok(Overflow::Stuck);
                    }
                }
            }
        }
    }

    /// One summary: plan the span older than the newest boundary whose tail
    /// reaches the keep targets, summarize it with the client's
    /// instructions and summarizer, and record it as the new context start.
    /// When the summarizer is the model that made the bot's last call and
    /// `sent`, the view that call sent, holds the whole span, the request
    /// may copy that call with the instructions in a request after it, so
    /// the provider reads it from cache; it does when that is estimated
    /// cheaper than a request of its own: the instructions, then the span.
    /// `again` when the view cannot fit (overflow): a summary already made
    /// at this head may take another step. A failed summary leaves the view
    /// unchanged and is reported live.
    #[allow(clippy::too_many_arguments)]
    async fn compact(
        &self,
        record: &mut agent_runtime::store::Bot,
        model_rounds: &mut usize,
        turn: i64,
        accounting: &mut Accounting,
        tools: &serde_json::value::RawValue,
        keeps: (i64, i64),
        again: bool,
        sent: Option<Sent<'_>>,
    ) -> Result<Compaction> {
        let _held = Held::from_now(&self.side.summary_ms);
        match self
            .plan(record, turn, accounting, tools, keeps, again, sent)
            .await?
        {
            Some(planned) => {
                self.summarize_now(planned, record, model_rounds, turn, accounting, tools)
                    .await
            }
            None => Ok(Compaction::Skipped),
        }
    }

    /// A planned summary's call, then its install, before the turn's call.
    async fn summarize_now(
        &self,
        planned: Planned<'_>,
        record: &mut agent_runtime::store::Bot,
        model_rounds: &mut usize,
        turn: i64,
        accounting: &mut Accounting,
        tools: &serde_json::value::RawValue,
    ) -> Result<Compaction> {
        match self
            .summarize(planned, record, model_rounds, turn, accounting, tools)
            .await?
        {
            Summary::Written(written) => self.install(self.store.slot().await?, *written).await,
            Summary::Parked(until) => Ok(Compaction::Parked(until)),
            Summary::Skipped => Ok(Compaction::Skipped),
        }
    }

    /// Everything a summary does before its call, at the boundary: plan
    /// the span, pick the summarizer, and choose between a copy of the
    /// view a call sent and a request of its own. `None` when there is
    /// nothing to summarize or the summarizer is not served, reported live.
    #[allow(clippy::too_many_arguments)]
    async fn plan(
        &self,
        record: &mut agent_runtime::store::Bot,
        turn: i64,
        accounting: &mut Accounting,
        tools: &serde_json::value::RawValue,
        (keep, keep_items): (i64, i64),
        again: bool,
        sent: Option<Sent<'_>>,
    ) -> Result<Option<Planned<'_>>> {
        let limit = self.input_limit();
        let (max_bytes, max_items) = (limit.bytes as i64, limit.items as i64);
        // Nodes are immutable and only this turn moves the bot's head, so
        // the reader's snapshot plans what the worker would. A catch-up walk
        // over a long backlog goes in pieces, so neither other bots' commits
        // nor their context reads wait behind all of it.
        let plan = match self
            .plan_compaction(keep, keep_items, max_bytes, max_items, again)
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
        let (name, _) = split_model(&reference)?;
        // The span goes to the summarizer as stored, so it must speak the
        // bot's family, as creation checked; a provider name can be bound to
        // another since.
        let summarizer = match self.providers.get(name) {
            Some(provider) if provider.family() == record.family()? => {
                match record.compaction_model {
                    // The bot's own model, whose cache a summary reads.
                    None => shaped(provider, self.settings())?,
                    Some(_) => shaped_other(provider, self.settings())?,
                }
            }
            found => {
                let (error, detail) = match found {
                    None => ("provider_unavailable", name),
                    Some(_) => ("provider_family_mismatch", reference.as_str()),
                };
                self.hub
                    .live(
                        &self.bot,
                        json!({"event":"compaction_failed","bot":self.bot,"turn":turn,"durable":false,
                            "error":error,"detail":detail}),
                    )
                    .await?;
                return Ok(None);
            }
        };
        // A turn may run on another model than the bot's, and then the
        // bot's summarizer can neither read that call's cache nor take its
        // routing token or thinking. A copy whose reply is no summary, a
        // tool call or no text, is asked again as a request of its own.
        let sent = sent.filter(|sent| sent.model == reference);
        // A call's view is read again only now that a summary may copy it.
        let sent = match sent {
            Some(Sent {
                view: SentView::Call(call),
                ahead,
                ..
            }) => self
                .view_of(call)
                .await?
                .map(|(view, whole)| (view, whole, ahead)),
            Some(Sent {
                view: SentView::Current(view),
                ahead,
                ..
            }) => Some((view.clone(), false, ahead)),
            None => None,
        };
        let mut choice = Choice::default();
        accounting.copied = None;
        let mut copy = None;
        if let Some((view, whole, ahead)) = sent {
            let instructions = record.compaction_instructions.clone().unwrap();
            let (made, made_copy) = self
                .summary_copy(
                    record,
                    (&view, whole),
                    ahead.as_ref(),
                    &plan,
                    (&summarizer, split_model(&reference)?.1),
                    &instructions,
                    tools,
                    accounting,
                )
                .await?;
            choice = made;
            let made_copy = made_copy.map(|(_, len, thinking, request)| (len, thinking, request));
            copy = made_copy.map(|(len, thinking, request)| (view, len, thinking, request));
        }
        Ok(Some(Planned {
            plan,
            reference,
            summarizer,
            copy,
            choice,
        }))
    }

    /// The summary's call, asked again as a request of its own when a
    /// copy's reply is no summary; `install` records what it wrote. A
    /// failed summary leaves the view unchanged and is reported live.
    async fn summarize(
        &self,
        planned: Planned<'_>,
        record: &mut agent_runtime::store::Bot,
        model_rounds: &mut usize,
        turn: i64,
        accounting: &mut Accounting,
        tools: &serde_json::value::RawValue,
    ) -> Result<Summary> {
        let Planned {
            plan,
            reference,
            summarizer,
            copy: mut made,
            mut choice,
        } = planned;
        let (name, model) = split_model(&reference)?;
        let (summary, usage) = loop {
            let mut instructions = record.compaction_instructions.clone().unwrap();
            // Only the first attempt copies; what a park record keeps of
            // the copy goes with it.
            let current = made.take();
            if current.is_none() {
                accounting.copied = None;
            }
            let copy = current.as_ref().map(|(view, len, thinking, request)| {
                let window = view.window.as_ref().expect("a copy reads a window");
                (&view.prefix.bytes, window, *len, *thinking, request.clone())
            });
            let copied = copy.is_some();
            if !copied {
                choice.copy = None;
            }
            // The summarizer's instructions differ from the bot's, so no
            // thinking block in the span is bound to a request of its own,
            // which leaves them out: summed once, for its estimate and each
            // attempt's body.
            let family = summarizer.family();
            let stripped = match family {
                agent_runtime::codec::Family::Anthropic if !copied => {
                    self.span_thinking(&plan).await?
                }
                _ => 0,
            };
            if choice.own_cost.is_none() {
                choice.own_cost = Some(
                    self.own_fixed(family, &instructions, tools)?
                        + own_items(family, &plan)?.saturating_sub(stripped),
                );
            }
            // Messages requires definitions for historical tool blocks. Reuse the
            // already encoded bot selection; tool_choice disables new calls.
            // Responses accepts historical calls without definitions. A copy
            // keeps the call's tools and tool choice, which the cache covers.
            let empty;
            let (body, summary_tools) = match &copy {
                Some((prefix, window, len, thinking, request)) => {
                    instructions = record.instructions.clone();
                    let copied = Copied {
                        prefix,
                        window,
                        len: *len,
                        thinking: *thinking,
                        request,
                    };
                    (Body::Copied(copied), tools)
                }
                None => match summarizer.family() {
                    agent_runtime::codec::Family::Anthropic => (Body::Span(&plan, stripped), tools),
                    agent_runtime::codec::Family::Responses => {
                        empty = self
                            .registry
                            .encoded(summarizer.family(), &[], &[], false)?;
                        (Body::Span(&plan, stripped), &*empty)
                    }
                },
            };
            let completion = match self
                .call_with(
                    &summarizer,
                    model,
                    &instructions,
                    summary_tools,
                    body,
                    true,
                    record,
                    model_rounds,
                    turn,
                    accounting,
                )
                .await
            {
                Ok(Some(completion)) => {
                    accounting.copied = None;
                    completion
                }
                Ok(None) => return Ok(Summary::Parked(accounting.parked_until)),
                // A stopping daemon sent no request: a summary at the
                // boundary parks the turn, planned again when it continues;
                // one beside the turn's call is left for a later boundary.
                Err(error) if error.code == SHUTDOWN => {
                    accounting.copied = None;
                    return match accounting.beside {
                        true => Ok(Summary::Skipped),
                        false => Err(error),
                    };
                }
                Err(error) => {
                    accounting.copied = None;
                    self.hub
                        .live(
                            &self.bot,
                            json!({"event":"compaction_failed","bot":self.bot,"turn":turn,"durable":false,
                                "error":error.code,"detail":error.detail}),
                        )
                        .await?;
                    return Ok(Summary::Skipped);
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
            let usage = completion.usage.map(|usage| priced_at(usage, name, model));
            let summary = completion_text(&completion.items);
            // A copy offers the bot's tools; a call there is not a summary.
            let invalid = if !completion.calls.is_empty() {
                Error::new("compaction_tool_call")
            } else if summary.len() > plan.summary_bytes {
                Error::new("compaction_summary_limit")
            } else if summary.trim().is_empty() {
                Error::new("empty_summary")
            } else {
                break (summary, usage);
            };
            self.store
                .op("compaction_usage", move |db| {
                    db.compaction_usage(turn, usage.as_ref())
                })
                .await?;
            // The request of its own is another call, checked as each is.
            if !copied
                || invalid.code == "compaction_summary_limit"
                || record.exhausted().is_some()
                || *model_rounds >= self.settings().max_rounds()
            {
                self.compaction_failed(turn, &invalid).await?;
                return Ok(Summary::Skipped);
            }
            self.hub
                .live(
                    &self.bot,
                    json!({"event":"compaction_failed","bot":self.bot,"turn":turn,"durable":false,
                        "error":invalid.code,"detail":invalid.detail,"fallback":true}),
                )
                .await?;
        };
        let mut request = choice.event();
        request["beside"] = accounting.beside.into();
        Ok(Summary::Written(Box::new(Written {
            plan,
            summary,
            usage,
            request,
        })))
    }

    /// The summary that ran beside the call, once the call is recorded:
    /// the round waits for it, counts what it spent toward the turn, and
    /// installs what it wrote before the round's tools run; whether it was
    /// installed. One that parked on its pool parks nothing, and is not
    /// tried again before the pool reopens.
    async fn join_summary(
        &self,
        running: Option<Beside<'_>>,
        landed: &mut Option<Landed>,
        tokens_used: &mut u64,
        model_rounds: &mut usize,
        accounting: &mut Accounting,
    ) -> Result<bool> {
        let _held = Held::from_now(&self.side.summary_ms);
        if let Some(running) = running {
            *landed = Some(running.await);
        }
        // A written summary leaves the slot only once its install has room
        // on the store's queue: a cancellation before then leaves it to
        // `execute`, which installs and bills it.
        let slot = match landed {
            Some(Landed {
                result: Ok(Summary::Written(_)),
                ..
            }) => Some(self.store.slot().await?),
            Some(_) => None,
            None => return Ok(false),
        };
        let Some(Landed { result, spent }) = landed.take() else {
            return Ok(false);
        };
        spent.count(tokens_used, model_rounds);
        let installed = match (result, slot) {
            (Ok(Summary::Written(written)), Some(slot)) => self.install(slot, *written).await,
            (Ok(Summary::Written(_)), None) => unreachable!("a written summary reserves a slot"),
            (Ok(Summary::Parked(until)), _) => {
                accounting.summary_retry_at = until;
                Ok(Compaction::Skipped)
            }
            (Ok(Summary::Skipped), _) => Ok(Compaction::Skipped),
            (Err(error), _) => Err(error),
        };
        Ok(matches!(installed?, Compaction::Done))
    }

    /// Record a summary as the new context start, at a boundary: the view
    /// changes only between calls. One job holds the install and, when it
    /// fails, the summary's bill, so once `slot` sends it no cancellation
    /// can lose either.
    async fn install(&self, slot: Slot<'_>, written: Written) -> Result<Compaction> {
        let Written {
            plan,
            summary,
            usage,
            request,
        } = written;
        let (bot, turn) = (self.bot.clone(), self.turn);
        let note_turns = self.settings().note_turns();
        let input_limit = self.input_limit();
        let installed = slot
            .op("compact", move |db| {
                let installed = db.compact(
                    &bot,
                    &plan,
                    &summary,
                    usage.as_ref(),
                    note_turns,
                    input_limit,
                    request,
                );
                if installed.is_err() {
                    db.compaction_usage(turn, usage.as_ref())?;
                }
                installed
            })
            .await;
        if let Err(error) = installed {
            if matches!(
                error.code.as_str(),
                "compaction_not_smaller" | "compaction_context_limit" | "compaction_version_shared"
            ) {
                self.compaction_failed(turn, &error).await?;
                return Ok(Compaction::Skipped);
            }
            return Err(error);
        }
        Ok(Compaction::Done)
    }

    /// A copy of a call's view for a summary, `whole` when the view is
    /// exactly what the call sent, else known sent through the span. Made
    /// when the view holds the whole span and a copy is estimated cheaper
    /// than a request of its own; returned with the choice and its
    /// estimates. What a park record keeps of the copy goes in
    /// `accounting`: the window's floor, start, and end when `whole`, and
    /// what went ahead of it when the view now (`ahead`) no longer sends
    /// that.
    #[allow(clippy::too_many_arguments)]
    async fn summary_copy<'a>(
        &self,
        record: &mut agent_runtime::store::Bot,
        (view, whole): (&'a Context, bool),
        ahead: Option<&Bytes>,
        plan: &agent_runtime::store::CompactionPlan,
        (summarizer, model): (&Provider, &str),
        instructions: &str,
        tools: &serde_json::value::RawValue,
        accounting: &mut Accounting,
    ) -> Result<(Choice, Option<(&'a Window, usize, Strip, Bytes)>)> {
        let family = summarizer.family();
        let span = view.window.as_ref().and_then(|window| {
            let at = window.ids.binary_search(plan.ids.first()?).ok()?;
            let span = at..at + plan.ids.len();
            (window.ids.get(span.end - 1) == plan.ids.last()).then_some((window, span))
        });
        let Some((window, span)) = span else {
            return Ok((Choice::default(), None));
        };
        let request = Bytes::from(agent_runtime::store::CompactionPlan::request(
            family,
            instructions,
            plan.summary_bytes,
        )?);
        let thinking = self
            .bound(record, &view.prefix.bytes, window, (summarizer, model))
            .await?;
        let sent = if whole { window.ids.len() } else { span.end };
        let choice = choose(&Shape {
            family,
            ids: &window.ids,
            sizes: &window.sizes,
            thinking: &window.thinking,
            strip: thinking,
            span,
            sent,
            past: !plan.catch_up,
            fixed: record.instructions.len() + tools.get().len(),
            ahead: ContextUsage {
                bytes: view.prefix.bytes.len(),
                items: view.prefix.items,
            },
            request: request.len(),
            own_fixed: self.own_fixed(family, instructions, tools)?,
            own_items: own_items(family, plan)?,
            limit: self.input_limit(),
        });
        let Some(len) = choice.copy else {
            return Ok((choice, None));
        };
        let prefix = if ahead == Some(&view.prefix.bytes) {
            None
        } else {
            let text =
                std::str::from_utf8(&view.prefix.bytes).map_err(|_| Error::new("storage_error"))?;
            Some((text.to_owned(), view.prefix.items))
        };
        accounting.copied = Some(agent_runtime::store::CopiedCall {
            floor: window.elided,
            first: window.ids[0],
            last: whole.then(|| window.ids[window.ids.len() - 1]),
            prefix,
        });
        Ok((choice, Some((window, len, thinking, request))))
    }

    /// The view `call` sent, read again for a summary to copy, and whether
    /// the copy may take its window whole: under the floor the call's
    /// window was read under, through the node it ended at when known,
    /// behind what went ahead of it. A view that outgrew the budget since
    /// is read only back to the saved start, where the call's window was
    /// read from: the call fit the budget, and the view since holds one
    /// round more. `None` when the window no longer starts, or ends, where
    /// the call's did.
    async fn view_of(&self, call: &LastCall) -> Result<Option<(Context, bool)>> {
        let (bot, floor) = (self.bot.clone(), call.floor);
        let (bytes, items) = (
            self.settings().context_bytes() as i64,
            self.settings().context_items() as i64,
        );
        let window = self
            .store
            .op("window", move |db| {
                match db.window_under(&bot, bytes, items, Some(floor)) {
                    Err(error) if error.code == "context_limit" => {
                        if db.context_start(&bot)?.is_none() {
                            return Ok(None);
                        }
                        db.window_under(&bot, UNBOUNDED, UNBOUNDED, Some(floor))
                    }
                    read => read,
                }
            })
            .await?;
        let Some(mut window) = window else {
            return Ok(None);
        };
        if window.ids.first() != Some(&call.first) {
            return Ok(None);
        }
        if let Some(last) = call.last {
            let Ok(at) = window.ids.binary_search(&last) else {
                return Ok(None);
            };
            window.truncate(at + 1);
        }
        let prefix = match &call.ahead {
            // A copy reads only the bytes and items of what went ahead.
            Some((bytes, items)) => ContextPrefix {
                bytes: bytes.clone(),
                items: *items,
                required: ContextUsage::default(),
            },
            None => {
                self.prefix(&window, self.settings().context_bytes() * 2 / 3)
                    .await?
            }
        };
        let view = Context {
            window: Some(window),
            prefix,
            thinking: Strip::default(),
        };
        Ok(Some((view, call.last.is_some())))
    }

    async fn plan_compaction(
        &self,
        keep: i64,
        keep_items: i64,
        max_bytes: i64,
        max_items: i64,
        again: bool,
    ) -> Result<Option<agent_runtime::store::CompactionPlan>> {
        use agent_runtime::store::Planning;
        let bot = self.bot.clone();
        let planning = self
            .store
            .read("compaction_plan", move |db| {
                db.compaction_plan(&bot, keep, keep_items, max_bytes, max_items, again)
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

    /// What a request of its own sends besides its items: the summarizer's
    /// instructions and tools, none on Responses.
    fn own_fixed(
        &self,
        family: agent_runtime::codec::Family,
        instructions: &str,
        tools: &serde_json::value::RawValue,
    ) -> Result<usize> {
        Ok(instructions.len()
            + match family {
                agent_runtime::codec::Family::Anthropic => tools.get().len(),
                agent_runtime::codec::Family::Responses => {
                    self.registry.encoded(family, &[], &[], false)?.get().len()
                }
            })
    }

    /// The thinking bytes a request of its own leaves out of the span.
    async fn span_thinking(&self, plan: &agent_runtime::store::CompactionPlan) -> Result<usize> {
        let ids = plan.ids.clone();
        Ok(self
            .store
            .read("thinking_of", move |db| db.thinking_of(&ids))
            .await?
            .iter()
            .map(|&bytes| bytes as usize)
            .sum())
    }

    /// The summarizer's request body: the previous summary, if any, then
    /// the span's items in store-read batches, then the request to write.
    /// `stripped` is the span's thinking, which the body leaves out.
    async fn span_items(
        &self,
        plan: &agent_runtime::store::CompactionPlan,
        stripped: usize,
    ) -> Result<Items> {
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
        let total = own_items(family, plan)?.saturating_sub(stripped);
        if total > self.input_limit().bytes
            || plan.ids.len() + 1 + usize::from(plan.previous_summary.is_some())
                > self.input_limit().items
        {
            return fail("compaction_input_limit");
        }
        let (head, tail) = (Bytes::from(head), Bytes::from(tail));
        let (store, chunks): (_, Arc<[_]>) = (
            self.store.clone(),
            batches(&plan.ids, &plan.sizes, &plan.pictured).into(),
        );
        let pictures: i64 = plan.pictured.iter().map(|&bytes| i64::from(bytes)).sum();
        // Elided results go as the stubs the model last saw.
        let elided = plan.elided;
        Ok(Items::weighing(
            (total as i64 + pictures) as usize,
            total,
            move || {
                stream::iter([Ok(head.clone())])
                    .chain(item_chunks(
                        store.clone(),
                        chunks.clone(),
                        Strip::from(i64::MAX),
                        elided,
                    ))
                    .chain(stream::iter([Ok(tail.clone())]))
                    .boxed()
            },
        ))
    }

    async fn rounds(
        &self,
        accounting: &mut Accounting,
        landed: &mut Option<Landed>,
    ) -> Result<Round> {
        let (bot, turn) = (self.bot.clone(), self.turn);
        let mut record = self.store.op("inspect", move |db| db.inspect(&bot)).await?;
        let context = self.store.op("context", move |db| db.context(turn)).await?;
        self.settings
            .get_or_init(|| self.with_one_lifetime(&record, &context.model));
        if self.rules.get().is_none() {
            let policy = record.settings.rules.clone().unwrap_or_default();
            let _ = self
                .rules
                .set(Arc::new(agent_runtime::rules::Rules::new(policy)));
        }
        // A turn may run at its own effort; everything after reads the record.
        record.effort = context.effort.clone();
        record.turn_budget = context.budget;
        let (provider, model) = split_model(&context.model)?;
        let Some(provider) = self.providers.get(provider) else {
            let error = Error::with("provider_unavailable", provider);
            // A turn this daemon resumed at a model call without its
            // provider, set up again only by a restart, which tries it at
            // once: held there, keeping what its park kept. A park on a
            // call the model made has that call's continuation to keep,
            // which a park at a model call cannot, and a turn that starts
            // here was let in by a start that found the provider: those
            // end, as before.
            let Some(waiting) = (self.resumed.as_ref()).filter(|w| {
                w.paced_since_ms.is_some() && record.settings.holds(w.held_since_ms, now_ms())
            }) else {
                return Err(error);
            };
            if let Some(route) = &waiting.route {
                let _ = accounting.route.set(route.clone());
            }
            accounting.summary_retry_at = waiting.summary_retry_ms.unwrap_or(0);
            accounting.held_since = waiting.held_since_ms;
            accounting.call_fallback = waiting.fallback.clone();
            accounting.compaction = waiting.compaction;
            accounting.copied = waiting.copied.clone().filter(|_| waiting.compaction);
            accounting.hold(&error);
            return Ok(Round::Paced(accounting.parked_until));
        };
        // The daemon runs whatever provider set started it; the one thing a
        // conversation needs is that its provider speaks the bot's encoding.
        if provider.family() != record.family()? {
            return Err(Error::with(
                "provider_family_mismatch",
                context.model.as_str(),
            ));
        }
        let shaped = shaped(provider, self.settings())?;
        let provider: &Provider = &shaped;
        self.window
            .get_or_init(|| agent_runtime::provider::context_window(provider, model));
        let workspace = PathBuf::from(&context.workspace);
        // What this turn's children inherit: the CLI a bot runs to delegate
        // needs a model for the peer (its own by default) and its effort
        // level when it has one, the bot's own name
        // so a created peer records who created it, this turn so a prompt it
        // submits records whose model wrote it, and its creator so a peer
        // can address the bot that spawned it.
        let mut environment = vec![
            ("AGENT_MODEL".to_owned(), context.model.clone()),
            ("AGENT_BOT".to_owned(), context.bot.clone()),
            ("AGENT_BOT_ID".to_owned(), context.bot_id.to_string()),
            ("AGENT_TURN".to_owned(), turn.to_string()),
        ];
        if let Some(level) = &record.effort {
            environment.push(("AGENT_EFFORT".to_owned(), level.clone()));
        }
        if let (Some(parent), Some(id)) = (&context.created_by, context.created_by_id) {
            environment.push(("AGENT_PARENT".to_owned(), parent.clone()));
            environment.push(("AGENT_PARENT_ID".to_owned(), id.to_string()));
        }
        let mut resume_window = false;
        // What a summary that parked had copied, for its retry to copy.
        let mut copied = None;
        if let Some(waiting) = &self.resumed {
            // The park did not end the turn, so neither does its route, nor
            // the wait for a summary's pool to reopen.
            if let Some(route) = &waiting.route {
                let _ = accounting.route.set(route.clone());
            }
            accounting.summary_retry_at = waiting.summary_retry_ms.unwrap_or(0);
            // A turn parked on a call the model made had replied; one parked
            // on its pool may have, before the call it parked at.
            accounting.answered = waiting.paced_since_ms.is_none() || {
                self.store
                    .read("turn_called", move |db| db.turn_called(turn))
                    .await?
            };
            // Only a pool park continues the same model call's retry budget.
            // A held turn's run of holds goes on until a call is answered.
            accounting.held_since = waiting.held_since_ms;
            if waiting.paced_since_ms.is_some() {
                accounting.call_attempts = waiting.call_attempts;
                accounting.call_spent_ms = waiting.call_spent_ms;
                accounting.call_fallback = waiting.fallback.clone();
                resume_window = !waiting.compaction;
                copied = waiting.copied.clone().filter(|_| waiting.compaction);
            } else {
                // A verdict park resumes at its gated call, which has not
                // started; an answer park first records the answer, and a
                // wait park the wait's result.
                if waiting.answer.is_some() {
                    let id = waiting.call_id.clone();
                    let outcome = match self
                        .store
                        .op("answer_take", move |db| db.answer_take(turn, &id, now_ms()))
                        .await?
                    {
                        CallAnswer::Given(outcome) => outcome,
                        // Woken by a later gated call's lapse, which ends
                        // the turn: the call ends unanswered.
                        CallAnswer::Pending { .. } => {
                            unserved("a later call's gate lapsed before the answer".into())
                        }
                    };
                    let id = waiting.call_id.clone();
                    self.store
                        .op("tool_finish", move |db| db.tool_finish(turn, &id, &outcome))
                        .await?;
                } else if waiting.poll {
                    // A terminal poll reads what the terminal wrote while
                    // the turn was parked, or how it ended; a restart ended
                    // it, so it no longer runs.
                    let results = self
                        .handles
                        .take_settled(&self.store, turn, &waiting.handles)
                        .await;
                    let ended = results.values().next().cloned();
                    let handle = match waiting.handles.first().map(|h| Handle::parse(h)) {
                        Some(Ok(Handle::Process(id))) => id,
                        _ => return fail("invalid_handle"),
                    };
                    let outcome = self
                        .registry
                        .poll_end(record.bot_id, handle, ended.as_deref())
                        .unwrap_or_else(failure);
                    let id = waiting.call_id.clone();
                    self.store
                        .op("tool_finish", move |db| db.tool_finish(turn, &id, &outcome))
                        .await?;
                } else if !waiting.approval && !waiting.load {
                    // A later call's lapse can wake the wait before its
                    // handles resolve: those are reported pending, as the
                    // wait's own timeout reports them.
                    let results = self
                        .handles
                        .take_settled(&self.store, turn, &waiting.handles)
                        .await;
                    // Background commands whose results this wait hands over.
                    let delivered: Vec<i64> = results
                        .iter()
                        .filter(|(_, result)| result["pending"] != true)
                        .filter_map(|(handle, _)| match Handle::parse(handle) {
                            Ok(Handle::Process(id)) => Some(id),
                            _ => None,
                        })
                        .collect();
                    // Ended early by a steer: the steer comes next, and
                    // the pending handles stay valid for another wait.
                    let steered = results.values().any(|result| result["pending"] == true) && {
                        let bot = self.bot.clone();
                        self.store
                            .read("joining_steer", move |db| db.joining_steer(&bot))
                            .await
                            .unwrap_or(false)
                    };
                    let mut result = wait_result(for_model(results));
                    if steered {
                        result["steered"] = json!(true);
                    }
                    let outcome = Outcome::text(result.to_string());
                    let id = waiting.call_id.clone();
                    self.store
                        .op("tool_finish", move |db| {
                            let (item, entry) = db.tool_finish(turn, &id, &outcome)?;
                            if let Some(node) = entry["data"]["node"].as_i64()
                                && !delivered.is_empty()
                            {
                                db.delivered(node, &delivered)?;
                            }
                            Ok((item, entry))
                        })
                        .await?;
                }
                // Calls that followed in the same model response, run as
                // their round was announced, whatever the rules say now.
                self.set_ruled(Some(Arc::new(waiting.rules())));
                let stop = self
                    .execute_calls(
                        waiting.pending.clone(),
                        (provider, model),
                        None,
                        &workspace,
                        &environment,
                        &record,
                        None,
                        accounting.route.get().map(String::as_str),
                        accounting.summary_retry(),
                    )
                    .await;
                self.set_ruled(None);
                if let Some(stop) = stop? {
                    return Ok(stop.round());
                }
            }
        }
        let mut model_rounds = context.model_rounds;
        let called = context.model.as_str();
        // This bot's tools, encoded once per distinct selection and shared.
        let tools = self.registry.encoded(
            provider.family(),
            &record.tools,
            &record.declared,
            record.sandboxed(),
        )?;
        // A stub names the `read` call that returns its result, so only a
        // bot that has the tool elides.
        let elides = record.tools.iter().any(|tool| tool == "read");
        // How a steer stayed queued for lack of room, while one still is.
        let mut capped = None;
        // A steer let in against the whole budget goes to the model before
        // any summary, so no summary spends the round or the budget it went
        // in for; the boundary after that call summarizes as usual.
        let mut steer_first = false;
        // The view this task's last call sent, for a summary to copy.
        let mut last: Option<LastCall> = None;
        // Whether this task has made a call, whose cache a summary beside
        // the next one reads.
        let mut sent_here = false;
        // A summary planned at this boundary to run beside its call.
        let mut beside: Option<Beside<'_>>;
        // Until this task calls or changes the view, the view is taken as
        // the bot's call before a new prompt or a wait sent it, through
        // its newest boundary, and its cache as still held, when that call
        // was on this turn's model: this turn's own before a wait, unless
        // a summary or elision rewrote the view after it, else the
        // previous turn's. Claude also keeps a history's cache only at
        // the effort that wrote it.
        let mut inherited = context.view_sent.unwrap_or_else(|| {
            context
                .previous_call
                .as_ref()
                .is_some_and(|(model, effort)| {
                    model == called
                        && (provider.family() != agent_runtime::codec::Family::Anthropic
                            || *effort == record.effort)
                })
        });
        while model_rounds < self.settings().max_rounds() {
            // A stopping daemon starts no call: the turn parks here, every
            // result so far committed, and the next start continues it.
            if self.stopping.load(Relaxed) {
                return Ok(Round::Drained);
            }
            // A refresh the last call left in flight ends here: the next call
            // reads the cache itself, and the budget counts what it billed.
            self.settle_refresh(&mut record, accounting).await?;
            // One planned at a boundary that started over has not run.
            beside = None;
            // The budget is checked before each call, so one call may overshoot.
            if let Some(error) = record.exhausted() {
                return Err(error);
            }
            // Past the threshold, the older turns are summarized before this
            // call, with the summarizer's own call paced and billed like any.
            // Resume the parked call, not the whole boundary. In particular,
            // an exhausted summary must not start over when the ordinary call
            // parks on the same pool. A later model round may compact again.
            let resuming = std::mem::take(&mut resume_window);
            // A summary that parked copies again what it copied.
            if let Some(call) = copied.take() {
                last = Some(call.into());
            }
            let output_bytes = provider.output_byte_estimate(model);
            let budget = self.soft_bytes(&record, record.instructions.len() + tools.get().len());
            let mut made_room = false;
            let mut context = match self
                .context(
                    self.settings().context_bytes(),
                    self.settings().context_items(),
                    self.settings().context_bytes() * 2 / 3,
                )
                .await
            {
                Ok(context) => context,
                Err(error) if error.code == "context_limit" => match self
                    .overflow(
                        &mut record,
                        &mut model_rounds,
                        turn,
                        accounting,
                        &tools,
                        elides,
                        (&mut last, called),
                    )
                    .await?
                {
                    Overflow::Fits(context) => {
                        (made_room, inherited) = (true, false);
                        *context
                    }
                    Overflow::Parked(until) => return Ok(Round::Paced(until)),
                    Overflow::Stuck => return Err(error),
                },
                Err(error) => return Err(error),
            };
            // The view usually sends what the last call sent ahead of its
            // window; while it does, the two share those bytes.
            if let Some((ahead, _)) = last.as_ref().and_then(|last| last.ahead.as_ref())
                && *ahead == context.prefix.bytes
            {
                context.prefix.bytes = ahead.clone();
            }
            // Steers go in before this call, measured against what this
            // view must send ahead of the turn: its summary, pinned context,
            // and notes, a note a tool wrote this round included, but not
            // the previews of omitted turns, which yield. One that goes in
            // sends the view back through the overflow, elision, and
            // compaction steps. It is measured against three quarters of
            // the budget, so those steps can make it room first.
            if self
                .absorb(&context.prefix, &mut capped, false, true)
                .await?
            {
                resume_window = resuming;
                continue;
            }
            // A summary copies the view a call sent, from before this
            // boundary's stubs.
            if elides
                && !resuming
                && self.elision_due(&context, output_bytes, budget)
                && self
                    .elide(context.prefix.bytes.len(), false, budget)
                    .await?
            {
                made_room = true;
                let elided = self
                    .context(
                        self.settings().context_bytes(),
                        self.settings().context_items(),
                        self.settings().context_bytes() * 2 / 3,
                    )
                    .await?;
                let sent = std::mem::replace(&mut context, elided);
                if std::mem::take(&mut inherited) && last.is_none() {
                    last = LastCall::of(&sent, false);
                }
            }
            // So do pictures past what a request takes, for any bot: their
            // stubs name no tool.
            if !resuming && context.overpictured() && self.elide_pictures().await? {
                made_room = true;
                let elided = self
                    .context(
                        self.settings().context_bytes(),
                        self.settings().context_items(),
                        self.settings().context_bytes() * 2 / 3,
                    )
                    .await?;
                let sent = std::mem::replace(&mut context, elided);
                if std::mem::take(&mut inherited) && last.is_none() {
                    last = LastCall::of(&sent, false);
                }
            }
            if !resuming && !std::mem::take(&mut steer_first) {
                // A client's request stays recorded until its summary ends:
                // one that parks finds it again as the turn resumes.
                let requested = std::mem::take(&mut record.compaction_requested)
                    | self.compact.swap(false, Relaxed);
                // Only behind a call this task made, whose cache the next
                // call reads: at a turn's start, or as a turn resumes, the
                // cache may have lapsed, and a summary first keeps that call
                // small.
                match self
                    .compact_if_due(
                        &mut record,
                        &mut model_rounds,
                        turn,
                        accounting,
                        &tools,
                        &mut context,
                        (last.as_ref(), inherited, called),
                        output_bytes,
                        sent_here.then_some(&mut beside),
                        requested,
                    )
                    .await?
                {
                    Compaction::Parked(parked) => return Ok(Round::Paced(parked)),
                    compaction => {
                        if requested {
                            let bot = self.bot.clone();
                            self.store
                                .op("request_compaction", move |db| {
                                    db.request_compaction(&bot, None, false)
                                })
                                .await?;
                        }
                        if let Compaction::Done = compaction {
                            made_room = true;
                            (last, inherited) = (None, false);
                        }
                    }
                }
            }
            // A steer the turn had no room for is tried again once elision
            // or a summary makes some, and so is one that arrived while this
            // boundary summarized. Past those steps a bot with a summarizer
            // measures it against the whole budget: when the turn's newest
            // round fills it, as one large result can, no summary makes
            // room, and the next round's summary can then take that round
            // behind the steer. Such a steer goes to the model next, before
            // this boundary's steps run again.
            if capped.is_some() && made_room {
                self.steers.store(true, Relaxed);
            }
            // A summary may have spent the last round or the budget; a steer
            // then stays queued rather than joining a turn that cannot call.
            let whole = self.summarizes(&record);
            if calls_left(&record, model_rounds)
                && self
                    .absorb(&context.prefix, &mut capped, whole, true)
                    .await?
            {
                (resume_window, steer_first) = (resuming, whole);
                continue;
            }
            if let Some(error) = record.exhausted() {
                return Err(error);
            }
            if model_rounds >= self.settings().max_rounds() {
                return fail("tool_round_limit");
            }
            // The view fits the budget alone but not beside what goes ahead
            // of it: the same forced recovery, then the steps again.
            let mut context = match self.fit_context(context, self.input_limit()).await {
                Ok(context) => context,
                Err(error) if error.code == "context_limit" => match self
                    .overflow(
                        &mut record,
                        &mut model_rounds,
                        turn,
                        accounting,
                        &tools,
                        elides,
                        (&mut last, called),
                    )
                    .await?
                {
                    Overflow::Fits(_) => {
                        inherited = false;
                        if capped.is_some() {
                            self.steers.store(true, Relaxed);
                        }
                        resume_window = resuming;
                        continue;
                    }
                    Overflow::Parked(until) => return Ok(Round::Paced(until)),
                    Overflow::Stuck => return Err(error),
                },
                Err(error) => return Err(error),
            };
            // Nor after this boundary's own steps: a summary that ran
            // while the stop began is kept, and its call does not start.
            if self.stopping.load(Relaxed) {
                return Ok(Round::Drained);
            }
            self.bind_thinking(&mut record, &mut context, (provider, model))
                .await?;
            // This call's view replaces the last one's, which goes before
            // the call streams.
            drop(last.take());
            inherited = false;
            // A summary planned at this boundary runs beside the call.
            let running = beside.take();
            // The call and its retries count from what is left after the
            // summary's share: its two rounds and the tokens it holds.
            let held = running
                .as_ref()
                .map(|_| (2, std::mem::take(&mut accounting.summary_held)));
            if let Some((rounds, tokens)) = held {
                model_rounds += rounds;
                record.tokens_used = record.tokens_used.saturating_add(tokens);
            }
            // The round limit's last call goes with the tools shown but no
            // call allowed, so the turn ends on a reply rather than failing
            // with its work done and unreported.
            let tool_calls = model_rounds + 1 < self.settings().max_rounds();
            if !tool_calls {
                self.hub
                    .live(
                        &self.bot,
                        json!({"event":"tool_calls_withheld","bot":self.bot,"turn":turn,
                            "durable":false,"reason":"tool_round_limit"}),
                    )
                    .await?;
            }
            let called = {
                let call = self.call(
                    provider,
                    model,
                    &tools,
                    &context,
                    tool_calls,
                    &mut record,
                    &mut model_rounds,
                    turn,
                    accounting,
                );
                match running {
                    None => (call.await, None),
                    Some(mut summary) => {
                        tokio::pin!(call);
                        tokio::select! {
                            biased;
                            called = &mut call => (called, Some(summary)),
                            // It waits outside the rounds once landed, so
                            // an interrupt still installs it.
                            done = summary.as_mut() => {
                                if let Ok(Summary::Written(_)) = &done.result {
                                    self.side.replaced.store(true, Relaxed);
                                }
                                *landed = Some(done);
                                (call.await, None)
                            }
                        }
                    }
                }
            };
            let (called, running) = called;
            self.side.replaced.store(false, Relaxed);
            if let Some((rounds, tokens)) = held {
                model_rounds -= rounds;
                record.tokens_used = record.tokens_used.saturating_sub(tokens);
            }
            // A call that withheld its replayed thinking recorded that on
            // the bot; refreshes send what it sent.
            if context.window.as_ref().is_some_and(|w| {
                w.family == agent_runtime::codec::Family::Anthropic && !w.ids.is_empty()
            }) {
                context.thinking = record.thinking;
            }
            let response = match called {
                Ok(Some(response)) => response,
                // It lands before the turn parks or fails.
                called => {
                    let joined = self
                        .join_summary(
                            running,
                            landed,
                            &mut record.tokens_used,
                            &mut model_rounds,
                            accounting,
                        )
                        .await;
                    called?;
                    joined?;
                    return Ok(Round::Paced(accounting.parked_until));
                }
            };
            sent_here = true;
            accounting.answered = true;
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
            // What the view cost in tokens, for the token threshold. One
            // fallback's attempts each counted the input.
            let sent = usage
                .as_ref()
                .filter(|usage| usage.input_tokens > 0 && usage.models.len() <= 1)
                .map(|usage| {
                    let bytes =
                        context.usage().bytes + record.instructions.len() + tools.get().len();
                    (bytes as u64, usage.input_tokens)
                });
            record.sent = sent.or(record.sent);
            let name = self.bot.clone();
            let gated = calls
                .iter()
                .any(|call| record.gated(call, &workspace, self.rules()));
            let rules = self
                .rules
                .get()
                .expect("rounds read the rules first")
                .clone();
            // Taken before the commit that stamps the gates' announcement,
            // so a gate's first lapse is never counted late.
            let planned = tokio::time::Instant::now();
            let append = self.store.op("append", move |db| {
                let entries = db.append_under(turn, items, &calls, usage.as_ref(), &rules)?;
                if let Some((bytes, tokens)) = sent {
                    db.record_sent(&name, bytes, tokens)?;
                }
                Ok((entries, gated.then(|| (db.verdicts_for(turn), planned))))
            });
            // A summary still running keeps streaming while the call's
            // items commit; one that lands waits outside the rounds.
            let (appended, running) = match running {
                None => (append.await, None),
                Some(mut summary) => {
                    tokio::pin!(append);
                    tokio::select! {
                        biased;
                        appended = &mut append => (appended, Some(summary)),
                        done = summary.as_mut() => {
                            *landed = Some(done);
                            (append.await, None)
                        }
                    }
                }
            };
            let announced = match appended {
                Ok((entries, verdicts)) => {
                    let nodes: Vec<i64> = entries
                        .iter()
                        .filter(|entry| entry["event"] == "message")
                        .filter_map(|entry| entry["data"]["node"].as_i64())
                        .collect();
                    provider.recorded(&self.bot, &nodes);
                    verdicts
                }
                Err(error) => {
                    self.failed_usage(response.usage.clone()).await?;
                    return Err(error);
                }
            };
            // The summary beside the call is installed after the call's
            // items and before its tools run. Its view replaces the one the
            // call sent, which a later summary no longer copies, and may
            // make a capped steer room.
            let installed = self
                .join_summary(
                    running,
                    landed,
                    &mut record.tokens_used,
                    &mut model_rounds,
                    accounting,
                )
                .await?;
            if installed && capped.is_some() {
                self.steers.store(true, Relaxed);
            }
            if installed && response.calls.is_empty() {
                // A steer that came during the call is weighed against the
                // view the summary left.
                context = match self
                    .context(
                        self.settings().context_bytes(),
                        self.settings().context_items(),
                        self.settings().context_bytes() * 2 / 3,
                    )
                    .await
                {
                    Ok(context) => context,
                    Err(error) if error.code == "context_limit" => return Ok(Round::Finished),
                    Err(error) => return Err(error),
                };
            }
            // A reply the provider paused partway through its own tools is
            // sent back as it is, and the provider resumes it.
            if response.calls.is_empty() && !response.paused {
                // A steer that arrived during the final call keeps the turn
                // going for one more round rather than ending it unheard,
                // when the budget and round limit allow one. A refresh the
                // reply left in flight is billed first, as the next round
                // would count it.
                self.settle_refresh(&mut record, accounting).await?;
                let whole = self.summarizes(&record);
                if calls_left(&record, model_rounds)
                    && self
                        .absorb(&context.prefix, &mut capped, whole, false)
                        .await?
                {
                    last = if installed {
                        None
                    } else {
                        LastCall::of(&context, true)
                    };
                    steer_first = whole;
                    continue;
                }
                return Ok(Round::Finished);
            }
            // The cache's lifetime runs from the sending of the call that
            // read it, or of a refresh sent while its reply streamed.
            let read_at = accounting
                .warm_read_at
                .unwrap_or_else(tokio::time::Instant::now);
            let stopped = accounting.warm_stopped;
            let summary_retry = accounting.summary_retry();
            let pending = &mut accounting.refresh;
            // No call has sent the view a summary installed here, so there
            // is no cache of it to keep warm.
            let mut warm = provider
                .keep_warm_after(model, record.effort.as_deref())
                .filter(|_| !installed)
                .map(|after| Warm {
                    provider,
                    model,
                    instructions: &record.instructions,
                    reasoning: record.effort.as_deref(),
                    tools: &tools,
                    context: &context,
                    thinking: context.thinking,
                    web_search: searches(&record),
                    after,
                    read_at,
                    stopped: stopped || record.exhausted().is_some(),
                    tokens: 0,
                    room: record
                        .ceiling()
                        .map(|c| c.saturating_sub(record.tokens_used)),
                    pending,
                });
            let stop = self
                .execute_calls(
                    response.calls,
                    (provider, model),
                    announced,
                    &workspace,
                    &environment,
                    &record,
                    warm.as_mut(),
                    accounting.route.get().map(String::as_str),
                    summary_retry,
                )
                .await?;
            let refreshed = warm.map_or(0, |warm| warm.tokens);
            record.tokens_used = record.tokens_used.saturating_add(refreshed);
            if let Some(stop) = stop {
                return Ok(stop.round());
            }
            // A summary installed after the call replaced the view it sent.
            last = if installed {
                None
            } else {
                LastCall::of(&context, true)
            };
        }
        fail("tool_round_limit")
    }

    /// The round boundary: queued steers become user items after everything
    /// recorded so far, while they fit beside what the view must send
    /// `ahead` of the turn; optional previews of omitted turns yield. The worker publishes each batch and answers the steers'
    /// waiters. One atomic read when nothing is waiting; the flag clears
    /// before the read, so a steer landing during it is seen next. A steer
    /// that stayed queued for lack of room is tried again once less goes
    /// ahead, a note cleared or shrunk, and once more against the `whole`
    /// budget when it stayed against three quarters. Returns whether any
    /// went in; `capped` holds how one stayed queued, and is left alone
    /// when nothing was tried.
    /// Whether a summary could take the rounds behind a steer admitted
    /// against the whole budget: the bot has compaction instructions and
    /// its summarizer is served here in the bot's family.
    fn summarizes(&self, record: &agent_runtime::store::Bot) -> bool {
        record.compaction_instructions.is_some()
            && split_model(&summarizer(record)).is_ok_and(|(name, _)| {
                self.providers
                    .get(name)
                    .is_some_and(|provider| record.family().is_ok_and(|f| provider.family() == f))
            })
    }

    /// End a refresh left in flight and count what it billed.
    async fn settle_refresh(
        &self,
        record: &mut agent_runtime::store::Bot,
        accounting: &mut Accounting,
    ) -> Result<()> {
        if let Some(refreshed) = settle(&mut accounting.refresh).await
            && let Some((tokens, _)) = self.account_refresh(refreshed).await?
        {
            record.tokens_used = record.tokens_used.saturating_add(tokens);
        }
        Ok(())
    }

    /// `attached` takes attached messages along with steers; a reply that
    /// called no tool takes steers alone, which an attached message must not
    /// turn into another round.
    async fn absorb(
        &self,
        ahead: &ContextPrefix,
        capped: &mut Option<Capped>,
        whole: bool,
        attached: bool,
    ) -> Result<bool> {
        let reserved = ahead.required;
        let retry = capped.is_some_and(|at| {
            reserved.bytes < at.ahead.bytes || reserved.items < at.ahead.items || whole && !at.whole
        });
        self.lift.clear();
        if !self.steers.swap(false, Relaxed) && !retry {
            return Ok(false);
        }
        let (mut steered, mut stayed) = (false, false);
        let (turn, bytes, items) = (
            self.turn,
            self.settings().context_bytes(),
            self.settings().context_items(),
        );
        let mut through = None;
        loop {
            let absorbed = self
                .store
                .op("absorb", move |db| {
                    db.absorb(turn, through, bytes, items, reserved, whole, attached)
                })
                .await?;
            through = absorbed.next_through;
            steered |= !absorbed.outcomes.is_empty();
            stayed |= absorbed.capped;
            // Attached messages this pass left out join the boundary after.
            if absorbed.attached_left {
                self.steers.store(true, Relaxed);
            }
            // Release the batch before loading another; new arrivals beyond
            // the initial snapshot wait for the next model-round boundary.
            if through.is_none() {
                break;
            }
        }
        *capped = stayed.then_some(Capped {
            ahead: reserved,
            whole,
        });
        Ok(steered)
    }

    /// The bot's fallback `reference` names, unsent and with no credit, when
    /// its provider is known and still speaks the bot's encoding.
    fn fallback(
        &self,
        record: &agent_runtime::store::Bot,
        reference: &str,
    ) -> Result<Option<Fallback>> {
        let (name, model) = split_model(reference)?;
        let Some(known) = self.providers.get(name) else {
            return Ok(None);
        };
        if known.family() != record.family()? {
            return Ok(None);
        }
        Ok(Some(Fallback {
            provider: Arc::new(shaped_other(known, self.settings())?.into_owned()),
            name: name.to_owned(),
            model: model.to_owned(),
            credit: None,
            pending: false,
        }))
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
        tool_calls: bool,
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
            tool_calls,
            record,
            model_rounds,
            turn,
            accounting,
        )
        .await
    }

    /// One model call with retries, pacing, and accounting: the turn's own
    /// request, or a compaction's request over a span of history. Without
    /// `tool_calls` the tools are shown but none may be called, which a
    /// span's request never allows.
    #[allow(clippy::too_many_arguments)]
    async fn call_with(
        &self,
        provider: &Provider,
        model: &str,
        instructions: &str,
        tools: &serde_json::value::RawValue,
        body: Body<'_>,
        tool_calls: bool,
        record: &mut agent_runtime::store::Bot,
        model_rounds: &mut usize,
        turn: i64,
        accounting: &mut Accounting,
    ) -> Result<Option<agent_runtime::provider::Completion>> {
        let summary = !matches!(body, Body::Window(_));
        accounting.compaction = summary;
        let started = std::time::Instant::now();
        let mut attempt = std::mem::take(&mut accounting.call_attempts);
        let prior_spent =
            std::time::Duration::from_millis(std::mem::take(&mut accounting.call_spent_ms));
        let paced_before = accounting.totals().1;
        let cache_key = cache_key(
            self.identity,
            record.cache_bot(),
            matches!(body, Body::Span(..)),
        );
        // A summary's view is replaced once it lands; only the turn's call
        // is refreshed while it streams.
        // A refresh renders the turn's request with its calls allowed, so
        // it would not read what a call without them sent.
        let mut warm_after = match body {
            Body::Window(_) if tool_calls => {
                provider.keep_warm_after(model, record.effort.as_deref())
            }
            Body::Window(_) => None,
            Body::Copied(_) | Body::Span(..) => None,
        };
        // Replayed thinking a 400 may have come from, withheld from this
        // call's later attempts.
        let mut withheld = None;
        // The bot's fallback model, once a classifier declined this request:
        // it answers this request alone, and the next goes to the bot's model.
        // A call parked on the fallback's pool resumes there.
        let mut fallback = match std::mem::take(&mut accounting.call_fallback) {
            Some(parked) => self.fallback(record, &parked.model)?.map(|to| Fallback {
                credit: parked.credit,
                ..to
            }),
            None => None,
        };
        let mut tool_calls = tool_calls;
        loop {
            // A stopping daemon sends no request, first or retry, and the
            // turn parks as at a boundary.
            if self.stopping.load(Relaxed) {
                return Err(Error::new(SHUTDOWN));
            }
            // A billed attempt that failed counts as a round: the retry that
            // is the turn's last allowed call goes without calls, as that
            // call would have.
            if tool_calls
                && matches!(body, Body::Window(_))
                && *model_rounds + 1 >= self.settings().max_rounds()
            {
                (tool_calls, warm_after) = (false, None);
                self.hub
                    .live(
                        &self.bot,
                        json!({"event":"tool_calls_withheld","bot":self.bot,"turn":turn,
                            "durable":false,"reason":"tool_round_limit"}),
                    )
                    .await?;
            }
            let items = match body {
                Body::Window(context) => self.items(context, withheld.unwrap_or(context.thinking)),
                Body::Copied(copy) => self.copied_items(copy),
                Body::Span(plan, stripped) => self.span_items(plan, stripped).await?,
            };
            accounting.begin(attempt > 0);
            // Deltas name their item past the last node the call sends, so
            // the numbers grow through the turn, and a call retried or
            // resumed streams its items again under the same ones.
            let after = match body {
                Body::Window(context) => (context.window.as_ref())
                    .and_then(|window| window.ids.last())
                    .map_or(0, |&node| node as u64),
                _ => 0,
            };
            (accounting.warm_read_at, accounting.warm_stopped) = (None, false);
            let sent = agent_runtime::provider::Sent::default();
            // The call's own provider and model, and where this attempt goes.
            let (own_provider, own_model) = (provider, model);
            let target = (fallback.as_ref()).map(|to| (to.provider.clone(), to.model.clone()));
            let (provider, model) = match &target {
                Some((to, model)) => (to.as_ref(), model.as_str()),
                None => (provider, model),
            };
            let call = provider.complete_accounted(
                    ModelRequest {
                        model,
                        instructions,
                        reasoning: record.effort.as_deref(),
                        tools,
                        allow_tool_calls: tool_calls && !matches!(body, Body::Span(..)),
                        // A summary offers it too, so its request reads
                        // the cache the bot's calls write.
                        web_search: searches(record),
                        fallback_credit: fallback.as_ref().and_then(|to| to.credit.as_deref()),
                        cache_key: Some(&cache_key),
                        items,
                        // A summary beside the turn's call keeps off the
                        // bot's connection, which that call holds.
                        chain: (!accounting.beside).then(|| self.chain(body)),
                        // A summary of its own goes off the turn's route; a
                        // copy follows it to the server that holds its cache.
                        route: (!matches!(body, Body::Span(..))).then_some(&accounting.route),
                        sent: Some(&sent),
                    },
                    |delta| {
                        let (kind, index, text) = match delta {
                            Delta::Text(index, text) => (if summary { "compaction_text_delta" } else { "text_delta" }, index, text),
                            Delta::Thinking(index, text) => (if summary { "compaction_thinking_delta" } else { "thinking_delta" }, index, text),
                        };
                        let item = after + index as u64 + 1;
                        self.hub.live(
                            &self.bot,
                            json!({"event":kind,"bot":self.bot,"turn":turn,"durable":false,"item":item,"text":text}),
                        )
                    },
                    &mut accounting.report,
                );
            // The fallback's answer is not the request a refresh renders.
            let result = match (warm_after.filter(|_| fallback.is_none()), body) {
                (Some(after), Body::Window(context)) => {
                    let mut warm = Warm {
                        provider,
                        model,
                        instructions,
                        reasoning: record.effort.as_deref(),
                        tools,
                        context,
                        thinking: withheld.unwrap_or(context.thinking),
                        web_search: searches(record),
                        after,
                        read_at: tokio::time::Instant::now(),
                        stopped: false,
                        tokens: 0,
                        room: record
                            .ceiling()
                            .map(|c| c.saturating_sub(record.tokens_used)),
                        pending: &mut accounting.refresh,
                    };
                    let result = self.stream_warm(call, &sent, &mut warm).await;
                    // The call's own read, unless a refresh read the cache since.
                    accounting.warm_read_at =
                        Some(sent.get().map_or(warm.read_at, |at| warm.read_at.max(at)));
                    accounting.warm_stopped = warm.stopped;
                    let refreshed = warm.tokens;
                    record.tokens_used = record.tokens_used.saturating_add(refreshed);
                    result?
                }
                _ => call.await,
            };
            let sent_ms = sent.get().map_or(0, epoch_ms);
            let error = match result {
                Ok(mut completion) => {
                    if let Some(to) = &fallback {
                        completion.usage = (completion.usage.take())
                            .map(|usage| priced_at(usage, &to.name, &to.model));
                        completion
                            .fallbacks
                            .push((Some(own_model.to_owned()), to.model.clone()));
                    }
                    if let Some(usage) = &mut completion.usage {
                        usage.sent_ms = sent_ms;
                        usage.context_window = self.window.get().copied().unwrap_or(0);
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
                    // The same body without the thinking was accepted, so
                    // later requests leave it out too.
                    if let (Body::Window(_), Some(strip), Some(prefix)) =
                        (body, withheld, record.thinking_prefix)
                    {
                        let bot = self.bot.clone();
                        let elided = record.thinking_elided;
                        self.store
                            .op("set_thinking", move |db| {
                                db.set_thinking(&bot, prefix, strip, elided)
                            })
                            .await?;
                        record.thinking = strip;
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
                    // An answer ends a run of holds.
                    accounting.held_since = None;
                    return Ok(Some(completion));
                }
                Err(mut error) => {
                    // A refusal's credit is for the retry below, not the record.
                    let credit = error
                        .facts
                        .as_mut()
                        .and_then(|facts| facts.remove("fallback_credit_token"));
                    if error.facts.as_ref().is_some_and(|facts| facts.is_empty()) {
                        error.facts = None;
                    }
                    let credit = credit.and_then(|token| token.as_str().map(str::to_owned));
                    // A classifier declined: the bot's fallback model answers
                    // this request, on the same body. The refusal's credit
                    // prices the retry as if it had run there all along,
                    // when it goes to the endpoint that declined it.
                    if fallback.is_none()
                        && error.code == "provider_refusal"
                        && matches!(body, Body::Window(_))
                        && let Some(reference) = &record.fallback
                    {
                        let reference = reference.clone();
                        fallback = self.fallback(record, &reference)?.map(|to| Fallback {
                            credit: credit.filter(|_| to.provider.url() == own_provider.url()),
                            pending: true,
                            ..to
                        });
                    }
                    error
                }
            };
            // What the fallback's own attempt billed is priced at its model.
            if let Some(to) = fallback.as_ref().filter(|to| !to.pending)
                && let Some(usage) = accounting.report.usage.take()
            {
                accounting.report.usage = Some(priced_at(usage, &to.name, &to.model));
            }
            let paced_ms = accounting.totals().1 - paced_before;
            let spent = prior_spent
                + started
                    .elapsed()
                    .saturating_sub(std::time::Duration::from_millis(paced_ms));
            if let Some(block) = accounting.report.park_for {
                // No HTTP call took place. Keep its retry state without
                // incrementing attempts or cumulative dispatched retries.
                accounting.call_attempts = attempt;
                accounting.call_fallback = fallback.as_ref().map(Fallback::parked);
                accounting.call_spent_ms = spent.as_millis() as u64;
                accounting.parked_until = now_ms() + block.as_millis() as u64;
                accounting.blocked = None;
                return Ok(None);
            }
            // Whatever the provider billed for a failed attempt is still spent.
            if let Some(usage) = &mut accounting.report.usage {
                usage.sent_ms = sent_ms;
                self.tokens.add(usage);
                *model_rounds += 1;
                record.tokens_used = record
                    .tokens_used
                    .saturating_add(usage.input_tokens)
                    .saturating_add(usage.output_tokens);
            }
            let usage = accounting.report.usage.take();
            if summary {
                if let Some(usage) = usage {
                    let reference = summarizer(record);
                    let usage = priced_at(usage, split_model(&reference)?.0, model);
                    self.store
                        .op("compaction_usage", move |db| {
                            db.compaction_usage(turn, Some(&usage))
                        })
                        .await?;
                }
            } else {
                self.failed_usage(usage).await?;
            }
            let mut resend = false;
            let mut retry = json!({"thinking_withheld":true});
            if let Some(to) = fallback.as_mut().filter(|to| to.pending) {
                // Sent next on the fallback model.
                to.pending = false;
                resend = true;
                retry = json!({"fallback":format!("{}/{}", to.name, to.model)});
            } else if let Some(to) = (fallback.as_mut())
                .filter(|to| to.credit.is_some() && error.code == "provider_http_400")
            {
                // A credit the provider would not take: the same retry
                // without it, before anything else the 400 could be.
                to.credit = None;
                resend = true;
                retry = json!({"fallback_credit":false});
            } else if let Body::Window(context) = body
                && withheld.is_none()
                && record.thinking_prefix.is_some()
                && let Some(strip) = withhold_thinking(context, &error)
            {
                withheld = Some(strip);
                resend = true;
            }
            // A retry is another billable call: none once failed usage has
            // spent the bot's or turn's budget, or the rounds are used up.
            let capped = record.exhausted().or_else(|| {
                (*model_rounds >= self.settings().max_rounds())
                    .then(|| Error::new("tool_round_limit"))
            });
            attempt += 1;
            let paced = agent_runtime::provider::paced(&error.code);
            let cap = if paced {
                MAX_PACED_ATTEMPTS
            } else {
                MAX_ATTEMPTS
            };
            // Sent again at once, without the thinking or on the fallback
            // model, within the same attempt and time limits as any retry.
            if resend && capped.is_none() && attempt < cap && spent < RETRY_BUDGET {
                let mut data = json!({"attempt":attempt,"error":error.code,"detail":error.detail,
                    "delay_ms":0,"parked":false});
                if let (Some(data), Value::Object(retry)) = (data.as_object_mut(), retry) {
                    data.extend(retry);
                }
                self.retried(data).await?;
                continue;
            }
            // Final provider errors are kept.
            let error = match capped {
                Some(capped) if retryable(&error.code) || resend => capped,
                _ => error,
            };
            // Time spent waiting fairly in the pool is the fleet's, not this
            // call's; the budget counts only the attempts and their backoff.
            if !retryable(&error.code) || attempt >= cap || spent >= RETRY_BUDGET {
                // A cause a person can fix, or an outage that outlasted its
                // retries, holds the turn rather than ending it. A summary
                // fails as before: the turn's own call goes on, and holds
                // there if the cause outlasts it.
                if !summary
                    && record.settings.holds(accounting.held_since, now_ms())
                    && (held(&error.code) || retryable(&error.code))
                {
                    accounting.call_fallback = fallback.as_ref().map(Fallback::parked);
                    accounting.hold(&error);
                    return Ok(None);
                }
                return Err(error);
            }
            // A pool closed by a rate limit for a while is not this turn's to
            // wait out with a live task and an active slot: park durably
            // and let the service retry when the block lifts. The retry is
            // announced now and counted only when dispatched. Its call-local
            // budget survives the park independently of cumulative totals.
            let pool = paced
                .then(|| provider.blocked_for(model))
                .flatten()
                .filter(|block| *block >= MIN_PARK);
            // A rate limit holds the pool, as do clustered overloads; a lone
            // overload backs off from half a second as the providers' SDKs
            // do, and other transient failures from a quarter, with a little
            // spread.
            let delay = match pool {
                Some(block) => block,
                None if paced && agent_runtime::provider::overload(&error.code) => {
                    backoff(attempt + 1, turn as u64)
                }
                None if paced => std::time::Duration::ZERO,
                None if matches!(
                    error.code.as_str(),
                    "provider_login_refreshed" | "provider_signature_expired"
                ) =>
                {
                    std::time::Duration::ZERO
                }
                None if error.code == "provider_stream_stalled" => {
                    backoff(attempt + STALL_START, turn as u64)
                }
                None => asked(&error).unwrap_or_else(|| backoff(attempt, turn as u64)),
            };
            // A backoff of a second or more parks too: an outage's retries
            // are not this turn's to sleep through with a live task and an
            // active slot. The backoff still counts toward the call's budget.
            let park = pool.or((delay >= BACKOFF_PARK).then_some(delay));
            self.retried(
                json!({"attempt":attempt,"error":error.code,"detail":error.detail,
                "delay_ms":delay.as_millis() as u64,"parked":park.is_some()}),
            )
            .await?;
            if let Some(block) = park {
                let backoff = if pool.is_some() {
                    std::time::Duration::ZERO
                } else {
                    block
                };
                accounting.call_attempts = attempt;
                accounting.call_spent_ms = (spent + backoff).as_millis() as u64;
                accounting.call_fallback = fallback.as_ref().map(Fallback::parked);
                accounting.parked_until = now_ms() + block.as_millis() as u64;
                accounting.blocked = None;
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
                thinking,
            }) => Chain {
                bot: &self.bot,
                window: Some((&prefix.bytes[..], window.elided, &window.ids[..])),
                tail: Box::new(move |skip| {
                    let span = Span {
                        ids: &window.ids[skip..],
                        sizes: &window.sizes[skip..],
                        thinking: &window.thinking[skip..],
                        pictured: &window.pictured[skip..],
                        elided: window.elided,
                    };
                    self.window_items(Bytes::new(), span, *thinking)
                }),
            },
            _ => Chain {
                bot: &self.bot,
                window: None,
                tail: Box::new(|_| Items::empty()),
            },
        }
    }

    /// Record a retry as the turn's event, so it outlives the live stream.
    async fn retried(&self, data: Value) -> Result<()> {
        let turn = self.turn;
        self.store
            .op("retried", move |db| db.retried(turn, data))
            .await
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

    /// Await a model call, refreshing its own prompt cache each time the
    /// cache has sat unread for `after`: a reply that streams for longer
    /// than the cache lives would otherwise let the prefix it read expire
    /// before the next call. The cache is read from when `sent` records the
    /// call was sent, which pacing or admission can delay. A refresh still
    /// in flight when the reply ends is left in `warm.pending`, for the tool
    /// run or the next round to settle.
    async fn stream_warm<T>(
        &self,
        call: impl std::future::Future<Output = T>,
        sent: &agent_runtime::provider::Sent,
        warm: &mut Warm<'_>,
    ) -> Result<T> {
        tokio::pin!(call);
        loop {
            if warm.stopped {
                return Ok(call.await);
            }
            if warm.pending.is_none() {
                // Nothing is cached before the send, and one still to come
                // leaves the cache unread for `after` no sooner than from now.
                let due = match sent.get() {
                    Some(at) => warm.read_at.max(at),
                    None => tokio::time::Instant::now(),
                } + warm.after;
                tokio::select! {
                    biased;
                    result = &mut call => return Ok(result),
                    () = tokio::time::sleep_until(due) => {}
                }
                let Some(at) = sent.get() else { continue };
                warm.read_at = warm.read_at.max(at);
                if warm.read_at + warm.after > tokio::time::Instant::now() {
                    continue;
                }
                // A summary beside the call has written the view that
                // replaces this one: its cache is not read again.
                if self.side.replaced.load(Relaxed) {
                    return Ok(call.await);
                }
                // Nor once the daemon is stopping: no call after this one
                // would read it, and the drain sends no new request.
                if self.stopping.load(Relaxed) {
                    return Ok(call.await);
                }
                *warm.pending = Some(self.refresh(warm));
            }
            let refreshed = {
                let task = &mut warm.pending.as_mut().expect("a refresh in flight").task;
                tokio::select! {
                    biased;
                    result = &mut call => return Ok(result),
                    joined = task => joined.unwrap_or_else(|_| fail("keep_warm_lost")),
                }
            };
            *warm.pending = None;
            self.refreshed(warm, refreshed).await?;
        }
    }

    /// Run a prepared tool, refreshing the last model call's prompt cache
    /// each time it has sat unread for `after`. The outer error is the
    /// runtime's; the inner one is the tool's result.
    /// Run a tool while keeping the cache warm. With `lift`, a steer moves
    /// the run to the background instead, handed back unfinished, once the
    /// paired signal says it is running: a command still waiting for a
    /// process slot stays in line in the foreground.
    async fn run_tool<T, F: Future<Output = Result<T>>>(
        &self,
        run: F,
        lift: Option<(&Lift, oneshot::Receiver<()>)>,
        warm: Option<&mut Warm<'_>>,
    ) -> Result<Ran<F, T>> {
        enum Next<T> {
            Ran(Result<T>),
            Lifted,
            Refreshed(Result<(agent_runtime::provider::Usage, tokio::time::Instant)>),
        }
        let mut run = Box::pin(run);
        let lifted = async {
            // A run that ends before it starts wins the race below.
            if let Some((lift, running)) = lift
                && running.await.is_ok()
            {
                lift.lifted().await
            } else {
                std::future::pending().await
            }
        };
        tokio::pin!(lifted);
        let Some(warm) = warm.filter(|warm| !warm.stopped) else {
            return Ok(tokio::select! {
                biased;
                result = &mut run => Ran::Done(result),
                () = &mut lifted => Ran::Lifted(run),
            });
        };
        loop {
            if warm.pending.is_none() {
                tokio::select! {
                    biased;
                    result = &mut run => return Ok(Ran::Done(result)),
                    () = &mut lifted => return Ok(Ran::Lifted(run)),
                    () = tokio::time::sleep_until(warm.read_at + warm.after) => {}
                }
                // A stopping daemon sends no new refresh: the next call that
                // would read the cache waits for the next start.
                if self.stopping.load(Relaxed) {
                    return Ok(tokio::select! {
                        biased;
                        result = &mut run => Ran::Done(result),
                        () = &mut lifted => Ran::Lifted(run),
                    });
                }
                *warm.pending = Some(self.refresh(warm));
            }
            let next = {
                let task = &mut warm.pending.as_mut().expect("a refresh in flight").task;
                tokio::select! {
                    biased;
                    result = &mut run => Next::Ran(result),
                    () = &mut lifted => Next::Lifted,
                    joined = task => Next::Refreshed(joined.unwrap_or_else(|_| fail("keep_warm_lost"))),
                }
            };
            match next {
                // The tool's result ends the refreshes, as does handing it to
                // the background. One already sent is billed, so it is
                // answered and recorded; an unsent one is dropped at no cost.
                Next::Ran(result) => {
                    if let Some(refreshed) = settle(warm.pending).await {
                        self.refreshed(warm, refreshed).await?;
                    }
                    return Ok(Ran::Done(result));
                }
                // The command keeps running while the refresh is answered,
                // so its timeout and pipes still apply; one that ends
                // meanwhile returns as usual.
                Next::Lifted => {
                    let mut finished = None;
                    let refreshed = {
                        let settled = settle(warm.pending);
                        tokio::pin!(settled);
                        tokio::select! {
                            biased;
                            refreshed = &mut settled => refreshed,
                            result = &mut run => {
                                finished = Some(result);
                                settled.await
                            }
                        }
                    };
                    if let Some(refreshed) = refreshed {
                        self.refreshed(warm, refreshed).await?;
                    }
                    return Ok(match finished {
                        Some(result) => Ran::Done(result),
                        None => Ran::Lifted(run),
                    });
                }
                Next::Refreshed(refreshed) => {
                    *warm.pending = None;
                    self.refreshed(warm, refreshed).await?;
                }
            }
            if warm.stopped {
                return Ok(tokio::select! {
                    biased;
                    result = &mut run => Ran::Done(result),
                    () = &mut lifted => Ran::Lifted(run),
                });
            }
        }
    }

    /// Send the last model call's request again, only to refresh its cache,
    /// as a task of its own: an interrupt that drops the turn's rounds must
    /// not drop a refresh already sent, which is billed.
    fn refresh(&self, warm: &Warm<'_>) -> Pending {
        let refresh = Arc::new(agent_runtime::provider::Refresh::default());
        let provider = warm.provider.clone();
        let (model, instructions) = (warm.model.to_owned(), warm.instructions.to_owned());
        let reasoning = warm.reasoning.map(str::to_owned);
        let tools = warm.tools.to_owned();
        let web_search = warm.web_search;
        let items = self.items(warm.context, warm.thinking);
        let sent = refresh.clone();
        let expires = warm.read_at + agent_runtime::provider::CACHE_LIFETIME;
        let task = tokio::spawn(async move {
            let request = ModelRequest {
                model: &model,
                instructions: &instructions,
                reasoning: reasoning.as_deref(),
                tools: &tools,
                allow_tool_calls: true,
                web_search,
                fallback_credit: None,
                cache_key: None,
                items,
                chain: None,
                route: None,
                sent: None,
            };
            provider.keep_warm(request, &sent, expires).await
        });
        Pending { refresh, task }
    }

    /// Record a refresh against `warm`. A refused one ends the refreshes
    /// until the next model call, which rebuilds the cache as it would have
    /// without them.
    async fn refreshed(
        &self,
        warm: &mut Warm<'_>,
        refreshed: Result<(agent_runtime::provider::Usage, tokio::time::Instant)>,
    ) -> Result<()> {
        match self.account_refresh(refreshed).await? {
            Some((tokens, sent_at)) => {
                warm.tokens = warm.tokens.saturating_add(tokens);
                // The refresh's own read, from when it was sent. One carried
                // over from an earlier attempt can be older than this one's.
                warm.read_at = warm.read_at.max(sent_at);
                // A cap reached ends them: no call after it would read the cache.
                warm.stopped |= warm.room.is_some_and(|room| warm.tokens >= room);
            }
            None => warm.stopped = true,
        }
        Ok(())
    }

    /// Record a refresh's answer: billed like any call but not a model
    /// round. Returns the tokens it billed and when it was sent, or `None`
    /// when it was refused.
    async fn account_refresh(
        &self,
        refreshed: Result<(agent_runtime::provider::Usage, tokio::time::Instant)>,
    ) -> Result<Option<(u64, tokio::time::Instant)>> {
        let (usage, sent_at) = match refreshed {
            Ok(refreshed) => refreshed,
            Err(error) => {
                self.hub
                    .live(
                        &self.bot,
                        json!({"event":"keep_warm_failed","bot":self.bot,"turn":self.turn,
                            "durable":false,"error":error.code,"detail":error.detail}),
                    )
                    .await?;
                return Ok(None);
            }
        };
        let tokens = usage.input_tokens.saturating_add(usage.output_tokens);
        self.record_refresh(usage, sent_at).await?;
        Ok(Some((tokens, sent_at)))
    }

    async fn record_refresh(
        &self,
        mut usage: agent_runtime::provider::Usage,
        sent_at: tokio::time::Instant,
    ) -> Result<()> {
        usage.sent_ms = epoch_ms(sent_at);
        self.tokens.add(&usage);
        let turn = self.turn;
        self.store
            .op("keep_warm_usage", move |db| {
                db.keep_warm_usage(turn, &usage)
            })
            .await
    }

    /// Run planned calls in order. Returns true when a wait parked the turn;
    /// the calls after it are stored with the parked state.
    /// Run a round's calls in order. `announced` is set when this round
    /// announced gated calls, with the time its commit began: the first of
    /// them waits for a verdict before its first check, since none can be
    /// older than the announcement.
    #[allow(clippy::too_many_arguments)]
    async fn execute_calls(
        &self,
        calls: Vec<ToolCall>,
        (provider, model): (&Provider, &str),
        mut announced: Option<(Arc<Notify>, tokio::time::Instant)>,
        workspace: &std::path::Path,
        environment: &[(String, String)],
        record: &Bot,
        mut warm: Option<&mut Warm<'_>>,
        route: Option<&str>,
        summary_retry_ms: Option<u64>,
    ) -> Result<Option<Stop>> {
        let (turn, allowed) = (self.turn, record.callable());
        let mut calls = calls.into_iter();
        while let Some(call) = calls.next() {
            // Nor any tool: the calls not yet started are answered as not
            // run when the turn parks.
            if self.stopping.load(Relaxed) {
                return Ok(Some(Stop::Drained));
            }
            // Calls in a row that only read or run commands, none of them
            // gated, run together, as Codex runs them.
            let together = match self.parallel(record, &call, workspace) {
                Some(first)
                    if (calls.as_slice().first())
                        .is_some_and(|next| self.parallel(record, next, workspace).is_some()) =>
                {
                    let mut batch = vec![(call.clone(), first)];
                    for next in calls.as_slice() {
                        let Some(prepared) = self.parallel(record, next, workspace) else {
                            break;
                        };
                        batch.push((next.clone(), prepared));
                    }
                    Some(batch)
                }
                _ => None,
            };
            // A command the machine's load holds back waits for it parked,
            // before it, the calls it starts with, or those after start: a
            // batch is let in or held whole.
            let starting: Vec<&ToolCall> = match &together {
                Some(batch) => batch.iter().map(|(call, _)| call).collect(),
                None => vec![&call],
            };
            if let Some(held) =
                (starting.iter()).find(|call| self.load_holds(record, call, workspace))
                && let Some(stop) = self
                    .await_load(&held.call_id, &call, &calls, route, summary_retry_ms)
                    .await?
            {
                return Ok(Some(stop));
            }
            if let Some(batch) = together {
                // The batch's first is `call`; the rest leave the round now.
                for _ in 1..batch.len() {
                    calls.next();
                }
                self.execute_parallel(batch, record, workspace, environment, warm.as_deref_mut())
                    .await?;
                continue;
            }
            // An escape from a sandbox runs unconfined only once a gate
            // allowed it.
            let approved = record.gated(&call, workspace, &self.ruled());
            if approved {
                match self
                    .approve(
                        &call,
                        &mut calls,
                        announced.take(),
                        record,
                        workspace,
                        route,
                        summary_retry_ms,
                    )
                    .await?
                {
                    Approval::Run => {}
                    Approval::Denied => continue,
                    Approval::Stop(stop) => return Ok(Some(stop)),
                }
            } else {
                let started = call.clone();
                self.store
                    .op("tool_start", move |db| db.tool_start(turn, &started))
                    .await?;
            }
            // A tool failure is a result the model can act on. Only the
            // scheduler closing is a runtime failure. The bot's selection is
            // enforced here, not only by what the model was shown.
            let prepared = if let Some(tool) = record.declared(&call.name) {
                agent_runtime::tools::check_declared(tool, &call.arguments)
                    .map(|()| Prepared::Declared(tool.result.clone()))
            } else if allowed.iter().any(|name| name == &call.name) {
                self.registry.prepare(&call.name, &call.arguments)
            } else {
                Err(Error::with("tool_not_available", call.name.as_str()))
            };
            // The command rules come next, as in Codex: a forbidden command
            // is refused, and one a rule asks about with no gate to ask is
            // refused too, as Codex refuses it under a policy that never
            // asks.
            let ruling = record.ruling(&call, &self.ruled());
            let prepared = prepared.and_then(|prepared| match &ruling {
                Ok(Ruling::Forbidden(why)) => Err(Error::with("command_forbidden", why.as_str())),
                Ok(Ruling::Prompt(why)) if !approved => {
                    Err(Error::with("command_needs_approval", why.as_str()))
                }
                Err(error) => Err(error.clone()),
                Ok(_) => Ok(prepared),
            });
            // A call its sandbox confines runs in it; one that can't be
            // confined fails rather than run unconfined. An escape no gate
            // approved is refused, as Codex refuses one under a policy that
            // never asks: a command, or a write or edit held to the sandbox.
            // A command a rule allows runs unconfined, as in Codex; one a
            // rule asked about runs where it asked to, once allowed.
            let mut sandbox = None;
            let confined = record.sandboxed()
                && agent_runtime::sandbox::confinable(&call.name)
                && !(approved && !record.confines(&call, workspace))
                && ruling != Ok(Ruling::Allow);
            let prepared = match prepared {
                Ok(Prepared::Shell { .. } | Prepared::Detach(_) | Prepared::Terminal { .. })
                    if confined && !record.confines(&call, workspace) =>
                {
                    Err(Error::with(
                        "escalation_refused",
                        "no approval lets this bot run a command outside its sandbox; \
                         run it without sandbox_permissions",
                    ))
                }
                Ok(prepared) if confined => self.sandbox(record, workspace).await.map(|policy| {
                    sandbox = policy;
                    prepared
                }),
                other => other,
            };
            let answered = record
                .declared(&call.name)
                .is_some_and(|tool| tool.answered_by.is_some());
            let outcome = match prepared {
                // A declared tool runs nothing: its meaning is its client's,
                // and so is its answer, when its client gives one.
                Ok(Prepared::Declared(_)) if answered => match self
                    .await_answer(&call.call_id, &mut calls, route, summary_retry_ms)
                    .await?
                {
                    ControlFlow::Continue(outcome) => outcome,
                    ControlFlow::Break(stop) => return Ok(Some(stop)),
                },
                Ok(Prepared::Declared(result)) => Outcome::text(result),
                // Codex's `web.run`, answered by the provider that serves
                // the bot; it keeps the bot's searches under its cache key.
                Ok(Prepared::WebSearch(commands)) => {
                    let session = cache_key(self.identity, record.cache_bot(), false);
                    let search = async {
                        provider
                            .run_search(model, &session, &commands)
                            .await
                            .map(Outcome::text)
                    };
                    match self.run_tool(search, None, warm.as_deref_mut()).await? {
                        Ran::Done(Ok(outcome)) => {
                            self.kept(outcome, record, workspace, &call.call_id).await
                        }
                        Ran::Done(Err(error)) => failure(error),
                        Ran::Lifted(_) => unreachable!("only a command moves to the background"),
                    }
                }
                Ok(Prepared::Wait {
                    handles,
                    timeout_ms,
                    any,
                }) => {
                    match self
                        .park(
                            &call.call_id,
                            handles,
                            timeout_ms,
                            any,
                            &mut calls,
                            route,
                            summary_retry_ms,
                        )
                        .await?
                    {
                        ControlFlow::Continue(outcome) => outcome,
                        ControlFlow::Break(stop) => return Ok(Some(stop)),
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
                            sandbox,
                            self.outputs(record, workspace),
                        )
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(error) if error.code == "capacity_exhausted" => failure(error),
                        Err(error) => return Err(error),
                    }
                }
                Ok(Prepared::Terminal { command, yield_ms }) => {
                    let (turn, started) = (self.turn, call.call_id.clone());
                    let id = self
                        .store
                        .op("process_start", move |db| db.process_start(turn, &started))
                        .await?;
                    let (sender, receiver) = oneshot::channel();
                    self.collect(
                        id,
                        &call.call_id,
                        receiver,
                        self.outputs(record, workspace),
                        Some(self.registry.clone()),
                    );
                    let start = self.registry.terminal(
                        record.bot_id,
                        id,
                        command,
                        workspace.to_path_buf(),
                        yield_ms,
                        environment.to_vec(),
                        sandbox,
                        sender,
                    );
                    match self.run_tool(start, None, warm.as_deref_mut()).await? {
                        Ran::Done(Ok(outcome)) => outcome,
                        Ran::Done(Err(error)) => failure(error),
                        Ran::Lifted(_) => unreachable!("only a command moves to the background"),
                    }
                }
                // A poll past the bot's hold parks the turn as `wait` does,
                // holding no slot until the terminal ends or the poll's
                // time runs out; a steer waiting for the boundary keeps it
                // live, as it makes a wait a poll.
                Ok(Prepared::WriteStdin {
                    handle,
                    ref chars,
                    yield_ms,
                    kill: false,
                }) if chars.is_empty()
                    && !self.lift.is_armed()
                    && yield_ms.unwrap_or(POLL_YIELD_MS) > self.settings().approval_hold_ms() =>
                {
                    let yield_ms = yield_ms.unwrap_or(POLL_YIELD_MS);
                    let deadline_ms = now_ms() + yield_ms;
                    let hold = self.registry.poll_hold(
                        record.bot_id,
                        handle,
                        self.settings().approval_hold_ms(),
                        sandbox,
                    );
                    match self.run_tool(hold, None, warm.as_deref_mut()).await? {
                        Ran::Done(Ok(Some(outcome))) => outcome,
                        Ran::Done(Ok(None)) => match self
                            .park_poll(
                                &call.call_id,
                                handle,
                                deadline_ms,
                                &mut calls,
                                route,
                                summary_retry_ms,
                            )
                            .await?
                        {
                            ControlFlow::Continue(outcome) => outcome,
                            ControlFlow::Break(stop) => return Ok(Some(stop)),
                        },
                        Ran::Done(Err(error)) => failure(error),
                        Ran::Lifted(_) => unreachable!("only a command moves to the background"),
                    }
                }
                Ok(Prepared::WriteStdin {
                    handle,
                    chars,
                    yield_ms,
                    kill,
                }) => {
                    let typed = self.registry.write_stdin(
                        record.bot_id,
                        handle,
                        &chars,
                        yield_ms,
                        kill,
                        sandbox,
                    );
                    match self.run_tool(typed, None, warm.as_deref_mut()).await? {
                        Ran::Done(Ok(outcome)) => outcome,
                        Ran::Done(Err(error)) => failure(error),
                        Ran::Lifted(_) => unreachable!("only a command moves to the background"),
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
                Ok(Prepared::Read {
                    source: ReadSource::Result { node },
                    offset,
                    limit,
                }) => match self.result_page(&call.call_id, node, offset, limit).await {
                    Ok(outcome) => outcome,
                    Err(error) => failure(error),
                },
                Ok(Prepared::History {
                    bot,
                    turn: wanted,
                    handle,
                    offset,
                    limit,
                }) => {
                    match Box::pin(self.history(&call.call_id, bot, wanted, handle, offset, limit))
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(error) => failure(error),
                    }
                }
                Ok(Prepared::HistorySearch { bot, query, before }) => {
                    match self.history_search(&call.call_id, bot, query, before).await {
                        Ok(outcome) => outcome,
                        Err(error) => failure(error),
                    }
                }
                // Stored with the result: one commit records the note, its
                // version node, and the tool outcome together.
                Ok(Prepared::Note { text }) => {
                    let outcome = Outcome {
                        output: json!({"bytes":text.len(),"cleared":text.is_empty()}).to_string(),
                        artifacts: Vec::new(),
                        files: Files::default(),
                        parts: Vec::new(),
                        note: Some(text),
                        failed: false,
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
                Ok(Prepared::Shell {
                    command,
                    timeout_ms,
                    background: false,
                }) => {
                    let (started, running) = oneshot::channel();
                    let run = self.registry.foreground(
                        command,
                        workspace.to_path_buf(),
                        timeout_ms,
                        environment.to_vec(),
                        sandbox,
                        started,
                    );
                    match self
                        .run_tool(run, Some((&self.lift, running)), warm.as_deref_mut())
                        .await?
                    {
                        Ran::Done(Ok(outcome)) => {
                            self.kept(outcome, record, workspace, &call.call_id).await
                        }
                        Ran::Done(Err(error)) if error.code == "tool_scheduler_closed" => {
                            return Err(error);
                        }
                        Ran::Done(Err(error)) => failure(error),
                        Ran::Lifted(run) => {
                            let outputs = self.outputs(record, workspace);
                            self.lift_shell(&call.call_id, run, outputs).await?
                        }
                    }
                }
                Ok(prepared) => match self
                    .run_tool(
                        self.registry
                            .execute(prepared, workspace, environment, sandbox),
                        None,
                        warm.as_deref_mut(),
                    )
                    .await?
                {
                    Ran::Done(Ok(outcome)) => {
                        self.kept(outcome, record, workspace, &call.call_id).await
                    }
                    Ran::Done(Err(error)) if error.code == "tool_scheduler_closed" => {
                        return Err(error);
                    }
                    Ran::Done(Err(error)) => failure(error),
                    Ran::Lifted(_) => unreachable!("only a command moves to the background"),
                },
                Err(error) => failure(error),
            };
            let id = call.call_id;
            self.store
                .op("tool_finish", move |db| db.tool_finish(turn, &id, &outcome))
                .await?;
        }
        Ok(None)
    }

    /// The call prepared, when it may run beside others: an ungated
    /// command in the foreground or a file read, which the bot may call. Codex
    /// runs these together too; a change to a file runs alone, in order.
    /// A sandboxed bot's command runs beside others only inside its
    /// sandbox; one that asks to leave it goes the lone way, to be refused.
    fn parallel(
        &self,
        record: &Bot,
        call: &ToolCall,
        workspace: &std::path::Path,
    ) -> Option<Prepared> {
        let ruled = self.ruled();
        if !matches!(call.name.as_str(), "shell" | "read")
            || record.gated(call, workspace, &ruled)
            || !matches!(
                record.ruling(call, &ruled),
                Ok(Ruling::None | Ruling::Allow)
            )
            || record.declared(&call.name).is_some()
            || !record.callable().iter().any(|name| name == &call.name)
            || (call.name == "shell" && record.sandboxed() && !record.confines(call, workspace))
        {
            return None;
        }
        match self.registry.prepare(&call.name, &call.arguments) {
            // A stored stream or result is paged to fit the context left,
            // which a read beside it would spend too: it runs alone.
            Ok(
                prepared @ (Prepared::Shell {
                    background: false, ..
                }
                | Prepared::Read {
                    source: ReadSource::Path(_),
                    ..
                }),
            ) => Some(prepared),
            _ => None,
        }
    }

    /// Run a batch of calls [`Self::parallel`] admits together, up to
    /// [`PARALLEL_CALLS`] at a time and no more than half the process
    /// budget: all started in one commit, each result recorded in call
    /// order as soon as it and those before it are done. A steer moves every command still running to the
    /// background, as it does a lone one.
    async fn execute_parallel(
        &self,
        batch: Vec<(ToolCall, Prepared)>,
        record: &Bot,
        workspace: &std::path::Path,
        environment: &[(String, String)],
        warm: Option<&mut Warm<'_>>,
    ) -> Result<()> {
        let turn = self.turn;
        let starting: Vec<ToolCall> = batch.iter().map(|(call, _)| call.clone()).collect();
        self.store
            .op("tool_start", move |db| {
                starting
                    .iter()
                    .try_for_each(|call| db.tool_start(turn, call).map(drop))
            })
            .await?;
        // A sandboxed bot's commands all run in its one sandbox; when it
        // can't be had, each command fails with why. A command a rule
        // allows runs unconfined.
        let ruled = self.ruled();
        let allowed = |call: &ToolCall| record.ruling(call, &ruled) == Ok(Ruling::Allow);
        let shells = batch
            .iter()
            .any(|(call, prepared)| matches!(prepared, Prepared::Shell { .. }) && !allowed(call));
        let sandbox = match shells && record.sandboxed() {
            true => self.sandbox(record, workspace).await,
            false => Ok(None),
        };
        let runs = batch.into_iter().map(|(call, prepared)| {
            let sandbox = match (&prepared, &sandbox) {
                _ if allowed(&call) => Ok(None),
                (Prepared::Shell { .. }, Err(error)) => Err(error.clone()),
                (_, sandbox) => Ok(sandbox.clone().unwrap_or_default()),
            };
            self.run_parallel(
                call.call_id,
                prepared,
                record,
                workspace,
                environment,
                sandbox,
            )
        });
        let record = async {
            // Results go to the store in call order as they come, each
            // queued without waiting on the one before, so results done
            // together share a commit. Waiting for room on the store's
            // queue happens beside the calls still running, which go on
            // being polled; meanwhile the channel holds at most one result
            // per call in the batch.
            let (done, mut recorded) = tokio::sync::mpsc::unbounded_channel();
            let run = async move {
                let mut results = stream::iter(runs).buffered(self.registry.wide(PARALLEL_CALLS));
                while let Some((id, outcome)) = results.next().await {
                    let _ = done.send((id, outcome?));
                }
                Ok::<_, Error>(())
            };
            // Each write's acknowledgement carries its result: dropped as
            // it comes, so committed results aren't held for the batch.
            let write = async {
                let mut written = stream::FuturesOrdered::new();
                loop {
                    tokio::select! {
                        Some(answer) = written.next(), if !written.is_empty() => drop(answer?),
                        next = recorded.recv() => match next {
                            Some((id, outcome)) => written.push_back(
                                self.store.slot().await?.op("tool_finish", move |db| {
                                    db.tool_finish(turn, &id, &outcome)
                                }),
                            ),
                            None => break,
                        },
                    }
                }
                while let Some(answer) = written.next().await {
                    answer?;
                }
                Ok::<_, Error>(())
            };
            let (ran, wrote) = tokio::join!(run, write);
            ran.and(wrote)
        };
        match self.run_tool(record, None, warm).await? {
            Ran::Done(result) => result,
            Ran::Lifted(_) => unreachable!("a batch is not lifted whole"),
        }
    }

    /// One call of a batch, run to its outcome: an error only when the
    /// scheduler closed.
    async fn run_parallel(
        &self,
        call_id: String,
        prepared: Prepared,
        record: &Bot,
        workspace: &std::path::Path,
        environment: &[(String, String)],
        sandbox: Result<Option<Arc<Policy>>>,
    ) -> (String, Result<Outcome>) {
        // A batch wider than its width starts its later calls as earlier
        // ones finish; once the daemon is stopping, those do not run.
        if self.stopping.load(Relaxed) {
            let stopped = Error::with(
                SHUTDOWN,
                "the daemon stopped before this call ran; it did not run",
            );
            return (call_id, Ok(failure(stopped)));
        }
        let sandbox = match sandbox {
            Ok(sandbox) => sandbox,
            Err(error) => return (call_id, Ok(failure(error))),
        };
        let ran = match prepared {
            Prepared::Shell {
                command,
                timeout_ms,
                background: false,
            } => {
                let (started, running) = oneshot::channel();
                let mut run = Box::pin(self.registry.foreground(
                    command,
                    workspace.to_path_buf(),
                    timeout_ms,
                    environment.to_vec(),
                    sandbox,
                    started,
                ));
                let lifted = async {
                    // A run that ends before it starts wins the race below.
                    match running.await {
                        Ok(()) => self.lift.lifted().await,
                        Err(_) => std::future::pending().await,
                    }
                };
                let finished = tokio::select! {
                    biased;
                    result = &mut run => Some(result),
                    () = lifted => None,
                };
                match finished {
                    Some(result) => result,
                    None => {
                        let outputs = self.outputs(record, workspace);
                        return (
                            call_id.clone(),
                            self.lift_shell(&call_id, run, outputs).await,
                        );
                    }
                }
            }
            prepared => {
                self.registry
                    .execute(prepared, workspace, environment, sandbox)
                    .await
            }
        };
        let outcome = match ran {
            Ok(outcome) => Ok(self.kept(outcome, record, workspace, &call_id).await),
            Err(error) if error.code == "tool_scheduler_closed" => Err(error),
            Err(error) => Ok(failure(error)),
        };
        (call_id, outcome)
    }

    /// Where the bot's cut outputs go whole, when its settings name a
    /// folder.
    fn outputs(&self, record: &Bot, workspace: &std::path::Path) -> Option<Outputs> {
        let settings = self.settings();
        Some(Outputs {
            folder: PathBuf::from(settings.outputs.as_ref()?),
            settings: settings.clone(),
            root: record
                .workspace
                .as_ref()
                .map_or_else(|| workspace.to_owned(), PathBuf::from),
            daemon: self.registry.daemon().clone(),
        })
    }

    /// A call's outcome as the model and the store take it: its cut
    /// streams in the bot's outputs folder where it has one, then named.
    async fn kept(
        &self,
        outcome: Outcome,
        record: &Bot,
        workspace: &std::path::Path,
        call_id: &str,
    ) -> Outcome {
        let outcome = match outcome.artifacts.is_empty() {
            true => outcome,
            false => match self.outputs(record, workspace) {
                Some(outputs) => outputs.keep(outcome, self.turn, call_id).await,
                None => outcome,
            },
        };
        annotate(outcome, self.turn, call_id)
    }

    /// The bot's sandbox, built from its settings. Reading its roots
    /// touches the filesystem, so it runs off the runtime thread; it fails
    /// rather than leave a call that must be confined unconfined.
    async fn sandbox(
        &self,
        record: &Bot,
        workspace: &std::path::Path,
    ) -> Result<Option<Arc<Policy>>> {
        let (settings, daemon, bot) = (
            record.settings.clone(),
            self.registry.daemon().clone(),
            record.bot_id,
        );
        let root = record
            .workspace
            .as_ref()
            .map_or_else(|| workspace.to_owned(), PathBuf::from);
        tokio::task::spawn_blocking(move || {
            // The probe for a sandbox runs once, here off the runtime
            // thread, also for a bot from before a restart.
            agent_runtime::sandbox::available()?;
            // The bot's own socket, so its commands may use the daemon as
            // far as a sandboxed bot may; opened first, so its folder is
            // there to be kept read-only.
            let (capability, listening) = daemon.capability(bot)?.unzip();
            let policy = Policy::new(
                settings.sandbox,
                Some(&root),
                &settings.writable,
                settings.network,
                &daemon,
            )?;
            Ok(policy.map(|policy| Policy {
                capability,
                listening,
                ..policy.with_sockets(&settings.unix_sockets)
            }))
        })
        .await
        .map_err(|_| Error::new("sandbox_unavailable"))
        .flatten()
        .map(|policy| policy.map(Arc::new))
    }

    /// Wait for a gated call's verdict: live for the hold, then parked like
    /// `wait`, holding no task and no slot. Answers wake the task through
    /// the worker, so each check is one job and no commit.
    #[allow(clippy::too_many_arguments)]
    async fn approve(
        &self,
        call: &ToolCall,
        calls: &mut std::vec::IntoIter<ToolCall>,
        announced: Option<(Arc<Notify>, tokio::time::Instant)>,
        record: &Bot,
        workspace: &std::path::Path,
        route: Option<&str>,
        summary_retry_ms: Option<u64>,
    ) -> Result<Approval> {
        let turn = self.turn;
        let hold =
            tokio::time::Instant::now() + Duration::from_millis(self.settings().approval_hold_ms());
        // Announced by this round's own commit, so any verdict comes later
        // and wakes this wait: check after waiting, not before. The gates'
        // expiry counts from that commit, however long the calls before
        // this one ran; the check judges the stored time. A later call's
        // gate lapsing first ends the turn, so it wakes this wait too.
        let mut first = announced.map(|(notify, at)| {
            let round = std::iter::once(call).chain(calls.as_slice());
            let expire_ms = record
                .gates
                .iter()
                .filter(|gate| {
                    round.clone().any(|c| {
                        record.gated(c, workspace, &self.ruled()) && gate.tools.contains(&c.name)
                    })
                })
                .filter_map(|gate| gate.expire_ms)
                .min();
            (notify, expire_ms.map(|ms| at + Duration::from_millis(ms)))
        });
        loop {
            let (notify, lapse) = match first.take() {
                Some(first) => first,
                None => {
                    // A drain that began while this waited for its verdict
                    // starts nothing: the call is answered as not run.
                    if self.stopping.load(Relaxed) {
                        return Ok(Approval::Stop(Stop::Drained));
                    }
                    // The job reads the clock when it judges, and the lapse
                    // is measured from a fresh read, so time spent queued for
                    // the worker moves neither.
                    let checked = call.clone();
                    match self
                        .store
                        .op("approval_start", move |db| {
                            db.approval_start(turn, &checked, now_ms())
                        })
                        .await?
                    {
                        Gated::Started => return Ok(Approval::Run),
                        Gated::Denied => return Ok(Approval::Denied),
                        Gated::Expired => return fail("approval_expired"),
                        Gated::Lapsed => {
                            return Err(Error::with(
                                "approval_expired",
                                "a later call's gate lapsed first",
                            ));
                        }
                        Gated::Pending { notify, expires_ms } => (
                            notify,
                            expires_ms.map(|at| {
                                tokio::time::Instant::now()
                                    + Duration::from_millis(at.saturating_sub(now_ms()))
                            }),
                        ),
                    }
                }
            };
            let wake = lapse.map_or(hold, |lapse| lapse.min(hold));
            if tokio::time::Instant::now() < wake {
                tokio::select! {
                    () = notify.notified() => continue,
                    () = tokio::time::sleep_until(wake) => {}
                }
            }
            // A lapsed gate is judged by the next check.
            if lapse.is_some_and(|lapse| tokio::time::Instant::now() >= lapse) {
                continue;
            }
            let pending: Vec<ToolCall> = std::iter::once(call.clone())
                .chain(calls.as_slice().iter().cloned())
                .collect();
            let route = route.map(str::to_owned);
            let parked = self
                .store
                .op("suspend_approval", move |db| {
                    db.suspend_approval(
                        turn,
                        &pending,
                        now_ms(),
                        route.as_deref(),
                        summary_retry_ms,
                    )
                })
                .await?;
            match parked {
                // A verdict landed as the hold ran out.
                None => continue,
                Some(Some(at)) => return Ok(Approval::Stop(Stop::Lapses(at))),
                Some(None) => return Ok(Approval::Stop(Stop::Parked)),
            }
        }
    }

    /// Whether the daemon's load gate holds `call` back now: a foreground
    /// command that would start at once, while the machine is over its
    /// load and one of the daemon's commands runs. A call that is refused
    /// (forbidden, asked about with no gate, an escape no gate allows) is
    /// answered at once, and a gated one waits for its verdict instead: a
    /// denial comes back as soon as it is given, and one allowed starts
    /// then, as the person or approver who allowed it meant.
    fn load_holds(&self, record: &Bot, call: &ToolCall, workspace: &std::path::Path) -> bool {
        if self.registry.admits()
            || record.declared(&call.name).is_some()
            || !record.callable().iter().any(|name| name == &call.name)
        {
            return false;
        }
        let ruled = self.ruled();
        let ruling = record.ruling(call, &ruled);
        !record.gated(call, workspace, &ruled)
            && matches!(ruling, Ok(Ruling::None | Ruling::Allow))
            && !(record.sandboxed()
                && ruling != Ok(Ruling::Allow)
                && !record.confines(call, workspace))
            && matches!(
                self.registry.prepare(&call.name, &call.arguments),
                Ok(Prepared::Shell {
                    background: false,
                    ..
                })
            )
    }

    /// Wait for the load gate to let `held` start, a command among `call`
    /// and those starting with it: live until its next readings (a second
    /// apart) have passed, then parked as a verdict waits, holding no task
    /// and no slot, with `call` and the rest of its round not started. The daemon wakes the turn at the gate's
    /// first reading that lets a command start. None: run it now.
    async fn await_load(
        &self,
        held: &str,
        call: &ToolCall,
        calls: &std::vec::IntoIter<ToolCall>,
        route: Option<&str>,
        summary_retry_ms: Option<u64>,
    ) -> Result<Option<Stop>> {
        let Some(mut open) = self.registry.load_gate() else {
            return Ok(None);
        };
        let hold = tokio::time::Instant::now() + LOAD_HOLD;
        loop {
            if self.registry.admits() {
                return Ok(None);
            }
            if self.stopping.load(Relaxed) {
                return Ok(Some(Stop::Drained));
            }
            tokio::select! {
                changed = open.changed() => if changed.is_err() {
                    return Ok(None);
                },
                () = tokio::time::sleep_until(hold) => break,
            }
        }
        let turn = self.turn;
        let pending: Vec<ToolCall> = std::iter::once(call.clone())
            .chain(calls.as_slice().iter().cloned())
            .collect();
        let (held, route) = (held.to_owned(), route.map(str::to_owned));
        let parked = self
            .store
            .op("suspend_load", move |db| {
                let at = now_ms();
                db.suspend_load(
                    turn,
                    &held,
                    &pending,
                    at,
                    route.as_deref(),
                    summary_retry_ms,
                )
            })
            .await?;
        self.registry.note_load_park();
        Ok(match parked {
            // A later call's gate lapsed: the turn goes on and meets it.
            None => None,
            Some(Some(at)) => Some(Stop::Lapses(at)),
            Some(None) => Some(Stop::Parked),
        })
    }

    /// Wait for an answered call's answer: live for the hold, then parked
    /// like `wait`, holding no task and no slot, until the answer comes or
    /// the call's time runs out. Answers wake the task through the worker,
    /// so each check is one job and no commit.
    async fn await_answer(
        &self,
        call_id: &str,
        calls: &mut std::vec::IntoIter<ToolCall>,
        route: Option<&str>,
        summary_retry_ms: Option<u64>,
    ) -> Result<ControlFlow<Stop, Outcome>> {
        let turn = self.turn;
        // A client that answers is a program, near at hand: a short hold
        // takes its answer live, and a slower one parks without the slot.
        let hold = tokio::time::Instant::now()
            + Duration::from_millis(self.settings().approval_hold_ms().min(ANSWER_HOLD_MS));
        loop {
            let id = call_id.to_owned();
            let (notify, expires_ms) = match self
                .store
                .op("answer_take", move |db| db.answer_take(turn, &id, now_ms()))
                .await?
            {
                CallAnswer::Given(outcome) => return Ok(ControlFlow::Continue(outcome)),
                CallAnswer::Pending { notify, expires_ms } => (notify, expires_ms),
            };
            // No deadline while a step of the call waits on a gate.
            let lapse = expires_ms.map(|at| {
                tokio::time::Instant::now() + Duration::from_millis(at.saturating_sub(now_ms()))
            });
            let wake = lapse.map_or(hold, |lapse| lapse.min(hold));
            if tokio::time::Instant::now() < wake {
                tokio::select! {
                    () = notify.notified() => continue,
                    () = tokio::time::sleep_until(wake) => {}
                }
            }
            // A call out of time is answered by the next check.
            if lapse.is_some_and(|lapse| tokio::time::Instant::now() >= lapse) {
                continue;
            }
            let pending: Vec<ToolCall> = calls.as_slice().to_vec();
            let (id, route) = (call_id.to_owned(), route.map(str::to_owned));
            match self
                .store
                .op("suspend_answer", move |db| {
                    db.suspend_answer(
                        turn,
                        &id,
                        &pending,
                        now_ms(),
                        route.as_deref(),
                        summary_retry_ms,
                    )
                })
                .await?
            {
                // An answer landed as the hold ran out.
                None => continue,
                Some(Some(at)) => return Ok(ControlFlow::Break(Stop::Lapses(at))),
                Some(None) => return Ok(ControlFlow::Break(Stop::Parked)),
            }
        }
    }

    /// Room for a page the model asked for: a share of what the input
    /// limit leaves beside the running turn and what goes ahead of it. Old
    /// turns can leave the window; the current turn and pinned items
    /// cannot. Indexed accounting avoids rebuilding a full window per read.
    async fn page_budget(
        &self,
        share: usize,
        exhausted: &str,
    ) -> Result<(agent_runtime::codec::Family, usize)> {
        let allowance = self.input_limit();
        let (bot, turn) = (self.bot.clone(), self.turn);
        let (family, used) = self
            .store
            .read("history_usage", move |db| db.history_usage(&bot, turn))
            .await?;
        let budget = allowance.bytes.saturating_sub(used.bytes + 1) / share;
        if used.items >= allowance.items || budget < 256 {
            return fail(exhausted);
        }
        Ok((family, budget))
    }

    /// A page of a result that went as its stub. It stays beside its call
    /// until the model answers it, so neither elision nor a cut can make
    /// room for it later: it takes at most the room the turn leaves.
    async fn result_page(
        &self,
        call_id: &str,
        node: i64,
        offset: usize,
        limit: usize,
    ) -> Result<Outcome> {
        let (family, budget) = self.page_budget(1, "read_context_exhausted").await?;
        let mut checked = self.read_results.lock().unwrap().contains(&node);
        let mut bytes = budget;
        loop {
            let bot = self.bot.clone();
            let page = match self
                .store
                .read("result_lines", move |db| {
                    db.result_lines(&bot, node, offset, limit, checked, bytes)
                })
                .await
            {
                Ok(page) => page,
                // No part of a piece fits the room left: not the text's fault.
                Err(error) if error.code == "read_line_too_long" => {
                    return fail("read_context_exhausted");
                }
                Err(error) => return Err(error),
            };
            if !checked {
                self.read_results.lock().unwrap().push(node);
                checked = true;
            }
            // Numbering, JSON escaping, redaction, and the result envelope
            // all count: shrink the page by what the item overshoots until
            // the item itself fits.
            let output = self.registry.redact_text(page);
            let item = family.tool_result_item(call_id, false, &output)?.len();
            if item <= budget {
                return Ok(Outcome::text(output));
            }
            bytes = (bytes * budget / item).min(bytes.saturating_sub(1));
            if bytes == 0 {
                return fail("read_context_exhausted");
            }
        }
    }

    /// A page of a conversation turn, the bot's own or another's, or with
    /// no turn the other's newest turns.
    async fn history(
        &self,
        call_id: &str,
        bot: Option<String>,
        wanted: Option<i64>,
        handle: Option<String>,
        offset: u64,
        limit: usize,
    ) -> Result<Outcome> {
        let (family, budget) = self.page_budget(2, "history_context_exhausted").await?;
        // A handle names its bot and the store's number for the turn.
        let (bot, wanted) = match handle.as_deref().map(Handle::parse).transpose()? {
            Some(Handle::Turn { bot, turn }) => {
                let named = bot.clone();
                let number = self
                    .store
                    .read("history_number", move |db| db.history_number(&named, turn))
                    .await?;
                (bot, Some(number))
            }
            Some(Handle::Process(_)) => {
                return Err(Error::with(
                    "invalid_handle",
                    "history reads turns, not commands",
                ));
            }
            None => (bot.unwrap_or_else(|| self.bot.clone()), wanted),
        };
        let Some(wanted) = wanted else {
            let listed = self
                .store
                .read("history_turns", move |db| {
                    db.history_turns(&bot, HISTORY_TURNS)
                })
                .await?;
            let output = self.registry.redact_text(listed.to_string());
            if family.tool_result_item(call_id, false, &output)?.len() > budget {
                return fail("history_context_exhausted");
            }
            return Ok(Outcome::text(output));
        };
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
            if family.tool_result_item(call_id, false, &output)?.len() <= budget {
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

    /// Matches of literal text in a bot's history, or with a GLOB pattern
    /// every matching bot's, newest first; the caller's own turn is left
    /// out of a search that holds it.
    async fn history_search(
        &self,
        call_id: &str,
        bot: Option<String>,
        query: String,
        before: Option<i64>,
    ) -> Result<Outcome> {
        let (family, budget) = self.page_budget(2, "history_context_exhausted").await?;
        let own = bot.is_none() || bot.as_deref() == Some(self.bot.as_str());
        let bot = bot.unwrap_or_else(|| self.bot.clone());
        // No name holds a GLOB's metacharacters, so one names many bots.
        let many = bot.contains(['*', '?', '[']);
        let turn = self.turn;
        let found = self
            .store
            .read("history_search", move |db| match many {
                true => db.history_search_bots(&bot, &query, before, Some(turn)),
                false => db.history_search(&bot, &query, before, own),
            })
            .await?;
        let output = self.registry.redact_text(found.to_string());
        if family.tool_result_item(call_id, false, &output)?.len() > budget {
            return fail("history_context_exhausted");
        }
        Ok(Outcome::text(output))
    }

    /// Make the turn durable as waiting, then register its handles. Returns
    /// an immediate result only for arguments that can never resolve.
    #[allow(clippy::too_many_arguments)]
    async fn park(
        &self,
        call_id: &str,
        handles: Vec<String>,
        timeout_ms: Option<u64>,
        any: bool,
        calls: &mut std::vec::IntoIter<ToolCall>,
        route: Option<&str>,
        summary_retry_ms: Option<u64>,
    ) -> Result<ControlFlow<Stop, Outcome>> {
        let mut processes = Vec::new();
        for text in &handles {
            match Handle::parse(text) {
                Err(error) => return Ok(ControlFlow::Continue(failure(error))),
                Ok(Handle::Turn { bot, turn }) if bot == self.bot && turn == self.turn => {
                    return Ok(ControlFlow::Continue(failure(Error::with(
                        "invalid_handle",
                        "a turn cannot wait on itself",
                    ))));
                }
                Ok(Handle::Process(id)) => processes.push(id),
                Ok(_) => {}
            }
        }
        // A bot waits only on commands it started; one its history
        // inherited from a fork's source is not its to collect. An owned
        // command stays owned, so checking before the wait parks is enough.
        if !processes.is_empty() {
            let bot = self.bot.clone();
            let refused = self
                .store
                .op("unowned_process", move |db| {
                    db.unowned_process(&bot, &processes)
                })
                .await?;
            if let Some(error) = refused {
                return Ok(ControlFlow::Continue(failure(error)));
            }
        }
        // A steer waiting for the next boundary makes the wait a poll: it
        // reports what resolved, and the rest pending.
        let timeout_ms = if self.lift.is_armed() {
            Some(0)
        } else {
            timeout_ms
        };
        // Only move the remaining calls once this wait can actually park.
        let pending: Vec<ToolCall> = calls.collect();
        let deadline_ms = timeout_ms.map(|t| now_ms() + t);
        let (turn, id, list) = (self.turn, call_id.to_owned(), handles.clone());
        let route = route.map(str::to_owned);
        // A gated call after the wait still lapses on time: the service
        // wakes the turn then, and the lapse ends it.
        let lapse = self
            .store
            .op("suspend", move |db| {
                db.suspend(
                    turn,
                    &id,
                    &list,
                    deadline_ms,
                    any,
                    &pending,
                    route.as_deref(),
                    summary_retry_ms,
                )?;
                db.next_lapse(turn)
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
        Ok(ControlFlow::Break(lapse.map_or(Stop::Parked, Stop::Lapses)))
    }

    /// Park the turn on a terminal poll, as `park` parks a wait on the
    /// terminal's handle until `deadline_ms`; the poll's result is read
    /// from the terminal when the turn resumes.
    async fn park_poll(
        &self,
        call_id: &str,
        handle: i64,
        deadline_ms: u64,
        calls: &mut std::vec::IntoIter<ToolCall>,
        route: Option<&str>,
        summary_retry_ms: Option<u64>,
    ) -> Result<ControlFlow<Stop, Outcome>> {
        let pending: Vec<ToolCall> = calls.collect();
        let handles = vec![format!("proc:{handle}")];
        let (turn, id, list) = (self.turn, call_id.to_owned(), handles.clone());
        let route = route.map(str::to_owned);
        let lapse = self
            .store
            .op("suspend", move |db| {
                db.suspend_wait(
                    turn,
                    &id,
                    &list,
                    Some(deadline_ms),
                    true,
                    &pending,
                    route.as_deref(),
                    summary_retry_ms,
                    true,
                )?;
                db.next_lapse(turn)
            })
            .await?;
        self.handles
            .attach(
                &self.store,
                Waiter::Turn(self.turn),
                &handles,
                Some(deadline_ms),
                true,
                Completion::Resume {
                    bot: self.bot.clone(),
                    turn: self.turn,
                },
            )
            .await;
        Ok(ControlFlow::Break(lapse.map_or(Stop::Parked, Stop::Lapses)))
    }

    /// Start a command now; its result is retrievable through a proc handle
    /// that the store keeps unique and durable.
    #[allow(clippy::too_many_arguments)]
    async fn background(
        &self,
        call_id: &str,
        command: String,
        workspace: PathBuf,
        timeout_ms: u64,
        environment: Vec<(String, String)>,
        sandbox: Option<Arc<Policy>>,
        outputs: Option<Outputs>,
    ) -> Result<Outcome> {
        // The place in line is taken before anything durable is written.
        let queued = self.registry.queue()?;
        let (turn, started) = (self.turn, call_id.to_owned());
        let id = self
            .store
            .op("process_start", move |db| db.process_start(turn, &started))
            .await?;
        let (sender, receiver) = oneshot::channel();
        self.registry.background(
            command,
            workspace,
            timeout_ms,
            environment,
            sandbox,
            queued,
            sender,
        );
        self.collect(id, call_id, receiver, outputs, None);
        Ok(Outcome::text(
            json!({"handle":format!("proc:{id}"),"background":true}).to_string(),
        ))
    }

    /// Move a running foreground command to the background: it keeps its
    /// process slot and timeout, and its result arrives on a proc handle as
    /// a background command's does. A failure to record it drops the
    /// command, which kills it.
    async fn lift_shell<F>(
        &self,
        call_id: &str,
        run: std::pin::Pin<Box<F>>,
        outputs: Option<Outputs>,
    ) -> Result<Outcome>
    where
        F: Future<Output = Result<Outcome>> + Send + 'static,
    {
        let (turn, started) = (self.turn, call_id.to_owned());
        let id = self
            .store
            .op("process_start", move |db| db.process_start(turn, &started))
            .await?;
        let (sender, receiver) = oneshot::channel();
        let run = self.registry.until_pulled(run);
        tokio::spawn(async move {
            let _ = sender.send(run.await);
        });
        self.collect(id, call_id, receiver, outputs, None);
        Ok(Outcome::text(
            json!({"handle":format!("proc:{id}"),"background":true,"steered":true}).to_string(),
        ))
    }

    /// Record a background command's result when it arrives and wake those
    /// waiting on its handle. A terminal's, `terminal`, stays in the
    /// registry with its end until then, so a poll never finds it gone
    /// with no result.
    fn collect(
        &self,
        id: i64,
        call_id: &str,
        receiver: oneshot::Receiver<Result<Outcome>>,
        outputs: Option<Outputs>,
        terminal: Option<Registry>,
    ) {
        let turn = self.turn;
        let (store, handles, failures, call_id_for_refs) = (
            self.store.clone(),
            self.handles.clone(),
            self.background_failures.clone(),
            call_id.to_owned(),
        );
        tokio::spawn(async move {
            let result = receiver.await.unwrap_or_else(|_| fail("process_lost"));
            let (value, artifacts, files) = match result {
                Ok(outcome) => {
                    let outcome = match outputs {
                        Some(outputs) if !outcome.artifacts.is_empty() => {
                            outputs.keep(outcome, turn, &call_id_for_refs).await
                        }
                        _ => outcome,
                    };
                    let outcome = annotate(outcome, turn, &call_id_for_refs);
                    (
                        serde_json::from_str(&outcome.output)
                            .unwrap_or(json!({"output":outcome.output})),
                        outcome.artifacts,
                        outcome.files,
                    )
                }
                Err(error) => (
                    json!({"error":error.code,"detail":error.detail}),
                    Vec::new(),
                    Files::default(),
                ),
            };
            // Commit the result and overflow together before waking waiters.
            // Report storage failures to the service instead of stranding waiters.
            let recorded = value.clone();
            match store
                .op("process_finish", move |db| {
                    db.process_finish(id, &recorded, &artifacts, &files)?;
                    files.recorded();
                    Ok(())
                })
                .await
            {
                Ok(()) => handles.process_finished(id, value),
                Err(error) => {
                    // A closed receiver means the service has already exited.
                    let _ = failures.send(error);
                }
            }
            if let Some(registry) = terminal {
                registry.forget_terminal(id);
            }
        });
    }
}

/// Cancel a refresh not yet sent, or wait for the answer to one that was.
/// It stays in `pending` until answered, so a caller dropped meanwhile
/// leaves it for the turn's end to settle.
async fn settle(
    pending: &mut Option<Pending>,
) -> Option<Result<(agent_runtime::provider::Usage, tokio::time::Instant)>> {
    let in_flight = pending.as_mut()?;
    let refreshed = if in_flight.refresh.cancel() {
        in_flight.task.abort();
        None
    } else {
        Some(
            (&mut in_flight.task)
                .await
                .unwrap_or_else(|_| fail("keep_warm_lost")),
        )
    };
    *pending = None;
    refreshed
}

/// When `at` was, in milliseconds since the Unix epoch.
fn epoch_ms(at: tokio::time::Instant) -> u64 {
    now_ms().saturating_sub(at.elapsed().as_millis() as u64)
}

/// The strip that withholds every thinking block a failed request replayed,
/// when the failure is a 400. A 400 is final for its body, and a replayed
/// block the provider cannot verify or decrypt is documented to fail with
/// one: no published error type names it, and the message is not matched.
/// The call sends the body once more without that thinking, and the bot
/// keeps it out only once a request without it is accepted. Server errors
/// are retried with the body unchanged.
fn withhold_thinking(context: &Context, error: &Error) -> Option<Strip> {
    let window = context.window.as_ref()?;
    let replayed = window.family == agent_runtime::codec::Family::Anthropic
        && window
            .ids
            .iter()
            .zip(&window.thinking)
            .any(|(id, bytes)| *bytes > 0 && !context.thinking.strips(*id));
    (replayed && error.code == "provider_http_400")
        .then(|| Strip::from(window.ids[window.ids.len() - 1] + 1))
}

/// Whether the turn may make another model call.
/// Whether the bot may search the web: its provider declares the search
/// when it offers one, and a fork's allowed list narrows it as any tool.
fn searches(record: &agent_runtime::store::Bot) -> bool {
    record.callable().iter().any(|tool| tool == "web_search")
}

fn calls_left(record: &agent_runtime::store::Bot, model_rounds: usize) -> bool {
    model_rounds < record.settings.max_rounds() && record.exhausted().is_none()
}

/// Failures of the provider's pace, capacity, or transport, none of which say
/// anything about the request. Every 5xx is one, as Anthropic's SDK treats it
/// (anthropic-sdk-python 4421d56, `_should_retry`): a CDN's 520 says no more
/// than a 502. Model-level outcomes (`provider_incomplete`) and client errors
/// are final.
fn retryable(code: &str) -> bool {
    matches!(
        code,
        "provider_rate_limited"
            | "provider_overloaded"
            | "provider_server_error"
            | "provider_http_429"
            | "provider_stream_failed"
            | "provider_stream_stalled"
            | "provider_login_refreshed"
            | "provider_signature_expired"
            | "provider_login_throttled"
            | "provider_plan_unavailable"
            | "truncated_sse_frame"
            | "provider_admission_timeout"
            | "provider_socket_expired"
    ) || code.starts_with("provider_connection_")
        || code.starts_with("provider_http_5")
}

/// Causes a person can fix that refuse a turn's start, which hold it in
/// line until they are: a provider the config lacks, no folder to work in.
/// A bot's budget is a lifetime cap no request raises, so it ends the turn.
pub fn held_at_start(code: &str) -> bool {
    matches!(code, "provider_unavailable" | "workspace_required")
}

/// Causes a person can fix, which hold the turn until they are: a login
/// refused or expired, a key or account the provider refuses, a provider
/// quota spent. The bot's and the turn's own token caps are limits their
/// makers set, and end it.
fn held(code: &str) -> bool {
    matches!(
        code,
        "provider_login_rejected"
            | "provider_login_expired"
            | "provider_login_unavailable"
            | "provider_quota_exhausted"
            | "provider_http_401"
            | "provider_http_403"
    )
}

/// A stream that stalled on this request stalls again when resent at once
/// with the same context: its retries start at 2 s.
const STALL_START: u32 = 3;

/// The wait a server error's response asked for, when it is one a retry
/// takes: up to a minute, as the OpenAI and Anthropic SDKs take it.
fn asked(error: &Error) -> Option<std::time::Duration> {
    let ms = error.facts.as_ref()?.get("retry_after_ms")?.as_u64()?;
    (ms <= 60_000).then(|| std::time::Duration::from_millis(ms))
}

/// Retry backoffs at least this long park the turn instead of sleeping in it.
const BACKOFF_PARK: std::time::Duration = std::time::Duration::from_secs(1);

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

/// Where a bot's cut outputs go whole: the outputs folder its settings
/// name, and what decides whether its sandbox may write there.
#[derive(Clone)]
struct Outputs {
    folder: PathBuf,
    settings: Settings,
    root: PathBuf,
    daemon: agent_runtime::sandbox::Daemon,
}

impl Outputs {
    /// Write each retained stream of `outcome` whole to a file of its own,
    /// `TURN-CALL_ID-STREAM`, in place of the store's copy, off the runtime
    /// thread. Only while the bot's sandbox cannot write the folder: a
    /// command that could would put a link there for the daemon to follow.
    /// Files are made new, never through a link, readable by their owner
    /// alone. A stream that cannot be written stays in the store.
    async fn keep(self, mut outcome: Outcome, turn: i64, call_id: &str) -> Outcome {
        if outcome.artifacts.is_empty() {
            return outcome;
        }
        let name: String = call_id
            .chars()
            .map(
                |c| match c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    true => c,
                    false => '_',
                },
            )
            .collect();
        let artifacts = std::mem::take(&mut outcome.artifacts);
        // A turn stopped while this runs drops what it returns, and the
        // files with it: no record will name them.
        let kept = tokio::task::spawn_blocking(move || {
            if self.exposed() {
                return Kept {
                    stored: artifacts,
                    files: Files::default(),
                };
            }
            let mut stored = Vec::new();
            let mut written = Vec::new();
            for (stream, data) in artifacts {
                let file = self.folder.join(format!("{turn}-{name}-{stream}"));
                match write_new(&file, &data).ok().and(file.to_str()) {
                    Some(path) => written.push((stream, path.to_owned(), data)),
                    None => stored.push((stream, data)),
                }
            }
            // The store commits only where a file is: its name must last
            // as its bytes do, or the store keeps them after all.
            if !written.is_empty()
                && std::fs::File::open(&self.folder)
                    .and_then(|folder| folder.sync_all())
                    .is_err()
            {
                for (stream, path, data) in written.drain(..) {
                    let _ = std::fs::remove_file(path);
                    stored.push((stream, data));
                }
            }
            let files = written
                .into_iter()
                .map(|(stream, path, _)| (stream, path))
                .collect();
            Kept {
                stored,
                files: Files::new(files),
            }
        })
        .await;
        // A task that failed took the data with it: the cut text stands alone.
        if let Ok(Kept { stored, files }) = kept {
            outcome.artifacts = stored;
            outcome.files = files;
        }
        outcome
    }

    /// Whether the bot's commands may write the folder, made first so the
    /// answer is about the folder itself, or it is not a folder. A sandbox
    /// that can't be built counts as one that may.
    fn exposed(&self) -> bool {
        use std::os::unix::fs::DirBuilderExt;
        let found = |path: &std::path::Path| std::fs::symlink_metadata(path).is_ok();
        let existing = self.folder.ancestors().find(|path| found(path));
        if std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.folder)
            .is_err()
            || !std::fs::symlink_metadata(&self.folder).is_ok_and(|found| found.is_dir())
        {
            return true;
        }
        // Folders it made last only once the folders that name them are
        // synced; the folder itself is synced with what is written in it.
        if existing != Some(self.folder.as_path()) {
            for parent in self.folder.ancestors().skip(1) {
                if std::fs::File::open(parent)
                    .and_then(|folder| folder.sync_all())
                    .is_err()
                {
                    return true;
                }
                if Some(parent) == existing {
                    break;
                }
            }
        }
        match Policy::new(
            self.settings.sandbox,
            Some(&self.root),
            &self.settings.writable,
            self.settings.network,
            &self.daemon,
        ) {
            Ok(Some(policy)) => policy.can_write(&self.folder),
            // Unconfined, its commands may write whatever the daemon may.
            Ok(None) => false,
            Err(_) => true,
        }
    }
}

/// A call's cut streams once kept: those the store keeps, and the files
/// that hold the others.
struct Kept {
    stored: Vec<(&'static str, Vec<u8>)>,
    files: Files,
}

/// `data` to a new file at `path`, never through a link, on disk before
/// it returns, as the store's commit is.
fn write_new(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if let Err(error) = file.write_all(data).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

/// Name retained streams in the model-facing result so the model can read
/// them back: `TURN/CALL_ID/STREAM`, and the files that hold any of them
/// whole, by path.
fn annotate(mut outcome: Outcome, turn: i64, call_id: &str) -> Outcome {
    if outcome.artifacts.is_empty() && outcome.files.is_empty() {
        return outcome;
    }
    let refs: Vec<String> = (outcome.artifacts.iter().map(|(stream, _)| *stream))
        .chain(outcome.files.iter().map(|(stream, _)| *stream))
        .map(|stream| format!("{turn}/{call_id}/{stream}"))
        .collect();
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&outcome.output)
        && value.is_object()
    {
        value["artifacts"] = json!(refs);
        if !outcome.files.is_empty() {
            let files: Vec<&str> = outcome
                .files
                .iter()
                .map(|(_, file)| file.as_str())
                .collect();
            value["files"] = json!(files);
        }
        outcome.output = value.to_string();
    }
    outcome
}

/// A model reads a peer turn's outcome, not its accounting: the turn view's
/// usage, timing and identity stay with clients and out of the context. A
/// pending turn is its status; an ended one is everything its end recorded,
/// facts included.
fn for_model(results: BTreeMap<String, Arc<Value>>) -> BTreeMap<String, Arc<Value>> {
    results
        .into_iter()
        .map(|(handle, result)| match Handle::parse(&handle) {
            Ok(Handle::Turn { .. }) => {
                let mut outcome = Arc::unwrap_or_clone(result);
                if let Some(fields) = outcome.as_object_mut() {
                    for key in TURN_VIEW_ACCOUNTING {
                        fields.remove(key);
                    }
                }
                (handle, Arc::new(outcome))
            }
            _ => (handle, result),
        })
        .collect()
}

#[cfg(test)]
use agent_runtime::store::pinned_item;

#[derive(Clone)]
struct Context {
    window: Option<Window>,
    prefix: ContextPrefix,
    /// The items sent without their thinking blocks.
    thinking: Strip,
}
impl Context {
    fn empty() -> Self {
        Self {
            window: None,
            prefix: ContextPrefix {
                bytes: Bytes::new(),
                items: 0,
                required: ContextUsage::default(),
            },
            thinking: Strip::default(),
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
    /// Whether the window sends more pictures than a request takes.
    fn overpictured(&self) -> bool {
        self.window.as_ref().is_some_and(|w| {
            w.pictures > MOST_PICTURES
                || w.pictured
                    .iter()
                    .map(|&bytes| i64::from(bytes.max(0)))
                    .sum::<i64>()
                    > MOST_PICTURE_BYTES
        })
    }
}

/// The most pictures a request sends, and the most bytes they add beyond
/// their weight, which the context budget does not bound: Anthropic takes
/// 100 pictures and 32 MB in a request.
const MOST_PICTURES: i64 = 100;
const MOST_PICTURE_BYTES: i64 = 20 << 20;

/// A bound on what a summary's request adds to what it summarizes: the
/// request after a copy, or the frame of a request of its own.
const SUMMARY_REQUEST_BYTES: usize = 16 * 1024;

/// A summary planned at a boundary to run beside its call.
type Beside<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Landed> + Send + 'a>>;

/// What a summary beside the call spent, counted toward the turn.
#[derive(Default)]
struct Spent {
    tokens: u64,
    rounds: usize,
}
impl Spent {
    fn count(self, tokens_used: &mut u64, model_rounds: &mut usize) {
        *tokens_used = tokens_used.saturating_add(self.tokens);
        *model_rounds += self.rounds;
    }
}

/// How long summaries held the turn, and the retries and pacing of those
/// beside its calls, kept outside the rounds so what an interrupt cuts
/// short still counts toward the turn's.
#[derive(Default)]
pub struct SideCounts {
    summary_ms: AtomicU64,
    retries: AtomicU64,
    paced_ms: AtomicU64,
    /// A summary beside the turn's call wrote its replacement for the view
    /// the call sent, so no refresh keeps that view's cache warm.
    replaced: AtomicBool,
}

/// Time from now until dropped, added to a summary counter.
struct Held<'a> {
    started: std::time::Instant,
    into: &'a AtomicU64,
}
impl<'a> Held<'a> {
    fn from_now(into: &'a AtomicU64) -> Self {
        Held {
            started: std::time::Instant::now(),
            into,
        }
    }
}
impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.into.fetch_add(elapsed_ms(self.started), Relaxed);
    }
}

/// A summary's own accounting, added to its [`SideCounts`] when it ends or
/// is cancelled.
struct Counted<'a> {
    spent: Accounting,
    into: &'a SideCounts,
}
impl Drop for Counted<'_> {
    fn drop(&mut self) {
        let (retries, paced_ms) = self.spent.totals();
        self.into.retries.fetch_add(retries, Relaxed);
        self.into.paced_ms.fetch_add(paced_ms, Relaxed);
    }
}

/// How a summary beside the call ended, and what it spent.
struct Landed {
    result: Result<Summary>,
    spent: Spent,
}

/// Milliseconds since `started`.
fn elapsed_ms(started: std::time::Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// A summary call's outcome: written, to install; parked on
/// its pool until the time given; or not made, reported live.
enum Summary {
    Written(Box<Written>),
    Parked(u64),
    Skipped,
}

/// A summary written for `plan`, what its call billed, and how it was
/// requested, for the `compacted` event.
struct Written {
    plan: agent_runtime::store::CompactionPlan,
    summary: String,
    usage: Option<agent_runtime::provider::Usage>,
    request: Value,
}

/// How a summary attempt ended: recorded, parked on its pool until the
/// time given, or not made (not due, nothing to cut, or a failure reported
/// live).
enum Compaction {
    Done,
    Parked(u64),
    Skipped,
}

/// A turn over its budget after `overflow`: its view now fits, its summary
/// parked, or nothing more can be reclaimed.
enum Overflow {
    Fits(Box<Context>),
    Parked(u64),
    Stuck,
}

/// What a model call sends: the bot's window, a copy of the bot's call
/// asking for a summary, or a summary request of its own over a span,
/// with the bytes of the span's thinking it leaves out.
#[derive(Clone, Copy)]
enum Body<'a> {
    Window(&'a Context),
    Copied(Copied<'a>),
    Span(&'a agent_runtime::store::CompactionPlan, usize),
}

/// A summary request sent as a copy of the bot's call: what went ahead of
/// the window when it was sent, the window's first `len` items read under
/// the same elision floor and thinking strip, and the compaction request
/// after them. It shares the call's instructions, tools, model, and cache
/// key, so the provider reads what the call sent from cache.
#[derive(Clone, Copy)]
struct Copied<'a> {
    prefix: &'a Bytes,
    window: &'a Window,
    len: usize,
    thinking: Strip,
    request: &'a Bytes,
}

/// A view a summary may copy, the turn's model, which made that call, and
/// what the view sends ahead of its window now, if known.
struct Sent<'a> {
    view: SentView<'a>,
    /// `provider/model`, the bot's or the turn's override.
    model: &'a str,
    ahead: Option<Bytes>,
}

enum SentView<'a> {
    /// The view before this boundary's stubs, taken as sent through the
    /// span by the call before a new prompt or a wait.
    Current(&'a Context),
    /// A call this task made, or a parked summary copied, read again.
    Call(&'a LastCall),
}

/// The view a call sent, kept small between rounds and read again only
/// when a summary may copy it: the elision floor its window was read
/// under, the node that window starts at, the node it ends at when the
/// copy may take it whole (else it is known sent only through a summary's
/// span, as a view an earlier task's call sent is), and what went ahead of
/// it, or `None` for what the view sends there now.
struct LastCall {
    floor: i64,
    first: i64,
    last: Option<i64>,
    ahead: Option<(Bytes, usize)>,
}

impl LastCall {
    /// What a summary needs to copy `view` again, `whole` when it is
    /// exactly what a call sent.
    fn of(view: &Context, whole: bool) -> Option<Self> {
        let window = view.window.as_ref()?;
        Some(Self {
            floor: window.elided,
            first: *window.ids.first()?,
            last: if whole {
                Some(*window.ids.last()?)
            } else {
                None
            },
            ahead: Some((view.prefix.bytes.clone(), view.prefix.items)),
        })
    }
}

impl From<agent_runtime::store::CopiedCall> for LastCall {
    fn from(call: agent_runtime::store::CopiedCall) -> Self {
        Self {
            floor: call.floor,
            first: call.first,
            last: call.last,
            ahead: call
                .prefix
                .map(|(prefix, items)| (Bytes::from(prefix), items)),
        }
    }
}

/// A steer that stayed queued for lack of room: what went ahead of the
/// turn then, and whether it was measured against the whole budget.
#[derive(Clone, Copy)]
struct Capped {
    ahead: ContextUsage,
    whole: bool,
}

/// A cached input byte counts as a tenth of one sent uncached, what
/// Anthropic's cache reads and OpenAI's GPT-5 family bill. Older OpenAI
/// models discount less, so there the estimate favors a copy a little
/// more than their bills do.
const CACHED_SHARE: usize = 10;

/// The items of a request of its own for the span, framed, with its
/// thinking still in.
fn own_items(
    family: agent_runtime::codec::Family,
    plan: &agent_runtime::store::CompactionPlan,
) -> Result<usize> {
    let (head, tail) = agent_runtime::store::CompactionPlan::frame(
        family,
        plan.previous_summary.as_deref(),
        plan.summary_bytes,
    )?;
    Ok(head.len()
        + plan.sizes.iter().map(|&size| size as usize).sum::<usize>()
        + plan.ids.len().saturating_sub(1)
        + tail.len())
}

/// The estimated cost of `bytes` of input of which `cached` are read from
/// the provider's cache.
fn input_cost(bytes: usize, cached: usize) -> usize {
    bytes - cached + cached / CACHED_SHARE
}

/// A summary planned at a boundary and ready to call: its span, its
/// summarizer, and, when it copies a call, the view that call sent with
/// the items it copies, their thinking strip, and the compaction request.
struct Planned<'p> {
    plan: agent_runtime::store::CompactionPlan,
    reference: String,
    summarizer: std::borrow::Cow<'p, Provider>,
    copy: Option<(Context, usize, Strip, Bytes)>,
    choice: Choice,
}

/// How a summary goes out: a copy of the call's first `copy` window
/// items, or a request of its own (`None`), with the estimate of each
/// option that could be sent.
#[derive(Debug, Default, PartialEq, Eq)]
struct Choice {
    copy: Option<usize>,
    copy_cost: Option<usize>,
    own_cost: Option<usize>,
}
impl Choice {
    /// The `request` field of the `compacted` event.
    fn event(&self) -> Value {
        json!({"form": if self.copy.is_some() { "copy" } else { "own" },
            "items": self.copy,
            "estimate": {"copy": self.copy_cost, "own": self.own_cost}})
    }
}

/// What a summary would send each way, from stored sizes: the view a copy
/// takes its window from, how far a call is known to have sent it, what
/// goes ahead of the window and after it, and a request of its own.
struct Shape<'a> {
    family: agent_runtime::codec::Family,
    ids: &'a [i64],
    sizes: &'a [u32],
    thinking: &'a [u32],
    strip: Strip,
    /// Where the span sits in the window.
    span: std::ops::Range<usize>,
    /// Items the call is known to have sent: the whole window when it is
    /// the call's view, else through the span, the call having ended
    /// somewhere past it in the window.
    sent: usize,
    /// Whether the copy may send the window past the span: not for a
    /// catch-up step, whose summary covers only its span.
    past: bool,
    /// Instructions and tools, then what goes ahead of the window, its
    /// bytes and items, and the request after it.
    fixed: usize,
    ahead: ContextUsage,
    request: usize,
    /// A request of its own: instructions and tools, then its frame and
    /// the span's items as the summarizer reads them.
    own_fixed: usize,
    own_items: usize,
    limit: ContextUsage,
}

/// The cheapest way to send a summary by estimated input cost. A copy
/// sends the window through the span's end, or whole. The Responses
/// cache reads any prefix of a call it holds; Anthropic's reads only at a
/// breakpoint, and the call placed its own where it ended, in the whole
/// window, so a copy through the span reads only the explicit one after
/// tools and instructions. The input limit counts a copy's items as
/// `fits` does, thinking included.
fn choose(shape: &Shape<'_>) -> Choice {
    use agent_runtime::codec::Family;
    let stripped = |range: std::ops::Range<usize>| -> usize {
        range
            .filter(|&n| shape.strip.strips(shape.ids[n]))
            .map(|n| shape.thinking[n] as usize)
            .sum()
    };
    let items = |len: usize| -> usize {
        shape.sizes[..len]
            .iter()
            .map(|&size| size as usize)
            .sum::<usize>()
            + len.saturating_sub(1)
    };
    let mut lens = vec![shape.span.end];
    if shape.past && shape.ids.len() > shape.span.end {
        lens.push(shape.ids.len());
    }
    let mut copy: Option<(usize, usize)> = None;
    for len in lens {
        let usage = ContextUsage {
            bytes: shape.ahead.bytes + items(len) + shape.request,
            items: shape.ahead.items + len + 1,
        };
        if !usage.fits(shape.limit) {
            continue;
        }
        let bytes = shape.fixed + usage.bytes - stripped(0..len);
        let reads = match shape.family {
            Family::Responses => len.min(shape.sent),
            Family::Anthropic if len == shape.ids.len() => shape.sent,
            Family::Anthropic => 0,
        };
        let cached = match reads {
            0 => shape.fixed,
            reads => shape.fixed + shape.ahead.bytes + items(reads) - stripped(0..reads),
        };
        let cost = input_cost(bytes, cached);
        if copy.is_none_or(|(_, best)| cost < best) {
            copy = Some((len, cost));
        }
    }
    let own_stripped = match shape.family {
        Family::Anthropic => shape.thinking[shape.span.clone()]
            .iter()
            .map(|&bytes| bytes as usize)
            .sum(),
        Family::Responses => 0,
    };
    let own = shape.own_fixed + shape.own_items.saturating_sub(own_stripped);
    Choice {
        copy: copy.filter(|(_, cost)| *cost < own).map(|(len, _)| len),
        copy_cost: copy.map(|(_, cost)| cost),
        own_cost: Some(own),
    }
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
        files: Files::default(),
        parts: Vec::new(),
        note: None,
        failed: true,
    }
}

/// `settings` with five minutes for an hour where any of the `models` a
/// request may go to must keep five minutes, as a longer marker may not
/// follow a shorter one. A model on a provider unknown here or of another
/// family is sent nothing.
fn one_lifetime<'a>(
    providers: &HashMap<String, Provider>,
    family: Option<agent_runtime::codec::Family>,
    settings: &Settings,
    models: impl IntoIterator<Item = &'a str>,
) -> Settings {
    let mut settings = settings.clone();
    let takes_hour = (models.into_iter())
        .filter_map(|reference| split_model(reference).ok())
        .filter_map(|(name, model)| Some((providers.get(name)?, model)))
        .filter(|(provider, _)| Some(provider.family()) == family)
        .all(|(provider, model)| provider.takes_cache_hour(model));
    if settings.cache_hour() && !takes_hour {
        settings.cache_ttl = Some(agent_runtime::store::CacheTtl::Minutes);
    }
    settings
}

/// The summarizer: the bot's own model unless the client named one of the
/// same family, checked at creation.
fn summarizer(record: &agent_runtime::store::Bot) -> String {
    record
        .compaction_model
        .clone()
        .unwrap_or_else(|| format!("{}/{}", record.provider, record.model))
}

/// A call another model may have answered on another provider binding: a
/// summary, charged to the turn that needed it, or a refused request's
/// fallback. Name both, as a server-side fallback names its attempts, so
/// the call is priced at that model's rates.
fn priced_at(
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
            cache_write_1h_tokens: usage.cache_write_1h_tokens,
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
    fn a_stall_waits_seconds_before_its_retry_and_an_overload_holds_the_pool() {
        for code in [
            "provider_http_529",
            "provider_http_429",
            "provider_rate_limited",
            "provider_overloaded",
        ] {
            assert!(agent_runtime::provider::paced(code), "{code}");
            assert!(super::retryable(code), "{code}");
        }
        assert!(!agent_runtime::provider::paced("provider_stream_stalled"));
        let first = super::backoff(1 + super::STALL_START, 7).as_millis();
        assert!((1600..=2400).contains(&first), "{first}");
        assert!(super::backoff(1, 7).as_millis() < 400);
    }
    #[test]
    fn one_model_that_keeps_five_minutes_keeps_every_request_at_five() {
        use super::{HashMap, Provider, Settings};
        use agent_runtime::{codec::Family, provider::Transport, store::CacheTtl};
        let provider = |url| {
            Provider::new(Transport::new(1, 1).unwrap(), Family::Anthropic, url, None).unwrap()
        };
        let providers = HashMap::from([
            (
                "anthropic".to_owned(),
                provider("https://api.anthropic.com/v1"),
            ),
            (
                "bedrock".to_owned(),
                provider("https://bedrock-runtime.us-east-1.amazonaws.com/anthropic/v1"),
            ),
        ]);
        let hour = Settings {
            cache_ttl: Some(CacheTtl::Hour),
            ..Settings::default()
        };
        let ttl = |models: &[&str]| {
            super::one_lifetime(
                &providers,
                Some(Family::Anthropic),
                &hour,
                models.iter().copied(),
            )
            .cache_ttl
        };
        assert_eq!(
            ttl(&[
                "anthropic/claude-opus-5-5",
                "bedrock/us.anthropic.claude-opus-5-5-v1:0"
            ]),
            Some(CacheTtl::Hour)
        );
        // A summarizer or fallback on older Claude there brings the turn's
        // own calls down too; a provider not here is sent nothing.
        assert_eq!(
            ttl(&[
                "anthropic/claude-opus-5-5",
                "bedrock/anthropic.claude-sonnet-4-20250514-v1:0"
            ]),
            Some(CacheTtl::Minutes)
        );
        assert_eq!(
            ttl(&["anthropic/claude-opus-5-5", "gone/claude-3-7-sonnet"]),
            Some(CacheTtl::Hour)
        );
        // Five minutes stays five minutes.
        assert_eq!(
            super::one_lifetime(
                &providers,
                Some(Family::Anthropic),
                &Settings::default(),
                ["anthropic/claude-opus-5-5"]
            )
            .cache_ttl,
            None
        );
    }

    #[test]
    fn pinned_blocks_cache_only_with_their_provider_format() {
        use agent_runtime::codec::Family;
        for hour in [false, true] {
            let anthropic: serde_json::Value = serde_json::from_slice(
                &super::pinned_item(Family::Anthropic, "stable", hour).unwrap(),
            )
            .unwrap();
            assert_eq!(
                anthropic["content"][0]["cache_control"],
                agent_runtime::codec::cache_control(hour)
            );
            assert_eq!(
                super::pinned_item(Family::Responses, "stable", hour).unwrap(),
                Family::Responses.user_item("stable").unwrap()
            );
        }
    }
    use super::*;
    use serde_json::Value;

    #[test]
    fn a_models_wait_keeps_a_turns_outcome_and_drops_its_accounting() {
        let view = json!({"bot":"Bob","bot_id":2,"turn":3,"handle":"turn:Bob/3",
            "request_id":"r","waiting_on":null,"input_tokens":10,"cached_input_tokens":0,
            "output_tokens":4,"model_rounds":1,"retries":0,"paced_ms":0,"started_ms":1,
            "finished_ms":2});
        let mut failed = view.clone();
        failed.as_object_mut().unwrap().extend(
            json!({"status":"failed","error":"turn_budget_exceeded","detail":"over",
                "budget_tokens":100,"tokens_used":120})
            .as_object()
            .unwrap()
            .clone(),
        );
        let mut pending = view.clone();
        pending["status"] = json!("running");
        pending["pending"] = json!(true);
        let results = BTreeMap::from([
            ("turn:Bob/3".to_string(), Arc::new(failed)),
            ("turn:Bob/4".to_string(), Arc::new(pending)),
        ]);
        let model = for_model(results);
        assert_eq!(
            *model["turn:Bob/3"],
            json!({"turn":3,"status":"failed","error":"turn_budget_exceeded","detail":"over",
                "budget_tokens":100,"tokens_used":120})
        );
        assert_eq!(
            *model["turn:Bob/4"],
            json!({"turn":3,"status":"running","pending":true})
        );
    }

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
                            effort: None,
                            budget_tokens: None,
                            tools: &[],
                            created_by: None,
                            created_by_id: None,
                            compaction_instructions: None,
                            compaction_model: None,
                            fallback: None,
                            gate: None,
                            settings: Default::default(),
                            declared: None,
                            request_id: None,
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
                    reader: store.reader(),
                },
            )
            .await;
        let (cancel, cancelled) = watch::channel(None);
        let task = Turn {
            identity: 0,
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
            settings: Default::default(),
            rules: Default::default(),
            ruled: Default::default(),
            window: Default::default(),
            resumed: None,
            steers: Arc::new(AtomicBool::new(true)),
            compact: Arc::default(),
            lift: Arc::default(),
            tokens: Arc::default(),
            read_results: Default::default(),
            side: Default::default(),
            stopping: Arc::default(),
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
        // Cancelled by an interrupt: the turn ends with the error its cause
        // names. (Shutdown parks the turn instead of ending it.)
        cancel.send(Some(INTERRUPTED)).unwrap();
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
        assert!(matches!(exit, Exit::Finished(Some(error)) if error.code == INTERRUPTED));
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

    #[tokio::test]
    async fn a_stopping_daemon_parks_the_turn_before_its_next_call() {
        use agent_runtime::{
            codec::Family,
            provider::Transport,
            store::{Binding, TurnOptions},
        };
        let dir = std::env::temp_dir().join(format!("agent-drain-park-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (store, publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let turn = store
            .op("create", |db| {
                db.create(
                    "Bob",
                    Some("/synthetic"),
                    Binding {
                        provider: "openai",
                        family: Family::Responses,
                        model: "synthetic",
                        instructions: "",
                        effort: None,
                        budget_tokens: None,
                        tools: &[],
                        created_by: None,
                        created_by_id: None,
                        compaction_instructions: None,
                        compaction_model: None,
                        fallback: None,
                        gate: None,
                        settings: Default::default(),
                        declared: None,
                        request_id: None,
                    },
                )?;
                Ok(db
                    .begin(
                        "Bob",
                        "first",
                        "work",
                        true,
                        &TurnOptions::default(),
                        |_, _| Ok(()),
                    )?
                    .turn)
            })
            .await
            .unwrap();
        let handles = Handles::new(mpsc::unbounded_channel().0);
        let publisher = tokio::spawn(crate::server::publish(
            publications,
            Hub::default(),
            handles.clone(),
        ));
        let (_cancel, cancelled) = watch::channel(None);
        // Nothing listens there: the turn must park before any call.
        let task = Turn {
            identity: 0,
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
            hub: Hub::default(),
            handles,
            background_failures: mpsc::unbounded_channel().0,
            settings: Default::default(),
            rules: Default::default(),
            ruled: Default::default(),
            window: Default::default(),
            resumed: None,
            steers: Arc::default(),
            compact: Arc::default(),
            lift: Arc::default(),
            tokens: Arc::default(),
            read_results: Default::default(),
            side: Default::default(),
            stopping: Arc::new(AtomicBool::new(true)),
        };
        let exit = tokio::time::timeout(Duration::from_secs(5), task.execute(cancelled))
            .await
            .unwrap();
        assert!(matches!(exit, Exit::Paced(0)));
        let (status, waiting) = store
            .op("parked", move |db| {
                let waiting = db.waiting_turns()?.into_iter().find(|w| w.turn == turn);
                Ok((db.turn_status("Bob", turn)?, waiting.unwrap()))
            })
            .await
            .unwrap();
        assert_eq!(status, "paced");
        assert_eq!(waiting.halt, Some(Halt::Shutdown));
        assert_eq!(waiting.deadline_ms, None);
        let paced = store
            .op("events", move |db| db.events("Bob", 0, 64))
            .await
            .unwrap();
        let paced = paced["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["event"] == "turn_paced")
            .unwrap()
            .clone();
        assert_eq!(paced["data"], json!({"resume_at_ms":null,"stopped":true}));
        drop(store);
        let _ = tokio::time::timeout(Duration::from_secs(2), publisher).await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_shut_load_gate_holds_a_batch_whole_when_a_later_call_is_a_command() {
        use agent_runtime::{
            codec::Family,
            provider::Transport,
            store::{Binding, TurnOptions},
        };
        let dir = std::env::temp_dir().join(format!("agent-load-batch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "hello").unwrap();
        let (store, _publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let tools = ["shell".to_owned(), "read".to_owned()];
        let calls: Vec<ToolCall> = [
            ("read", "read-1", json!({"path": dir.join("notes.txt")})),
            ("shell", "shell-1", json!({"command": "true"})),
        ]
        .into_iter()
        .map(|(name, id, arguments)| ToolCall {
            custom: false,
            name: name.into(),
            call_id: id.into(),
            arguments: arguments.to_string(),
        })
        .collect();
        let round = calls.clone();
        let workspace = dir.clone();
        let (turn, record) = store
            .op("create", move |db| {
                db.create(
                    "Bob",
                    workspace.to_str(),
                    Binding {
                        provider: "openai",
                        family: Family::Responses,
                        model: "synthetic",
                        instructions: "",
                        effort: None,
                        budget_tokens: None,
                        tools: &tools,
                        created_by: None,
                        created_by_id: None,
                        compaction_instructions: None,
                        compaction_model: None,
                        fallback: None,
                        gate: None,
                        settings: Default::default(),
                        declared: None,
                        request_id: None,
                    },
                )?;
                let turn = (db.begin(
                    "Bob",
                    "first",
                    "work",
                    true,
                    &TurnOptions::default(),
                    |_, _| Ok(()),
                )?)
                .turn;
                let items = (round.iter())
                    .map(|call| {
                        serde_json::to_vec(&json!({"type":"function_call","call_id":call.call_id,
                            "name":call.name,"arguments":call.arguments}))
                        .unwrap()
                        .into()
                    })
                    .collect();
                db.append(turn, items, &round, None)?;
                Ok((turn, db.inspect("Bob")?))
            })
            .await
            .unwrap();
        // The machine is loaded and one of the daemon's commands runs.
        let (_gate, readings) = watch::channel(false);
        let registry = (Registry::new("shell,read").unwrap())
            .with_process_budget(4)
            .with_load_readings(readings);
        let (started, running) = tokio::sync::oneshot::channel();
        let busy = tokio::spawn(registry.foreground(
            "sleep 30".into(),
            dir.clone(),
            60_000,
            Vec::new(),
            None,
            started,
        ));
        running.await.unwrap();
        assert!(!registry.admits());
        let provider = Provider::new(
            Transport::new(1, 1).unwrap(),
            Family::Responses,
            "http://127.0.0.1:9/v1",
            None,
        )
        .unwrap();
        let task = Turn {
            identity: 0,
            bot: "Bob".into(),
            turn,
            store: store.clone(),
            providers: Arc::default(),
            registry: registry.clone(),
            hub: Hub::default(),
            handles: Handles::new(mpsc::unbounded_channel().0),
            background_failures: mpsc::unbounded_channel().0,
            settings: Default::default(),
            rules: Default::default(),
            ruled: Default::default(),
            window: Default::default(),
            resumed: None,
            steers: Arc::default(),
            compact: Arc::default(),
            lift: Arc::default(),
            tokens: Arc::default(),
            read_results: Default::default(),
            side: Default::default(),
            stopping: Arc::new(AtomicBool::new(false)),
        };
        let policy = serde_json::from_value(json!({"prefix_rules": [
            {"pattern": ["rm"], "decision": "forbidden"},
            {"pattern": ["git", "push"], "decision": "prompt"},
        ]}))
        .unwrap();
        let _ = task
            .rules
            .set(Arc::new(agent_runtime::rules::Rules::new(policy)));
        let _ = task.settings.set(record.settings.clone());
        // A forbidden command starts nothing: it is refused at once,
        // however loaded the machine.
        let forbidden = ToolCall {
            custom: false,
            name: "shell".into(),
            call_id: "shell-2".into(),
            arguments: json!({"command": "rm -rf x"}).to_string(),
        };
        assert!(task.load_holds(&record, &calls[1], &dir));
        assert!(!task.load_holds(&record, &forbidden, &dir));
        // Nor is one a rule asks about with no gate to ask: it is refused.
        let asked = ToolCall {
            call_id: "shell-3".into(),
            arguments: json!({"command": "git push"}).to_string(),
            ..forbidden.clone()
        };
        assert!(!task.load_holds(&record, &asked, &dir));
        // The read first would run with the command beside it: the gate
        // holds both, neither started, and names the command it holds.
        let stop = tokio::time::timeout(
            Duration::from_secs(10),
            task.execute_calls(
                calls.clone(),
                (&provider, "synthetic"),
                None,
                &dir,
                &[],
                &record,
                None,
                None,
                None,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(stop, Some(Stop::Parked)));
        let (waiting, started) = store
            .op("parked", move |db| {
                let waiting = db.waiting_turns()?.into_iter().find(|w| w.turn == turn);
                let events = db.events("Bob", 0, 64)?;
                let started = (events["events"].as_array().unwrap().iter())
                    .filter(|e| e["event"] == "tool_started")
                    .count();
                Ok((waiting.unwrap(), started))
            })
            .await
            .unwrap();
        assert!(waiting.load);
        assert_eq!(waiting.call_id, "shell-1");
        assert_eq!(waiting.pending, calls);
        assert_eq!(started, 0);
        busy.abort();
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_ahead_batches_stop_at_either_bound() {
        let ids: Vec<i64> = (1..=200).collect();
        let small = vec![10u32; 200];
        let by_count = batches(&ids, &small, &[]);
        assert_eq!(
            by_count.iter().map(Vec::len).collect::<Vec<_>>(),
            [64, 64, 64, 8]
        );
        // Three 100 KiB items fill a batch; a 1 MiB item travels alone.
        let sizes = [100 << 10, 100 << 10, 100 << 10, 1 << 20, 10, 10];
        let by_bytes = batches(&ids[..6], &sizes, &[]);
        assert_eq!(by_bytes, [vec![1, 2], vec![3], vec![4], vec![5, 6]]);
        // A picture's bytes count as read, though it weighs little.
        let pictured = batches(&ids[..3], &[10, 10, 10], &[1 << 20, 0, 0]);
        assert_eq!(pictured, [vec![1], vec![2, 3]]);
        assert!(batches(&[], &[], &[]).is_empty());
    }

    /// A window of four 4000-byte items whose span is the first two, sent
    /// whole by the call; 1000 bytes of instructions and tools, 500 ahead
    /// of the window, a 200-byte request, and a request of its own of
    /// 100 + 8301 bytes.
    fn shape<'a>(family: agent_runtime::codec::Family, sizes: &'a [u32]) -> Shape<'a> {
        const IDS: [i64; 4] = [1, 2, 3, 4];
        const THINKING: [u32; 4] = [0; 4];
        Shape {
            family,
            ids: &IDS[..sizes.len()],
            sizes,
            thinking: &THINKING[..sizes.len()],
            strip: Strip::default(),
            span: 0..2,
            sent: sizes.len(),
            past: true,
            fixed: 1000,
            ahead: ContextUsage {
                bytes: 500,
                items: 1,
            },
            request: 200,
            own_fixed: 100,
            own_items: 300 + 8001,
            limit: ContextUsage {
                bytes: 1 << 20,
                items: 1000,
            },
        }
    }

    #[test]
    fn a_summary_copies_what_the_cache_reads_when_that_is_cheaper() {
        use agent_runtime::codec::Family::{Anthropic, Responses};
        let four = [4000; 4];
        let choice = |shape: Shape<'_>| {
            let choice = choose(&shape);
            (choice.copy, choice.copy_cost, choice.own_cost)
        };
        // The Responses cache reads any prefix of the call: through the
        // span, 9501 of 9701 bytes cached, costs 200 + 950.
        assert_eq!(
            choice(shape(Responses, &four)),
            (Some(2), Some(1150), Some(8401))
        );
        // Anthropic's reads where the call ended: the whole window, 200 +
        // 1750. Through the span only the instructions and tools are read.
        assert_eq!(
            choice(shape(Anthropic, &four)),
            (Some(4), Some(1950), Some(8401))
        );
        // A view an earlier call sent through its newest boundary is known
        // sent through the span: a long tail after it is sent uncached,
        // and the request of its own is cheaper; a short one is not.
        let inherited = |sizes| Shape {
            sent: 2,
            ..shape(Anthropic, sizes)
        };
        assert_eq!(choice(inherited(&four)), (None, Some(8801), Some(8401)));
        assert_eq!(
            choice(inherited(&[4000, 4000, 500])),
            (Some(3), Some(1651), Some(8401))
        );
        // A catch-up step's summary covers only its span.
        let step = Shape {
            past: false,
            ..shape(Anthropic, &four)
        };
        assert_eq!(choice(step), (None, Some(8801), Some(8401)));
        // A whole copy over the input limit is trimmed to the span, and
        // with neither fitting the summary is a request of its own.
        let limit = |family, bytes| Shape {
            limit: ContextUsage { bytes, items: 1000 },
            ..shape(family, &four)
        };
        assert_eq!(
            choice(limit(Responses, 10_000)),
            (Some(2), Some(1150), Some(8401))
        );
        assert_eq!(
            choice(limit(Anthropic, 10_000)),
            (None, Some(8801), Some(8401))
        );
        assert_eq!(choice(limit(Responses, 8000)), (None, None, Some(8401)));
        // Thinking a request strips counts out of either estimate: a copy
        // strips what the call did, a request of its own all of it.
        let thinking = [1000, 0, 0, 0];
        let stripped = Shape {
            thinking: &thinking,
            strip: Strip {
                below: 2,
                from: 0,
                to: 0,
            },
            ..shape(Anthropic, &four)
        };
        assert_eq!(choice(stripped), (Some(4), Some(1850), Some(7401)));
    }
}
