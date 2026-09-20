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
    provider::{Delta, Items, Provider, Report, Request as ModelRequest, ToolCall},
    store::{Store, Window},
    tools::{Outcome, Prepared, ReadSource, Registry},
};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use serde_json::json;
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
/// Read-ahead while a body streams: a batch stops at either bound, so the
/// memory held per in-flight request is a number, not a function of item
/// sizes. An item larger than the byte bound travels alone.
const WINDOW_BATCH: usize = 64;
const WINDOW_BATCH_BYTES: u64 = 256 * 1024;

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
            // Commit the park and its accounting together, outside cancellation.
            self.store
                .op("suspend_paced", move |db| {
                    db.suspend_paced(turn, at, attempts, spent, retries, paced_ms)
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

    /// The bot's current context window as a streamed request body: a note
    /// about omitted turns, then the window's items in store-read batches.
    async fn items(&self) -> Result<Items> {
        let (bot, bytes, count) = (self.bot.clone(), self.context_bytes, self.context_items);
        let window = self
            .store
            .op("window", move |db| {
                db.window(&bot, bytes as i64, count as i64)
            })
            .await?;
        let Some(Window {
            family,
            ids,
            sizes,
            item_bytes,
            omitted_items,
            omitted_turns,
        }) = window
        else {
            return Ok(Items::empty());
        };
        let mut total = item_bytes as usize + ids.len().saturating_sub(1);
        let mut head = Vec::new();
        if omitted_items > 0 {
            // Omission is explicit: the model is told what is missing and how
            // to read it. This note is part of the request, never the store.
            let note = format!(
                "[context note] {omitted_turns} earlier turn(s) with {omitted_items} messages are not shown. \
                 Use the history tool with a turn number from 1 to {omitted_turns} to read any of them."
            );
            head = family.user_item(&note)?;
            if !ids.is_empty() {
                head.push(b',');
            }
            total += head.len();
        }
        let store = self.store.clone();
        let batches = batches(&ids, &sizes);
        let body = stream::iter([Ok(Bytes::from(head))]).chain(
            stream::iter(batches.into_iter().enumerate()).then(move |(index, chunk)| {
                let store = store.clone();
                async move {
                    let mut bytes = store
                        .read("items_by_ids", move |db| db.items_by_ids(&chunk))
                        .await
                        .map_err(|error| std::io::Error::other(error.code))?;
                    if index != 0 {
                        bytes.insert(0, b',');
                    }
                    Ok(Bytes::from(bytes))
                }
            }),
        );
        Ok(Items {
            bytes: total,
            stream: body.boxed(),
        })
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
        // needs a model for the peer, and the natural default is its own.
        // What this turn's children inherit: the CLI a bot runs to delegate
        // needs a model for the peer (its own by default), the bot's own name
        // so a created peer records who created it, and its creator so a peer
        // can address the bot that spawned it.
        let mut environment = vec![
            ("AGENT_MODEL".to_owned(), context.model.clone()),
            ("AGENT_BOT".to_owned(), context.bot.clone()),
        ];
        if let Some(parent) = &context.created_by {
            environment.push(("AGENT_PARENT".to_owned(), parent.clone()));
        }
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
            } else {
                let outcome = Outcome::text(wait_result(self.handles.take(turn)).to_string());
                let id = waiting.call_id;
                self.store
                    .op("tool_finish", move |db| db.tool_finish(turn, &id, &outcome))
                    .await?;
                // Calls that followed the wait in the same model response.
                if self
                    .execute_calls(waiting.pending, &workspace, &environment, &record.tools)
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
            let Some(response) = self
                .call(
                    provider,
                    model,
                    &tools,
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
            if let Err(error) = self
                .store
                .op("append", move |db| {
                    db.append(turn, items, &calls, usage.as_ref())
                })
                .await
            {
                self.failed_usage(response.usage.clone()).await?;
                return Err(error);
            }
            if response.calls.is_empty() {
                // A steer that arrived during the final call keeps the turn
                // going for one more round rather than ending it unheard.
                if self.absorb().await? {
                    continue;
                }
                return Ok(Round::Finished);
            }
            if self
                .execute_calls(response.calls, &workspace, &environment, &record.tools)
                .await?
            {
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

    /// One model call with retries. Each attempt rebuilds the request from
    /// the store, so nothing about the turn changes between attempts; a
    /// refusal for pace holds the provider's pool rather than this turn.
    #[allow(clippy::too_many_arguments)]
    async fn call(
        &self,
        provider: &Provider,
        model: &str,
        tools: &serde_json::value::RawValue,
        record: &mut agent_runtime::store::Bot,
        model_rounds: &mut usize,
        turn: i64,
        accounting: &mut Accounting,
    ) -> Result<Option<agent_runtime::provider::Completion>> {
        let started = std::time::Instant::now();
        let mut attempt = std::mem::take(&mut accounting.call_attempts);
        let prior_spent =
            std::time::Duration::from_millis(std::mem::take(&mut accounting.call_spent_ms));
        let paced_before = accounting.totals().1;
        loop {
            let items = self.items().await?;
            accounting.begin(attempt > 0);
            let result = provider
                .complete_accounted(
                    ModelRequest {
                        model,
                        instructions: &record.instructions,
                        reasoning: record.reasoning.as_deref(),
                        tools,
                        items,
                    },
                    |delta| {
                        let (kind, text) = match delta {
                            Delta::Text(text) => ("text_delta", text),
                            Delta::Thinking(text) => ("thinking_delta", text),
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
            self.failed_usage(accounting.report.usage.take()).await?;
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

    async fn failed_usage(&self, usage: Option<agent_runtime::provider::Usage>) -> Result<()> {
        if let Some(usage) = usage {
            let turn = self.turn;
            self.store
                .op("failed_usage", move |db| db.failed_usage(turn, &usage))
                .await?;
        }
        Ok(())
    }

    /// Run planned calls in order. Returns true when a wait parked the turn;
    /// the calls after it are stored with the parked state.
    async fn execute_calls(
        &self,
        calls: Vec<ToolCall>,
        workspace: &std::path::Path,
        environment: &[(String, String)],
        allowed: &[String],
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
                Ok(prepared) => match self
                    .registry
                    .execute(prepared, workspace, environment)
                    .await
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
        let (bot, turn, bytes, items) = (
            self.bot.clone(),
            self.turn,
            self.context_bytes,
            self.context_items,
        );
        let (family, budget, mut page) = self
            .store
            .op("turn_usage", move |db| {
                let (family, used, count) = db.turn_usage(&bot, turn)?;
                // Reserve half the remaining bytes for subsequent model/tool work.
                // Account against this turn only: older turns can leave the window.
                let budget = bytes.saturating_sub(used) / 2;
                if count >= items || budget < 256 {
                    return fail("history_context_exhausted");
                }
                Ok((
                    family,
                    budget,
                    db.history_read(&bot, wanted, offset, limit.min(budget))?,
                ))
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
            | "truncated_sse_frame"
            | "provider_admission_timeout"
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

fn failure(error: Error) -> Outcome {
    Outcome {
        output: json!({"error":error.code,"detail":error.detail}).to_string(),
        artifacts: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
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
