//! One process hosts every bot. Client sessions (stdio or Unix socket) speak
//! the same JSONL protocol; turns run as tasks; events fan out through a hub.
mod hub;
mod session;
mod socket;
mod turn;

use agent_runtime::{
    Error, Result,
    codec::{Family, split_model},
    fail, fail_with,
    output::Output,
    provider::{Provider, Transport},
    store::{Binding, Store, TurnOptions},
    tools::Registry,
};
use hub::{Hub, replay};
use serde::Deserialize;
use serde_json::{Value, json};
use session::{Inbound, socket_reader, stdio_reader};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::BufReader,
    sync::{mpsc, watch},
    task::JoinSet,
};
use turn::Turn;
const DEFAULT_INSTRUCTIONS: &str = "You are a software engineering agent working in the current workspace. \
Complete the requested task using the available tools, verify your work, and finish with a short summary. \
To delegate a subtask to another agent with its own conversation, run \
\"$AGENT_BIN\" run --detach --new --bot NAME -- TASK from the shell; it returns a durable turn handle immediately. \
Continue an existing agent with \"$AGENT_BIN\" run --detach --bot NAME -- TASK. \
Use \"$AGENT_BIN\" ls to inspect peer status. Blocking run/follow inside a shell tool is rejected. \
Use \"$AGENT_BIN\" fork --source NAME --checkpoint N --bot NEW to branch an earlier checkpoint.";

#[derive(Deserialize)]
pub struct Request {
    id: Value,
    #[serde(flatten)]
    command: Command,
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Create {
        bot: String,
        workspace: Option<String>,
        model: Option<String>,
        instructions: Option<String>,
        reasoning: Option<String>,
    },
    Resume {
        bot: String,
    },
    Fork {
        source: String,
        checkpoint: i64,
        bot: String,
        workspace: Option<String>,
    },
    Submit {
        bot: String,
        request_id: String,
        prompt: String,
        workspace: Option<String>,
        model: Option<String>,
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
    Artifact {
        bot: String,
        turn: i64,
        call_id: String,
        stream: Option<String>,
        #[serde(default)]
        offset: u64,
        limit: Option<usize>,
    },
    Follow {
        bot: String,
        #[serde(default)]
        after: i64,
    },
    Unfollow {
        bot: String,
    },
    Bots {
        after: Option<String>,
        limit: Option<usize>,
    },
    Shutdown,
}
pub struct ProviderSpec {
    pub name: String,
    pub family: Family,
    pub url: String,
    pub key_env: Option<String>,
}
impl ProviderSpec {
    /// `NAME[=FAMILY[,URL[,KEY_ENV]]]`. Known names have defaults; the key
    /// variable is read only when named here or implied by a default endpoint.
    pub fn parse(spec: &str) -> Result<Self> {
        let (name, rest) = spec.split_once('=').unwrap_or((spec, ""));
        let mut fields = rest.split(',').filter(|s| !s.is_empty());
        let (family, url, key) = fields.next().map(|f| f.to_owned()).map_or_else(
            || (None, None, None),
            |f| {
                (
                    Some(f),
                    fields.next().map(str::to_owned),
                    fields.next().map(str::to_owned),
                )
            },
        );
        let (default_family, default_url, default_key) = match name {
            "openai" => (
                "responses",
                "https://api.openai.com/v1",
                Some("OPENAI_API_KEY"),
            ),
            "anthropic" => (
                "anthropic",
                "https://api.anthropic.com/v1",
                Some("ANTHROPIC_API_KEY"),
            ),
            "openrouter" => (
                "responses",
                "https://openrouter.ai/api/v1",
                Some("OPENROUTER_API_KEY"),
            ),
            _ => ("", "", None),
        };
        let family = family.as_deref().unwrap_or(default_family);
        let family = Family::parse(family).ok_or(Error::with("invalid_provider_spec", spec))?;
        let url = match url {
            Some(url) => url,
            None if !default_url.is_empty() => default_url.to_owned(),
            None => return fail_with("invalid_provider_spec", spec),
        };
        let key_env = match key {
            Some(key) => Some(key),
            None if url == default_url => default_key.map(str::to_owned),
            None => None,
        };
        if name.is_empty() || name.len() > 64 || split_model(&format!("{name}/x")).is_err() {
            return fail_with("invalid_provider_spec", spec);
        }
        Ok(Self {
            name: name.to_owned(),
            family,
            url,
            key_env,
        })
    }
}

pub struct Configuration {
    pub store: PathBuf,
    pub socket: Option<PathBuf>,
    pub providers: Vec<ProviderSpec>,
    pub model: Option<String>,
    pub instructions: Option<String>,
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
        .ok_or(Error::new("workspace_not_utf8"))?
        .into())
}
struct Service {
    store: Store,
    providers: Arc<HashMap<String, Provider>>,
    registry: Registry,
    hub: Hub,
    default_model: Option<String>,
    default_instructions: String,
    active: HashMap<String, (i64, watch::Sender<bool>)>,
    jobs: JoinSet<(String, i64, Result<()>)>,
    replays: JoinSet<()>,
}

