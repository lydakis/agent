//! One turn: model rounds, committed tool plans, tool execution, and the
//! terminal event. Tool failures are results the model sees; only the runtime
//! failing ends a turn early. A `wait` parks the turn: the task ends, the
//! store holds the state (including any calls still to run), and a resumed
//! task records the results and carries on.
use super::{
    handles::{Completion, Handle, Handles, Waiter, now_ms, wait_result},
    hub::Hub,
};
use agent_runtime::{
    Error, Result,
    codec::split_model,
    fail,
    history::History,
    provider::{Delta, Provider, Request as ModelRequest, ToolCall},
    store::Store,
    tools::{Outcome, Prepared, Registry},
};
use serde_json::json;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot, watch};

pub const MAX_ROUNDS: usize = 200;

pub struct Turn {
    pub bot: String,
    pub turn: i64,
    pub store: Store,
    pub providers: Arc<HashMap<String, Provider>>,
    pub registry: Registry,
    pub hub: Hub,
    pub handles: Handles,
    pub background_failures: mpsc::UnboundedSender<Error>,
    /// Continue a parked turn: record its wait results, then keep going.
    pub resume: bool,
}

enum Round {
    Finished,
    /// The turn is parked or was ended elsewhere; nothing to finish here.
    Parked,
}

impl Turn {
    pub async fn execute(&self, mut cancelled: watch::Receiver<bool>) -> Result<()> {
        // Only an explicit interrupt cancels. A dropped sender (the service
        // replacing this task's slot) must not end the turn.
        let interrupt = async {
            if cancelled.wait_for(|c| *c).await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        let result = tokio::select! {
            biased;
            _ = interrupt => fail("cancelled"),
            result = self.rounds() => result,
        };
        let error = match result {
            Ok(Round::Parked) => return Ok(()),
            Ok(Round::Finished) => None,
            Err(error) => {
                self.handles.forget(Waiter::Turn(self.turn));
                Some(error)
            }
        };
        let turn = self.turn;
        let entries = self
            .store
            .call(move |db| db.finish(turn, error.as_ref()))
            .await?;
        for entry in entries {
            self.hub.durable(&self.bot, entry).await?;
        }
        let bot = self.bot.clone();
        if let Some(outcome) = self
            .store
            .call(move |db| db.turn_outcome(&bot, turn))
            .await?
        {
            self.handles.turn_finished(&self.bot, turn, outcome);
        }
        Ok(())
    }

    async fn rounds(&self) -> Result<Round> {
        let (bot, turn) = (self.bot.clone(), self.turn);
        let record = self.store.call(move |db| db.inspect(&bot)).await?;
        let context = self.store.call(move |db| db.context(turn)).await?;
        let (provider, model) = split_model(&context.model)?;
        let provider = self
            .providers
            .get(provider)
            .ok_or(Error::with("provider_unavailable", provider))?;
        let workspace = PathBuf::from(&context.workspace);
        let name = self.bot.clone();
        let mut history = self.store.call(move |db| db.load(&name)).await?;
        if self.resume {
            let (waiting, entry) = match self.store.call(move |db| db.resume(turn)).await {
                Ok(resumed) => resumed,
                Err(error) if error.code == "turn_not_waiting" => return Ok(Round::Parked),
                Err(error) => return Err(error),
            };
            self.hub.durable(&self.bot, entry).await?;
            let outcome = Outcome::text(wait_result(self.handles.take(turn)).to_string());
            let id = waiting.call_id;
            let (item, entry) = self
                .store
                .call(move |db| db.tool_finish(turn, &id, &outcome))
                .await?;
            self.hub.durable(&self.bot, entry).await?;
            history.append(item)?;
            // Calls that followed the wait in the same model response.
            if self
                .execute_calls(waiting.pending, &workspace, &mut history)
                .await?
            {
                return Ok(Round::Parked);
            }
        }
        for _ in context.model_rounds..MAX_ROUNDS {
            let response = provider
                .complete(
                    ModelRequest {
                        model,
                        instructions: &record.instructions,
                        reasoning: record.reasoning.as_deref(),
                        history: &history,
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
                )
                .await?;
            let items = response.items.clone();
            let calls: Vec<ToolCall> = response.calls.clone();
            let usage = response.usage.clone();
            let entries = self
                .store
                .call(move |db| db.append(turn, items, &calls, usage.as_ref()))
                .await?;
            for entry in entries {
                self.hub.durable(&self.bot, entry).await?;
            }
            for item in response.items {
                history.append(item)?;
            }
            if response.calls.is_empty() {
                return Ok(Round::Finished);
            }
            if self
                .execute_calls(response.calls, &workspace, &mut history)
                .await?
            {
                return Ok(Round::Parked);
            }
        }
        fail("tool_round_limit")
    }

    /// Run planned calls in order. Returns true when a wait parked the turn;
    /// the calls after it are stored with the parked state.
    async fn execute_calls(
        &self,
        calls: Vec<ToolCall>,
        workspace: &std::path::Path,
        history: &mut History,
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
                }) => {
                    match self
                        .park(&call.call_id, handles, timeout_ms, &mut calls)
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
                Ok(prepared) => match self.registry.execute(prepared, workspace).await {
                    Ok(outcome) => outcome,
                    Err(error) if error.code == "tool_scheduler_closed" => return Err(error),
                    Err(error) => failure(error),
                },
                Err(error) => failure(error),
            };
            let id = call.call_id;
            let (item, entry) = self
                .store
                .call(move |db| db.tool_finish(turn, &id, &outcome))
                .await?;
            self.hub.durable(&self.bot, entry).await?;
            history.append(item)?;
        }
        Ok(false)
    }

    /// Make the turn durable as waiting, then register its handles. Returns
    /// an immediate result only for arguments that can never resolve.
    async fn park(
        &self,
        call_id: &str,
        handles: Vec<String>,
        timeout_ms: Option<u64>,
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
            .call(move |db| db.suspend(turn, &id, &list, deadline_ms, &pending))
            .await?;
        self.hub.durable(&self.bot, entry).await?;
        self.handles
            .attach(
                &self.store,
                Waiter::Turn(self.turn),
                &handles,
                deadline_ms,
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
        let (store, handles, failures) = (
            self.store.clone(),
            self.handles.clone(),
            self.background_failures.clone(),
        );
        tokio::spawn(async move {
            let result = receiver.await.unwrap_or_else(|_| fail("process_lost"));
            let (value, artifacts) = match result {
                Ok(outcome) => (
                    serde_json::from_str(&outcome.output)
                        .unwrap_or(json!({"output":outcome.output})),
                    outcome.artifacts,
                ),
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

fn failure(error: Error) -> Outcome {
    Outcome {
        output: json!({"error":error.code,"detail":error.detail}).to_string(),
        artifacts: Vec::new(),
    }
}
