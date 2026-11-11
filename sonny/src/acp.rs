//! `sonny acp`: Sonny as an agent of the Agent Client Protocol, for T3
//! Code, which runs it as a provider ("Local command"), a process for each
//! thread. A thread in a folder is that folder's lead, as `sonny` there
//! finds or makes it, set up the same way: its rules and skills, its tools,
//! its permissions row, and the lookout. Each task the lead starts opens as a
//! thread through T3 Code's tools, the lead thread's child while the
//! person's prompt runs, and that thread is the task's.
//!
//! Any other client, such as a bridge from a chat app, is served the same
//! way: it picks the lead behind a session with the Agent option, and
//! finds the leads to reopen with `session/list`.
//!
//! It speaks the protocol's version 2 draft as T3 Code does: the turns a
//! bot takes on its own, as when the lookout wakes a lead because a task
//! finished, reach the thread between the person's prompts, as work the
//! agent started.
use crate::{approvals, asks, declared, lookout, plan, project, reactions, t3};
use agent_client::{Client, Events};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OnceCell, oneshot};

pub const VERB: &str = "acp";

/// The protocol version Sonny speaks: the draft that lets an agent start
/// work of its own between prompts.
const VERSION: i64 = 2;

/// The most of a call's output a thread is shown.
const OUTPUT_BYTES: usize = 8 * 1024;

/// The longest the person's prompt waits for its tasks' threads to open.
const OPENING: std::time::Duration = std::time::Duration::from_secs(30);

/// The longest a thread's start waits for the providers to list models.
const LISTING: std::time::Duration = std::time::Duration::from_secs(5);

/// JSON-RPC's codes for what Sonny refuses.
const INVALID_PARAMS: i64 = -32602;
const NOT_FOUND: i64 = -32601;
const FAILED: i64 = -32603;
/// Sonny's extension for reactions, both ways: a client reacts to one of
/// the bot's replies, and is told when the bot reacts.
const REACT: &str = "_sonny/react";

/// The model and effort pickers' first choice: what the thread's bot
/// runs, or what Sonny gives a new lead.
const DEFAULT: &str = "default";

/// The option that picks the lead behind a thread.
const AGENT: &str = "agent";

/// `sonny acp [--store PATH] [--socket PATH]`.
pub fn run(args: &[String]) -> Result<(), String> {
    let (mut store, mut socket) = (None, None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .cloned()
                .ok_or("usage: sonny acp [--store PATH] [--socket PATH]")
        };
        match arg.as_str() {
            "--store" => store = Some(PathBuf::from(value()?)),
            "--socket" => socket = Some(PathBuf::from(value()?)),
            _ => return Err("usage: sonny acp [--store PATH] [--socket PATH]".into()),
        }
    }
    let (store, socket) = crate::daemon_paths(store, socket)?;
    let t3 = t3::T3::from_env();
    for name in t3::T3::VARS {
        // SAFETY: nothing else runs yet: no thread is started before this.
        unsafe { std::env::remove_var(name) };
    }
    let options = crate::Options {
        store,
        socket,
        bot: None,
        new: false,
        model: None,
        workspace: None,
        layout: false,
        rex_host: None,
        sound: false,
        side: None,
        embedded: false,
        window_place: None,
        lead: project::Pick::Last,
        send: None,
    };
    let agent = Arc::new(Agent {
        out: Arc::new(Out::default()),
        options,
        t3,
        sessions: Mutex::new(HashMap::new()),
        ready: OnceCell::new(),
        offer: Mutex::new(Offer::default()),
        version: AtomicI64::new(VERSION),
        forms: AtomicBool::new(false),
    });
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(agent.serve())
}

/// The client's side of the line: what Sonny writes, and the answers to
/// what it asked.
#[derive(Default)]
struct Out {
    stdout: Mutex<()>,
    next: AtomicI64,
    waiting: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
}

impl Out {
    fn send(&self, message: Value) {
        let _held = self.stdout.lock();
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{message}");
        let _ = stdout.flush();
    }

    fn answer(&self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn fail(&self, id: &Value, code: i64, message: &str) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
    }

    fn update(&self, session: &str, update: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": session, "update": update}}));
    }

    /// Ask the client: its answer, or `None` when it gave an error or none.
    async fn ask(&self, method: &str, params: Value) -> Option<Value> {
        Self::answer_to(self.asking(method, params)).await
    }

    /// Ask the client now, to wait on its answer later: questions put
    /// together reach it in the order they were put.
    fn asking(&self, method: &str, params: Value) -> oneshot::Receiver<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut waiting) = self.waiting.lock() {
            waiting.insert(id, tx);
        }
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx
    }

    /// The client's answer to what [`Out::asking`] asked.
    async fn answer_to(asked: oneshot::Receiver<Value>) -> Option<Value> {
        asked.await.ok()?.get("result").cloned()
    }

    fn answered(&self, message: Value) {
        let Some(id) = message["id"].as_i64() else {
            return;
        };
        let waiting = self.waiting.lock().ok().and_then(|mut w| w.remove(&id));
        if let Some(tx) = waiting {
            let _ = tx.send(message);
        }
    }
}

struct Agent {
    out: Arc<Out>,
    options: crate::Options,
    t3: Option<t3::T3>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// The daemon, reached once, with Sonny's skills installed first. A
    /// failure is not kept: the next thread tries again, so a daemon
    /// restarted under a running T3 is reached without restarting T3.
    ready: OnceCell<()>,
    /// The models the providers list, and the daemon's providers.
    offer: Mutex<Offer>,
    /// The version the client asked for, when older.
    version: AtomicI64,
    /// Whether the client fills in forms (`elicitation/create`), which
    /// put the lead's questions to the person.
    forms: AtomicBool,
}

impl Agent {
    async fn serve(self: Arc<Self>) -> Result<(), String> {
        let (tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
        // Stdin is read on a thread of its own: tokio's stdin is too.
        std::thread::spawn(move || {
            for line in std::io::stdin().lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        while let Some(line) = lines.recv().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                if !line.trim().is_empty() {
                    self.out.send(json!({"jsonrpc": "2.0", "id": null,
                        "error": {"code": -32700, "message": "not JSON"}}));
                }
                continue;
            };
            match (message.get("method"), message.get("id")) {
                (Some(_), Some(_)) => {
                    let agent = self.clone();
                    tokio::spawn(async move { agent.request(message).await });
                }
                (Some(_), None) => self.notified(&message).await,
                (None, Some(_)) => self.out.answered(message),
                (None, None) => {}
            }
        }
        Ok(())
    }

    async fn request(self: Arc<Self>, message: Value) {
        let id = &message["id"];
        let params = &message["params"];
        let answered = match message["method"].as_str().unwrap_or("") {
            "initialize" => Ok(self.initialize(params)),
            "authenticate" | "auth/login" => Ok(json!({})),
            "session/new" => self.open(params, None).await,
            "session/resume" | "session/load" => match params["sessionId"].as_str() {
                Some(session) => self.open(params, Some(session)).await,
                None => Err((INVALID_PARAMS, "sessionId is missing".into())),
            },
            "session/prompt" => match self.session(params) {
                Some(session) => session.prompt(&self, id, params).await,
                None => Err((INVALID_PARAMS, "no such session".into())),
            },
            "session/list" => self.listed(params).await,
            REACT => match self.session(params) {
                Some(session) => session.react(&self, params).await,
                None => Err((INVALID_PARAMS, "no such session".into())),
            },
            "session/set_config_option" => match self.session(params) {
                Some(session) => session.set_option(&self, params).await,
                None => Err((INVALID_PARAMS, "no such session".into())),
            },
            "session/close" => {
                let closed = params["sessionId"].as_str().and_then(|session| {
                    self.sessions
                        .lock()
                        .ok()
                        .and_then(|mut s| s.remove(session))
                });
                if let Some(closed) = closed {
                    closed.stop();
                }
                Ok(json!({}))
            }
            other => Err((NOT_FOUND, format!("{other} is not a method Sonny has"))),
        };
        match answered {
            Ok(Value::Null) => {}
            Ok(result) => self.out.answer(id, result),
            Err((code, message)) => self.out.fail(id, code, &message),
        }
    }

    async fn notified(&self, message: &Value) {
        if message["method"] == "session/cancel"
            && let Some(session) = self.session(&message["params"])
        {
            session.cancel().await;
        }
    }

    fn session(&self, params: &Value) -> Option<Arc<Session>> {
        let id = params["sessionId"].as_str()?;
        self.sessions.lock().ok()?.get(id).cloned()
    }

    fn initialize(&self, params: &Value) -> Value {
        let asked = params["protocolVersion"].as_i64().unwrap_or(VERSION);
        let version = asked.clamp(1, VERSION);
        self.version.store(version, Ordering::Relaxed);
        let forms = params["clientCapabilities"]["elicitation"]["form"].is_object();
        self.forms.store(forms, Ordering::Relaxed);
        json!({
            "protocolVersion": version,
            "agentCapabilities": {
                "loadSession": false,
                "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
                "mcpCapabilities": {"http": false, "sse": false, "acp": false},
                "sessionCapabilities": {"resume": {}, "close": {}, "list": {}}
            },
            "authMethods": [],
            "agentInfo": {"name": "sonny", "title": "Sonny", "version": env!("CARGO_PKG_VERSION")}
        })
    }

    /// Whether the client follows work the agent starts on its own.
    fn v2(&self) -> bool {
        self.version.load(Ordering::Relaxed) >= 2
    }

    /// A thread's options: its model, picked from what the providers list
    /// as `/model` lists it, with the model list's notes and the models
    /// only it names, and its effort. Sonny's own choice is first in both.
    /// A bot's thread offers only its family's models, since a bot changes
    /// model only within it, and its family's efforts; `bound` is the
    /// model the bot runs and the daemon's providers.
    fn options(&self, model: &str, effort: &str, bound: Option<(&str, &Value)>) -> Value {
        let offer = self
            .offer
            .lock()
            .map(|offer| offer.clone())
            .unwrap_or_default();
        let providers = bound.map_or(&offer.providers, |(_, providers)| providers);
        let mut models = vec![json!({"value": DEFAULT, "name": "Default",
            "description": "The model the lead or task runs, or the one Sonny gives a new lead"})];
        for listed in offer.models {
            if let Some((current, _)) = bound
                && family(providers, &listed.id) != family(providers, current)
            {
                continue;
            }
            models.push(choice(
                &listed.id,
                listed.name.as_deref(),
                listed.note.as_deref(),
            ));
        }
        if !models.iter().any(|choice| choice["value"] == model) {
            models.push(choice(model, None, None));
        }
        // A bot's family's efforts; before there is one, the picked
        // model's, or every family's.
        let of = bound
            .map(|(current, _)| current)
            .or(Some(model).filter(|model| *model != DEFAULT));
        let mut efforts = vec![json!({"value": DEFAULT, "name": "Default",
            "description": "The effort the lead or task runs at, or the one Sonny gives a new lead"})];
        for level in levels(providers, of) {
            efforts.push(json!({"value": level, "name": level}));
        }
        if !efforts.iter().any(|choice| choice["value"] == effort) {
            efforts.push(json!({"value": effort, "name": effort}));
        }
        // Both names: version 2's schema says `configId`, but T3 Code finds
        // an option by `id` whatever the version.
        json!([{"id": "model", "configId": "model", "name": "Model", "category": "model",
            "type": "select", "currentValue": model, "options": models},
        {"id": "effort", "configId": "effort", "name": "Effort", "category": "thought_level",
            "type": "select", "currentValue": effort, "options": efforts}])
    }

    /// What the providers list now, as `/model` asks them, with what
    /// the model list says of them and the models only it names. A
    /// listing that does not come in time leaves the file's.
    async fn list(&self, client: Option<&Client>) {
        let path = agent_client::models::path();
        let file = path
            .as_deref()
            .and_then(|path| agent_client::models::read(path).ok())
            .unwrap_or_default();
        let hidden = path
            .as_deref()
            .map(agent_client::models::hidden)
            .unwrap_or_default();
        let mut offer = Offer::default();
        if let Some(client) = client {
            offer.providers = client.providers().clone();
            let listing =
                tokio::time::timeout(LISTING, client.request("provider_models", json!({}))).await;
            if let Ok(Ok(listing)) = listing
                && let Some(providers) = listing["providers"].as_object()
            {
                let mut names: Vec<&String> = providers.keys().collect();
                names.sort();
                for name in names {
                    for model in providers[name]["models"].as_array().into_iter().flatten() {
                        if let Some(id) = model["id"].as_str()
                            && !hidden.contains(&format!("{name}/{id}"))
                        {
                            offer.models.push(Listed {
                                id: format!("{name}/{id}"),
                                name: model["name"].as_str().map(str::to_owned),
                                note: None,
                            });
                        }
                    }
                }
            }
        }
        for model in file {
            match offer.models.iter_mut().find(|listed| listed.id == model.id) {
                Some(listed) => listed.note = model.note,
                None => offer.models.push(Listed {
                    id: model.id,
                    name: None,
                    note: model.note,
                }),
            }
        }
        if let Ok(mut kept) = self.offer.lock() {
            *kept = offer;
        }
    }

    /// The daemon, started as `sonny` starts it, with Sonny's skills
    /// installed before any lead is made, so its instructions list them.
    async fn ready(&self) -> Result<(), String> {
        self.ready
            .get_or_try_init(|| async {
                crate::skills::install_own();
                crate::approvals::at_start();
                crate::trigger::at_start();
                crate::connect(&self.options, crate::Asking::AtStart)
                    .await
                    .map(drop)
            })
            .await
            .map(drop)
    }

    /// A thread starts (`session/new`) or comes back (`session/resume`).
    /// A new one is not a bot yet: its first prompt says whether it is a
    /// task's thread or its folder's lead.
    async fn open(self: &Arc<Self>, params: &Value, resumed: Option<&str>) -> Reply {
        let cwd = params["cwd"]
            .as_str()
            .map(PathBuf::from)
            .filter(|cwd| cwd.is_absolute())
            .ok_or((INVALID_PARAMS, "cwd must be an absolute path".to_owned()))?;
        self.ready().await.map_err(|e| (FAILED, e))?;
        // One connection for what the thread's pickers list.
        let lister = Client::connect(&self.options.socket).await.ok();
        let lister = lister.as_ref().map(|(client, _)| client.as_ref());
        self.list(lister).await;
        let leads = match lister {
            Some(client) if self.t3.is_none() => folder_leads(client, &cwd).await,
            _ => Vec::new(),
        };
        // A listed id names a lead, or a task one of them started.
        let listed = resumed.and_then(listed_bot);
        let known = match (&listed, lister) {
            (Some(pinned), lister) if !project::is_lead(&pinned.bot) => match lister {
                Some(client) => a_task(client, pinned).await,
                None => false,
            },
            _ => true,
        };
        if let Some(client) = lister {
            client.close().await;
        }
        let listed = listed.filter(|_| known);
        let id = match resumed {
            Some(id) => id.to_owned(),
            None => crate::unique("sonny"),
        };
        let session = Arc::new(Session {
            id: id.clone(),
            cwd,
            state: AsyncMutex::new(State {
                leads,
                ..State::default()
            }),
            follower: Mutex::new(None),
            switching: AsyncMutex::new(()),
        });
        // Open before it follows its bot; one it replaces stops following.
        let replaced = self
            .sessions
            .lock()
            .ok()
            .and_then(|mut sessions| sessions.insert(id.clone(), session.clone()));
        if let Some(old) = replaced {
            old.stop();
        }
        // A thread comes back as the bot it was, or not at all.
        let bound = match resumed {
            // A listed id names its lead itself, whatever the session was
            // moved to since.
            Some(id) => match listed.or_else(|| t3::recall(id)) {
                Some(pinned) => {
                    session
                        .bind(self, Some(pinned), false, &project::Pick::Last)
                        .await
                }
                None => Err(format!("Sonny does not know thread {id}")),
            },
            None => Ok(()),
        };
        if let Err(error) = bound {
            if let Ok(mut sessions) = self.sessions.lock() {
                sessions.remove(&id);
            }
            return Err((FAILED, error));
        }
        let options = session.options(self).await;
        Ok(match resumed {
            Some(_) => json!({"configOptions": options}),
            None => json!({"sessionId": id, "configOptions": options}),
        })
    }

    /// `session/list`: the leads Sonny can open a session on, the one
    /// folder's when the client names a `cwd`, each with the tasks it
    /// started, else every project's leads, a page at a time. Each is
    /// listed by an id `session/resume` takes, which names the bot and its
    /// identity, so one removed and made again under its name is not the
    /// one reopened.
    async fn listed(&self, params: &Value) -> Reply {
        self.ready().await.map_err(|e| (FAILED, e))?;
        let (client, _events) = Client::connect(&self.options.socket)
            .await
            .map_err(|e| (FAILED, e.to_string()))?;
        let cwd = params["cwd"].as_str().map(PathBuf::from);
        let listing = match &cwd {
            Some(cwd) if !cwd.is_absolute() => {
                Err((INVALID_PARAMS, "cwd must be an absolute path".to_owned()))
            }
            Some(cwd) => match project::folder_project(cwd) {
                Ok((project, dir)) => {
                    let leads = project::leads(&client, &project, &dir)
                        .await
                        .map_err(|e| (FAILED, e));
                    match leads {
                        Ok(leads) => with_tasks(&client, leads)
                            .await
                            .map(|listed| (listed, None))
                            .map_err(|e| (FAILED, e)),
                        Err(error) => Err(error),
                    }
                }
                // A folder no lead can work in has none.
                Err(_) => Ok((Vec::new(), None)),
            },
            None => project::every_lead(&client, params["cursor"].as_str())
                .await
                .map_err(|e| (FAILED, e)),
        };
        let store = client.store().map(str::to_owned);
        client.close().await;
        let (bots, next) = listing?;
        let id = |bot: &Value| {
            let (name, bot_id) = (bot["name"].as_str()?, bot["bot_id"].as_i64()?);
            Some(lead_session(name, bot_id, store.as_deref()))
        };
        let sessions: Vec<Value> = bots
            .iter()
            .filter_map(|bot| {
                let mut listed = json!({"sessionId": id(bot)?, "cwd": bot["workspace"],
                    "title": bot["name"]});
                // A task names the lead that started it.
                if !project::is_lead(bot["name"].as_str()?)
                    && let (Some(lead), Some(by)) =
                        (bot["created_by"].as_str(), bot["created_by_id"].as_i64())
                {
                    listed["_meta"] = json!({"lead": lead_session(lead, by, store.as_deref())});
                }
                Some(listed)
            })
            .collect();
        let mut listed = json!({"sessions": sessions});
        if let Some(next) = next {
            listed["nextCursor"] = json!(next);
        }
        Ok(listed)
    }
}

/// The agent picker: the folder's leads, after `default`, the lead the
/// thread's first prompt opens as `sonny` opens one there. `current` is
/// the thread's bot, or the lead picked for it, which is listed whatever
/// folder it works in.
fn agent_option(leads: &[String], current: &str) -> Value {
    let mut choices = vec![json!({"value": DEFAULT, "name": "Default",
        "description": "A new lead named after your first words, or the folder's first lead when it has none"})];
    for lead in leads {
        choices.push(json!({"value": lead, "name": lead}));
    }
    if current != DEFAULT && !leads.iter().any(|lead| lead == current) {
        choices.push(json!({"value": current, "name": current}));
    }
    json!({"id": AGENT, "configId": AGENT, "name": "Agent",
        "description": "The lead behind this thread", "type": "select",
        "currentValue": current, "options": choices})
}

/// Who a client says sent a prompt or chose for a call, by its
/// `_meta.person`: a name as the daemon takes a prompt's `person`, 1 to
/// 128 of `A-Z a-z 0-9 . _ -`. The daemon shows the model who wrote it,
/// and none of it is the daemon's own person's consent.
fn person(params: &Value) -> Result<Option<&str>, String> {
    match &params["_meta"]["person"] {
        Value::Null => Ok(None),
        Value::String(name)
            if (1..=128).contains(&name.len())
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) =>
        {
            Ok(Some(name))
        }
        _ => Err("_meta.person must be 1 to 128 of A-Z a-z 0-9 . _ -".into()),
    }
}

