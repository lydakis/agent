use agent_runtime::{
    Error, Result, fail,
    output::Output,
    provider::{Provider, ToolCall},
    store::Store,
    tools::Registry,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{BufRead, Read},
    path::{Path, PathBuf},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};

#[derive(Deserialize)]
struct Request {
    id: Value,
    #[serde(flatten)]
    command: Command,
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Create {
        bot: String,
        workspace: String,
    },
    Resume {
        bot: String,
    },
    Fork {
        source: String,
        checkpoint: i64,
        bot: String,
        workspace: String,
    },
    Submit {
        bot: String,
        request_id: String,
        prompt: String,
    },
    Interrupt {
        bot: String,
        turn: i64,
    },
    Events {
        bot: String,
        after: i64,
        limit: usize,
    },
    Item {
        bot: String,
        node: i64,
    },
    Shutdown,
}

pub struct Configuration {
    pub store: PathBuf,
    pub base_url: String,
    pub model: String,
    pub key_env: Option<String>,
    pub tools: String,
}

fn name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return fail("invalid_name");
    }
    Ok(())
}
fn workspace(path: &str) -> Result<String> {
    if !Path::new(path).is_absolute() || !Path::new(path).is_dir() {
        return fail("workspace_must_exist_and_be_absolute");
    }
    Ok(std::fs::canonicalize(path)?
        .to_str()
        .ok_or(Error("workspace_not_utf8".into()))?
        .into())
}

pub async fn run(config: Configuration, output: Output) -> Result<()> {
    let key = config
        .key_env
        .as_ref()
        .map(|key| std::env::var(key).map_err(|_| Error("provider_key_unavailable".into())))
        .transpose()?;
    let mut registry = Registry::new(&config.tools)?;
    if let (Some(name), Some(value)) = (&config.key_env, &key) {
        registry = registry.exclude_credential(name, value);
    }
    let tools = registry.schemas();
    let instructions = "Complete the requested task using the available tools.";
    let provider = Provider::new(
        &config.base_url,
        &config.model,
        instructions,
        tools.clone(),
        key,
    )?;
    let binding = json!({"schema":1,"base_url":config.base_url,"model":config.model,"instructions":instructions,"tools":tools}).to_string();
    let store = Store::open(&config.store, binding).await?;
    let (sender, mut input) = mpsc::channel::<Result<Request>>(8);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        loop {
            let mut line = Vec::new();
            let result = (&mut reader)
                .take(1024 * 1024 + 1)
                .read_until(b'\n', &mut line);
            match result {
                Ok(0) => break,
                Ok(_) if line.len() <= 1024 * 1024 && line.ends_with(b"\n") => {
                    let request = serde_json::from_slice(&line).map_err(Error::from);
                    if sender.blocking_send(request).is_err() {
                        break;
                    }
                }
                _ => {
                    let _ = sender.blocking_send(fail("input_line_limit_or_truncation"));
                    break;
                }
            }
        }
    });
    output.send(json!({"event":"ready","protocol":1,"capabilities":["create","resume","fork_completed_checkpoint",
        "submit","interrupt","events","item"],"tools":registry.names(),"durability":"sqlite_full","partial_text_durable":false})).await?;
    let mut jobs = JoinSet::new();
    let mut active: HashMap<String, (i64, watch::Sender<bool>)> = HashMap::new();
    loop {
        tokio::select! {
            joined = jobs.join_next(), if !jobs.is_empty() => {
                let (bot, turn, result) = joined.unwrap().map_err(|_| Error("turn_task_failed".into()))?;
                if active.get(&bot).is_some_and(|(id,_)| *id == turn) { active.remove(&bot); }
                result?;
            }
            request = input.recv() => {
                let Some(request) = request else { break };
                let request = match request {
                    Ok(request) => request,
                    Err(error) => { output.send(json!({"id":null,"error":error.0})).await?; continue; }
                };
                if !request.id.is_u64() && request.id.as_str().is_none_or(|id| id.len() > 128) {
                    output.send(json!({"id":null,"error":"invalid_request_id"})).await?; continue;
                }
                let shutting_down = matches!(request.command, Command::Shutdown);
                let result = dispatch(request.command, &store, (&provider, &registry), &output, &mut active, &mut jobs).await;
                output.respond(request.id, result).await?;
                if shutting_down { break; }
            }
        }
    }
    for (_, cancel) in active.values() {
        let _ = cancel.send(true);
    }
    while let Some(result) = jobs.join_next().await {
        result.map_err(|_| Error("turn_task_failed".into()))?.2?;
    }
    Ok(())
}