pub async fn run(config: Configuration) -> Result<()> {
    let transport = Transport::new()?;
    let registry = Registry::new(&config.tools)?;
    let schemas = registry.schemas();
    let mut providers = HashMap::new();
    let mut credentials = Vec::new();
    let mut binding = serde_json::Map::new();
    for spec in &config.providers {
        let key = spec
            .key_env
            .as_ref()
            .map(|env| {
                std::env::var(env)
                    .map_err(|_| Error::with("provider_key_unavailable", env.as_str()))
            })
            .transpose()?;
        if let (Some(env), Some(value)) = (&spec.key_env, &key) {
            credentials.push((env.clone(), value.clone()));
        }
        if providers
            .insert(
                spec.name.clone(),
                Provider::new(transport.clone(), spec.family, &spec.url, key, &schemas)?,
            )
            .is_some()
        {
            return fail_with("duplicate_provider", spec.name.as_str());
        }
        binding.insert(
            spec.name.clone(),
            json!({"family":spec.family.name(),"url":spec.url}),
        );
    }
    if providers.is_empty() {
        return fail("no_providers");
    }
    if let Some(model) = &config.model {
        let (provider, _) = split_model(model)?;
        if !providers.contains_key(provider) {
            return fail_with("provider_unavailable", provider);
        }
    }
    let binding = json!({"schema":3,"providers":binding,"tools":registry.names()}).to_string();
    let store = Store::open(&config.store, binding).await?;
    let mut environment = vec![
        (
            "AGENT_BIN".into(),
            std::env::current_exe()?.to_string_lossy().into_owned(),
        ),
        (
            "AGENT_STORE".into(),
            std::fs::canonicalize(&config.store)?
                .to_string_lossy()
                .into_owned(),
        ),
        ("AGENT_SHELL_CONTEXT".into(), "1".into()),
    ];
    if let Some(path) = &config.socket {
        environment.push((
            "AGENT_SOCKET".into(),
            std::path::absolute(path)?.to_string_lossy().into_owned(),
        ));
    }
    let registry = registry
        .exclude_credentials(credentials)
        .with_environment(environment);
    let hub = Hub::default();
    let mut provider_names: Vec<&String> = providers.keys().collect();
    provider_names.sort();
    let ready = json!({"event":"ready","protocol":3,
        "capabilities":["create","resume","fork_completed_checkpoint","submit","interrupt","events","item","artifact","follow","bots"],
        "tools":registry.names(),"providers":provider_names,"default_model":config.model,
        "durability":"sqlite_full","partial_text_durable":false});
    let (sender, mut inbound) = mpsc::channel::<Inbound>(64);
    let mut sessions: HashMap<u64, Output> = HashMap::new();
    let (stdout, stdout_writer) = Output::stdout();
    let stdio_owner = config.socket.is_none();
    let mut socket_owner = None;
    if let Some(socket) = &config.socket {
        let (owner, listener) = socket::Owner::bind(socket).await?;
        socket_owner = Some(owner);
        let accept_sender = sender.clone();
        tokio::spawn(async move {
            let mut next = 1u64;
            while let Ok((stream, _)) = listener.accept().await {
                let (read, write) = stream.into_split();
                let output = Output::writer(write);
                if accept_sender
                    .send(Inbound::Open(next, output))
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::spawn(socket_reader(
                    next,
                    BufReader::new(read),
                    accept_sender.clone(),
                ));
                next += 1;
            }
        });
        stdout.send(ready.clone()).await?;
    } else {
        sessions.insert(0, stdout.clone());
        hub.add_firehose(0, stdout.clone());
        stdio_reader(sender.clone());
        stdout.send(ready.clone()).await?;
    }
    drop(stdout);
    drop(sender);
    let mut service = Service {
        store,
        providers: Arc::new(providers),
        registry,
        hub,
        default_model: config.model.clone(),
        default_instructions: config
            .instructions
            .clone()
            .unwrap_or_else(|| DEFAULT_INSTRUCTIONS.to_owned()),
        active: HashMap::new(),
        jobs: JoinSet::new(),
        replays: JoinSet::new(),
    };
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    loop {
        tokio::select! {
            _ = service.replays.join_next(), if !service.replays.is_empty() => {}
            joined = service.jobs.join_next(), if !service.jobs.is_empty() => {
                let (bot, turn, result) = joined.unwrap().map_err(|_| Error::new("turn_task_failed"))?;
                if service.active.get(&bot).is_some_and(|(id,_)| *id == turn) { service.active.remove(&bot); }
                result?;
            }
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
            message = inbound.recv() => {
                let Some(message) = message else { break };
                match message {
                    Inbound::Open(id, output) => {
                        let _ = output.try_send(ready.clone());
                        sessions.insert(id, output);
                    }
                    Inbound::Closed(id) => {
                        sessions.remove(&id);
                        service.hub.close_session(id);
                        if id == 0 && stdio_owner { break; }
                    }
                    Inbound::Request(id, request) => {
                        let Some(output) = sessions.get(&id).cloned() else { continue };
                        let (request_id, result, shutting_down) = match request {
                            Ok(request) if request.id.is_u64() || request.id.as_str().is_some_and(|id| id.len() <= 128) => {
                                let shutting_down = matches!(request.command, Command::Shutdown);
                                let result = service.dispatch(request.command, id, &output).await;
                                (request.id, result, shutting_down)
                            }
                            Ok(_) => (Value::Null, fail("invalid_request_id"), false),
                            Err(error) => (Value::Null, Err(error), false),
                        };
                        if id == 0 && stdio_owner {
                            if !matches!(tokio::time::timeout(Duration::from_secs(5), output.respond(request_id, result)).await, Ok(Ok(()))) {
                                return fail("output_closed");
                            }
                        } else if output.try_respond(request_id, result).is_err() {
                            sessions.remove(&id);
                            service.hub.close_session(id);
                            output.close();
                        }
                        if shutting_down {
                            // The socket writer is a task on this runtime. Give
                            // it time to acknowledge shutdown before exiting.
                            let _ = tokio::time::timeout(Duration::from_secs(5), output.drain()).await;
                            break;
                        }
                    }
                }
            }
        }
    }
    for (_, cancel) in service.active.values() {
        let _ = cancel.send(true);
    }
    while let Some(result) = service.jobs.join_next().await {
        result.map_err(|_| Error::new("turn_task_failed"))?.2?;
    }
    service.replays.abort_all();
    while service.replays.join_next().await.is_some() {}
    drop(sessions);
    // The stdio firehose owns another sender. Release it before waiting for
    // the output worker to drain and exit, including shutdown without EOF.
    drop(service);
    stdout_writer
        .join()
        .map_err(|_| Error::new("output_worker_failed"))??;
    drop(socket_owner);
    Ok(())
}