/// The id `session/list` gives a lead or a task: its name, its identity,
/// and the lineage of the store it is in, since identities are a store's
/// own.
fn lead_session(bot: &str, bot_id: i64, store: Option<&str>) -> String {
    match store {
        Some(store) => format!("{bot}#{bot_id}@{store}"),
        None => format!("{bot}#{bot_id}"),
    }
}

/// The bot a listed session id names, as `session/resume` reopens it:
/// that bot of that store, or none. A lead's, or one `a_task` takes.
fn listed_bot(session: &str) -> Option<t3::Pinned> {
    let (listed, store) = match session.rsplit_once('@') {
        Some((listed, store)) => (listed, Some(store.to_owned())),
        None => (session, None),
    };
    let (bot, bot_id) = listed.rsplit_once('#')?;
    (!bot.is_empty()).then_some(())?;
    Some(t3::Pinned {
        bot: bot.to_owned(),
        bot_id: Some(bot_id.parse().ok()?),
        store,
    })
}

/// Whether `pinned`, no lead, is a task a lead started: made by a lead,
/// and not a side chat of it. That one of that identity, or none.
async fn a_task(client: &Client, pinned: &t3::Pinned) -> bool {
    let inspected = client
        .request(
            "inspect",
            json!({"bot": pinned.bot, "bot_id": pinned.bot_id}),
        )
        .await;
    inspected.is_ok_and(|record| task(&record))
}

/// Whether `record`, a bot as `bots` or `inspect` gives it, is a task a
/// lead started.
fn task(record: &Value) -> bool {
    let (name, maker) = (record["name"].as_str(), record["created_by"].as_str());
    name.is_some_and(|name| !project::is_lead(name))
        && maker.is_some_and(project::is_lead)
        && crate::side_copy(record).is_none()
}

/// The tasks the lead `lead_id` started, every one, by name.
async fn tasks_of(client: &Client, lead_id: i64) -> Result<Vec<Value>, String> {
    let mut found = Vec::new();
    let mut after = Value::Null;
    loop {
        let page = client
            .request(
                "bots",
                json!({"created_by_id": lead_id, "limit": 256, "after": after}),
            )
            .await
            .map_err(|e| e.to_string())?;
        found.extend(
            (page["bots"].as_array().into_iter().flatten())
                .filter(|bot| task(bot))
                .cloned(),
        );
        after = page["next_after"].clone();
        if after.is_null() {
            return Ok(found);
        }
    }
}

/// `leads`, each followed by the tasks it started.
async fn with_tasks(client: &Client, leads: Vec<Value>) -> Result<Vec<Value>, String> {
    let mut listed = Vec::with_capacity(leads.len());
    for lead in leads {
        let tasks = match lead["bot_id"].as_i64() {
            Some(lead_id) => tasks_of(client, lead_id).await?,
            None => Vec::new(),
        };
        listed.push(lead);
        listed.extend(tasks);
    }
    Ok(listed)
}

/// The bot a lead's `start_task` call names, as `start_task` names it.
fn task_bot(project: &str, arguments: &Value) -> Option<String> {
    let short = arguments["name"].as_str()?.trim();
    Some(match short.strip_prefix(&format!("{project}.")) {
        Some(_) => short.to_owned(),
        None => format!("{project}.{short}"),
    })
}

/// The leads working in the folder `cwd` reaches, by name, first one
/// first: what the agent picker offers.
async fn folder_leads(client: &Client, cwd: &Path) -> Vec<String> {
    let Ok((project, dir)) = project::folder_project(cwd) else {
        return Vec::new();
    };
    (project::leads(client, &project, &dir)
        .await
        .unwrap_or_default())
    .iter()
    .filter_map(|lead| lead["name"].as_str().map(str::to_owned))
    .collect()
}

type Reply = Result<Value, (i64, String)>;

/// A thread: the bot it is, once its first prompt says, and the person's
/// prompt waiting for its turn's end.
struct Session {
    id: String,
    cwd: PathBuf,
    state: AsyncMutex<State>,
    /// What follows its bot, stopped when the thread closes.
    follower: Mutex<Option<tokio::task::AbortHandle>>,
    /// Held while the bot's model changes: one change at a time.
    switching: AsyncMutex<()>,
}

impl Session {
    fn stop(&self) {
        if let Some(follower) = self.follower.lock().ok().and_then(|mut f| f.take()) {
            follower.abort();
        }
    }
}

#[derive(Default)]
struct State {
    bound: Option<Bound>,
    /// The person's prompt, answered when its turn ends; its turn once the
    /// daemon named it.
    prompt: Option<(Value, Option<i64>)>,
    /// The turn running, and whether this thread said it started.
    running: Option<i64>,
    /// Words since the last call, by kind, and the part of the turn they
    /// are in: a new message after each call.
    part: u32,
    /// The words of this part sent as they streamed.
    streamed: String,
    /// The calls shown running, by turn and call: a later turn may use a
    /// lifted call's id again.
    calls: HashMap<(Option<i64>, String), (String, Value)>,
    /// The calls a message lifted from their turn, which end later, as
    /// `lifted_ended` says: kept past their turn's end.
    lifted: HashSet<(Option<i64>, String)>,
    /// Calls put to the person, by call and request, and the lead's
    /// questions, by ask.
    asked: HashSet<String>,
    /// The last of the bot's kept events this thread handled, to follow on
    /// from when its stream drops.
    cursor: i64,
    /// The person stopped the prompt before its turn was known.
    cancelled: bool,
    /// The last steer a running turn took in, and that turn: for a prompt
    /// whose steer went in before its turn was named.
    steered: Option<(i64, i64)>,
    /// The model and effort the person picked, which the bot takes when
    /// it is next idle; a new lead starts on the model.
    picked: Option<String>,
    effort: Option<String>,
    /// The lead the client picked for a thread that is no bot yet, which
    /// its first prompt opens.
    agent: Option<String>,
    /// The leads of the thread's folder, as the agent picker lists them.
    leads: Vec<String>,
    /// Tasks whose threads are opening, as children of the person's prompt
    /// while it runs, and the answers of the prompts whose turns ended
    /// meanwhile, held until they are open.
    opening: usize,
    held: Vec<(Value, Value)>,
    /// The bot's replies this thread showed, newest last, each by its
    /// `messageId` with the bot's identity, its node and first line: what
    /// a client reacts to, while the thread is still that bot.
    replies: VecDeque<(String, Option<i64>, i64, String)>,
}

/// How many of the bot's latest replies a client may react to.
const REPLIES: usize = 64;

impl State {
    /// A turn's calls are over with it, but for those a message lifted.
    fn end_calls(&mut self) {
        let lifted = &self.lifted;
        self.calls.retain(|call, _| lifted.contains(call));
    }
}

struct Bound {
    bot: String,
    /// Its identity, which every prompt names: one sent after the bot was
    /// removed and another made under its name reaches neither.
    bot_id: Option<i64>,
    client: Arc<Client>,
    /// The lead's project, for a lead's thread.
    project: Option<String>,
    workspace: String,
    /// The folder a moved lead comes back to with its next turn.
    moving: Option<String>,
    /// `provider/model`, as the daemon names it, and its effort.
    model: String,
    effort: Option<String>,
}

/// What a thread may pick from: the models listed, and the daemon's
/// providers, whose families and efforts they are.
#[derive(Clone, Default)]
struct Offer {
    models: Vec<Listed>,
    providers: Value,
}