type Jobs = JoinSet<(String, i64, Result<()>)>;
async fn dispatch(
    command: Command,
    store: &Store,
    execution: (&Provider, &Registry),
    output: &Output,
    active: &mut HashMap<String, (i64, watch::Sender<bool>)>,
    jobs: &mut Jobs,
) -> Result<Value> {
    let (provider, registry) = execution;
    match command {
        Command::Create {
            bot,
            workspace: path,
        } => {
            name(&bot)?;
            let path = workspace(&path)?;
            store
                .call(move |db| Ok(serde_json::to_value(db.create(&bot, &path)?)?))
                .await
        }
        Command::Resume { bot } => {
            store
                .call(move |db| Ok(serde_json::to_value(db.inspect(&bot)?)?))
                .await
        }
        Command::Fork {
            source,
            checkpoint,
            bot,
            workspace: path,
        } => {
            name(&bot)?;
            let path = workspace(&path)?;
            store
                .call(move |db| {
                    Ok(serde_json::to_value(
                        db.fork(&source, checkpoint, &bot, &path)?,
                    )?)
                })
                .await
        }
        Command::Events { bot, after, limit } => {
            store.call(move |db| db.events(&bot, after, limit)).await
        }
        Command::Item { bot, node } => store.call(move |db| db.item(&bot, node)).await,
        Command::Submit {
            bot,
            request_id,
            prompt,
        } => {
            name(&request_id)?;
            if prompt.len() > 256 * 1024 {
                return fail("prompt_limit");
            }
            let capacity = active.len() < 1024;
            let (b, r) = (bot.clone(), request_id.clone());
            let started = store
                .call(move |db| db.begin(&b, &r, &prompt, capacity))
                .await?;
            if started.fresh {
                let (cancel, cancelled) = watch::channel(false);
                active.insert(bot.clone(), (started.turn, cancel));
                let (store, provider, output) = (store.clone(), provider.clone(), output.clone());
                let registry = registry.clone();
                let task_bot = bot.clone();
                let turn = started.turn;
                jobs.spawn(async move {
                    let result = execute(
                        task_bot.clone(),
                        turn,
                        store,
                        provider,
                        output,
                        cancelled,
                        registry,
                    )
                    .await;
                    (task_bot, turn, result)
                });
            }
            Ok(
                json!({"bot":bot,"turn":started.turn,"request_id":request_id,"duplicate":!started.fresh}),
            )
        }
        Command::Interrupt { bot, turn } => {
            let Some((running, cancel)) = active.get(&bot) else {
                return fail("no_active_turn");
            };
            if *running != turn {
                return fail("stale_turn");
            }
            cancel
                .send(true)
                .map_err(|_| Error("no_active_turn".into()))?;
            Ok(json!({"interrupt_requested":true,"turn":turn}))
        }
        Command::Shutdown => Ok(json!({"shutting_down":true})),
    }
}

async fn execute(
    bot: String,
    turn: i64,
    store: Store,
    provider: Provider,
    output: Output,
    mut cancelled: watch::Receiver<bool>,
    registry: Registry,
) -> Result<()> {
    let result = tokio::select! {
        biased;
        _ = cancelled.changed() => fail("cancelled"),
        result = execute_inner(&bot,turn,&store,&provider,&output,&registry) => result,
    };
    let error = result.err().map(|e| e.0);
    let event = store
        .call(move |db| db.finish(turn, error.as_deref()))
        .await?;
    output.send(event).await
}

