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
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot, watch};

pub const MAX_ROUNDS: usize = 200;
/// Attempts per model call before its failure is the turn's failure, and the
/// wall-clock budget those attempts may span. A model call has no side
/// effects, so retrying one is always safe; a tool is never rerun.
const MAX_ATTEMPTS: u32 = 8;
/// Refusals for pace are spaced by the pool and are not the request's fault,
/// so they get many more attempts inside the same time budget.
const MAX_PACED_ATTEMPTS: u32 = 64;
const RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(300);
/// Items fetched from the store per read-ahead batch while a body streams.
const WINDOW_BATCH: usize = 64;

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
}

/// Lives outside the cancellable rounds future. No allocation or per-attempt
/// storage write: each execution segment flushes once, including when parked.
#[derive(Default)]
struct Accounting {
    retries: u64,
    paced_ms: u64,
    retrying: bool,
    report: Report,
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
}

pub enum Exit {
    Finished(Option<Error>),
    Parked,
}

pub struct Finished {
    pub entries: Vec<Value>,
    pub outcome: Value,
}

impl Finished {
    pub fn record(
        db: &mut agent_runtime::store::Database,
        bot: &str,
        turn: i64,
        error: Option<&Error>,
        keep: Option<usize>,
    ) -> Result<Self> {
        let entries = db.finish(turn, error)?;
        if let Some(keep) = keep {
            db.prune(bot, keep)?;
        }
        let outcome = db
            .turn_outcome(bot, turn)?
            .ok_or_else(|| Error::new("stale_turn"))?;
        Ok(Self { entries, outcome })
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
        let mut result = tokio::select! {
            biased;
            _ = interrupt => fail("cancelled"),
            result = self.rounds(&mut accounting) => result,
        };
        // The rounds future is gone, so cancellation cannot discard this flush.
        let (retries, paced_ms) = accounting.totals();
        if retries > 0 || paced_ms > 0 {
            let turn = self.turn;
            if let Err(error) = self
                .store
                .call(move |db| db.note_pacing(turn, retries, paced_ms))
                .await
            {
                result = Err(error);
            }
        }
        // An interrupt may arrive while the non-cancellable flush is pending.
        // Honor it before retiring a parked task whose receiver is still live.
        if result.is_ok() && *cancelled.borrow() {
            result = fail("cancelled");
        }
        let error = match result {
            Ok(Round::Parked) => return Exit::Parked,
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
            .call(move |db| db.window(&bot, bytes as i64, count as i64))
            .await?;
        let Some(Window {
            family,
            ids,
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
        let batches = ids
            .chunks(WINDOW_BATCH)
            .map(|chunk| chunk.to_vec())
            .collect::<Vec<_>>();
        let body = stream::iter([Ok(Bytes::from(head))]).chain(
            stream::iter(batches.into_iter().enumerate()).then(move |(index, chunk)| {
                let store = store.clone();
                async move {
                    let mut bytes = store
                        .call(move |db| db.items_by_ids(&chunk))
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
        let mut record = self.store.call(move |db| db.inspect(&bot)).await?;
        let context = self.store.call(move |db| db.context(turn)).await?;
        let (provider, model) = split_model(&context.model)?;
        let provider = self
            .providers
            .get(provider)
            .ok_or(Error::with("provider_unavailable", provider))?;
        let workspace = PathBuf::from(&context.workspace);
        if self.resume {
            let (waiting, entry) = match self.store.call(move |db| db.resume(turn)).await {
                Ok(resumed) => resumed,
                Err(error) if error.code == "turn_not_waiting" => return Ok(Round::Parked),
                Err(error) => return Err(error),
            };
            self.hub.durable(&self.bot, entry).await?;
            let outcome = Outcome::text(wait_result(self.handles.take(turn)).to_string());
            let id = waiting.call_id;
            let (_, entry) = self
                .store
                .call(move |db| db.tool_finish(turn, &id, &outcome))
                .await?;
            self.hub.durable(&self.bot, entry).await?;
            // Calls that followed the wait in the same model response.
            if self.execute_calls(waiting.pending, &workspace).await? {
                return Ok(Round::Parked);
            }
        }
        let mut model_rounds = context.model_rounds;
        while model_rounds < MAX_ROUNDS {
            // The budget is checked before each call, so one call may overshoot.
            if let Some(error) = budget_error(record.budget_tokens, record.tokens_used) {
                return Err(error);
            }
            let response = self
                .call(
                    provider,
                    model,
                    &mut record,
                    &mut model_rounds,
                    turn,
                    accounting,
                )
                .await?;
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
            let entries = self
                .store
                .call(move |db| db.append(turn, items, &calls, usage.as_ref()))
                .await;
            let entries = match entries {
                Ok(entries) => entries,
                Err(error) => {
                    self.failed_usage(response.usage.clone()).await?;
                    return Err(error);
                }
            };
            for entry in entries {
                self.hub.durable(&self.bot, entry).await?;
            }
            if response.calls.is_empty() {
                return Ok(Round::Finished);
            }
            if self.execute_calls(response.calls, &workspace).await? {
                return Ok(Round::Parked);
            }
        }
        fail("tool_round_limit")
    }

    /// One model call with retries. Each attempt rebuilds the request from
    /// the store, so nothing about the turn changes between attempts; a
    /// refusal for pace holds the provider's pool rather than this turn.
    async fn call(
        &self,
        provider: &Provider,
        model: &str,
        record: &mut agent_runtime::store::Bot,
        model_rounds: &mut usize,
        turn: i64,
        accounting: &mut Accounting,
    ) -> Result<agent_runtime::provider::Completion> {
        let started = std::time::Instant::now();
        let mut attempt = 0u32;
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
            let paced_ms = accounting.totals().1 - paced_before;
            let error = match result {
                Ok(completion) => return Ok(completion),
                Err(error) => error,
            };
            // Whatever the provider billed for a failed attempt is still spent.
            if let Some(usage) = &accounting.report.usage {
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
            let spent = started
                .elapsed()
                .saturating_sub(std::time::Duration::from_millis(paced_ms));
            if !retryable(&error.code) || attempt >= cap || spent >= RETRY_BUDGET {
                return Err(error);
            }
            // Rate limits already hold the pool; other transient failures
            // back off exponentially with a little spread.
            let delay = if paced {
                std::time::Duration::ZERO
            } else {
                backoff(attempt, turn as u64)
            };
            self.hub
                .live(
                    &self.bot,
                    json!({"event":"retry","bot":self.bot,"turn":turn,"durable":false,
                        "attempt":attempt,"error":error.code,"detail":error.detail,
                        "delay_ms":delay.as_millis() as u64}),
                )
                .await?;
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
    }

    async fn failed_usage(&self, usage: Option<agent_runtime::provider::Usage>) -> Result<()> {
        if let Some(usage) = usage {
            let turn = self.turn;
            let entry = self
                .store
                .call(move |db| db.failed_usage(turn, &usage))
                .await?;
            self.hub.durable(&self.bot, entry).await?;
        }
        Ok(())
    }

    /// Run planned calls in order. Returns true when a wait parked the turn;
    /// the calls after it are stored with the parked state.
    async fn execute_calls(
        &self,
        calls: Vec<ToolCall>,
        workspace: &std::path::Path,
    ) -> Result<bool> {
        let turn = self.turn;
        let mut calls = calls.into_iter();
        while let Some(call) = calls.next() {
            let started = call.clone();
            let entry = self
                .store
                .call(move |db| db.tool_start(turn, &started))
                .await?;
            self.hub.durable(&self.bot, entry).await?;
            // A tool failure is a result the model can act on. Only the
            // scheduler closing is a runtime failure.
            let outcome = match self.registry.prepare(&call.name, &call.arguments) {
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
                    self.background(&call.call_id, command, workspace.to_path_buf(), timeout_ms)
                        .await?
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
                        .call(move |db| {
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
                Ok(prepared) => match self.registry.execute(prepared, workspace).await {
                    Ok(outcome) => annotate(outcome, turn, &call.call_id),
                    Err(error) if error.code == "tool_scheduler_closed" => return Err(error),
                    Err(error) => failure(error),
                },
                Err(error) => failure(error),
            };
            let id = call.call_id;
            let (_, entry) = self
                .store
                .call(move |db| db.tool_finish(turn, &id, &outcome))
                .await?;
            self.hub.durable(&self.bot, entry).await?;
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
            .call(move |db| {
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
        let entry = self
            .store
            .call(move |db| db.suspend(turn, &id, &list, deadline_ms, any, &pending))
            .await?;
        self.hub.durable(&self.bot, entry).await?;
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
    ) -> Result<Outcome> {
        let (turn, started) = (self.turn, call_id.to_owned());
        let id = self
            .store
            .call(move |db| db.process_start(turn, &started))
            .await?;
        let (sender, receiver) = oneshot::channel();
        self.registry
            .background(command, workspace, timeout_ms, sender);
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
                .call(move |db| db.process_finish(id, &recorded, &artifacts))
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