/// A model a thread may pick: `provider/model`, the name its provider's
/// listing gives it, and the model list's note.
#[derive(Clone)]
struct Listed {
    id: String,
    name: Option<String>,
    note: Option<String>,
}

/// A model picker's choice for `provider/model`: shown by the name its
/// provider lists, else the model's own id, beside its provider, since an
/// id such as Bedrock's `global.anthropic.…` is too long to read in the
/// picker. The full id is in its description, after the file's note.
fn choice(id: &str, name: Option<&str>, note: Option<&str>) -> Value {
    // The provider's own name for it, else the one Sonny gives it.
    let label = match (name, crate::models::provider(id)) {
        (Some(name), Some(provider)) => format!("{name} · {provider}"),
        (Some(name), None) => name.to_owned(),
        (None, _) => crate::models::named(id),
    };
    let description = match note {
        Some(note) => format!("{note} ({id})"),
        None => id.to_owned(),
    };
    json!({"value": id, "name": label, "description": description})
}

impl Session {
    /// Make this thread `bot`'s, or its folder's lead's, and follow it.
    /// `claimed`: a task that waited for this thread, whose first turn
    /// runs already. With no bot, `pick` picks which of the folder's leads.
    /// A bot pinned to its identity and store is that one or none.
    async fn bind(
        self: &Arc<Self>,
        agent: &Arc<Agent>,
        bot: Option<t3::Pinned>,
        claimed: bool,
        pick: &project::Pick,
    ) -> Result<(), String> {
        let (client, mut events) = Client::connect(&agent.options.socket)
            .await
            .map_err(|e| e.to_string())?;
        // A pick made before the thread had a bot is for a lead Sonny
        // makes: T3 Code puts its last pick to every thread it opens, and
        // the folder's lead that is there already, every other thread's
        // too, keeps its own.
        let (bot, expected, project, moving, made) = match bot {
            Some(t3::Pinned { bot, bot_id, store }) => {
                if let Some(store) = store
                    && client.store() != Some(store.as_str())
                {
                    return Err(format!(
                        "this thread was {bot} of another store than this daemon's"
                    ));
                }
                if bot_id.is_some() {
                    client
                        .request("inspect", json!({"bot": bot, "bot_id": bot_id}))
                        .await
                        .map_err(|e| format!("this thread's {bot} is gone: {e}"))?;
                }
                let project = project::lead_project(&bot).map(str::to_owned);
                services(agent, &client, &bot, project.as_deref()).await?;
                (bot, bot_id, project, None, false)
            }
            None => {
                let picked = self.state.lock().await.picked.clone();
                let lead = project::open(&client, &self.cwd, picked.as_deref(), pick).await?;
                let moving = lead.moving.as_ref().map(|dir| dir.display().to_string());
                setup_lead(agent, &self.id, &client, &lead, moving.as_deref()).await?;
                (lead.bot, None, Some(lead.project), moving, lead.new)
            }
        };
        client
            .request("follow", json!({"bot": bot, "after": 0}))
            .await
            .map_err(|e| e.to_string())?;
        // Calls started before this thread came and not done yet: a task a
        // lead is starting still opens its thread when it is done.
        let (mut started, mut lifted) = (HashMap::new(), HashSet::new());
        let cursor = live(&mut events, |event| {
            let data = &event["data"];
            let call = (
                event["turn"].as_i64(),
                data["call_id"].as_str().unwrap_or("").to_owned(),
            );
            match event["event"].as_str() {
                Some("tool_started") => {
                    started.insert(call, data.clone());
                }
                Some("call_lifted") if started.contains_key(&call) => {
                    lifted.insert(call);
                }
                Some("tool_completed") if !lifted.contains(&call) => {
                    started.remove(&call);
                }
                Some("lifted_ended") if data["waits_for"] != "answer" => {
                    lifted.remove(&call);
                    started.remove(&call);
                }
                _ => {}
            }
            async {}
        })
        .await;
        let record = client
            .request("inspect", json!({"bot": bot, "bot_id": expected}))
            .await
            .map_err(|e| e.to_string())?;
        let bot_id = record["bot_id"].as_i64();
        let workspace = record["workspace"].as_str().unwrap_or_default().to_owned();
        let running = record["running_turn"].as_i64();
        let model = format!(
            "{}/{}",
            record["provider"].as_str().unwrap_or(""),
            record["model"].as_str().unwrap_or("")
        );
        let effort = record["effort"].as_str().map(str::to_owned);
        let mut calls = HashMap::new();
        for (key, data) in started {
            if (running.is_some() && key.0 == running) || lifted.contains(&key) {
                let (call, name, arguments) = started_call(&client, &bot, &data).await;
                if name != reactions::TOOL && name != plan::TOOL {
                    calls.insert((key.0, call), (name, arguments));
                }
            }
        }
        lifted.retain(|call| calls.contains_key(call));
        let pinned = t3::Pinned {
            bot: bot.clone(),
            bot_id,
            store: client.store().map(str::to_owned),
        };
        t3::remember(&self.id, &pinned);
        // A lead made just now joins the picker's list.
        let leads = match agent.t3 {
            None => Some(folder_leads(&client, &self.cwd).await),
            Some(_) => None,
        };
        {
            let mut state = self.state.lock().await;
            if let Some(leads) = leads {
                state.leads = leads;
                state.agent = None;
            }
            state.running = running;
            state.calls = calls;
            state.lifted = lifted;
            state.cursor = cursor;
            // A task keeps the model its lead gave it, whatever T3 Code
            // picked for the thread it opened.
            if !made || state.picked.as_ref() == Some(&model) {
                state.picked = None;
            }
            if !made || state.effort == effort {
                state.effort = None;
            }
            state.bound = Some(Bound {
                bot,
                bot_id,
                client,
                project,
                workspace,
                moving,
                model,
                effort,
            });
            // The thread T3 Code opened for a task stands for its first
            // turn, which runs already.
            if claimed && let Some(turn) = state.running {
                state.prompt = state.prompt.take().map(|(id, _)| (id, Some(turn)));
            }
        }
        self.show_options(agent).await;
        let (session, agent) = (self.clone(), agent.clone());
        // Calls already waiting for the person, whose events were before
        // this thread came, are put to it too.
        let waiting = {
            let state = self.state.lock().await;
            (state.bound.as_ref()).map(|b| (b.client.clone(), b.bot.clone(), state.running))
        };
        let follower = self.clone();
        // A lead's tasks' questions come to its thread too, as Sonny's lead
        // window pins them; T3 Code shows each task's in its own thread.
        let tasks = match &waiting {
            Some((_, bot, _))
                if agent.forms.load(Ordering::Relaxed)
                    && agent.t3.is_none()
                    && project::is_lead(bot) =>
            {
                Some(bot.clone()).zip(bot_id)
            }
            _ => None,
        };
        let handle = tokio::spawn(async move {
            if let Some((client, bot, turn)) = &waiting {
                // With no turn running too: calls a message lifted wait
                // past theirs.
                follower.approvals(&agent, client, bot, *turn).await;
                if agent.forms.load(Ordering::Relaxed) {
                    follower.questions(&agent, client, bot).await;
                }
            }
            match tasks {
                Some(lead) => {
                    let watching = follower.clone().watch_tasks(&agent, lead);
                    tokio::join!(follower.follow(&agent, events), watching);
                }
                None => follower.follow(&agent, events).await,
            }
        });
        if let Ok(mut old) = session.follower.lock()
            && let Some(old) = old.replace(handle.abort_handle())
        {
            old.abort();
        }
        Ok(())
    }

    /// The thread's options as they are now.
    async fn options(&self, agent: &Agent) -> Value {
        let state = self.state.lock().await;
        let bound = state.bound.as_ref();
        let model = (state.picked.as_deref())
            .or(bound.map(|bound| bound.model.as_str()))
            .unwrap_or(DEFAULT);
        let effort = (state.effort.as_deref())
            .or(bound.and_then(|bound| bound.effort.as_deref()))
            .unwrap_or(DEFAULT);
        let mut options = agent.options(
            model,
            effort,
            bound.map(|bound| (bound.model.as_str(), bound.client.providers())),
        );
        // T3 Code carries a thread's option picks over to the next thread
        // it opens, which would put every new thread on one lead: there,
        // `/switch` picks.
        if agent.t3.is_none()
            && let Value::Array(options) = &mut options
        {
            let current = (bound.map(|bound| bound.bot.as_str()))
                .or(state.agent.as_deref())
                .unwrap_or(DEFAULT);
            options.push(agent_option(&state.leads, current));
        }
        options
    }

    async fn show_options(&self, agent: &Agent) {
        let options = self.options(agent).await;
        agent.out.update(
            &self.id,
            json!({"sessionUpdate": "config_option_update", "configOptions": options}),
        );
    }

    /// A client's reaction to one of the bot's replies, by its
    /// `messageId`: attached to the bot's next model call, as a reaction
    /// in Sonny's window is, so it starts no turn. Without a person named
    /// it is the person's own, kept where the window draws it, and taking
    /// it off takes it off there; the bot is told only of one added.
    async fn react(&self, agent: &Agent, params: &Value) -> Reply {
        let person = person(params).map_err(|error| (INVALID_PARAMS, error))?;
        let emoji = params["emoji"].as_str().map(str::trim).unwrap_or("");
        if crate::transcript::reaction(emoji) != Some(emoji) {
            return Err((INVALID_PARAMS, "emoji must be one emoji".into()));
        }
        let remove = params["remove"] == true;
        let (client, bot, bot_id, node, first, last) = {
            let state = self.state.lock().await;
            let Some(bound) = &state.bound else {
                return Err((INVALID_PARAMS, "the thread has no bot yet".into()));
            };
            let message = params["messageId"].as_str().unwrap_or("");
            // Only the replies of the bot the thread is now.
            let replies: Vec<_> = (state.replies.iter())
                .filter(|(_, by, ..)| *by == bound.bot_id)
                .collect();
            let Some(at) = replies.iter().rposition(|(id, ..)| id == message) else {
                return Err((
                    INVALID_PARAMS,
                    "messageId is none of the latest replies this thread showed".into(),
                ));
            };
            let (_, _, node, first) = replies[at].clone();
            let last = at + 1 == replies.len();
            (
                bound.client.clone(),
                bound.bot.clone(),
                bound.bot_id,
                node,
                first,
                last,
            )
        };
        let kept = match (person, bot_id) {
            (None, Some(bot_id)) => {
                let store = agent_client::socket::canonical_store(&agent.options.store)
                    .unwrap_or_else(|_| agent.options.store.clone());
                let mut kept = reactions::Reactions::load(Some(&reactions::key(&store, bot_id)));
                if !(kept.set(node, emoji, !remove)).map_err(|error| (FAILED, error))? {
                    return Ok(json!({}));
                }
                Some(kept)
            }
            _ => None,
        };
        if remove {
            return Ok(json!({}));
        }
        let told = reactions::message(emoji, (!last).then_some(first.as_str()));
        let mut submitted = json!({"bot": bot, "bot_id": bot_id, "request_id": crate::unique("acp"),
            "prompt": told, "delivery": "attach"});
        if let Some(person) = person {
            submitted["person"] = json!(person);
        }
        if let Err(error) = client.request("submit", submitted).await {
            if let Some(mut kept) = kept {
                let _ = kept.set(node, emoji, false);
            }
            return Err((FAILED, error.to_string()));
        }
        Ok(json!({}))
    }

    /// The person picks a model or an effort. A bot's thread takes a
    /// model of its family only, at once when the bot is idle and with its
    /// next turn when it is not; a thread that is no bot yet keeps it for
    /// the lead it makes.
    async fn set_option(self: &Arc<Self>, agent: &Arc<Agent>, params: &Value) -> Reply {
        let option = params.get("configId").unwrap_or(&params["id"]);
        let Some(value) = params["value"].as_str() else {
            return Err((INVALID_PARAMS, "an option is picked by its value".into()));
        };
        let _one = self.switching.lock().await;
        if option == AGENT && agent.t3.is_none() {
            self.pick_agent(agent, value).await?;
            return Ok(json!({"configOptions": self.options(agent).await}));
        }
        if option == "effort" {
            let mut state = self.state.lock().await;
            state.effort = match &state.bound {
                Some(bound) if value != DEFAULT && Some(value) != bound.effort.as_deref() => {
                    let of = state.picked.as_deref().unwrap_or(&bound.model);
                    if !levels(bound.client.providers(), Some(of)).contains(&value) {
                        return Err((INVALID_PARAMS, format!("{of} has no effort {value}")));
                    }
                    Some(value.to_owned())
                }
                Some(_) => None,
                None => (value != DEFAULT).then(|| value.to_owned()),
            };
        } else if option != "model" {
            return Err((INVALID_PARAMS, format!("Sonny has no option {option}")));
        } else {
            let mut state = self.state.lock().await;
            let picked = match &state.bound {
                Some(bound) if value != DEFAULT && value != bound.model => {
                    let providers = bound.client.providers();
                    let wanted = family(providers, value);
                    if wanted.is_null() {
                        return Err((INVALID_PARAMS, format!("no provider serves {value}")));
                    }
                    if wanted != family(providers, &bound.model) {
                        return Err((
                            INVALID_PARAMS,
                            format!(
                                "{} stays in {}'s family: its history is in that family's shape",
                                bound.bot, bound.model
                            ),
                        ));
                    }
                    Some(value.to_owned())
                }
                Some(_) => None,
                None => (value != DEFAULT).then(|| value.to_owned()),
            };
            state.picked = picked;
        }
        self.switch(agent)
            .await
            .map_err(|error| (INVALID_PARAMS, error))?;
        Ok(json!({"configOptions": self.options(agent).await}))
    }

