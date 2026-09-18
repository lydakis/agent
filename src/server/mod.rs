//! One process hosts every bot. Client sessions (stdio or Unix socket) speak
//! the same JSONL protocol; turns run as tasks; events fan out through a hub.
mod handles;
mod hub;
mod session;
mod socket;
mod turn;

use agent_runtime::{
    Error, Result,
    codec::{Family, split_model},
    fail, fail_with,
    output::Output,
    provider::{Provider, STREAMS_PER_CONNECTION, Transport},
    store::{Binding, Bot, Delivery, Store, TurnOptions},
    tools::Registry,
};
use handles::{Completion, Handles, Waiter};
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
\"$AGENT_BIN\" run --detach --new --bot NAME -- TASK from the shell; it prints a turn handle immediately. \
Continue an existing agent with \"$AGENT_BIN\" run --detach --bot NAME -- TASK. \
Collect results with the wait tool on that handle; it returns the peer's status and final text. \
Long commands can run with shell background=true and be collected the same way. \
Blocking run/follow inside a shell tool is rejected. \
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
        budget_tokens: Option<u64>,
    },
    Resume {
        bot: String,
    },
    Fork {
        source: String,
        /// A node id from the source's history; defaults to its current head.
        checkpoint: Option<i64>,
        bot: String,
        workspace: Option<String>,
        budget_tokens: Option<u64>,
    },
    /// Remove an idle bot and everything only it owns.
    Delete {
        bot: String,
    },
    /// Drop events, tool intents, processes, and artifacts of all but the
    /// newest `keep_turns` turns; the transcript and turn rows stay.
    Prune {
        bot: String,
        keep_turns: usize,
    },
    /// A bot's turns with status, workspace, model, tokens, and timing.
    Turns {
        bot: String,
        #[serde(default)]
        after: i64,
        limit: Option<usize>,
    },
    /// A turn's outcome without waiting: the wait payload, or its live status.
    Result {
        bot: String,
        turn: i64,
    },
    Submit {
        bot: String,
        request_id: String,
        prompt: String,
        workspace: Option<String>,
        model: Option<String>,
        /// `reject` (default), `queue`, or `steer`.
        delivery: Option<String>,
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
    /// Block this request until the handles resolve; the response carries
    /// the same result shape as the wait tool.
    Wait {
        handles: Vec<String>,
        timeout_ms: Option<u64>,
        /// Answer on the first resolved handle; the rest are reported pending.
        #[serde(default)]
        any: bool,
    },
    /// The daemon's live state for a fleet controller: sessions, turns,
    /// connections, pools, storage worker, and handle registry.
    Stats,
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
    /// Concurrent child processes; default 64 per logical CPU; zero unbounded.
    pub max_processes: Option<usize>,
    /// Turns with a live task (model call or foreground tool); default 4,096; zero unbounded.
    pub max_active: Option<usize>,
    /// Provider requests awaiting response headers; unbounded by default,
    /// since providers hold headers until the first token and the bound
    /// would cap throughput at permits per first-token latency.
    pub max_connecting: Option<usize>,
    /// Generated tokens per Responses call, including reasoning; none by default.
    pub max_output_tokens: Option<u32>,
    /// Exit a socket daemon after this many seconds with no sessions, no
    /// active turns, and no running background commands; none by default.
    pub idle_exit: Option<u64>,
    /// Model context per request: newest turns within these bounds. Stored
    /// history itself is unbounded. Defaults 8 MiB and 4,096 items.
    pub context_bytes: Option<usize>,
    pub context_items: Option<usize>,
    /// Prune every bot to this many turns' records after each of its turns
    /// finishes; none by default.
    pub retain_turns: Option<usize>,
}

#[derive(Clone, Copy)]
pub struct Limits {
    pub processes: usize,
    pub active: usize,
    pub connecting: usize,
    /// HTTP/2 connections per provider, enough for `active` turns to stream
    /// at once at the providers' advertised streams per connection.
    pub connections: usize,
    pub context_bytes: usize,
    pub context_items: usize,
}
pub const MIN_CONTEXT_BYTES: usize = 1024;
pub const MIN_CONTEXT_ITEMS: usize = 2;