async fn execute_inner(
    bot: &str,
    turn: i64,
    store: &Store,
    provider: &Provider,
    output: &Output,
    registry: &Registry,
) -> Result<()> {
    let workspace_bot = bot.to_owned();
    let workspace = store
        .call(move |db| Ok(PathBuf::from(db.inspect(&workspace_bot)?.workspace)))
        .await?;
    let name = bot.to_owned();
    let mut history = store.call(move |db| db.load(&name)).await?;
    for _ in 0..8 {
        let response = provider.complete(&history, |text| {
            let output = output.clone();
            let bot = bot.to_owned();
            async move { output.send(json!({"event":"text_delta","bot":bot,"turn":turn,"durable":false,"text":text})).await }
        }).await?;
        // Capability validation happens before committing tool plans.
        let prepared = response
            .calls
            .iter()
            .map(|call| registry.prepare(&call.name, &call.arguments))
            .collect::<Result<Vec<_>>>()?;
        let items = response.items.clone();
        let calls: Vec<ToolCall> = response
            .calls
            .iter()
            .map(|c| ToolCall {
                name: c.name.clone(),
                call_id: c.call_id.clone(),
                arguments: c.arguments.clone(),
            })
            .collect();
        store.call(move |db| db.append(turn, items, &calls)).await?;
        for item in response.items {
            history.append(item)?;
        }
        if response.calls.is_empty() {
            return Ok(());
        }
        for (call, prepared) in response.calls.into_iter().zip(prepared) {
            let (id, name) = (call.call_id.clone(), call.name.clone());
            store
                .call(move |db| db.tool_start(turn, &id, &name))
                .await?;
            output.send(json!({"event":"tool_started","bot":bot,"turn":turn,"call_id":call.call_id,"name":call.name})).await?;
            let result = registry.execute(prepared, &workspace).await?;
            let id = call.call_id;
            let item = store
                .call(move |db| db.tool_finish(turn, &id, &result))
                .await?;
            history.append(item)?;
        }
    }
    fail("tool_round_limit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn saturated_dispatch_reconciles_duplicates_but_rejects_new_work() {
        let dir = std::env::temp_dir().join(format!("agent-admission-test-{}", std::process::id()));
        let store = Store::open(&dir.join("state.sqlite"), "test".into())
            .await
            .unwrap();
        let turn = store
            .call(|db| {
                db.create("Bob", "/synthetic")?;
                db.create("Other", "/synthetic")?;
                Ok(db.begin("Bob", "same", "work", true)?.turn)
            })
            .await
            .unwrap();
        let registry = Registry::new("echo").unwrap();
        let provider = Provider::new(
            "http://127.0.0.1:1/v1",
            "synthetic-model",
            "test",
            registry.schemas(),
            None,
        )
        .unwrap();
        let (output, writer) = Output::stdout();
        let mut active = (0..1024)
            .map(|index| (index.to_string(), (turn, watch::channel(false).0)))
            .collect();
        let mut jobs = JoinSet::new();
        let duplicate = dispatch(
            Command::Submit {
                bot: "Bob".into(),
                request_id: "same".into(),
                prompt: "work".into(),
            },
            &store,
            (&provider, &registry),
            &output,
            &mut active,
            &mut jobs,
        )
        .await
        .unwrap();
        assert_eq!(duplicate["turn"], turn);
        assert_eq!(duplicate["duplicate"], true);
        let error = dispatch(
            Command::Submit {
                bot: "Other".into(),
                request_id: "new".into(),
                prompt: "work".into(),
            },
            &store,
            (&provider, &registry),
            &output,
            &mut active,
            &mut jobs,
        )
        .await
        .unwrap_err();
        assert_eq!(error.0, "active_agent_limit");
        assert!(jobs.is_empty());
        assert!(
            store
                .call(|db| Ok(db.inspect("Other")?.head.is_none()))
                .await
                .unwrap()
        );
        drop(output);
        writer.join().unwrap().unwrap();
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