impl Service {
    async fn dispatch(&mut self, command: Command, session: u64, output: &Output) -> Result<Value> {
        let store = &self.store;
        match command {
            Command::Create {
                bot,
                workspace: path,
                model,
                instructions,
                reasoning,
            } => {
                name(&bot)?;
                let path = path.as_deref().map(workspace).transpose()?;
                let reference = model
                    .or_else(|| self.default_model.clone())
                    .ok_or(Error::new("model_required"))?;
                let (provider, model) = split_model(&reference)?;
                let family = self
                    .providers
                    .get(provider)
                    .ok_or(Error::with("provider_unavailable", provider))?
                    .family();
                if let Some(level) = &reasoning
                    && !matches!(level.as_str(), "low" | "medium" | "high")
                {
                    return fail("invalid_reasoning_level");
                }
                let instructions =
                    instructions.unwrap_or_else(|| self.default_instructions.clone());
                if instructions.len() > 64 * 1024 {
                    return fail("instructions_limit");
                }
                let (provider, model) = (provider.to_owned(), model.to_owned());
                store
                    .call(move |db| {
                        Ok(serde_json::to_value(db.create(
                            &bot,
                            path.as_deref(),
                            Binding {
                                provider: &provider,
                                family,
                                model: &model,
                                instructions: &instructions,
                                reasoning: reasoning.as_deref(),
                            },
                        )?)?)
                    })
                    .await
            }
            Command::Resume { bot } => {
                store
                    .call(move |db| Ok(serde_json::to_value(db.inspect(&bot)?)?))
                    .await
            }
            Command::Bots { after, limit } => {
                store
                    .call(move |db| db.list(after.as_deref(), limit.unwrap_or(64)))
                    .await
            }
            Command::Fork {
                source,
                checkpoint,
                bot,
                workspace: path,
            } => {
                name(&bot)?;
                let path = path.as_deref().map(workspace).transpose()?;
                store
                    .call(move |db| {
                        Ok(serde_json::to_value(db.fork(
                            &source,
                            checkpoint,
                            &bot,
                            path.as_deref(),
                        )?)?)
                    })
                    .await
            }
            Command::Events { bot, after, limit } => {
                store.call(move |db| db.events(&bot, after, limit)).await
            }
            Command::Item { bot, node } => store.call(move |db| db.item(&bot, node)).await,
            Command::Artifact {
                bot,
                turn,
                call_id,
                stream,
                offset,
                limit,
            } => {
                store
                    .call(move |db| match stream {
                        Some(stream) => db.artifact_page(
                            &bot,
                            turn,
                            &call_id,
                            &stream,
                            offset,
                            limit.unwrap_or(64 * 1024),
                        ),
                        None if offset == 0 && limit.is_none() => db.artifact(&bot, turn, &call_id),
                        None => fail("artifact_stream_required"),
                    })
                    .await
            }
            Command::Follow { bot, after } => {
                if after < 0 {
                    return fail("invalid_event_page");
                }
                let check = bot.clone();
                store.call(move |db| db.inspect(&check)).await?;
                let sub = self.hub.subscribe(&bot, session, output.clone(), after);
                let (store, hub, replay_bot, output) =
                    (store.clone(), self.hub.clone(), bot.clone(), output.clone());
                let replay_sub = sub.clone();
                let task = self.replays.spawn(async move {
                    if replay(store, hub, replay_bot, replay_sub).await.is_err() {
                        output.close();
                    }
                });
                sub.lock().unwrap().set_replay(task);
                Ok(json!({"following":bot,"after":after}))
            }
            Command::Unfollow { bot } => {
                self.hub.unsubscribe(&bot, session);
                Ok(json!({"following":Value::Null}))
            }
            Command::Submit {
                bot,
                request_id,
                prompt,
                workspace: path,
                model,
            } => {
                name(&request_id)?;
                if prompt.len() > 256 * 1024 {
                    return fail("prompt_limit");
                }
                // A turn may run in another checkout or on another model of
                // the same family; the conversation encoding never changes.
                let options = TurnOptions {
                    workspace: path.as_deref().map(workspace).transpose()?,
                    model: match model {
                        Some(reference) => {
                            let (provider, _) = split_model(&reference)?;
                            let family = self
                                .providers
                                .get(provider)
                                .ok_or(Error::with("provider_unavailable", provider))?
                                .family();
                            let check = bot.clone();
                            let current = store.call(move |db| db.inspect(&check)).await?;
                            if current.family()? != family {
                                return fail_with("provider_family_mismatch", reference);
                            }
                            Some(reference)
                        }
                        None => None,
                    },
                };
                let capacity = self.active.len() < 1024;
                let (b, r) = (bot.clone(), request_id.clone());
                let started = store
                    .call(move |db| db.begin(&b, &r, &prompt, capacity, &options))
                    .await?;
                let mut cursor = None;
                if let Some(entry) = started.entry {
                    cursor = entry["cursor"].as_i64();
                    self.hub.durable(&bot, entry).await?;
                }
                if started.fresh {
                    let (cancel, cancelled) = watch::channel(false);
                    self.active.insert(bot.clone(), (started.turn, cancel));
                    let turn = Turn {
                        bot: bot.clone(),
                        turn: started.turn,
                        store: store.clone(),
                        providers: self.providers.clone(),
                        registry: self.registry.clone(),
                        hub: self.hub.clone(),
                    };
                    self.jobs.spawn(async move {
                        let (bot, id) = (turn.bot.clone(), turn.turn);
                        let result = turn.execute(cancelled).await;
                        (bot, id, result)
                    });
                }
                Ok(
                    json!({"bot":bot,"turn":started.turn,"request_id":request_id,
                    "duplicate":!started.fresh,"cursor":cursor}),
                )
            }
            Command::Interrupt { bot, turn } => {
                let Some((running, cancel)) = self.active.get(&bot) else {
                    return fail("no_active_turn");
                };
                if *running != turn {
                    return fail("stale_turn");
                }
                cancel
                    .send(true)
                    .map_err(|_| Error::new("no_active_turn"))?;
                Ok(json!({"interrupt_requested":true,"turn":turn}))
            }
            Command::Shutdown => Ok(json!({"shutting_down":true})),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(spec: &str) -> (String, &'static str, String, Option<String>) {
        let parsed = ProviderSpec::parse(spec).unwrap();
        (
            parsed.name,
            parsed.family.name(),
            parsed.url,
            parsed.key_env,
        )
    }

    #[test]
    fn provider_specs_default_known_endpoints_and_keys() {
        assert_eq!(
            spec("anthropic"),
            (
                "anthropic".into(),
                "anthropic",
                "https://api.anthropic.com/v1".into(),
                Some("ANTHROPIC_API_KEY".into())
            )
        );
        assert_eq!(
            spec("openai=responses,http://127.0.0.1:9/v1"),
            (
                "openai".into(),
                "responses",
                "http://127.0.0.1:9/v1".into(),
                None
            )
        );
        assert_eq!(
            spec("gw=responses,https://gw.example.test/v1,GW_KEY"),
            (
                "gw".into(),
                "responses",
                "https://gw.example.test/v1".into(),
                Some("GW_KEY".into())
            )
        );
        assert!(ProviderSpec::parse("custom").is_err());
        assert!(ProviderSpec::parse("openai=chat").is_err());
    }

    #[tokio::test]
    async fn saturated_dispatch_reconciles_duplicates_but_rejects_new_work() {
        let dir = std::env::temp_dir().join(format!("agent-admission-test-{}", std::process::id()));
        let store = Store::open(&dir.join("state.sqlite"), "test".into())
            .await
            .unwrap();
        let binding = || Binding {
            provider: "openai",
            family: Family::Responses,
            model: "synthetic-model",
            instructions: "test",
            reasoning: None,
        };
        let turn = store
            .call(move |db| {
                db.create("Bob", Some("/synthetic"), binding())?;
                db.create("Other", Some("/synthetic"), binding())?;
                Ok(db
                    .begin("Bob", "same", "work", true, &TurnOptions::default())?
                    .turn)
            })
            .await
            .unwrap();
        let registry = Registry::new("echo").unwrap();
        let provider = Provider::new(
            Transport::new().unwrap(),
            Family::Responses,
            "http://127.0.0.1:1/v1",
            None,
            &registry.schemas(),
        )
        .unwrap();
        let (output, writer) = Output::stdout();
        let mut service = Service {
            store: store.clone(),
            providers: Arc::new(HashMap::from([("openai".to_owned(), provider)])),
            registry,
            hub: Hub::default(),
            default_model: None,
            default_instructions: "test".into(),
            active: (0..1024)
                .map(|index| (index.to_string(), (turn, watch::channel(false).0)))
                .collect(),
            jobs: JoinSet::new(),
            replays: JoinSet::new(),
        };
        let duplicate = service
            .dispatch(
                Command::Submit {
                    bot: "Bob".into(),
                    request_id: "same".into(),
                    prompt: "work".into(),
                    workspace: None,
                    model: None,
                },
                0,
                &output,
            )
            .await
            .unwrap();
        assert_eq!(duplicate["turn"], turn);
        assert_eq!(duplicate["duplicate"], true);
        let error = service
            .dispatch(
                Command::Submit {
                    bot: "Other".into(),
                    request_id: "new".into(),
                    prompt: "work".into(),
                    workspace: None,
                    model: None,
                },
                0,
                &output,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "active_agent_limit");
        assert!(service.jobs.is_empty());
        assert!(
            store
                .call(|db| Ok(db.inspect("Other")?.head.is_none()))
                .await
                .unwrap()
        );
        drop(output);
        drop(service);
        writer.join().unwrap().unwrap();
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