impl Limits {
    pub fn resolve(config: &Configuration) -> Limits {
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        let active = config.max_active.unwrap_or(4096);
        Limits {
            processes: config.max_processes.unwrap_or(64 * cpus),
            active,
            connecting: config.max_connecting.unwrap_or(0),
            connections: if active == 0 {
                64
            } else {
                active.div_ceil(STREAMS_PER_CONNECTION).clamp(1, 256)
            },
            context_bytes: config
                .context_bytes
                .unwrap_or(8 * 1024 * 1024)
                .max(MIN_CONTEXT_BYTES),
            context_items: config.context_items.unwrap_or(4096).max(MIN_CONTEXT_ITEMS),
        }
    }
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
    transport: Arc<Transport>,
    providers: Arc<HashMap<String, Provider>>,
    registry: Registry,
    hub: Hub,
    default_model: Option<String>,
    default_instructions: String,
    handles: Handles,
    background_failures: mpsc::UnboundedSender<Error>,
    limits: Limits,
    retain_turns: Option<usize>,
    /// Open client sessions, kept by the run loop for `stats`.
    sessions: usize,
    limit_active: usize,
    /// Per bot: the running turn, the task owning it, and its cancel signal.
    /// A parked turn's task ends while a resumed task may already own the slot.
    active: HashMap<String, (i64, u64, watch::Sender<bool>)>,
    next_task: u64,
    jobs: JoinSet<(String, i64, u64, turn::Exit)>,
    replays: JoinSet<()>,
    /// A turn may be ready to start: set whenever a slot opens or a turn
    /// is queued, cleared when the store has none. Keeps the idle loop free
    /// of a store read per iteration.
    ready_hint: bool,
}