    /// The client picks the lead behind the thread: one of its folder's,
    /// or `default`, the lead its first prompt opens as `sonny` would. A
    /// thread that is no bot yet keeps the pick for its first prompt; a
    /// lead's thread moves to the lead picked, once its prompt is answered.
    /// The caller holds `switching`.
    async fn pick_agent(
        self: &Arc<Self>,
        agent: &Arc<Agent>,
        value: &str,
    ) -> Result<(), (i64, String)> {
        let mut state = self.state.lock().await;
        let known = state.leads.iter().any(|lead| lead == value);
        if value != DEFAULT && !known && state.bound.as_ref().is_none_or(|b| b.bot != value) {
            return Err((
                INVALID_PARAMS,
                format!("{value} is not a lead of this folder"),
            ));
        }
        let Some(bound) = &state.bound else {
            state.agent = (value != DEFAULT).then(|| value.to_owned());
            return Ok(());
        };
        if value == DEFAULT || value == bound.bot {
            return Ok(());
        }
        if state.prompt.is_some() {
            return Err((
                INVALID_PARAMS,
                format!("{} is answering; pick another lead once it has", bound.bot),
            ));
        }
        drop(state);
        let pick = project::Pick::Named(project::lead_short(value).to_owned());
        self.bind(agent, None, false, &pick)
            .await
            .map_err(|error| (FAILED, error))
    }

