//! One turn: model rounds, committed tool plans, tool execution, and the
//! terminal event. Tool failures are results the model sees; only the runtime
//! failing ends a turn early.
use super::hub::Hub;
use agent_runtime::{
    Error, Result,
    codec::split_model,
    fail,
    provider::{Delta, Provider, Request as ModelRequest, ToolCall},
    store::Store,
    tools::{Outcome, Registry},
};
use serde_json::json;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::watch;

pub const MAX_ROUNDS: usize = 200;

pub struct Turn {
    pub bot: String,
    pub turn: i64,
    pub store: Store,
    pub providers: Arc<HashMap<String, Provider>>,
    pub registry: Registry,
    pub hub: Hub,
}

impl Turn {
    pub async fn execute(&self, mut cancelled: watch::Receiver<bool>) -> Result<()> {
        let result = tokio::select! {
            biased;
            _ = cancelled.changed() => fail("cancelled"),
            result = self.rounds() => result,
        };
        let error = result.err();
        let turn = self.turn;
        let entry = self
            .store
            .call(move |db| db.finish(turn, error.as_ref()))
            .await?;
        self.hub.durable(&self.bot, entry).await
    }

    async fn rounds(&self) -> Result<()> {
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
        for _ in 0..MAX_ROUNDS {
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
                return Ok(());
            }
            for call in response.calls {
                let started = call.clone();
                let entry = self
                    .store
                    .call(move |db| db.tool_start(turn, &started))
                    .await?;
                self.hub.durable(&self.bot, entry).await?;
                // A tool failure is a result the model can act on. Only the
                // scheduler closing is a runtime failure.
                let outcome = match self.registry.prepare(&call.name, &call.arguments) {
                    Ok(prepared) => match self.registry.execute(prepared, &workspace).await {
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
        }
        fail("tool_round_limit")
    }
}

fn failure(error: Error) -> Outcome {
    Outcome {
        output: json!({"error":error.code,"detail":error.detail}).to_string(),
        artifacts: Vec::new(),
    }
}