pub async fn run(config: Configuration) -> Result<()> {
    let limits = Limits::resolve(&config);
    let transport = Transport::new(limits.connecting, limits.connections)?;
    let registry = Registry::new(&config.tools)?;
    let schemas = registry.schemas();
    let mut providers = HashMap::new();
    let mut credentials = Vec::new();
    let mut bindings = serde_json::Map::new();
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
        let mut provider = Provider::new(transport.clone(), spec.family, &spec.url, key, &schemas)?;
        if let Some(cap) = config.max_output_tokens
            && spec.family == Family::Responses
        {
            provider = provider.with_max_output_tokens(cap)?;
        }
        if providers.insert(spec.name.clone(), provider).is_some() {
            return fail_with("duplicate_provider", spec.name.as_str());
        }
        bindings.insert(
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
    let store = Store::open(&config.store).await?;
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
        .with_environment(environment)
        .with_process_budget(limits.processes);
    let hub = Hub::default();
    let ready = json!({"event":"ready","protocol":3,
        "capabilities":["create","resume","fork_any_node","context_window","submit","delivery","interrupt","events","item","artifact","follow","follow_all","bots","wait","wait_any","stats","turns","result","budgets","delete","prune"],
        "limits":{"processes":limits.processes,"active":limits.active,"connecting":limits.connecting,
            "connections":limits.connections,
            "output_tokens":config.max_output_tokens,"idle_exit_seconds":config.idle_exit,
            "context_bytes":limits.context_bytes,"context_items":limits.context_items,
            "retain_turns":config.retain_turns},
        "schema":agent_runtime::store::Database::SCHEMA,
        "tools":registry.names(),"providers":bindings,"default_model":config.model,
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
    let mut stdout_closed = stdout.subscribe_closed();
    drop(stdout);
    drop(sender);
    let (resume_sender, mut resumes) = mpsc::unbounded_channel();
    let (failure_sender, mut failures) = mpsc::unbounded_channel();
    let handles = Handles::new(resume_sender);
    // Turns parked before a restart keep waiting; their processes are gone.
    for waiting in store.call(|db| db.waiting_turns()).await? {
        handles
            .attach(
                &store,
                Waiter::Turn(waiting.turn),
                &waiting.handles,
                waiting.deadline_ms,
                waiting.any,
                Completion::Resume {
                    bot: waiting.bot.clone(),
                    turn: waiting.turn,
                },
            )
            .await;
    }
    let mut service = Service {
        store,
        transport,
        providers: Arc::new(providers),
        registry,
        hub,
        default_model: config.model.clone(),
        default_instructions: config
            .instructions
            .clone()
            .unwrap_or_else(|| DEFAULT_INSTRUCTIONS.to_owned()),
        handles,
        background_failures: failure_sender,
        limits,
        retain_turns: config.retain_turns,
        sessions: sessions.len(),
        limit_active: limits.active,
        active: HashMap::new(),
        next_task: 0,
        jobs: JoinSet::new(),
        replays: JoinSet::new(),
        // Queued turns that survived a restart start as capacity allows.
        ready_hint: true,
    };
    let idle_exit = config
        .idle_exit
        .filter(|_| config.socket.is_some())
        .map(Duration::from_secs);
    let mut last_activity = std::time::Instant::now();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    loop {
        tokio::select! {
            _ = stdout_closed.wait_for(|closed| *closed), if stdio_owner => return fail("output_closed"),
            Some(error) = failures.recv() => return Err(error),
            _ = service.replays.join_next(), if !service.replays.is_empty() => {}
            joined = service.jobs.join_next(), if !service.jobs.is_empty() => {
                let (bot, turn, task, exit) = joined.unwrap().map_err(|_| Error::new("turn_task_failed"))?;
                service.complete(bot, turn, task, exit).await?;
            }
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
            _ = tokio::time::sleep(idle_exit.unwrap_or(Duration::MAX).min(Duration::from_secs(3600))), if idle_exit.is_some() => {
                // Idle means no client, no live turn, and no running command.
                // Parked turns are durable and resume on the next start.
                let running = service.store.call(|db| db.running_processes()).await?;
                if sessions.is_empty() && service.active.is_empty() && running == 0 {
                    if last_activity.elapsed() >= idle_exit.unwrap() { break; }
                } else {
                    last_activity = std::time::Instant::now();
                }
            }
            Some((bot, turn)) = resumes.recv(), if service.has_capacity() => {
                service.resume(bot, turn).await?;
            }
            // One queued turn per iteration, so requests interleave with a
            // long backlog; resumes hold no slot and are not starved because
            // the branch choice is fair.
            _ = std::future::ready(()), if service.ready_hint && service.has_capacity() => {
                service.dispatch_ready().await?;
            }
            message = inbound.recv() => {
                let Some(message) = message else { break };
                last_activity = std::time::Instant::now();
                match message {
                    Inbound::Open(id, output) => {
                        let _ = output.try_send(ready.clone());
                        sessions.insert(id, output);
                        service.sessions = sessions.len();
                    }
                    Inbound::Closed(id) => {
                        sessions.remove(&id);
                        service.sessions = sessions.len();
                        service.hub.close_session(id);
                        service.handles.close_session(id);
                        if id == 0 && stdio_owner { break; }
                    }
                    Inbound::Request(id, request) => {
                        let Some(output) = sessions.get(&id).cloned() else { continue };
                        let (request_id, result, shutting_down) = match request {
                            Ok(request) if request.id.is_u64() || request.id.as_str().is_some_and(|id| id.len() <= 128) => {
                                let shutting_down = matches!(request.command, Command::Shutdown);
                                let result = service.dispatch(request.command, id, &output, request.id.clone()).await;
                                if result.as_ref().is_err_and(|e| e.code == "deferred") { continue; }
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
                            service.sessions = sessions.len();
                            service.hub.close_session(id);
                            service.handles.close_session(id);
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
    for (_, _, cancel) in service.active.values() {
        let _ = cancel.send(true);
    }
    while let Some(result) = service.jobs.join_next().await {
        let (bot, turn, task, exit) = result.map_err(|_| Error::new("turn_task_failed"))?;
        service.complete(bot, turn, task, exit).await?;
    }
    service.handles.shutdown();
    service.replays.abort_all();
    while service.replays.join_next().await.is_some() {}
    // Socket writers need runtime time to flush terminal events and wait
    // results. Drain concurrently under one deadline, so slow clients cannot
    // multiply shutdown latency. Stdout also drains through its worker below.
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        futures_util::future::join_all(sessions.values().map(Output::drain)),
    )
    .await;
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

// Runs on the storage worker using the bot already read for admission. No
// extra query or channel round trip; defaults do not allocate a model string.
fn validate_provider(
    providers: &HashMap<String, Provider>,
    bot: &Bot,
    model: Option<&str>,
) -> Result<()> {
    let name = match model {
        Some(model) => split_model(model)?.0,
        None => &bot.provider,
    };
    let provider = providers
        .get(name)
        .ok_or_else(|| Error::with("provider_unavailable", name))?;
    if provider.family() != bot.family()? {
        return fail_with(
            "provider_family_mismatch",
            model
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{}/{}", bot.provider, bot.model)),
        );
    }
    Ok(())
}

impl Service {
    fn has_capacity(&self) -> bool {
        self.limit_active == 0 || self.active.len() < self.limit_active
    }

    async fn resume(&mut self, bot: String, turn: i64) -> Result<()> {
        // A wake-up can outlive an interrupt, deletion, or reuse of the name.
        // Check only its identity and state, without loading the full bot.
        let pending = self
            .store
            .call(move |db| Ok(db.can_resume(&bot, turn)?.then_some(bot)))
            .await?;
        if let Some(bot) = pending {
            self.spawn(bot, turn, true);
        }
        Ok(())
    }

    /// Start the oldest turn waiting for a slot, or learn there is none.
    /// A turn that cannot start (its bot's outcome uncertain, budget spent)
    /// ends with that error; the bot's next queued turn takes its place.
    async fn dispatch_ready(&mut self) -> Result<()> {
        let Some((bot, turn)) = self.store.call(|db| db.next_ready()).await? else {
            self.ready_hint = false;
            return Ok(());
        };
        let providers = self.providers.clone();
        match self
            .store
            .call(move |db| db.start(turn, |bot, model| validate_provider(&providers, bot, model)))
            .await
        {
            Ok(entry) => {
                self.hub.durable(&bot, entry).await?;
                self.spawn(bot, turn, false);
            }
            Err(error) if error.code == "stale_turn" || error.code == "bot_busy" => {}
            Err(error) => self.end_queued(bot, turn, error).await?,
        }
        Ok(())
    }

    /// End a turn that never started and answer its waiters.
    async fn end_queued(&mut self, bot: String, turn: i64, error: Error) -> Result<()> {
        let keep = self.retain_turns;
        let owner = bot.clone();
        let (entries, outcome) = self
            .store
            .call(move |db| {
                let finished = db.end_queued(turn, &error)?;
                if let Some(keep) = keep {
                    db.prune(&owner, keep)?;
                }
                Ok(finished)
            })
            .await?;
        for entry in entries {
            self.hub.durable(&bot, entry).await?;
        }
        self.handles.turn_finished(&bot, turn, outcome);
        self.ready_hint = true;
        Ok(())
    }

    /// Run a turn as a task: fresh after submission, or resuming a parked one.
    fn spawn(&mut self, bot: String, turn: i64, resume: bool) {
        let (cancel, cancelled) = watch::channel(false);
        self.next_task += 1;
        let task_id = self.next_task;
        self.active.insert(bot.clone(), (turn, task_id, cancel));
        let task = Turn {
            bot,
            turn,
            store: self.store.clone(),
            providers: self.providers.clone(),
            registry: self.registry.clone(),
            hub: self.hub.clone(),
            handles: self.handles.clone(),
            background_failures: self.background_failures.clone(),
            context_bytes: self.limits.context_bytes,
            context_items: self.limits.context_items,
            resume,
        };
        self.jobs.spawn(async move {
            let (bot, id) = (task.bot.clone(), task.turn);
            let result = task.execute(cancelled).await;
            (bot, id, task_id, result)
        });
    }

    /// Commit and publish completion without dispatching another command in
    /// between. The durable busy state prevents newer accepted events from
    /// overtaking the terminal event while the task waits to be reaped.
    async fn complete(
        &mut self,
        bot: String,
        turn: i64,
        task: u64,
        exit: turn::Exit,
    ) -> Result<()> {
        if self
            .active
            .get(&bot)
            .is_some_and(|(_, owner, _)| *owner == task)
        {
            self.active.remove(&bot);
            // A slot opened, and a finish may promote the bot's next turn.
            self.ready_hint = true;
        }
        if let turn::Exit::Finished(error) = exit {
            let keep = self.retain_turns;
            let (bot, finished) = self
                .store
                .call(move |db| {
                    let finished = turn::Finished::record(db, &bot, turn, error.as_ref(), keep)?;
                    Ok((bot, finished))
                })
                .await?;
            // Interrupt may already have released the task's active slot.
            // Finishing still promotes queued work in that case.
            self.ready_hint = true;
            for entry in finished.entries {
                self.hub.durable(&bot, entry).await?;
            }
            self.handles.turn_finished(&bot, turn, finished.outcome);
        }
        Ok(())
    }

    async fn dispatch(
        &mut self,
        command: Command,
        session: u64,
        output: &Output,
        request_id: Value,
    ) -> Result<Value> {
        let store = &self.store;
        match command {
            Command::Create {
                bot,
                workspace: path,
                model,
                instructions,
                reasoning,
                budget_tokens,
            } => {
                if budget_tokens == Some(0) {
                    return fail("invalid_budget");
                }
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
                    && !matches!(level.as_str(), "low" | "medium" | "high" | "xhigh" | "max")
                {
                    return fail("invalid_reasoning_level");
                }
                let instructions =
                    instructions.unwrap_or_else(|| self.default_instructions.clone());
                if instructions.len() > 64 * 1024 {
                    return fail("instructions_limit");
                }
                let (provider, model) = (provider.to_owned(), model.to_owned());
                let (created, event) = store
                    .call(move |db| {
                        db.create(
                            &bot,
                            path.as_deref(),
                            Binding {
                                provider: &provider,
                                family,
                                model: &model,
                                instructions: &instructions,
                                reasoning: reasoning.as_deref(),
                                budget_tokens,
                            },
                        )
                    })
                    .await?;
                self.hub.durable(&created.name, event).await?;
                Ok(serde_json::to_value(created)?)
            }
            Command::Turns { bot, after, limit } => {
                store
                    .call(move |db| db.turns(&bot, after, limit.unwrap_or(64)))
                    .await
            }
            Command::Delete { bot } => {
                if self.active.contains_key(&bot) {
                    return fail("bot_busy");
                }
                let name = bot.clone();
                let deleted = store.call(move |db| db.delete_bot(&name)).await?;
                // Followers learn the bot is gone; nothing durable remains to replay.
                self.hub
                    .live(&bot, json!({"event":"deleted","bot":bot,"durable":false}))
                    .await?;
                Ok(deleted)
            }
            Command::Prune { bot, keep_turns } => {
                store.call(move |db| db.prune(&bot, keep_turns)).await
            }
            Command::Result { bot, turn } => {
                store
                    .call(move |db| match db.turn_outcome(&bot, turn)? {
                        Some(outcome) => Ok(outcome),
                        None => {
                            let status = db.turn_status(&bot, turn)?;
                            Ok(json!({"turn":turn,"status":status,"finished":false}))
                        }
                    })
                    .await
            }
            Command::Resume { bot } => {
                store
                    .call(move |db| Ok(serde_json::to_value(db.inspect(&bot)?)?))
                    .await
            }
            Command::Stats => {
                let (waiting, running, queued) = store.call(|db| db.counts()).await?;
                let (waiters, retained) = self.handles.stats();
                let providers: serde_json::Map<String, Value> = self
                    .providers
                    .iter()
                    .map(|(name, provider)| (name.clone(), provider.status()))
                    .collect();
                Ok(json!({
                    "sessions": self.sessions,
                    "active_turns": self.active.len(),
                    "active_limit": self.limit_active,
                    "waiting_turns": waiting,
                    "queued_turns": queued,
                    "running_processes": running,
                    "queued_processes": self.registry.pending(),
                    "process_limit": self.limits.processes,
                    "transport": {"in_flight_by_shard": self.transport.loads()},
                    "providers": providers,
                    "store": store.stats(),
                    "handles": {"waiters": waiters, "retained": retained},
                }))
            }
            Command::Wait {
                handles,
                timeout_ms,
                any,
            } => {
                if handles.is_empty() || handles.len() > 64 {
                    return fail("invalid_handles");
                }
                if timeout_ms.is_some_and(|t| t > 86_400_000) {
                    return fail("invalid_timeout");
                }
                for handle in &handles {
                    handles::Handle::parse(handle)?;
                }
                let waiter = self.handles.request_waiter();
                let deadline = timeout_ms.map(|t| handles::now_ms() + t);
                self.handles
                    .attach(
                        store,
                        waiter,
                        &handles,
                        deadline,
                        any,
                        Completion::Respond {
                            session,
                            output: output.clone(),
                            request: request_id,
                        },
                    )
                    .await;
                // The response is sent by the completion, not by this dispatch.
                Err(Error::new("deferred"))
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
                budget_tokens,
            } => {
                if budget_tokens == Some(0) {
                    return fail("invalid_budget");
                }
                name(&bot)?;
                let path = path.as_deref().map(workspace).transpose()?;
                let (created, event) = store
                    .call(move |db| {
                        db.fork(&source, checkpoint, &bot, path.as_deref(), budget_tokens)
                    })
                    .await?;
                self.hub.durable(&created.name, event).await?;
                Ok(serde_json::to_value(created)?)
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
                // `*` follows every bot from a store-wide cursor.
                if bot != hub::ALL {
                    let check = bot.clone();
                    store.call(move |db| db.inspect(&check)).await?;
                }
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
                delivery,
            } => {
                name(&request_id)?;
                if prompt.len() > 256 * 1024 {
                    return fail("prompt_limit");
                }
                let delivery = match delivery.as_deref() {
                    None => Delivery::Reject,
                    Some(mode) => Delivery::parse(mode)
                        .ok_or_else(|| Error::with("invalid_delivery", mode))?,
                };
                // A turn may run in another checkout or on another model of
                // the same family; the conversation encoding never changes.
                let options = TurnOptions {
                    workspace: path.as_deref().map(workspace).transpose()?,
                    model,
                    delivery,
                };
                let capacity = self.has_capacity();
                let (b, r) = (bot.clone(), request_id.clone());
                let providers = self.providers.clone();
                let started = store
                    .call(move |db| {
                        db.begin(&b, &r, &prompt, capacity, &options, |bot, model| {
                            validate_provider(&providers, bot, model)
                        })
                    })
                    .await?;
                let mut cursor = None;
                if let Some(entry) = started.entry {
                    cursor = entry["cursor"].as_i64();
                    self.hub.durable(&bot, entry).await?;
                }
                if started.fresh && started.status == "running" {
                    self.spawn(bot.clone(), started.turn, false);
                }
                if started.status == "ready" {
                    self.ready_hint = true;
                }
                Ok(
                    json!({"bot":bot,"turn":started.turn,"request_id":request_id,
                    "duplicate":!started.fresh,"status":started.status,"cursor":cursor,
                    "handle":format!("turn:{bot}/{}", started.turn)}),
                )
            }
            Command::Interrupt { bot, turn } => {
                if let Some((_, _, cancel)) = self
                    .active
                    .get(&bot)
                    .filter(|(running, _, _)| *running == turn)
                {
                    if cancel.send(true).is_ok() {
                        return Ok(json!({"interrupt_requested":true,"turn":turn}));
                    }
                    // The task exited but its JoinSet result has not been reaped.
                    // Release its slot; the result still gets checked by the loop.
                    self.active.remove(&bot);
                }
                // A queued turn has no task either; end it where it stands.
                // An unknown id is judged below against the bot's state.
                let check = bot.clone();
                let status = store
                    .call(move |db| db.turn_status(&check, turn))
                    .await
                    .unwrap_or_default();
                if matches!(status.as_str(), "queued" | "ready") {
                    self.end_queued(bot, turn, Error::new("cancelled")).await?;
                    return Ok(json!({"interrupt_requested":true,"turn":turn,"queued":true}));
                }
                if self.active.contains_key(&bot) {
                    return fail("stale_turn");
                }
                // A parked turn has no task; confirm durable state and end it.
                let check = bot.clone();
                let state = store.call(move |db| db.inspect(&check)).await?;
                if state.running_turn.is_none() {
                    return fail("no_active_turn");
                }
                if state.running_turn != Some(turn) || state.status != "waiting" {
                    return fail("stale_turn");
                }
                self.handles.forget(Waiter::Turn(turn));
                let name = bot.clone();
                let keep = self.retain_turns;
                let finished = store
                    .call(move |db| {
                        turn::Finished::record(
                            db,
                            &name,
                            turn,
                            Some(&Error::new("cancelled")),
                            keep,
                        )
                    })
                    .await?;
                self.ready_hint = true;
                for entry in finished.entries {
                    self.hub.durable(&bot, entry).await?;
                }
                self.handles.turn_finished(&bot, turn, finished.outcome);
                Ok(json!({"interrupt_requested":true,"turn":turn,"parked":true}))
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
    async fn interrupt_reconciles_a_parked_task_before_it_is_reaped() {
        let dir = std::env::temp_dir().join(format!(
            "agent-parked-interrupt-test-{}",
            std::process::id()
        ));
        let store = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let turn = store
            .call(|db| {
                db.create(
                    "Bob",
                    Some("/synthetic"),
                    Binding {
                        provider: "openai",
                        family: Family::Responses,
                        model: "synthetic-model",
                        instructions: "test",
                        reasoning: None,
                        budget_tokens: None,
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
                let call = agent_runtime::provider::ToolCall {
                    name: "wait".into(),
                    call_id: "wait-1".into(),
                    arguments: r#"{"handles":["proc:1"]}"#.into(),
                };
                let item = serde_json::to_vec(&json!({"type":"function_call","name":call.name,
                "call_id":call.call_id,"arguments":call.arguments}))?
                .into();
                db.append(turn, vec![item], std::slice::from_ref(&call), None)?;
                db.tool_start(turn, &call)?;
                db.suspend(turn, &call.call_id, &["proc:1".into()], None, false, &[])?;
                Ok(turn)
            })
            .await
            .unwrap();
        let (cancel, cancelled) = watch::channel(false);
        let mut service = Service {
            store: store.clone(),
            transport: Transport::new(64, 1).unwrap(),
            providers: Arc::new(HashMap::new()),
            registry: Registry::new("wait").unwrap(),
            hub: Hub::default(),
            default_model: None,
            default_instructions: "test".into(),
            handles: Handles::new(mpsc::unbounded_channel().0),
            limits: Limits {
                processes: 16,
                active: 1024,
                connecting: 64,
                connections: 11,
                context_bytes: 8 << 20,
                context_items: 4096,
            },
            retain_turns: None,
            sessions: 0,
            background_failures: mpsc::unbounded_channel().0,
            limit_active: 1,
            active: HashMap::from([("Bob".into(), (turn, 1, cancel))]),
            next_task: 1,
            jobs: JoinSet::new(),
            replays: JoinSet::new(),
            ready_hint: false,
        };
        service.jobs.spawn(async move {
            drop(cancelled);
            ("Bob".into(), turn, 1, turn::Exit::Parked)
        });
        service.active["Bob"].2.closed().await;
        let output = Output::writer(tokio::io::sink());
        let stale = service
            .dispatch(
                Command::Interrupt {
                    bot: "Bob".into(),
                    turn: turn + 1,
                },
                0,
                &output,
                Value::Null,
            )
            .await
            .unwrap_err();
        assert_eq!(stale.code, "stale_turn");
        let reply = service
            .dispatch(
                Command::Interrupt {
                    bot: "Bob".into(),
                    turn,
                },
                0,
                &output,
                Value::Null,
            )
            .await
            .unwrap();
        assert_eq!(reply["parked"], true);
        assert!(service.has_capacity());
        assert_eq!(service.jobs.join_next().await.unwrap().unwrap().1, turn);
        let next = store
            .call(move |db| {
                let state = db.inspect("Bob")?;
                assert_eq!(state.status, "interrupted");
                assert_eq!(state.running_turn, None);
                let events = db.events("Bob", 0, 256)?;
                let events = events["events"].as_array().unwrap();
                assert_eq!(
                    events
                        .iter()
                        .filter(|e| e["event"] == "turn_finished")
                        .count(),
                    1
                );
                let completed = events
                    .iter()
                    .find(|e| e["event"] == "tool_completed")
                    .unwrap();
                assert_eq!(completed["data"]["cancelled"], true);
                let next = db
                    .begin(
                        "Bob",
                        "second",
                        "continue",
                        true,
                        &TurnOptions::default(),
                        |_, _| Ok(()),
                    )?
                    .turn;
                db.begin(
                    "Bob",
                    "third",
                    "queued",
                    true,
                    &TurnOptions {
                        delivery: Delivery::Queue,
                        ..TurnOptions::default()
                    },
                    |_, _| Ok(()),
                )?;
                Ok(next)
            })
            .await
            .unwrap();
        // A finished task can likewise be reaped after interrupt released
        // its active slot. Its successor must still wake the dispatcher.
        service.ready_hint = false;
        service
            .complete("Bob".into(), next, 2, turn::Exit::Finished(None))
            .await
            .unwrap();
        assert!(service.ready_hint);
        assert!(store.call(|db| db.next_ready()).await.unwrap().is_some());
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn saturated_dispatch_reconciles_duplicates_but_rejects_new_work() {
        let dir = std::env::temp_dir().join(format!("agent-admission-test-{}", std::process::id()));
        let store = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let binding = || Binding {
            provider: "openai",
            family: Family::Responses,
            model: "synthetic-model",
            instructions: "test",
            reasoning: None,
            budget_tokens: None,
        };
        let turn = store
            .call(move |db| {
                db.create("Bob", Some("/synthetic"), binding())?;
                db.create("Other", Some("/synthetic"), binding())?;
                Ok(db
                    .begin(
                        "Bob",
                        "same",
                        "work",
                        true,
                        &TurnOptions::default(),
                        |_, _| Ok(()),
                    )?
                    .turn)
            })
            .await
            .unwrap();
        let registry = Registry::new("echo").unwrap();
        let transport = Transport::new(64, 1).unwrap();
        let provider = Provider::new(
            transport.clone(),
            Family::Responses,
            "http://127.0.0.1:1/v1",
            None,
            &registry.schemas(),
        )
        .unwrap();
        let (output, writer) = Output::stdout();
        let mut service = Service {
            store: store.clone(),
            transport,
            providers: Arc::new(HashMap::from([("openai".to_owned(), provider)])),
            registry,
            hub: Hub::default(),
            default_model: None,
            default_instructions: "test".into(),
            handles: Handles::new(mpsc::unbounded_channel().0),
            limits: Limits {
                processes: 16,
                active: 1024,
                connecting: 64,
                connections: 11,
                context_bytes: 8 << 20,
                context_items: 4096,
            },
            retain_turns: None,
            sessions: 0,
            background_failures: mpsc::unbounded_channel().0,
            limit_active: 1024,
            active: (0..1024)
                .map(|index| (index.to_string(), (turn, 0, watch::channel(false).0)))
                .collect(),
            next_task: 0,
            jobs: JoinSet::new(),
            replays: JoinSet::new(),
            ready_hint: false,
        };
        let duplicate = service
            .dispatch(
                Command::Submit {
                    bot: "Bob".into(),
                    request_id: "same".into(),
                    prompt: "work".into(),
                    workspace: None,
                    model: None,
                    delivery: None,
                },
                0,
                &output,
                Value::Null,
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
                    delivery: None,
                },
                0,
                &output,
                Value::Null,
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