    /// Once the bot is idle, as before a prompt's turn or after a turn:
    /// the model the person picked meanwhile, and the model it runs, as
    /// another client may have changed it.
    async fn settle(&self, agent: &Agent) {
        let _one = self.switching.lock().await;
        if let Err(error) = self.switch(agent).await {
            agent.out.update(
                &self.id,
                json!({"sessionUpdate": "notice", "severity": "warning",
                "title": "Sonny kept the model", "description": error}),
            );
        }
        let Some((client, bot)) = (self.state.lock().await.bound.as_ref())
            .map(|bound| (bound.client.clone(), bound.bot.clone()))
        else {
            return;
        };
        let Ok(record) = client.request("inspect", json!({"bot": bot})).await else {
            return;
        };
        let model = format!(
            "{}/{}",
            record["provider"].as_str().unwrap_or(""),
            record["model"].as_str().unwrap_or("")
        );
        let effort = record["effort"].as_str().map(str::to_owned);
        let changed = {
            let mut state = self.state.lock().await;
            match &mut state.bound {
                Some(bound)
                    if bound.bot == bot && (bound.model != model || bound.effort != effort) =>
                {
                    bound.model = model;
                    bound.effort = effort;
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.show_options(agent).await;
        }
    }

    /// Give an idle bot the model and effort the person picked. The
    /// caller holds `switching`.
    async fn switch(&self, agent: &Agent) -> Result<(), String> {
        let (client, asked, model, effort) = {
            let state = self.state.lock().await;
            let (Some(bound), None) = (&state.bound, state.running) else {
                return Ok(());
            };
            if state.picked.is_none() && state.effort.is_none() {
                return Ok(());
            }
            let model = state.picked.clone().unwrap_or_else(|| bound.model.clone());
            let effort = state.effort.clone().or_else(|| bound.effort.clone());
            let asked = json!({"bot": bound.bot, "model": model, "effort": effort});
            (bound.client.clone(), asked, model, effort)
        };
        let answer = client.request("configure", asked).await;
        let mut state = self.state.lock().await;
        match answer {
            Ok(_) => {
                state.picked = None;
                state.effort = None;
                if let Some(bound) = &mut state.bound {
                    bound.model = model;
                    bound.effort = effort;
                }
                drop(state);
                self.show_options(agent).await;
                Ok(())
            }
            // It took a turn meanwhile: the pick waits for the next.
            Err(error) if error.code == "bot_busy" => Ok(()),
            Err(error) => {
                state.picked = None;
                state.effort = None;
                drop(state);
                self.show_options(agent).await;
                Err(error.to_string())
            }
        }
    }

    async fn prompt(self: &Arc<Self>, agent: &Arc<Agent>, id: &Value, params: &Value) -> Reply {
        let person = person(params).map_err(|error| (INVALID_PARAMS, error))?;
        let (text, images) = words(&params["prompt"]);
        if agent.t3.is_some() && images.is_empty() && t3::is_completion(&text) {
            return Ok(json!({"stopReason": "end_turn"}));
        }
        let bound = {
            let mut state = self.state.lock().await;
            state.cancelled = false;
            state.bound.is_some()
        };
        if !bound {
            // The first prompt of a thread T3 Code opened for a task is
            // that task's brief.
            let claimed = t3::claim(&text, &self.cwd);
            let task = claimed.is_some();
            // A task's thread stands for the turn the task runs already.
            if task {
                self.state.lock().await.prompt = Some((id.clone(), None));
            }
            // A new thread is a new lead of its folder, named after these
            // words, unless they are `/switch NAME`, which opens one there,
            // or `/new [NAME]`, which makes one so named.
            let chosen = lead_asked(&text);
            let picked = self.state.lock().await.agent.clone();
            let pick = match (&chosen, picked) {
                (Some(chosen), _) => chosen.clone(),
                (None, Some(lead)) => project::Pick::Named(project::lead_short(&lead).to_owned()),
                (None, None) => project::Pick::Thread(text.clone()),
            };
            let claimed = claimed.map(t3::Pinned::named);
            if let Err(error) = self.bind(agent, claimed, task, &pick).await {
                self.state.lock().await.prompt = None;
                return Err((FAILED, error));
            }
            if chosen.is_some() {
                let bot = (self.state.lock().await.bound.as_ref()).map(|b| b.bot.clone());
                let said = format!("This thread is {}.", bot.unwrap_or_default());
                agent.out.update(
                    &self.id,
                    json!({"sessionUpdate": "agent_message_chunk",
                        "messageId": format!("{}-lead", self.id), "content": {"type": "text", "text": said}}),
                );
                return Ok(json!({"stopReason": "end_turn"}));
            }
            if task {
                let mut state = self.state.lock().await;
                match &state.prompt {
                    // Its turn ended before the thread came: nothing to
                    // wait for.
                    Some((prompt, None)) if prompt == id => {
                        state.prompt = None;
                        drop(state);
                        self.say_last(agent).await;
                        return Ok(json!({"stopReason": "end_turn"}));
                    }
                    Some((prompt, Some(_))) if prompt == id => {}
                    // It ended since, and the follower answered it.
                    _ => return Ok(Value::Null),
                }
                if agent.v2() {
                    agent.out.update(
                        &self.id,
                        json!({"sessionUpdate": "state_update", "state": "running"}),
                    );
                }
                return Ok(Value::Null);
            }
        }
        // A model or effort picked while the bot worked: the bot takes it
        // before this turn.
        self.settle(agent).await;
        let mut state = self.state.lock().await;
        if std::mem::take(&mut state.cancelled) {
            return Ok(json!({"stopReason": "cancelled"}));
        }
        let Some(bound) = &state.bound else {
            return Err((FAILED, "the thread has no bot".into()));
        };
        // As in Sonny's window: a steer, joining the running turn or one
        // started meanwhile, else starting one; a move needs a turn of its
        // own, which a steer may not be, so a prompt that moves the lead
        // queues.
        let steer = state.running.is_some();
        let moving = (!steer).then_some(bound.moving.as_ref()).flatten();
        let mut asked = json!({"bot": bound.bot, "bot_id": bound.bot_id,
            "request_id": crate::unique("acp"),
            "prompt": crate::stamp::now(&text),
            "delivery": if moving.is_some() { "queue" } else { "steer" },
            "images": if images.is_empty() { Value::Null } else { json!(images) }});
        if let Some(person) = person {
            asked["person"] = json!(person);
        }
        if let Some(dir) = moving {
            asked["workspace"] = json!(dir);
        }
        let client = bound.client.clone();
        // The lead its folder opens next in Sonny.
        project::wrote_to(bound.client.store(), &bound.workspace, &bound.bot);
        // A prompt that steers joins the turn an earlier one waits on,
        // which is answered now: its words are in. Its own turn is the one
        // the daemon names for it, which the running turn may take in.
        if let Some((earlier, _)) = state.prompt.replace((id.clone(), None)) {
            agent
                .out
                .answer(&earlier, json!({"stopReason": "end_turn"}));
        }
        drop(state);
        match client.request("submit", asked).await {
            Ok(reply) => {
                let mut state = self.state.lock().await;
                if let Some(bound) = &mut state.bound
                    && !steer
                {
                    bound.moving = None;
                }
                // Stopped while it was being sent.
                if std::mem::take(&mut state.cancelled)
                    && let (Some(bound), Some(turn)) = (&state.bound, reply["turn"].as_i64())
                {
                    let _ = (bound.client)
                        .request("interrupt", json!({"bot": bound.bot, "turn": turn}))
                        .await;
                }
                let steered = state.steered;
                if let Some((prompt, turn)) = &mut state.prompt
                    && prompt == id
                {
                    *turn = match (reply["turn"].as_i64(), steered) {
                        (Some(sent), Some((steer, into))) if sent == steer => Some(into),
                        (sent, _) => sent,
                    };
                }
                Ok(Value::Null)
            }
            Err(error) => {
                self.state.lock().await.prompt = None;
                Err((FAILED, error.to_string()))
            }
        }
    }

    async fn cancel(&self) {
        let mut state = self.state.lock().await;
        let turn = state
            .running
            .or(state.prompt.as_ref().and_then(|(_, turn)| *turn));
        match (&state.bound, turn) {
            (Some(bound), Some(turn)) => {
                let _ = (bound.client)
                    .request("interrupt", json!({"bot": bound.bot, "turn": turn}))
                    .await;
            }
            // Its turn is not known yet: it is stopped once it is.
            _ if state.prompt.is_some() || state.bound.is_none() => state.cancelled = true,
            _ => {}
        }
    }

    /// The bot's last reply, for a task's thread that came after its turn.
    async fn say_last(&self, agent: &Agent) {
        let mut state = self.state.lock().await;
        let Some(bound) = &state.bound else { return };
        let (client, bot, bot_id) = (bound.client.clone(), bound.bot.clone(), bound.bot_id);
        let Ok(page) = client
            .request("history_nodes", json!({"bot": bot, "limit": 64}))
            .await
        else {
            return;
        };
        let nodes: Vec<i64> = page["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|node| node["node"].as_i64())
            .collect();
        let Ok(read) = client
            .request("history_items", json!({"bot": bot, "nodes": nodes}))
            .await
        else {
            return;
        };
        // Newest first, as the nodes were asked for.
        let reply = (read["items"].as_array().into_iter().flatten())
            .filter_map(|entry| Some((entry["node"].as_i64()?, crate::responses(&entry["item"]))))
            .flat_map(|(node, items)| items.into_iter().map(move |item| (node, item)))
            .filter(|(_, item)| item["role"] == "assistant")
            .map(|(node, item)| (node, crate::text(&item)))
            .find(|(_, text)| !text.is_empty());
        if let Some((node, reply)) = reply {
            let message = format!("{bot}-last");
            agent.out.update(
                &self.id,
                json!({"sessionUpdate": "agent_message_chunk",
                "messageId": message, "content": {"type": "text", "text": reply}}),
            );
            // A client reacts to it as to any reply it was shown.
            if state.replies.len() == REPLIES {
                state.replies.pop_front();
            }
            (state.replies).push_back((message, bot_id, node, first_line(&reply)));
        }
    }

    /// The bot's events, as this thread's updates, until it is closed.
    async fn follow(self: Arc<Self>, agent: &Arc<Agent>, mut events: Events) {
        let open = || {
            agent
                .sessions
                .lock()
                .is_ok_and(|s| s.contains_key(&self.id))
        };
        loop {
            while let Some(event) = events.recv().await {
                if !open() {
                    return;
                }
                // The daemon let this stream go, as when it fell behind.
                if event["event"] == "follow_lagged" {
                    break;
                }
                self.on(agent, &event).await;
                if event["event"] == "turn_finished" {
                    self.settle(agent).await;
                }
            }
            let mut failed = false;
            events = loop {
                if !open() {
                    return;
                }
                match self.reattach(agent).await {
                    Ok(events) => break events,
                    Err(error) => {
                        // The person's prompt is not left waiting while the
                        // daemon comes back.
                        if !failed && let Some((prompt, _)) = self.state.lock().await.prompt.take()
                        {
                            agent.out.fail(&prompt, FAILED, &error);
                        }
                        failed = true;
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
            };
        }
    }

    /// Follow the bot again on a new connection, and settle what happened
    /// while this thread was not following: a turn that ended, calls that
    /// wait for the person.
    async fn reattach(&self, agent: &Arc<Agent>) -> Result<Events, String> {
        let bot = (self.state.lock().await.bound.as_ref())
            .map(|b| b.bot.clone())
            .ok_or("the thread has no bot")?;
        let (client, mut events) = Client::connect(&agent.options.socket)
            .await
            .map_err(|e| e.to_string())?;
        let after = {
            let mut state = self.state.lock().await;
            if let Some(bound) = &mut state.bound {
                bound.client = client.clone();
            }
            state.cursor
        };
        client
            .request("follow", json!({"bot": bot, "after": after}))
            .await
            .map_err(|e| e.to_string())?;
        // What was kept while the thread was away is handled as it would
        // have been: a task started then still opens its thread.
        let cursor = live(
            &mut events,
            |event| async move { self.on(agent, &event).await },
        )
        .await;
        let record = client
            .request("inspect", json!({"bot": bot}))
            .await
            .map_err(|e| e.to_string())?;
        let running = record["running_turn"].as_i64();
        {
            let mut state = self.state.lock().await;
            state.cursor = state.cursor.max(cursor);
            // The replay ended what it saw end; a turn it could not show
            // (its events gone) ended if the daemon runs another.
            let was = state.running;
            if was.is_some() && running != was {
                if agent.v2() {
                    agent.out.update(
                        &self.id,
                        json!({"sessionUpdate": "state_update",
                        "state": "idle", "stopReason": "end_turn"}),
                    );
                }
                if matches!(&state.prompt, Some((_, at)) if *at == was)
                    && let Some((prompt, _)) = state.prompt.take()
                {
                    agent.out.answer(&prompt, json!({"stopReason": "end_turn"}));
                }
                state.end_calls();
            }
            state.running = running;
        }
        self.approvals(agent, &client, &bot, running).await;
        Ok(events)
    }

    async fn on(&self, agent: &Arc<Agent>, event: &Value) {
        let mut state = self.state.lock().await;
        let Some(bound) = &state.bound else { return };
        if event["bot"] != bound.bot.as_str() {
            return;
        }
        let (client, bot) = (bound.client.clone(), bound.bot.clone());
        if let Some(cursor) = event["cursor"].as_i64() {
            state.cursor = state.cursor.max(cursor);
        }
        let turn = event["turn"].as_i64();
        let data = &event["data"];
        let out = &agent.out;
        let id = self.id.as_str();
        match event["event"].as_str().unwrap_or("") {
            "accepted" => {
                // A steer joins the running turn, whose calls go on.
                if state.running != turn {
                    state.part = 0;
                    state.streamed.clear();
                    state.end_calls();
                }
                state.running = turn;
                let ours = match &mut state.prompt {
                    Some((_, at @ None)) => {
                        *at = turn;
                        true
                    }
                    Some((_, at)) => *at == turn,
                    None => false,
                };
                if agent.v2() {
                    out.update(
                        id,
                        json!({"sessionUpdate": "state_update", "state": "running"}),
                    );
                }
                // A turn the bot took on its own, as one the lookout woke
                // it for: what started it, as the thread's next message.
                if !ours
                    && let Some(node) = data["node"].as_i64()
                    && let Some(item) = items(&client, &bot, &[node]).await.into_iter().next()
                    && !reactions::is_reaction(&crate::text(&item))
                {
                    out.update(
                        id,
                        json!({"sessionUpdate": "user_message",
                        "messageId": format!("{bot}-{node}"),
                        "content": [{"type": "text", "text": crate::text(&item)}]}),
                    );
                }
            }
            "text_delta" => {
                let text = event["text"].as_str().unwrap_or("");
                state.streamed.push_str(text);
                out.update(id, json!({"sessionUpdate": "agent_message_chunk",
                    "messageId": message_id(&bot, turn, state.part), "content": {"type": "text", "text": text}}));
            }
            "thinking_delta" => {
                let text = event["text"].as_str().unwrap_or("");
                out.update(
                    id,
                    json!({"sessionUpdate": "agent_thought_chunk",
                    "messageId": format!("{}-thought", message_id(&bot, turn, state.part)),
                    "content": {"type": "text", "text": text}}),
                );
            }
            // Words that did not stream, as from a thread that came late or
            // whose stream dropped partway: what was not sent yet.
            "message" => {
                // Only the assistant's words settle what streamed: a
                // reasoning item before them leaves it as it is.
                let Some(node) = data["node"].as_i64() else {
                    return;
                };
                let Some(item) = items(&client, &bot, &[node]).await.into_iter().next() else {
                    return;
                };
                let text = crate::text(&item);
                if item["role"] != "assistant" || text.is_empty() {
                    return;
                }
                let streamed = std::mem::take(&mut state.streamed);
                let message = message_id(&bot, turn, state.part);
                if let Some(rest) = text.strip_prefix(streamed.as_str())
                    && !rest.is_empty()
                {
                    out.update(
                        id,
                        json!({"sessionUpdate": "agent_message_chunk",
                        "messageId": message, "content": {"type": "text", "text": rest}}),
                    );
                }
                if state.replies.len() == REPLIES {
                    state.replies.pop_front();
                }
                let by = state.bound.as_ref().and_then(|bound| bound.bot_id);
                state
                    .replies
                    .push_back((message, by, node, first_line(&text)));
            }
            "tool_started" => {
                let (call, name, arguments) = started_call(&client, &bot, data).await;
                state.part += 1;
                state.streamed.clear();
                // The bot's reaction to the person's newest message here, as
                // a client may show it; one in another bot is that bot's.
                if name == reactions::TOOL {
                    if agent.t3.is_none()
                        && reactions::target(&arguments).is_none()
                        && let Some((emoji, remove)) = reactions::call(&arguments)
                    {
                        out.send(json!({"jsonrpc": "2.0", "method": REACT,
                            "params": {"sessionId": id, "emoji": emoji, "remove": remove}}));
                    }
                    return;
                }
                if name == plan::TOOL {
                    out.update(
                        id,
                        json!({"sessionUpdate": "plan", "entries": plan_entries(&arguments)}),
                    );
                    return;
                }
                out.update(id, tool_call(&call, &name, &arguments));
                state.calls.insert((turn, call), (name, arguments));
            }
            // A call a message lifted goes on in the background: its
            // turn's answer to it is its handle, and it ends later.
            "call_lifted" => {
                let key = (turn, data["call_id"].as_str().unwrap_or("").to_owned());
                if state.calls.contains_key(&key) {
                    state.lifted.insert(key);
                }
            }
            "tool_completed" | "lifted_ended" => {
                let call = data["call_id"].as_str().unwrap_or("").to_owned();
                let key = (turn, call.clone());
                let lifted = event["event"] == "lifted_ended";
                if !lifted && state.lifted.contains(&key) {
                    return;
                }
                // Its approval is decided: a call of a later turn under the
                // same id is put to the person afresh.
                let prompted = asked_key(turn, &call, "");
                state.asked.retain(|asked| !asked.starts_with(&prompted));
                match lifted {
                    // Allowed, one its client answers waits on for that
                    // answer; any other runs as a background command,
                    // whose end this thread does not show.
                    true if data["waits_for"] == "answer" => return,
                    true if data["outcome"] == "allowed" => {
                        if state.lifted.remove(&key) {
                            state.calls.remove(&key);
                        }
                        return;
                    }
                    true if !state.lifted.remove(&key) => return,
                    _ => {}
                }
                let Some((name, arguments)) = state.calls.remove(&key) else {
                    return;
                };
                let (mut shown, mut failed) = (String::new(), data["failed"] == true);
                if lifted {
                    let result = &data["result"];
                    shown = (result["answer"].as_str())
                        .or(result["output"].as_str())
                        .or(result["detail"].as_str())
                        .unwrap_or_default()
                        .to_owned();
                } else if let Some(node) = data["node"].as_i64()
                    && let Some(item) = items(&client, &bot, &[node])
                        .await
                        .into_iter()
                        .find(|item| item["call_id"] == call.as_str())
                {
                    (shown, failed) = crate::output(&item);
                    failed |= data["failed"] == true;
                }
                if data["denied"] == true {
                    failed = true;
                }
                let status = if failed { "failed" } else { "completed" };
                out.update(
                    id,
                    json!({"sessionUpdate": "tool_call_update", "toolCallId": call,
                    "status": status, "content": [text_content(&cut(&shown))]}),
                );
                // A question the lead put, taken: put to the person as a
                // form, where the client fills them in.
                if name == asks::TOOL
                    && !failed
                    && agent.forms.load(Ordering::Relaxed)
                    && let Some(turn) = turn
                    && let Some(ask) = asks::call(&asks::id(turn, &call), &arguments)
                    && !ask.options.is_empty()
                    && state.asked.insert(ask.id.clone())
                {
                    let asked = out.asking("elicitation/create", form(id, &ask, Some(&call)));
                    let lead = (bot, state.bound.as_ref().and_then(|b| b.bot_id));
                    let (agent, session) = (agent.clone(), self.id.clone());
                    tokio::spawn(async move { elicit(asked, &agent, &session, lead, &ask).await });
                    return;
                }
                // A task the lead started, which a client opens as a
                // thread of its own by the session id it is given.
                if name == crate::served::START
                    && !failed
                    && shown.starts_with("Started ")
                    && agent.t3.is_none()
                    && let Some(project) = state.bound.as_ref().and_then(|b| b.project.as_deref())
                    && let Some(task) = task_bot(project, &arguments)
                    && let Ok(record) = client.request("inspect", json!({"bot": task})).await
                    && let Some(bot_id) = record["bot_id"].as_i64()
                {
                    let session = lead_session(&task, bot_id, client.store());
                    out.update(
                        id,
                        json!({"sessionUpdate": "tool_call_update", "toolCallId": call,
                        "_meta": {"task": {"sessionId": session, "title": task,
                        "cwd": record["workspace"]}}}),
                    );
                    return;
                }
                // A task the lead started opens as a thread of its own.
                if name == crate::served::START
                    && !failed
                    && shown.starts_with("Started ")
                    && let Some(project) = state.bound.as_ref().and_then(|b| b.project.clone())
                    && agent.t3.is_some()
                {
                    let workspace = state
                        .bound
                        .as_ref()
                        .map(|b| b.workspace.clone())
                        .unwrap_or_default();
                    let this = (agent.sessions.lock().ok())
                        .and_then(|sessions| sessions.get(&self.id).cloned());
                    if let Some(session) = this {
                        state.opening += 1;
                        let session_id = session.id.clone();
                        let agent = agent.clone();
                        tokio::spawn(async move {
                            let (opener, thread) = (agent.clone(), session.cwd.clone());
                            let opening = tokio::spawn(async move {
                                let at = Where {
                                    project: &project,
                                    workspace: &workspace,
                                    thread: &thread,
                                };
                                let session = &session_id;
                                open_task(&opener, session, &client, &at, &arguments, &call).await;
                            });
                            // The person's prompt waits for its tasks' threads,
                            // not for ever: one still opening opens on its own.
                            let _ = tokio::time::timeout(OPENING, opening).await;
                            let mut state = session.state.lock().await;
                            state.opening -= 1;
                            if state.opening == 0 {
                                for (prompt, result) in std::mem::take(&mut state.held) {
                                    agent.out.answer(&prompt, result);
                                }
                            }
                        });
                    }
                }
            }
            "approval_requested" | "approval_asked" => {
                // Calls announced only for the reviewer, as in auto mode,
                // wait on no person: nothing to list.
                if event["event"] == "approval_requested"
                    && !(data["calls"].as_array().into_iter().flatten()).any(for_person)
                {
                    return;
                }
                drop(state);
                self.approvals(agent, &client, &bot, turn).await;
            }
            "turn_finished" => {
                // A steer the running turn took in: the prompt waits on that
                // turn now.
                if data["status"] == "steered" {
                    let into = data["into"].as_i64();
                    if let (Some(steer), Some(into)) = (turn, into) {
                        state.steered = Some((steer, into));
                    }
                    if let Some((_, at)) = &mut state.prompt
                        && *at == turn
                    {
                        *at = into;
                    }
                    return;
                }
                if state.running.is_some() && state.running != turn {
                    return;
                }
                state.running = None;
                state.end_calls();
                let reason = match data["status"].as_str().unwrap_or("") {
                    "interrupted" | "cancelled" => "cancelled",
                    _ => "end_turn",
                };
                let failed = data["status"] == "failed";
                let mut ended =
                    json!({"sessionUpdate": "state_update", "state": "idle", "stopReason": reason});
                if failed {
                    let detail = data["error"].as_str().unwrap_or("the turn failed");
                    ended["error"] = json!({"code": FAILED, "message": detail});
                }
                if agent.v2() {
                    out.update(id, ended);
                }
                // A prompt whose turn is not named yet waits: the turn that
                // ended may be one it was sent behind.
                let ours = matches!(&state.prompt, Some((_, at)) if at.is_some() && *at == turn);
                if ours && let Some((prompt, _)) = state.prompt.take() {
                    let mut result = json!({"stopReason": reason});
                    if failed {
                        result["error"] = json!({"code": FAILED,
                            "message": data["error"].as_str().unwrap_or("the turn failed")});
                    }
                    // A task T3 Code opens as a child of this prompt opens
                    // while its run is on.
                    if state.opening > 0 {
                        state.held.push((prompt, result));
                    } else {
                        out.answer(&prompt, result);
                    }
                }
            }
            "deleted" => {
                if let Some((prompt, _)) = state.prompt.take() {
                    out.fail(&prompt, FAILED, &format!("{bot} was deleted"));
                }
            }
            _ => {}
        }
    }

    /// The calls of `turn` waiting for the person, and those a message
    /// lifted past their turn (listed with a handle, as their steps are),
    /// each put to them once: the gate it waits on is the person's
    /// (`ask`), or the judge handed it over (`auto`).
    async fn approvals(
        &self,
        agent: &Arc<Agent>,
        client: &Arc<Client>,
        bot: &str,
        turn: Option<i64>,
    ) {
        // Every page: a round's calls are listed together, and the later
        // ones are not announced again.
        let mut listed = Vec::new();
        let mut after = 0;
        loop {
            let Ok(mut page) = client
                .request(
                    "approvals",
                    json!({"bot": bot, "after": after, "limit": 64}),
                )
                .await
            else {
                break;
            };
            if let Value::Array(calls) = page["approvals"].take() {
                listed.extend(calls);
            }
            match page["next_after"].as_i64() {
                Some(next) => after = next,
                None => break,
            }
        }
        for call in &listed {
            let lifted = call["handle"].is_string();
            if (call["turn"].as_i64() != turn && !lifted) || !for_person(call) {
                continue;
            }
            let key = asked_key(
                call["turn"].as_i64(),
                call["call_id"].as_str().unwrap_or(""),
                &call["request"].to_string(),
            );
            if !self.state.lock().await.asked.insert(key) {
                continue;
            }
            let (agent, client, bot, call) =
                (agent.clone(), client.clone(), bot.to_owned(), call.clone());
            let session = self.id.clone();
            tokio::spawn(async move { ask(&agent, &session, &client, &bot, &call).await });
        }
    }
}

impl Session {
    /// The lead's questions still open from before this thread followed
    /// it, each put to the person as a form. Read back from its history,
    /// newest first, only to the person's own last words: a question
    /// before those is set aside, as Sonny's window sets it aside.
    async fn questions(&self, agent: &Arc<Agent>, client: &Arc<Client>, bot: &str) {
        for ask in open_asks(client, bot).await {
            let bot_id = {
                let mut state = self.state.lock().await;
                if !state.asked.insert(ask.id.clone()) {
                    continue;
                }
                state.bound.as_ref().and_then(|b| b.bot_id)
            };
            let asked = (agent.out).asking("elicitation/create", form(&self.id, &ask, None));
            let (agent, session, lead) = (agent.clone(), self.id.clone(), (bot.to_owned(), bot_id));
            tokio::spawn(async move { elicit(asked, &agent, &session, lead, &ask).await });
        }
    }
}

impl Session {
    /// Put each question the lead's tasks have open to the person as a
    /// form, "From TASK", whose pick answers it in that task: those open
    /// now, then each as it is asked, from tasks the lead starts since
    /// too. On a connection of its own, which follows only the calls of
    /// the lead and its tasks, from now; once the daemon drops it, it is
    /// made again and what is open read again.
    async fn watch_tasks(self: Arc<Self>, agent: &Arc<Agent>, (lead, lead_id): (String, i64)) {
        loop {
            let _ = self.tasks_asked(agent, &lead, lead_id).await;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    async fn tasks_asked(
        &self,
        agent: &Arc<Agent>,
        lead: &str,
        lead_id: i64,
    ) -> Result<(), String> {
        let (client, mut events) = Client::connect(&agent.options.socket)
            .await
            .map_err(|e| e.to_string())?;
        let since = now_ms();
        // Only the calls, and for the lead also its turns' ends.
        let follow = |bot: &str| {
            json!({"bot": bot, "since_ms": since,
                "kinds": ["tool_started", "tool_completed", "turn_finished"]})
        };
        client
            .request("follow", follow(lead))
            .await
            .map_err(|e| e.to_string())?;
        // The tasks followed, by name, each the identity it is now: a
        // follow is by name, and a bot made later under it is not the task.
        let mut watched: HashMap<String, i64> = HashMap::new();
        let store = agent_client::socket::canonical_store(&agent.options.store)
            .unwrap_or_else(|_| agent.options.store.clone());
        // Followed before they are read, so none asked between is missed.
        let tasks = tasks_of(&client, lead_id).await?;
        for task in &tasks {
            if let (Some(bot), Some(id)) = (task["name"].as_str(), task["bot_id"].as_i64())
                && watched.insert(bot.to_owned(), id).is_none()
            {
                (client.request("follow", follow(bot)).await).map_err(|e| e.to_string())?;
            }
        }
        for task in tasks.iter().filter(|task| lately(task, since)) {
            self.task_asked(agent, &client, &store, task).await;
        }
        // The lead's `start_task` calls started, and the tasks' asks, by
        // task and call.
        let (mut starting, mut asking) = (HashSet::new(), HashSet::new());
        while let Some(event) = events.recv().await {
            if event["event"] == "follow_lagged" {
                break;
            }
            let Some(bot) = event["bot"].as_str() else {
                continue;
            };
            let data = &event["data"];
            let call = data["call_id"].as_str().unwrap_or("").to_owned();
            let done = data["failed"] != true && data["denied"] != true;
            let kind = event["event"].as_str().unwrap_or("");
            if bot == lead {
                match kind {
                    "tool_started" if data["name"] == crate::served::START => {
                        starting.insert(call);
                        continue;
                    }
                    // A task started: followed from now on.
                    "tool_completed" if starting.remove(&call) && done => {}
                    // One whose start came before this followed: the
                    // lead's turn ends after it.
                    "turn_finished" => {}
                    _ => continue,
                }
                for task in tasks_of(&client, lead_id).await? {
                    let (Some(bot), Some(id)) = (task["name"].as_str(), task["bot_id"].as_i64())
                    else {
                        continue;
                    };
                    match watched.insert(bot.to_owned(), id) {
                        Some(was) if was == id => continue,
                        Some(_) => {}
                        None => {
                            (client.request("follow", follow(bot)).await)
                                .map_err(|e| e.to_string())?;
                        }
                    }
                    // Its questions may have come before it was followed.
                    self.task_asked(agent, &client, &store, &task).await;
                }
                continue;
            }
            // A task's question put, or its turn over, which takes in one
            // whose ask began before this followed: read what it has open,
            // if it is still the task followed.
            let key = (bot.to_owned(), call);
            if kind == "tool_started" && data["name"] == asks::TOOL {
                asking.insert(key);
                continue;
            }
            let asked = kind == "tool_completed" && asking.remove(&key) && done;
            let ended = kind == "turn_finished";
            if (asked || ended)
                && let Some(&id) = watched.get(bot)
                && let Ok(task) = client
                    .request("inspect", json!({"bot": bot, "bot_id": id}))
                    .await
                && task["created_by_id"].as_i64() == Some(lead_id)
            {
                self.task_asked(agent, &client, &store, &task).await;
            }
        }
        client.close().await;
        Ok(())
    }

    /// The questions `task`, as `bots` or `inspect` gives it, has open,
    /// each put to the person once.
    async fn task_asked(&self, agent: &Arc<Agent>, client: &Client, store: &Path, task: &Value) {
        let (Some(bot), Some(bot_id)) = (task["name"].as_str(), task["bot_id"].as_i64()) else {
            return;
        };
        // One its lead marked done, with no turn since, has nothing open.
        let running = task["running_turn"].as_i64();
        let newest = running.or(task["last"]["turn"].as_i64());
        if crate::resolved::holds(crate::resolved::read(store, bot), newest, running.is_some()) {
            return;
        }
        for ask in open_asks(client, bot).await {
            if !(self.state.lock().await.asked).insert(format!("{bot} {}", ask.id)) {
                continue;
            }
            let asked = (agent.out).asking("elicitation/create", task_form(&self.id, bot, &ask));
            let (agent, session, task) = (
                agent.clone(),
                self.id.clone(),
                (bot.to_owned(), Some(bot_id)),
            );
            tokio::spawn(async move { elicit(asked, &agent, &session, task, &ask).await });
        }
    }
}

/// The questions `bot` has open with options to pick from, the oldest
/// first, as they were asked. Read back from its history, newest first,
/// only to the person's own last words: a question before those is set
/// aside, as Sonny's window sets it aside.
async fn open_asks(client: &Client, bot: &str) -> Vec<asks::Ask> {
    let (mut asked, mut open) = (asks::Asked::default(), Vec::new());
    let _ = crate::said::walk(client, bot, 0, |at, entry| {
        open.extend(
            (asked.entry(at, entry).into_iter())
                .filter(|(ask, state)| *state == asks::State::Open && !ask.options.is_empty())
                .map(|(ask, _)| ask),
        );
        asked.moved_on()
    })
    .await;
    open.reverse();
    open
}

/// How far back a task's question is looked for when a thread comes:
/// one idle longer has none the person is waited on for, as `sonny ask
/// list` reads it.
const LATELY_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Whether `task`, as `bots` lists it, ran lately, before `now` (ms).
fn lately(task: &Value, now: i64) -> bool {
    task["running_turn"].is_i64()
        || task["last"]["finished_ms"]
            .as_i64()
            .is_some_and(|ended| now - ended <= LATELY_MS)
}

fn now_ms() -> i64 {
    (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH))
        .map_or(0, |d| d.as_millis() as i64)
}

/// A task's question, as a form that says whose it is.
fn task_form(session: &str, task: &str, ask: &asks::Ask) -> Value {
    let mut params = form(session, ask, None);
    let message = params["message"].as_str().unwrap_or_default();
    params["message"] = json!(format!("From {task}\n\n{message}"));
    params
}

/// The form field a question's pick is in.
const PICK: &str = "pick";

/// A question the lead put, as a form: the question, what a yes covers,
/// why it is asked and what the lead recommends, then one pick among its options, each by
/// its number, as a pick in Sonny's window is.
fn form(session: &str, ask: &asks::Ask, call: Option<&str>) -> Value {
    // What a yes covers, as the card and the answer say it, right under
    // the question: the pick is the person's consent to that alone, so it
    // is never sent unseen.
    let mut message = match &ask.covers {
        Some(covers) => format!("{}\nCovers: {}", ask.question, covers.line()),
        None => ask.question.clone(),
    };
    if !ask.why.is_empty() {
        message = format!("{message}\n\n{}", ask.why);
    }
    if let Some(choice) = ask.recommended.and_then(|at| ask.options.get(at)) {
        message = match ask.reason.is_empty() {
            true => format!("{message}\n\nRecommended: {}.", choice.label),
            false => format!("{message}\n\nRecommended: {}. {}", choice.label, ask.reason),
        };
    }
    let options: Vec<Value> = (ask.options.iter().enumerate())
        .map(|(at, choice)| {
            let mut option = json!({"const": (at + 1).to_string(), "title": choice.label});
            if !choice.explanation.is_empty() {
                option["description"] = json!(choice.explanation);
            }
            option
        })
        .collect();
    let mut params = json!({"sessionId": session, "mode": "form", "message": message,
        "requestedSchema": {"type": "object", "required": [PICK],
            "properties": {PICK: {"type": "string", "title": "Pick one", "oneOf": options}}}});
    if let Some(call) = call {
        params["toolCallId"] = json!(call);
    }
    params
}

/// Put a question the lead asked to the person as a form, and send their
/// pick to that lead, by its name and identity, as their answer, once,
/// whichever window answers first.
/// A pick by someone the client names is that person's words, as their
/// prompts are: the lead reads it, but it answers nothing in Sonny's
/// windows, where only the person answers.
async fn elicit(
    asked: oneshot::Receiver<Value>,
    agent: &Agent,
    session: &str,
    (bot, bot_id): (String, Option<i64>),
    ask: &asks::Ask,
) {
    let Some(answer) = Out::answer_to(asked).await else {
        return;
    };
    // Declined or dismissed: the question stays open, there and in
    // Sonny's windows.
    let picked = match answer["action"].as_str() {
        Some("accept") => answer["content"][PICK].as_str().unwrap_or(""),
        _ => return,
    };
    if !asks::picks(ask, picked) {
        return;
    }
    // Someone named by a name the daemon would not take: their pick is
    // nobody's answer, least of all the person's.
    let Ok(person) = person(&answer) else {
        return;
    };
    // Through the thread's connection as it is now: the one the question
    // came on may have dropped while the person chose.
    let session = (agent.sessions.lock().ok()).and_then(|s| s.get(session).cloned());
    let Some(session) = session else { return };
    let Some(client) = (session.state.lock().await.bound.as_ref()).map(|b| b.client.clone()) else {
        return;
    };
    let words = asks::answer(ask, picked);
    let mut submitted = json!({"bot": bot, "bot_id": bot_id, "delivery": "steer"});
    let _ = match person {
        Some(person) => {
            submitted["prompt"] = json!(crate::stamp::now(&words));
            submitted["person"] = json!(person);
            submitted["request_id"] = json!(crate::unique("acp"));
            client
                .request("submit", submitted)
                .await
                .map(drop)
                .map_err(|e| e.to_string())
        }
        None => {
            submitted["prompt"] = json!(crate::stamp::now(&words));
            crate::answer_once(&client, &ask.id, submitted)
                .await
                .map(drop)
        }
    };
}

/// Whether `call`, as the daemon lists or announces it, waits on the
/// person: the gate it waits on is theirs, or the reviewer handed it over.
fn for_person(call: &Value) -> bool {
    let asked = match &call["asked"] {
        Value::Array(asked) => !asked.is_empty(),
        asked => !asked.is_null(),
    };
    asked || (call["gates"].as_array()).is_some_and(|gates| gates.iter().any(|g| g == "manual"))
}

/// Put one call to the person, and answer it with their choice.
async fn ask(agent: &Agent, session: &str, client: &Client, bot: &str, call: &Value) {
    let id = call["call_id"].as_str().unwrap_or("");
    let name = call["name"].as_str().unwrap_or("tool");
    // A long call is listed cut: the person is shown the whole of it, read
    // from the node that planned it, or may only deny it.
    let listed_whole = call["arguments"].is_object()
        && call["arguments_cut"].as_array().is_none_or(Vec::is_empty)
        && call["arguments_omitted"].as_u64().unwrap_or(0) == 0;
    let arguments = match listed_whole {
        true => Some(call["arguments"].clone()),
        false => client
            .request(
                "history_items",
                json!({"bot": bot, "nodes": [call["node"]]}),
            )
            .await
            .ok()
            .and_then(|read| agent_client::approver::call_arguments(&read["items"][0]["item"], id)),
    };
    let mut update = tool_call(id, name, arguments.as_ref().unwrap_or(&Value::Null));
    if let Value::Object(fields) = &mut update {
        fields.remove("sessionUpdate");
    }
    let mut options = vec![json!({"optionId": "deny", "name": "Deny", "kind": "reject_once"})];
    let mut title = update["title"].as_str().unwrap_or(name).to_owned();
    match arguments {
        Some(_) => options.insert(
            0,
            json!({"optionId": "allow", "name": "Allow", "kind": "allow_once"}),
        ),
        None => {
            title = format!("{title} (its whole call could not be read, so it can only be denied)")
        }
    }
    let answer = agent
        .out
        .ask(
            "session/request_permission",
            json!({"sessionId": session, "title": title, "toolCall": update, "options": options}),
        )
        .await;
    // No choice, as when the client gave up on it: the call waits for an
    // answer from anywhere else.
    let decision = match answer.as_ref().map(|a| &a["outcome"]) {
        Some(outcome) if outcome["outcome"] == "selected" && outcome["optionId"] == "allow" => {
            "allow"
        }
        Some(outcome) if outcome["outcome"] == "selected" => "deny",
        _ => return,
    };
    // The person the client says chose, else the client. Someone named
    // by a name the daemon would not take chose nothing: the call waits.
    let by = match answer.as_ref().map(person) {
        Some(Err(_)) => return,
        Some(Ok(Some(person))) => person.to_owned(),
        _ => "acp".to_owned(),
    };
    // Through the thread's connection as it is now: the one the call was
    // listed on may have dropped while the person chose.
    let Some(session) = agent
        .sessions
        .lock()
        .ok()
        .and_then(|s| s.get(session).cloned())
    else {
        return;
    };
    let client = (session.state.lock().await.bound.as_ref()).map(|b| b.client.clone());
    let sent = match client {
        Some(client) => client
            .request(
                "answer",
                json!({"bot": bot, "turn": call["turn"], "call_id": id, "request": call["request"],
                    "tag": crate::person_gate(call), "decision": decision, "by": by}),
            )
            .await
            .is_ok(),
        None => false,
    };
    // Not delivered: the call is put to the person again once the thread
    // follows its bot again.
    if !sent {
        let key = asked_key(call["turn"].as_i64(), id, &call["request"].to_string());
        session.state.lock().await.asked.remove(&key);
    }
}

/// Handle the events a follow replays, until it is live: the cursor it is
/// live from.
async fn live<F, R>(events: &mut Events, mut each: F) -> i64
where
    F: FnMut(Value) -> R,
    R: std::future::Future<Output = ()>,
{
    let replay = async {
        while let Some(event) = events.recv().await {
            if event["event"] == "follow_live" {
                return event["cursor"].as_i64().unwrap_or(0);
            }
            each(event).await;
        }
        0
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), replay)
        .await
        .unwrap_or(0)
}

/// What a bot this thread comes back to needs running, as when its lead
/// was set up: the lookout watching the lead's tasks, the tool host
/// answering Sonny's tools, and a judge for an `auto` gate.
async fn services(
    agent: &Agent,
    client: &Client,
    bot: &str,
    project: Option<&str>,
) -> Result<(), String> {
    let record = client
        .request("inspect", json!({"bot": bot}))
        .await
        .map_err(|e| e.to_string())?;
    if let Some(project) = project {
        lookout::subscribe_lead(&agent.options.store, bot, project)?;
    }
    crate::start_companions(client).await?;
    if approvals::reviews(approvals::mode(&record)) {
        let model = format!(
            "{}/{}",
            record["provider"].as_str().unwrap_or(""),
            record["model"].as_str().unwrap_or("")
        );
        crate::serve_auto(client, &model).await?;
    }
    Ok(())
}

/// Set up the lead a thread is, as `sonny` sets up the lead it opens: the
/// tools Sonny gives it, its folder's rules and skills as they are today,
/// the permissions row a new lead starts in, the lookout watching its tasks,
/// and a judge for its calls.
async fn setup_lead(
    agent: &Agent,
    session: &str,
    client: &Client,
    lead: &project::Lead,
    moving: Option<&str>,
) -> Result<(), String> {
    let record = client
        .request("inspect", json!({"bot": lead.bot}))
        .await
        .map_err(|e| e.to_string())?;
    let key = project::lead(&agent.options.socket, &record);
    let workspace = moving
        .or(record["workspace"].as_str())
        .unwrap_or_default()
        .to_owned();
    let mut asked = json!({"bot": lead.bot});
    let mut left_out = Vec::new();
    if let Ok((reviewed, left)) = project::review(
        Path::new(&workspace),
        &record,
        &key,
        lead.new,
        project::chosen(&agent.options.store, &lead.bot).as_deref(),
    ) {
        left_out = left;
        if let project::Instructions::Stale(text) = reviewed {
            asked["instructions"] = json!(text);
        }
    }
    let declare = declared::declare(&record, crate::served::Role::LEAD);
    if let Some(declare) = &declare {
        asked["declare"] = declare.clone();
    }
    let mode = approvals::mode(&record);
    let state = crate::state::of(&record, &lead.bot);
    let sandbox = |mode| approvals::sandbox(mode, Path::new(&workspace), state.as_deref());
    let mut gating = lead.new.then(approvals::default);
    if gating.is_none()
        && let Some(mode) = mode.filter(|mode| *mode != "full")
        && !approvals::sandboxed_as(&record, &sandbox(mode))
    {
        gating = Some(mode);
    }
    let gating = gating.or(declare.as_ref().and(mode).filter(|mode| *mode != "full"));
    if let (Some(mode), Value::Object(fields)) = (gating, &mut asked) {
        let tools: Vec<String> = record["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let declared = declare.as_ref().unwrap_or(&record["declared"]);
        fields.extend(approvals::gate(
            mode,
            &approvals::gateable(&tools, declared),
        ));
        fields.insert("settings".into(), Value::Object(sandbox(mode)));
    }
    // Its shell names its state folder, in every row.
    if let Some(env) = (state.as_deref()).and_then(|state| crate::state::env(&record, state)) {
        asked["settings"]["env"] = env;
    }
    // A lead at work keeps its setup until the next thread finds it idle.
    if asked.as_object().is_some_and(|fields| fields.len() > 1) {
        match client.request("configure", asked.clone()).await {
            Ok(_) => {
                if let Some(text) = asked["instructions"].as_str() {
                    project::wrote(&key, text);
                }
            }
            Err(error) if error.code == "bot_busy" => {}
            // As in Sonny's window: the lead works as it was, and the
            // thread says why.
            Err(error) => agent.out.update(
                session,
                json!({"sessionUpdate": "notice",
                "severity": "warning", "title": format!("{} kept its setup", lead.bot),
                "description": error.to_string()}),
            ),
        }
    }
    // What it opened without: the thread says so, and the lead is told
    // with its next model call, as in Sonny's window.
    if !left_out.is_empty() {
        agent.out.update(
            session,
            json!({"sessionUpdate": "notice", "severity": "warning",
            "title": format!("{} opened without what it could not use", lead.bot),
            "description": left_out.join("\n")}),
        );
        if let Err(error) = project::tell_warnings(client, &lead.bot, &left_out).await {
            agent.out.update(
                session,
                json!({"sessionUpdate": "notice", "severity": "warning",
                "title": format!("{} was not told what it opened without", lead.bot),
                "description": error}),
            );
        }
    }
    lookout::subscribe_lead(&agent.options.store, &lead.bot, &lead.project)?;
    crate::start_companions(client).await?;
    if approvals::reviews(gating.or(mode)) {
        let model = format!(
            "{}/{}",
            record["provider"].as_str().unwrap_or(""),
            record["model"].as_str().unwrap_or("")
        );
        crate::serve_auto(client, &model).await?;
    }
    Ok(())
}

/// Where a lead's thread is: its project, the lead's workspace, and the
/// folder T3 Code opened the thread in.
struct Where<'a> {
    project: &'a str,
    workspace: &'a str,
    thread: &'a Path,
}

/// Open the task a lead's `start_task` call started as a thread of its
/// own, with the task's brief as its first message; the thread is the
/// task's when that message reaches it. While the person's prompt runs it
/// is the lead thread's child, which T3 Code opens in the lead's folder;
/// otherwise a thread of its own, where the lead's call says.
async fn open_task(
    agent: &Agent,
    session: &str,
    client: &Client,
    at: &Where<'_>,
    arguments: &Value,
    call: &str,
) {
    let (project, lead_workspace) = (at.project, at.workspace);
    let Some(t3) = &agent.t3 else { return };
    let Some(bot) = task_bot(project, arguments) else {
        return;
    };
    let name = bot.strip_prefix(&format!("{project}.")).unwrap_or(&bot);
    let opened = async {
        let record = client
            .request("inspect", json!({"bot": bot}))
            .await
            .map_err(|e| e.to_string())?;
        let turns = client
            .request("turns", json!({"bot": bot, "limit": 1}))
            .await
            .map_err(|e| e.to_string())?;
        let first = &turns["turns"][0]["turn"];
        let prompts = client
            .request("prompts", json!({"bot": bot, "turn": first}))
            .await
            .map_err(|e| e.to_string())?;
        let brief = prompts["prompts"][0]["text"]
            .as_str()
            .ok_or("the task has no brief")?
            .to_owned();
        let workspace = record["workspace"].as_str().unwrap_or_default();
        let message = t3::expect(&bot, &brief, at.thread)?;
        let child = json!({"task": message, "title": name, "clientRequestId": bot});
        // The person's prompt waits for it: not for ever. One that is late
        // may open yet, so its task still waits for it and no other thread
        // is opened for it.
        let delegating = t3.call("delegate_task", child);
        match tokio::time::timeout(OPENING, delegating).await {
            Ok(Ok(delegated)) => return Ok(delegated),
            Ok(Err(_)) => {}
            Err(_) => return Ok(Value::Null),
        }
        let message = t3::expect(&bot, &brief, Path::new(workspace))?;
        let mut launch = json!({"title": name, "message": message, "clientRequestId": bot});
        if workspace != lead_workspace {
            launch["workspaceStrategy"] =
                json!({"type": "existing_worktree", "worktreePath": workspace});
        }
        match t3.call("t3_thread_launch", launch.clone()).await {
            // A folder T3 Code does not take as one of the project's: the
            // thread works in the project's, as the task's still says.
            Err(_) if !launch["workspaceStrategy"].is_null() => {
                let message = t3::expect(&bot, &brief, Path::new(lead_workspace))?;
                let launch = json!({"title": name, "message": message, "clientRequestId": bot});
                t3.call("t3_thread_launch", launch).await
            }
            launched => launched,
        }
    }
    .await;
    match opened {
        Ok(launched) => {
            if let Some(link) = launched["link"].as_str() {
                agent.out.update(
                    session,
                    json!({"sessionUpdate": "tool_call_update", "toolCallId": call,
                    "content": [text_content(&format!("Opened {bot}: {link}"))]}),
                );
            }
        }
        Err(error) => {
            t3::forget(&bot);
            agent.out.update(
                session,
                json!({"sessionUpdate": "notice", "severity": "warning",
                "title": format!("{bot} has no thread"), "description": error}),
            );
        }
    }
}

/// The lead a thread's first words ask for, as Sonny's commands say it:
/// `/switch NAME` one of the folder's, `/new [NAME]` a new one.
fn lead_asked(text: &str) -> Option<project::Pick> {
    let text = text.trim();
    let (command, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let mut words = rest.split_whitespace();
    match (command, words.next(), words.next()) {
        ("/new", name, None) => Some(project::Pick::New(name.map(str::to_owned))),
        ("/switch", Some(name), None) => Some(project::Pick::Named(name.to_owned())),
        _ => None,
    }
}

/// A prompt's words and pictures: text as it is, a file or resource as its
/// path or its text, a picture as the daemon takes one.
fn words(prompt: &Value) -> (String, Vec<Value>) {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in prompt.as_array().into_iter().flatten() {
        match block["type"].as_str().unwrap_or("") {
            // Each block as T3 Code sends it: its wrapper around the person's
            // words, and its note after them, are its own.
            "text" => {
                let block = block["text"].as_str().unwrap_or("");
                if !t3::is_note(block) {
                    text.push_str(t3::request(block));
                }
            }
            "image" => images.push(json!({"media_type": block["mimeType"], "data": block["data"]})),
            "resource_link" => {
                let uri = block["uri"].as_str().unwrap_or("");
                text.push_str(uri.strip_prefix("file://").unwrap_or(uri));
            }
            "resource" => {
                let resource = &block["resource"];
                let uri = resource["uri"].as_str().unwrap_or("");
                let path = uri.strip_prefix("file://").unwrap_or(uri);
                match resource["text"].as_str() {
                    Some(body) => text.push_str(&format!("\n{path}:\n```\n{body}\n```\n")),
                    None => text.push_str(path),
                }
            }
            _ => {}
        }
    }
    (text, images)
}

/// The update that shows a call: what it does, in a few words.
fn tool_call(call: &str, name: &str, arguments: &Value) -> Value {
    let path = arguments["path"].as_str();
    let (kind, title) = match name {
        "shell" => (
            "execute",
            arguments["command"]
                .as_str()
                .map(|c| c.lines().next().unwrap_or("").to_owned())
                .unwrap_or_else(|| "shell".into()),
        ),
        "read" => ("read", format!("Read {}", path.unwrap_or(""))),
        "write" | "edit" | "apply_patch" => ("edit", format!("Edit {}", path.unwrap_or("files"))),
        "web_search" => ("fetch", "Search the web".to_owned()),
        "wait" => ("other", "Wait".to_owned()),
        _ => ("other", name.to_owned()),
    };
    let mut update = json!({"sessionUpdate": "tool_call", "toolCallId": call, "title": title.trim(),
        "name": name, "kind": kind, "status": "in_progress", "rawInput": arguments});
    if let Some(path) = path {
        update["locations"] = json!([{"path": path}]);
    }
    update
}

/// A `plan` call's steps, as the protocol's plan.
fn plan_entries(arguments: &Value) -> Vec<Value> {
    arguments["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|step| {
            let (status, content) = match step.split_at_checked(4) {
                Some(("[x] ", rest)) => ("completed", rest),
                Some(("[>] ", rest)) => ("in_progress", rest),
                Some(("[ ] ", rest)) => ("pending", rest),
                _ => ("pending", step),
            };
            json!({"content": content, "priority": "medium", "status": status})
        })
        .collect()
}

fn text_content(text: &str) -> Value {
    json!({"type": "content", "content": {"type": "text", "text": text}})
}

/// The family a `provider/model` belongs to, as the daemon's providers
/// say: null for a provider it does not serve.
fn family<'a>(providers: &'a Value, model: &str) -> &'a Value {
    let provider = model
        .split_once('/')
        .map_or(model, |(provider, _)| provider);
    &providers[provider]["family"]
}

/// The efforts `model`'s provider takes, as the daemon's providers say;
/// with no model, every provider's.
fn levels<'a>(providers: &'a Value, model: Option<&str>) -> Vec<&'a str> {
    let named = model.map(|model| {
        model
            .split_once('/')
            .map_or(model, |(provider, _)| provider)
    });
    let mut levels = Vec::new();
    for (name, binding) in providers.as_object().into_iter().flatten() {
        if named.is_some_and(|named| named != name) {
            continue;
        }
        for level in binding["efforts"].as_array().into_iter().flatten() {
            if let Some(level) = level.as_str()
                && !levels.contains(&level)
            {
                levels.push(level);
            }
        }
    }
    levels
}

/// At most `OUTPUT_BYTES` of a call's output, on a character boundary.
fn cut(text: &str) -> String {
    if text.len() <= OUTPUT_BYTES {
        return text.to_owned();
    }
    let mut end = OUTPUT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…", &text[..end])
}

/// A reply's first line, trimmed and cut, as the bot is told which reply
/// a reaction answers.
fn first_line(text: &str) -> String {
    let first = text.lines().find(|line| !line.trim().is_empty());
    crate::transcript::cut(first.unwrap_or("").trim(), crate::QUOTED)
}

fn message_id(bot: &str, turn: Option<i64>, part: u32) -> String {
    format!("{bot}-{}-{part}", turn.unwrap_or(0))
}

async fn items(client: &Client, bot: &str, nodes: &[i64]) -> Vec<Value> {
    match client
        .request("history_items", json!({"bot": bot, "nodes": nodes}))
        .await
    {
        Ok(read) => read["items"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|entry| crate::responses(&entry["item"]))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// A started call's id, name and arguments, read whole when its event
/// carried only their start.
async fn started_call(client: &Client, bot: &str, data: &Value) -> (String, String, Value) {
    let name = data["name"].as_str().unwrap_or("tool").to_owned();
    let call = data["call_id"].as_str().unwrap_or("").to_owned();
    let arguments = match data["arguments_truncated"] == true {
        true => whole_arguments(client, bot, &call).await,
        false => data["arguments"]
            .as_str()
            .and_then(|a| serde_json::from_str(a).ok())
            .unwrap_or(Value::Null),
    };
    (call, name, arguments)
}

/// Call `id`'s whole arguments, from the bot's newest items, for one whose
/// event carried only their start.
async fn whole_arguments(client: &Client, bot: &str, id: &str) -> Value {
    let Ok(page) = client
        .request("history_nodes", json!({"bot": bot, "limit": 64}))
        .await
    else {
        return Value::Null;
    };
    let nodes: Vec<i64> = page["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| node["node"].as_i64())
        .collect();
    items(client, bot, &nodes)
        .await
        .into_iter()
        .find(|item| item["type"] == "function_call" && item["call_id"] == id)
        .and_then(|call| serde_json::from_str(call["arguments"].as_str()?).ok())
        .unwrap_or(Value::Null)
}

/// What marks call `call` of `turn`'s approval request `request` as put
/// to the person, and, with no request, every request of that call: a
/// later turn's call of the same id is its own.
fn asked_key(turn: Option<i64>, call: &str, request: &str) -> String {
    let turn = turn.map_or(String::new(), |turn| turn.to_string());
    format!("{turn}/{call}#{request}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_turns_call_of_the_same_id_is_asked_afresh() {
        let older = asked_key(Some(3), "c1", "1");
        let newer = asked_key(Some(4), "c1", "1");
        assert_ne!(older, newer);
        // Ending turn 3's call lets go of its marks alone.
        assert!(older.starts_with(&asked_key(Some(3), "c1", "")));
        assert!(!newer.starts_with(&asked_key(Some(3), "c1", "")));
        assert!(!asked_key(Some(3), "c10", "1").starts_with(&asked_key(Some(3), "c1", "")));
    }

    #[test]
    fn a_model_shows_by_the_name_its_provider_lists() {
        let id = "bedrock/global.anthropic.claude-opus-5-5";
        assert_eq!(
            choice(id, Some("Global Claude Opus 5.5"), None),
            json!({"value": id, "name": "Global Claude Opus 5.5 · bedrock", "description": id})
        );
        // Unnamed, by its own id; the file's note comes before the full id.
        assert_eq!(
            choice("openai/gpt-6-sol", None, Some("faster")),
            json!({"value": "openai/gpt-6-sol", "name": "GPT 6 Sol · openai",
                "description": "faster (openai/gpt-6-sol)"})
        );
    }

    #[test]
    fn a_prompt_is_words_paths_and_pictures() {
        let prompt = json!([
            {"type": "text", "text": "Look at "},
            {"type": "resource_link", "name": "a.rs", "uri": "file:///src/a.rs"},
            {"type": "resource", "resource": {"uri": "file:///src/b.rs", "text": "fn b() {}"}},
            {"type": "image", "mimeType": "image/png", "data": "AAAA"}
        ]);
        let (text, images) = words(&prompt);
        assert_eq!(text, "Look at /src/a.rs\n/src/b.rs:\n```\nfn b() {}\n```\n");
        assert_eq!(
            images,
            vec![json!({"media_type": "image/png", "data": "AAAA"})]
        );
    }

    #[test]
    fn a_prompt_from_t3_code_is_the_persons_words() {
        // As T3 Code sends every prompt: its instructions around the
        // person's words, then its note as a block of its own.
        let prompt = json!([
            {"type": "text", "text": "<t3_code_instructions>\nUse delegate_task.\n</t3_code_instructions>\n\n<user_request>\nFix the cart.\n</user_request>"},
            {"type": "image", "mimeType": "image/png", "data": "AAAA"},
            {"type": "text", "text": "<runtime_info>In case you're asked: you are running in T3 Code.</runtime_info>\n\n<pull_request_linking>Link PRs.</pull_request_linking>"}
        ]);
        let (text, images) = words(&prompt);
        assert_eq!(text, "Fix the cart.");
        assert_eq!(images.len(), 1);
        // A later prompt carries no instructions, only the note.
        let prompt = json!([
            {"type": "text", "text": "And the tests."},
            {"type": "text", "text": "<runtime_info>In case you're asked.</runtime_info>"}
        ]);
        assert_eq!(words(&prompt).0, "And the tests.");
    }

    #[test]
    fn a_plan_is_the_protocols_plan() {
        let entries =
            plan_entries(&json!({"steps": ["[x] Read", "[>] Write", "[ ] Test", "Ship"]}));
        let status: Vec<&str> = entries
            .iter()
            .map(|e| e["status"].as_str().unwrap())
            .collect();
        assert_eq!(status, ["completed", "in_progress", "pending", "pending"]);
        assert_eq!(entries[1]["content"], "Write");
        assert_eq!(entries[3]["content"], "Ship");
    }

    #[test]
    fn a_call_says_what_it_does() {
        let shell = tool_call("c1", "shell", &json!({"command": "cargo test\necho done"}));
        assert_eq!(
            (shell["kind"].as_str(), shell["title"].as_str()),
            (Some("execute"), Some("cargo test"))
        );
        let edit = tool_call("c2", "edit", &json!({"path": "/src/a.rs"}));
        assert_eq!(edit["locations"], json!([{"path": "/src/a.rs"}]));
        assert_eq!(edit["title"], "Edit /src/a.rs");
    }

    #[test]
    fn the_agent_picker_lists_the_folders_leads_and_the_threads_own() {
        let leads = ["calc.lead".to_owned(), "calc.lead.billing".to_owned()];
        let picker = agent_option(&leads, DEFAULT);
        let values: Vec<&str> = (picker["options"].as_array().unwrap().iter())
            .map(|choice| choice["value"].as_str().unwrap())
            .collect();
        assert_eq!(values, ["default", "calc.lead", "calc.lead.billing"]);
        assert_eq!(
            (&picker["id"], &picker["currentValue"]),
            (&json!("agent"), &json!("default"))
        );
        // A thread on a bot of another folder still names it.
        let picker = agent_option(&leads, "shop.fix-cart");
        assert_eq!(picker["options"].as_array().unwrap().len(), 4);
        assert_eq!(picker["currentValue"], "shop.fix-cart");
    }

    #[test]
    fn a_question_is_a_form_of_one_pick_among_its_options() {
        let ask = asks::call(
            "ask:3/c1",
            &json!({"question": "Which database?", "why": "It decides the schema.",
                "options": [{"label": "SQLite", "explanation": "One file."}, {"label": "Postgres"}],
                "recommended": 2, "reason": "It scales.", "if_unanswered": "wait"}),
        )
        .unwrap();
        let form = form("s1", &ask, Some("c1"));
        assert_eq!(
            form["message"],
            "Which database?\n\nIt decides the schema.\n\nRecommended: Postgres. It scales."
        );
        assert_eq!(
            (&form["sessionId"], &form["toolCallId"]),
            (&json!("s1"), &json!("c1"))
        );
        assert_eq!(form["requestedSchema"]["required"], json!([PICK]));
        assert_eq!(
            form["requestedSchema"]["properties"][PICK]["oneOf"],
            json!([{"const": "1", "title": "SQLite", "description": "One file."},
                {"const": "2", "title": "Postgres"}])
        );
        // Put again once the thread follows anew, it names no call.
        assert!(super::form("s1", &ask, None).get("toolCallId").is_none());
        // A yes that approves an action says what it covers, as the answer
        // it sends does.
        let approving = asks::call(
            "ask:3/c2",
            &json!({"question": "Publish the site?", "options": [{"label": "Yes"}, {"label": "No"}],
                "covers": {"action": "publish", "target": "site", "revision": "3f2a9c1"},
                "if_unanswered": "wait"}),
        )
        .unwrap();
        let message = super::form("s1", &approving, None)["message"].clone();
        assert_eq!(
            message,
            "Publish the site?\nCovers: publish · site · at 3f2a9c1"
        );
        assert!(asks::answer(&approving, "1").contains("\nCovers: publish · site · at 3f2a9c1"));
    }

    #[test]
    fn a_client_names_the_person_in_a_name_the_daemon_takes() {
        assert_eq!(person(&json!({"prompt": []})), Ok(None));
        assert_eq!(
            person(&json!({"_meta": {"person": "ana.lopez"}})),
            Ok(Some("ana.lopez"))
        );
        for refused in [
            json!(""),
            json!("Ana Lopez"),
            json!("a".repeat(129)),
            json!({"name": "ana"}),
        ] {
            assert!(
                person(&json!({"_meta": {"person": refused}})).is_err(),
                "{refused}"
            );
        }
    }

    #[test]
    fn a_listed_session_names_a_bot_and_its_identity() {
        let id = lead_session("calc.lead.billing", 7, Some("00ab"));
        assert_eq!(id, "calc.lead.billing#7@00ab");
        let pinned = listed_bot(&id).unwrap();
        assert_eq!(
            (pinned.bot.as_str(), pinned.bot_id, pinned.store.as_deref()),
            ("calc.lead.billing", Some(7), Some("00ab"))
        );
        let unstored = listed_bot(&lead_session("calc.lead", 7, None)).unwrap();
        assert_eq!((unstored.bot.as_str(), unstored.store), ("calc.lead", None));
        // A task too, which only `a_task` lets open; only by its identity.
        assert_eq!(listed_bot("calc.fix-cart#7").unwrap().bot, "calc.fix-cart");
        assert!(listed_bot("calc.lead").is_none());
        assert!(listed_bot("calc.lead#x").is_none());
        assert!(listed_bot("#7").is_none());
    }

    #[test]
    fn a_tasks_question_says_whose_it_is_and_only_lately_ones_are_read() {
        let ask = asks::call(
            "ask:3/c1",
            &json!({"question": "Ship it?", "why": "", "options": [{"label": "Yes"}, {"label": "No"}]}),
        )
        .unwrap();
        let put = task_form("s", "calc.fix", &ask);
        assert_eq!(put["message"], "From calc.fix\n\nShip it?");
        assert!(put.get("toolCallId").is_none());
        let now = 10 * LATELY_MS;
        assert!(lately(&json!({"running_turn": 4}), now));
        assert!(lately(
            &json!({"last": {"finished_ms": now - LATELY_MS}}),
            now
        ));
        assert!(!lately(
            &json!({"last": {"finished_ms": now - LATELY_MS - 1}}),
            now
        ));
        assert!(!lately(&json!({"running_turn": null}), now));
    }

    #[test]
    fn a_task_is_a_bot_a_lead_started() {
        let bot = |name: &str, by: Option<&str>| json!({"name": name, "created_by": by});
        assert!(task(&bot("calc.fix", Some("calc.lead"))));
        assert!(task(&bot("calc.fix", Some("calc.lead.billing"))));
        // Not a lead, one no lead started, nor a lead's side chat.
        assert!(!task(&bot("calc.lead.billing", Some("calc.lead"))));
        assert!(!task(&bot("calc.check", Some("calc.fix"))));
        assert!(!task(&bot("calc.loose", None)));
        let mut side = bot("calc.lead.side-1a2b", Some("calc.lead"));
        side["expire_ms"] = json!(60_000);
        assert!(!task(&side));
        // Named as `start_task` names it.
        let named = |name: &str| task_bot("calc", &json!({"name": name}));
        assert_eq!(named(" fix ").as_deref(), Some("calc.fix"));
        assert_eq!(named("calc.fix").as_deref(), Some("calc.fix"));
        assert_eq!(task_bot("calc", &json!({})), None);
    }

    #[test]
    fn only_a_call_waiting_on_the_person_is_put_to_them() {
        assert!(for_person(&json!({"gates": ["manual"]})));
        assert!(for_person(
            &json!({"gates": ["auto"], "asked": [{"tag": "auto"}]})
        ));
        assert!(!for_person(&json!({"gates": ["auto"]})));
        assert!(!for_person(&json!({"gates": ["auto"], "asked": []})));
    }

    #[test]
    fn long_output_is_cut_on_a_character() {
        let long = "é".repeat(OUTPUT_BYTES);
        let cut = cut(&long);
        assert!(cut.len() <= OUTPUT_BYTES + 4);
        assert!(cut.ends_with('…'));
    }
}
