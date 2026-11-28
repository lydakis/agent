//! `sonny acp`: Sonny as an agent of the Agent Client Protocol, version 2
//! (`schema-v2.0.0-beta.4`), for T3 Code, which runs it as a provider
//! ("Local command"), a process for each thread. A thread in a folder is
//! that folder's lead, as `sonny` there finds or makes it, set up the same
//! way: its rules and skills, its tools, its permissions row, and the
//! lookout. Each task the lead starts opens as a thread through T3 Code's
//! tools, the lead thread's child while the person's prompt runs, and that
//! thread is the task's.
//!
//! Any other client, such as a bridge from a chat app, is served the same
//! way: it picks the lead behind a session with the Agent option, finds
//! the leads to reopen with `session/list`, and sees each task the lead
//! starts as the protocol's subagent of the lead's session.
//!
//! A prompt is answered once the daemon has taken it in, and the work it
//! starts ends with an idle `state_update`; the turns a bot takes on its
//! own, as when the lookout wakes a lead because a task finished, reach
//! the session between the person's prompts, as work the agent started.
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

/// The protocol version Sonny speaks, the only one.
const VERSION: i64 = 2;

/// The most of a call's output a thread is shown.
const OUTPUT_BYTES: usize = 8 * 1024;

/// The longest the person's work waits for its tasks' threads to open.
const OPENING: std::time::Duration = std::time::Duration::from_secs(30);

/// The longest a thread's start waits for the providers to list models.
const LISTING: std::time::Duration = std::time::Duration::from_secs(5);

/// JSON-RPC's codes, and the protocol's, for what Sonny refuses.
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const FAILED: i64 = -32603;
const SESSION_NOT_FOUND: i64 = -32002;
const UNSUPPORTED_VERSION: i64 = -32022;
const REQUEST_CANCELLED: i64 = -32800;
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
    let agent = Agent::new(store, socket, t3);
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
    /// Sessions whose history is being replayed, with what else was sent
    /// for them meanwhile: it waits until the replay is over, as the
    /// protocol has it, requests too.
    held: Mutex<HashMap<String, Vec<Value>>>,
    /// What a test's agent wrote, in order, in place of stdout.
    #[cfg(test)]
    written: Mutex<Vec<Value>>,
}

impl Out {
    /// Send `message`, or keep it while its session's history replays.
    /// A task's session is kept with its lead's, in the order sent, so a
    /// subagent is announced before anything comes under its id.
    fn send(&self, message: Value) {
        if let Some(session) = message["params"]["sessionId"].as_str()
            && let Ok(mut held) = self.held.lock()
            && let Some(kept) = (held.iter_mut())
                .find(|(lead, _)| {
                    session == lead.as_str()
                        || (session.strip_prefix(lead.as_str())).is_some_and(|r| r.starts_with('/'))
                })
                .map(|(_, kept)| kept)
        {
            kept.push(message);
            return;
        }
        self.write(&message);
    }

    fn write(&self, message: &Value) {
        #[cfg(test)]
        {
            if let Ok(mut written) = self.written.lock() {
                written.push(message.clone());
            }
            return;
        }
        #[allow(unreachable_code)]
        let _held = self.stdout.lock();
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{message}");
        let _ = stdout.flush();
    }

    fn answer(&self, id: &Value, result: Value) {
        self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn fail(&self, id: &Value, code: i64, message: &str, data: Option<Value>) {
        let mut error = json!({"code": code, "message": message});
        if let Some(data) = data {
            error["data"] = data;
        }
        self.write(&json!({"jsonrpc": "2.0", "id": id, "error": error}));
    }

    fn update(&self, session: &str, update: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": session, "update": update}}));
    }

    /// Hold what is sent for `session` from now until [`Out::release`].
    fn hold(&self, session: &str) {
        if let Ok(mut held) = self.held.lock() {
            held.insert(session.to_owned(), Vec::new());
        }
    }

    /// A replayed update of a held session, sent ahead of what is held.
    fn replayed(&self, session: &str, update: Value) {
        self.write(&json!({"jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": session, "update": update}}));
    }

    /// Send what was held for `session`, in order, and stop holding: while
    /// it goes, nothing else for the session overtakes it.
    fn release(&self, session: &str) {
        let Ok(mut held) = self.held.lock() else {
            return;
        };
        for message in held.remove(session).unwrap_or_default() {
            self.write(&message);
        }
    }

    /// Ask the client now, to wait on its answer later: questions put
    /// together reach it in the order they were put.
    fn asking(&self, method: &str, params: Value) -> oneshot::Receiver<Value> {
        let (id, answer) = self.reserve();
        self.request(id, method, params);
        answer
    }

    /// An id for a request to the client, and where its answer comes.
    fn reserve(&self) -> (i64, oneshot::Receiver<Value>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut waiting) = self.waiting.lock() {
            waiting.insert(id, tx);
        }
        (id, rx)
    }

    fn request(&self, id: i64, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    }

    /// Take back request `id`: the client is told to dismiss it, and
    /// whatever waits on its answer gets none.
    fn withdraw(&self, id: i64) {
        let waiting = self.waiting.lock().ok().and_then(|mut w| w.remove(&id));
        drop(waiting);
        self.write(&json!({"jsonrpc": "2.0", "method": "$/cancel_request",
            "params": {"requestId": id}}));
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
    /// Whether the client said `initialize`, which comes first.
    initialized: AtomicBool,
    /// Whether the client fills in forms (`elicitation/create`), which
    /// put the lead's questions to the person.
    forms: AtomicBool,
}

impl Agent {
    /// Sonny's agent for the daemon at `store` and `socket`, run by T3
    /// Code when `t3` reaches it.
    fn new(store: PathBuf, socket: PathBuf, t3: Option<t3::T3>) -> Arc<Self> {
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
            draft: None,
            reopened: false,
            land: false,
            before: None,
        };
        Arc::new(Agent {
            out: Arc::new(Out::default()),
            options,
            t3,
            sessions: Mutex::new(HashMap::new()),
            ready: OnceCell::new(),
            offer: Mutex::new(Offer::default()),
            initialized: AtomicBool::new(false),
            forms: AtomicBool::new(false),
        })
    }

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
        let method = message["method"].as_str().unwrap_or("");
        let answered = match method {
            "initialize" => self.initialize(params),
            _ if !self.initialized.load(Ordering::Relaxed) => {
                Err((INVALID_REQUEST, "initialize comes first".into()))
            }
            "session/new" => self.open(params, None).await,
            "session/resume" => match params["sessionId"].as_str() {
                Some(session) => self.open(params, Some(session)).await,
                None => Err((INVALID_PARAMS, "sessionId is missing".into())),
            },
            "session/prompt" => match self.session(params) {
                Ok(session) => session.prompt(&self, params).await,
                Err(error) => Err(error),
            },
            "session/list" => self.listed(params).await,
            REACT => match self.session(params) {
                Ok(session) => session.react(&self, params).await,
                Err(error) => Err(error),
            },
            "session/set_config_option" => match self.session(params) {
                Ok(session) => session.set_option(&self, params).await,
                Err(error) => Err(error),
            },
            "session/close" => self.close(params),
            other => Err((
                METHOD_NOT_FOUND,
                format!("{other} is not a method Sonny has"),
            )),
        };
        match answered {
            Ok(result) => self.out.answer(id, result),
            Err((code, message)) => {
                // The data each of these codes carries, from what was asked.
                let data = match code {
                    SESSION_NOT_FOUND => Some(json!({"sessionId": params["sessionId"]})),
                    UNSUPPORTED_VERSION => Some(json!({"requested": params["protocolVersion"],
                        "supported": [VERSION]})),
                    _ => None,
                };
                self.out.fail(id, code, &message, data)
            }
        }
    }

    async fn notified(&self, message: &Value) {
        // A subagent's session is cancelled as any other: it says it may be.
        if message["method"] == "session/cancel"
            && let Some(session) = self.find(&message["params"])
        {
            session.cancel(&self.out).await;
        }
    }

    fn find(&self, params: &Value) -> Option<Arc<Session>> {
        let id = params["sessionId"].as_str()?;
        self.sessions.lock().ok()?.get(id).cloned()
    }

    /// The session a request names, which the client may ask things of: a
    /// subagent's it only watches and may cancel.
    fn session(&self, params: &Value) -> Result<Arc<Session>, (i64, String)> {
        let session = self
            .find(params)
            .ok_or((SESSION_NOT_FOUND, "no such session".to_owned()))?;
        if session.parent.is_some() {
            return Err((
                INVALID_PARAMS,
                "a task's session is its lead's subagent: it takes only session/cancel".into(),
            ));
        }
        Ok(session)
    }

    /// The protocol's handshake: version 2 or none.
    fn initialize(&self, params: &Value) -> Reply {
        let asked = params["protocolVersion"]
            .as_i64()
            .filter(|version| (0..=65535).contains(version))
            .ok_or((
                INVALID_PARAMS,
                "protocolVersion must be a whole number from 0 to 65535".to_owned(),
            ))?;
        if asked < VERSION {
            return Err((UNSUPPORTED_VERSION, "Unsupported protocol version".into()));
        }
        let forms = !params["capabilities"]["elicitation"]["form"].is_null();
        self.forms.store(forms, Ordering::Relaxed);
        self.initialized.store(true, Ordering::Relaxed);
        // The `mcpServers` a client gives a session are not used, so no
        // transport is offered; T3 Code gives its own anyway, which Sonny
        // reaches through the endpoint it names in the environment.
        Ok(json!({
            "protocolVersion": VERSION,
            "info": {"name": "sonny", "title": "Sonny", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"session": {"prompt": {"image": {}, "embeddedContext": {}}}},
            "authMethods": []
        }))
    }

    /// Stop following a task's session, which its lead's no longer lists.
    fn forget(&self, child: &str) {
        let removed = self.sessions.lock().ok().and_then(|mut s| s.remove(child));
        let Some(removed) = removed else { return };
        if let Some((parent, _)) = &removed.parent
            && let Some(parent) = self
                .sessions
                .lock()
                .ok()
                .and_then(|s| s.get(parent).cloned())
            && let Ok(mut children) = parent.children.lock()
        {
            children.remove(child);
        }
        removed.stop(&self.out);
    }

    /// `session/close`: Sonny stops following the session's bot, and its
    /// subagents', and withdraws what it asked there. The bots work on, as
    /// when a window of Sonny's closes. One unknown is closed already.
    fn close(&self, params: &Value) -> Reply {
        let id = params["sessionId"]
            .as_str()
            .ok_or((INVALID_PARAMS, "sessionId is missing".to_owned()))?;
        let Ok(mut sessions) = self.sessions.lock() else {
            return Ok(json!({}));
        };
        match sessions.get(id) {
            None => return Ok(json!({})),
            Some(session) if session.parent.is_some() => {
                return Err((
                    INVALID_PARAMS,
                    "a task's session closes with its lead's".into(),
                ));
            }
            Some(_) => {}
        }
        let closed: Vec<Arc<Session>> = sessions
            .values()
            .filter(|session| {
                session.id == id || session.parent.as_ref().is_some_and(|(p, _)| p == id)
            })
            .cloned()
            .collect();
        for session in &closed {
            sessions.remove(&session.id);
        }
        drop(sessions);
        for session in closed {
            session.stop(&self.out);
        }
        Ok(json!({}))
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
        json!([{"configId": "model", "name": "Model", "category": "model",
            "type": "select", "currentValue": model, "options": models},
        {"configId": "effort", "name": "Effort", "category": "thought_level",
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
    /// task's thread or its folder's lead. One that comes back is the bot
    /// it was, its history replayed first when the client asks, then where
    /// it stands now.
    async fn open(self: &Arc<Self>, params: &Value, resumed: Option<&str>) -> Reply {
        let cwd = params["cwd"]
            .as_str()
            .map(PathBuf::from)
            .filter(|cwd| cwd.is_absolute())
            .ok_or((INVALID_PARAMS, "cwd must be an absolute path".to_owned()))?;
        // Only from the start: Sonny keeps no other cursor.
        let replay = match &params["replayFrom"] {
            Value::Null => false,
            from if resumed.is_some() && from["type"] == "start" => true,
            _ if resumed.is_none() => false,
            _ => return Err((INVALID_PARAMS, "Sonny replays from the start only".into())),
        };
        // A listed id names a lead; a task shows as its lead's subagent.
        let listed = resumed.and_then(listed_bot);
        if let Some(pinned) = &listed
            && !project::is_lead(&pinned.bot)
        {
            return Err((
                INVALID_PARAMS,
                format!("{} is a task: it shows as its lead's subagent", pinned.bot),
            ));
        }
        if let Some(id) = resumed
            && (self.sessions.lock().ok())
                .is_some_and(|s| s.get(id).is_some_and(|s| s.parent.is_some()))
        {
            return Err((INVALID_PARAMS, "a task's session is not resumed".into()));
        }
        self.ready().await.map_err(|e| (FAILED, e))?;
        // One connection for what the thread's pickers list.
        let lister = Client::connect(&self.options.socket).await.ok();
        let lister = lister.as_ref().map(|(client, _)| client.as_ref());
        self.list(lister).await;
        let leads = match lister {
            Some(client) if self.t3.is_none() => folder_leads(client, &cwd).await,
            _ => Vec::new(),
        };
        if let Some(client) = lister {
            client.close().await;
        }
        let id = match resumed {
            Some(id) => id.to_owned(),
            None => crate::unique("sonny"),
        };
        let session = Arc::new(Session {
            id: id.clone(),
            cwd,
            parent: None,
            state: AsyncMutex::new(State {
                leads,
                replaying: replay,
                ..State::default()
            }),
            follower: Mutex::new(None),
            switching: AsyncMutex::new(()),
            children: Mutex::new(HashMap::new()),
        });
        // What the session's bot does is held while its history replays.
        if resumed.is_some() {
            self.out.hold(&id);
        }
        // Open before it follows its bot; one it replaces stops following.
        let replaced = self
            .sessions
            .lock()
            .ok()
            .and_then(|mut sessions| sessions.insert(id.clone(), session.clone()));
        if let Some(old) = replaced {
            old.drop_children(self);
            old.stop(&self.out);
        }
        // A thread comes back as the bot it was, or not at all.
        let bound = match resumed {
            // A listed id names its lead itself, whatever the session was
            // moved to since.
            Some(id) => match listed.or_else(|| t3::recall(id)) {
                Some(pinned) => session.bind(self, Some(pinned), &project::Pick::Last).await,
                None => Err(format!("Sonny does not know thread {id}")),
            },
            None => Ok(()),
        };
        if let Err(error) = bound {
            if let Ok(mut sessions) = self.sessions.lock() {
                sessions.remove(&id);
            }
            self.out.release(&id);
            return Err((FAILED, error));
        }
        if resumed.is_some() {
            if replay {
                session.replay(&self.out).await;
                self.out
                    .replayed(&id, json!({"sessionUpdate": "replay_complete"}));
            }
            self.out.release(&id);
            session.current(self).await;
        }
        let options = session.options(self).await;
        Ok(match resumed {
            Some(_) => json!({"configOptions": options}),
            None => json!({"sessionId": id, "configOptions": options}),
        })
    }

    /// `session/list`: the leads Sonny can open a session on, the one
    /// folder's when the client names a `cwd`, else every project's, a
    /// page at a time. Each is listed by an id `session/resume` takes,
    /// which names the bot and its identity, so one removed and made again
    /// under its name is not the one reopened. Tasks are their leads'
    /// subagents, not sessions of their own.
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
                Ok((project, dir)) => project::leads(&client, &project, &dir)
                    .await
                    .map(|leads| (leads, None))
                    .map_err(|e| (FAILED, e)),
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
        let sessions: Vec<Value> = bots
            .iter()
            .filter_map(|bot| {
                let (name, bot_id) = (bot["name"].as_str()?, bot["bot_id"].as_i64()?);
                Some(
                    json!({"sessionId": lead_session(name, bot_id, store.as_deref()),
                    "cwd": bot["workspace"], "title": name}),
                )
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
    json!({"configId": AGENT, "name": "Agent",
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

/// A thread: the bot it is, once its first prompt says, and where its
/// work stands.
struct Session {
    id: String,
    cwd: PathBuf,
    /// For a task's session, the lead's whose subagent it is, by session
    /// and by bot.
    parent: Option<(String, String)>,
    state: AsyncMutex<State>,
    /// What follows its bot, stopped when the thread closes.
    follower: Mutex<Option<tokio::task::AbortHandle>>,
    /// Held while the bot's model changes: one change at a time.
    switching: AsyncMutex<()>,
    /// The subagents of a lead's session, by session, each with the state
    /// it last reported.
    children: Mutex<HashMap<String, Value>>,
}

impl Session {
    /// Stop following the bot, and take back what was asked for it.
    fn stop(self: &Arc<Self>, out: &Arc<Out>) {
        if let Some(follower) = self.follower.lock().ok().and_then(|mut f| f.take()) {
            follower.abort();
        }
        let withdraw = |state: &mut State| {
            for (request, _) in state.asking.drain() {
                out.withdraw(request);
            }
        };
        match self.state.try_lock() {
            Ok(mut state) => withdraw(&mut state),
            // The follower holds it until its abort lands: what it asked
            // is taken back once it lets go.
            Err(_) => {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let (session, out) = (self.clone(), out.clone());
                    runtime.spawn(async move {
                        let mut state = session.state.lock().await;
                        for (request, _) in state.asking.drain() {
                            out.withdraw(request);
                        }
                    });
                }
            }
        }
    }
}

/// A choice put to the person and not made yet: the call it is about, by
/// turn and id, and the turn it holds up, none once a message lifted the
/// call out of its turn, which then waits as background work.
#[derive(Debug)]
struct Waiting {
    holds: Option<i64>,
    call: (Option<i64>, String),
}

#[derive(Default)]
struct State {
    bound: Option<Bound>,
    /// The turn running.
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
    /// `lifted_ended` says: kept past their turn's end, as the session's
    /// background work.
    lifted: HashSet<(Option<i64>, String)>,
    /// Calls waiting on a gate, by turn and call, with the node that
    /// planned them: one a message lifts gets no `tool_started`, so it is
    /// known from here once it is allowed.
    gated: HashMap<(Option<i64>, String), i64>,
    /// Calls put to the person, by call and request, and the lead's
    /// questions, by ask.
    asked: HashSet<String>,
    /// The person's choices asked for and not made, by request.
    asking: HashMap<i64, Waiting>,
    /// This session's messages no turn has taken in yet: a cancel takes
    /// them back.
    queued: HashSet<i64>,
    /// Whether the session replays its history once it is bound, and the
    /// bot's newest node when its follow began, the last one the replay
    /// shows: what comes after shows live.
    replaying: bool,
    upto: Option<i64>,
    /// The last of the bot's kept events this thread handled, to follow on
    /// from when its stream drops.
    cursor: i64,
    /// Prompts being sent, and whether the person stopped one meanwhile.
    sending: usize,
    cancelled: bool,
    /// Messages shown already whose other sighting is still to come: one
    /// this session sent and showed, until its turn takes it in or it
    /// ends in line.
    sent: HashSet<i64>,
    /// Messages a turn took in while prompts were being sent, shown
    /// then: the prompt whose answer names one does not show it again.
    early: HashSet<i64>,
    /// The state last reported, as `state_update` has it.
    reported: Option<Value>,
    /// The model and effort the person picked, which the bot takes when
    /// it is next idle; a new lead starts on the model.
    picked: Option<String>,
    effort: Option<String>,
    /// The lead the client picked for a thread that is no bot yet, which
    /// its first prompt opens.
    agent: Option<String>,
    /// The leads of the thread's folder, as the agent picker lists them.
    leads: Vec<String>,
    /// Tasks whose threads are opening in T3 Code, as children of the work
    /// running, and the end of that work, held until they are open.
    opening: usize,
    held: Vec<Value>,
    /// The bot's replies this thread showed, newest last, each by its
    /// `messageId` with the bot's identity, its node and first line: what
    /// a client reacts to, while the thread is still that bot.
    replies: VecDeque<(String, Option<i64>, i64, String)>,
}

/// How many of the bot's latest replies a client may react to.
const REPLIES: usize = 64;

impl State {
    /// Whether message `message`, which a turn took in, is still to be
    /// shown: not when this session showed it as it sent it.
    fn unseen(&mut self, message: i64) -> bool {
        if self.sent.remove(&message) {
            return false;
        }
        if self.sending > 0 {
            self.early.insert(message);
        }
        true
    }

    /// Take back what this session asked the person about `call`, whose
    /// gate is decided: whether it asked anything.
    fn settle_asks(&mut self, out: &Out, call: &(Option<i64>, String)) -> bool {
        let moot: Vec<i64> = (self.asking.iter())
            .filter(|(_, waiting)| waiting.call == *call)
            .map(|(request, _)| *request)
            .collect();
        for request in &moot {
            self.asking.remove(request);
            out.withdraw(*request);
        }
        !moot.is_empty()
    }

    /// A turn's calls are over with it, but for those a message lifted:
    /// each still shown running ends as `ended` says, `failed` or
    /// `cancelled`, or is let go of unsaid.
    fn end_calls(&mut self, out: &Out, session: &str, ended: Option<&str>) {
        let lifted = &self.lifted;
        let over: Vec<_> = (self.calls.keys())
            .filter(|call| !lifted.contains(*call))
            .cloned()
            .collect();
        for (turn, call) in over {
            self.calls.remove(&(turn, call.clone()));
            if let Some(status) = ended {
                out.update(
                    session,
                    json!({"sessionUpdate": "tool_call_update",
                    "toolCallId": shown_id(turn, &call), "status": status}),
                );
            }
        }
    }

    /// The session's state as `state_update` reports it: idle, running, or
    /// waiting on the person, with its background work: the calls a
    /// message lifted, shown, and the subagents at work.
    fn snapshot(&self, children: &HashMap<String, Value>) -> Value {
        let phase = match self.running {
            None => "idle",
            Some(turn) if self.asking.values().any(|w| w.holds == Some(turn)) => "requires_action",
            Some(_) => "running",
        };
        let mut background: Vec<Value> = (self.lifted.iter())
            .filter(|call| self.calls.contains_key(*call) || self.gated.contains_key(*call))
            .map(|(turn, call)| json!({"type": "tool_call", "toolCallId": shown_id(*turn, call)}))
            .collect();
        background.sort_by(|a, b| a["toolCallId"].as_str().cmp(&b["toolCallId"].as_str()));
        let mut working: Vec<&String> = (children.iter())
            .filter(|(_, child)| {
                matches!(
                    child["state"]["state"].as_str(),
                    Some("running" | "requires_action")
                )
            })
            .map(|(child, _)| child)
            .collect();
        working.sort();
        background.extend(
            working
                .into_iter()
                .map(|child| json!({"type": "subagent", "sessionId": child})),
        );
        json!({"state": phase, "background": background})
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
    /// The session's subagents, each as its last `subagent_update` said.
    fn children(&self) -> HashMap<String, Value> {
        self.children.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Report the session's state, if it changed.
    fn report(&self, agent: &Arc<Agent>, state: &mut State) {
        let now = state.snapshot(&self.children());
        if state.reported.as_ref() != Some(&now) {
            self.say_state(agent, state, now);
        }
    }

    /// Report `now`, which may say how the work ended: on the session, and
    /// for a task, as its subagent's state on its lead's session. An end
    /// waits while tasks' threads open in T3 Code, as children of the work.
    fn say_state(&self, agent: &Arc<Agent>, state: &mut State, now: Value) {
        let mut plain = now.clone();
        if let Value::Object(fields) = &mut plain {
            fields.remove("stopReason");
            fields.remove("error");
        }
        state.reported = Some(plain);
        let mut update = now.clone();
        update["sessionUpdate"] = json!("state_update");
        if state.opening > 0 && now["state"] == "idle" {
            state.held.push(update);
        } else {
            agent.out.update(&self.id, update);
        }
        if let Some((parent, _)) = &self.parent {
            self.mirror(agent, parent, now);
        }
    }

    /// A subagent's state, on its lead's session too, whose background
    /// work it is while it works.
    fn mirror(&self, agent: &Arc<Agent>, parent: &str, now: Value) {
        agent.out.update(
            parent,
            json!({"sessionUpdate": "subagent_update", "sessionId": self.id, "state": now}),
        );
        let parent = (agent.sessions.lock().ok()).and_then(|s| s.get(parent).cloned());
        let Some(parent) = parent else { return };
        if let Ok(mut children) = parent.children.lock()
            && let Some(child) = children.get_mut(&self.id)
        {
            child["state"] = now;
        }
        let agent = agent.clone();
        tokio::spawn(async move {
            let mut state = parent.state.lock().await;
            parent.report(&agent, &mut state);
        });
    }

    /// Where the session stands, once it comes back: each piece of
    /// background work as it is now, then its state.
    async fn current(&self, agent: &Arc<Agent>) {
        let mut state = self.state.lock().await;
        let mut lifted: Vec<_> = state.lifted.iter().cloned().collect();
        lifted.sort();
        for key in lifted {
            if let Some((name, arguments)) = state.calls.get(&key) {
                let shown = shown_id(key.0, &key.1);
                (agent.out).update(&self.id, tool_call(&shown, name, arguments, "in_progress"));
            }
        }
        let mut children: Vec<(String, Value)> = self.children().into_iter().collect();
        children.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, child) in children {
            let mut update = child;
            update["sessionUpdate"] = json!("subagent_update");
            agent.out.update(&self.id, update);
        }
        let now = state.snapshot(&self.children());
        self.say_state(agent, &mut state, now);
    }

    /// Let go of the session's subagents: they stop being followed here.
    fn drop_children(&self, agent: &Agent) {
        let ids: Vec<String> = self
            .children
            .lock()
            .map(|mut c| c.drain().map(|(id, _)| id).collect())
            .unwrap_or_default();
        let dropped: Vec<Arc<Session>> = match agent.sessions.lock() {
            Ok(mut sessions) => ids.iter().filter_map(|id| sessions.remove(id)).collect(),
            Err(_) => Vec::new(),
        };
        for child in dropped {
            child.stop(&agent.out);
        }
    }

    /// Make this thread `bot`'s, or its folder's lead's, and follow it.
    /// With no bot, `pick` picks which of the folder's leads. A bot pinned
    /// to its identity and store is that one or none.
    async fn bind(
        self: &Arc<Self>,
        agent: &Arc<Agent>,
        bot: Option<t3::Pinned>,
        pick: &project::Pick,
    ) -> Result<(), String> {
        // The subagents of the lead it was are not this one's.
        self.drop_children(agent);
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
        if self.state.lock().await.replaying {
            let newest = client
                .request("history_nodes", json!({"bot": bot, "limit": 1}))
                .await
                .map_err(|e| e.to_string())?;
            self.state.lock().await.upto = Some(newest["nodes"][0]["node"].as_i64().unwrap_or(0));
        }
        client
            .request("follow", json!({"bot": bot, "after": 0}))
            .await
            .map_err(|e| e.to_string())?;
        // Calls started before this thread came and not done yet: a task a
        // lead is starting still opens its thread when it is done.
        let (mut started, mut lifted) = (HashMap::new(), HashSet::new());
        let (mut gated, mut answering) = (HashMap::new(), HashSet::new());
        // Each history node is written with an event naming it, so the
        // newest node the events before the live cursor name is where the
        // replay ends and the live follow takes over.
        let mut written = None;
        // Each turn's replies so far, as the live follow numbers them: one
        // more with each call it started.
        let mut parts: HashMap<i64, u32> = HashMap::new();
        let cursor = live(&mut events, |event| {
            let data = &event["data"];
            written = written.max(data["node"].as_i64());
            if let Some(turn) = event["turn"].as_i64() {
                match event["event"].as_str() {
                    Some("accepted") => {
                        parts.insert(turn, 0);
                    }
                    Some("tool_started") => *parts.entry(turn).or_default() += 1,
                    _ => {}
                }
            }
            let call = (
                event["turn"].as_i64(),
                data["call_id"].as_str().unwrap_or("").to_owned(),
            );
            match event["event"].as_str() {
                Some("approval_requested") => {
                    gated.extend(planned(&event));
                }
                Some("tool_started") => {
                    gated.remove(&call);
                    started.insert(call, data.clone());
                }
                Some("call_lifted") if started.contains_key(&call) || gated.contains_key(&call) => {
                    lifted.insert(call);
                }
                Some("tool_completed") if !lifted.contains(&call) => {
                    gated.remove(&call);
                    started.remove(&call);
                }
                // Lifted while it waited on its gate and allowed: it
                // started with no `tool_started`, and its answer ends it.
                Some("lifted_ended") if data["waits_for"] == "answer" => {
                    if data["outcome"] == "allowed" && !started.contains_key(&call) {
                        answering.insert(call);
                    }
                }
                Some("lifted_ended") => {
                    lifted.remove(&call);
                    started.remove(&call);
                    gated.remove(&call);
                    answering.remove(&call);
                }
                _ => {}
            }
            async {}
        })
        .await;
        {
            let mut state = self.state.lock().await;
            if state.replaying {
                state.upto = state.upto.max(written);
            }
        }
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
        for key in answering {
            if let Some(node) = gated.remove(&key)
                && let Some((name, arguments)) = planned_call(&client, &bot, node, &key.1).await
                && name != reactions::TOOL
                && name != plan::TOOL
            {
                calls.insert(key, (name, arguments));
            }
        }
        lifted.retain(|call| calls.contains_key(call) || gated.contains_key(call));
        let pinned = t3::Pinned {
            bot: bot.clone(),
            bot_id,
            store: client.store().map(str::to_owned),
        };
        // A thread T3 Code may resume; a task's session is its lead's.
        if self.parent.is_none() {
            t3::remember(&self.id, &pinned);
        }
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
            state.part = running
                .and_then(|turn| parts.get(&turn).copied())
                .unwrap_or(0);
            state.calls = calls;
            state.lifted = lifted;
            state.gated = gated;
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
        }
        // A task's model is its lead's to pick: its session offers none.
        if self.parent.is_none() {
            self.show_options(agent).await;
        }
        let (session, agent) = (self.clone(), agent.clone());
        // Calls already waiting for the person, whose events were before
        // this thread came, are put to it too.
        let waiting = {
            let state = self.state.lock().await;
            (state.bound.as_ref()).map(|b| (b.client.clone(), b.bot.clone(), state.running))
        };
        let follower = self.clone();
        // A lead's tasks are its subagents, for a client that shows them:
        // T3 Code shows each task in a thread of its own.
        let lead = match &waiting {
            Some((_, bot, _)) if agent.t3.is_none() && project::is_lead(bot) => bot_id,
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
                if let Some(lead) = lead {
                    follower.adopt_tasks(&agent, client, lead).await;
                }
            }
            follower.follow(&agent, events).await;
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
        let option = &params["configId"];
        let (Some(value), "id") = (
            params["value"].as_str(),
            params["type"].as_str().unwrap_or(""),
        ) else {
            return Err((
                INVALID_PARAMS,
                "an option is picked by its value's id (type `id`)".into(),
            ));
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
        if state.running.is_some() || state.sending > 0 {
            return Err((
                INVALID_PARAMS,
                format!("{} is answering; pick another lead once it has", bound.bot),
            ));
        }
        drop(state);
        let pick = project::Pick::Named(project::lead_short(value).to_owned());
        self.bind(agent, None, &pick)
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

    /// The person's prompt, answered with its message's id once the bot has
    /// it: the work it starts or joins is reported as the session's state.
    async fn prompt(self: &Arc<Self>, agent: &Arc<Agent>, params: &Value) -> Reply {
        let person = person(params).map_err(|error| (INVALID_PARAMS, error))?;
        let key = idempotency(params).map_err(|error| (INVALID_PARAMS, error))?;
        let shown = &params["prompt"];
        accepted_blocks(shown).map_err(|error| (INVALID_PARAMS, error))?;
        let (text, images) = words(shown);
        // T3 Code's note that a child thread ended starts nothing: the lead
        // hears of its tasks from the lookout.
        if agent.t3.is_some() && images.is_empty() && t3::is_completion(&text) {
            return Ok(self.handled(agent, shown, None).await);
        }
        let bound = {
            let mut state = self.state.lock().await;
            state.cancelled = false;
            state.bound.as_ref().map(|bound| bound.bot.clone())
        };
        // The person wrote: they are here, and back for the agent they
        // wrote to, if it was told they were away.
        crate::away::here(bound.as_deref());
        if bound.is_none() {
            // The first prompt of a thread T3 Code opened for a task is
            // that task's brief.
            let claimed = t3::claim(&text, &self.cwd);
            let task = claimed.is_some();
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
            self.state.lock().await.sending += 1;
            let bound = self.bind(agent, claimed, &pick).await;
            let cancelled = {
                let mut state = self.state.lock().await;
                state.sending -= 1;
                std::mem::take(&mut state.cancelled)
            };
            bound.map_err(|error| (FAILED, error))?;
            // The agent they wrote to, now that the thread has one.
            let bot = (self.state.lock().await.bound.as_ref()).map(|b| b.bot.clone());
            crate::away::here(bot.as_deref());
            if chosen.is_some() {
                let said = format!("This thread is {}.", bot.unwrap_or_default());
                return Ok(self.handled(agent, shown, Some(&said)).await);
            }
            if task {
                return self.claimed(agent, shown).await;
            }
            if cancelled {
                return Err((
                    REQUEST_CANCELLED,
                    "the prompt was cancelled before it was sent".into(),
                ));
            }
        }
        // A model or effort picked while the bot worked: the bot takes it
        // before this turn.
        self.settle(agent).await;
        let mut state = self.state.lock().await;
        if std::mem::take(&mut state.cancelled) {
            return Err((
                REQUEST_CANCELLED,
                "the prompt was cancelled before it was sent".into(),
            ));
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
        // A key the client sends again with its prompt names the message
        // it sent first: the daemon takes one message for one request id.
        let request = match &key {
            Some(key) => keyed(key),
            None => crate::unique("acp"),
        };
        let mut asked = json!({"bot": bound.bot, "bot_id": bound.bot_id,
            "request_id": request,
            "prompt": text,
            "delivery": if moving.is_some() { "queue" } else { "steer" },
            "images": if images.is_empty() { Value::Null } else { json!(images) }});
        if let Some(person) = person {
            asked["person"] = json!(person);
        }
        if let Some(dir) = moving {
            asked["workspace"] = json!(dir);
        }
        let (client, bot) = (bound.client.clone(), bound.bot.clone());
        // The lead its folder opens next in Sonny.
        project::wrote_to(bound.client.store(), &bound.workspace, &bound.bot);
        state.sending += 1;
        drop(state);
        let reply = client.request("submit", asked).await;
        let mut state = self.state.lock().await;
        state.sending -= 1;
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) if error.code == "idempotency_conflict" => {
                return Err((INVALID_PARAMS, error.to_string()));
            }
            Err(error) => return Err((FAILED, error.to_string())),
        };
        if let Some(bound) = &mut state.bound
            && !steer
        {
            bound.moving = None;
        }
        let message = reply["message"].as_i64();
        // Stopped while it was being sent: it is in, so it is answered,
        // and its work stopped.
        if std::mem::take(&mut state.cancelled)
            && let (Some(bound), Some(turn)) = (&state.bound, message)
        {
            let _ = (bound.client)
                .request("interrupt", json!({"bot": bound.bot, "turn": turn}))
                .await;
        }
        let inserted = user_id(&bot, message);
        // Shown already when its turn took it before the daemon answered.
        let fresh = message.is_none_or(|message| !state.early.remove(&message));
        if state.sending == 0 {
            // No prompt in flight is left to name the others.
            state.early.clear();
        }
        let duplicate = reply["duplicate"] == true;
        if fresh
            && !duplicate
            && let Some(message) = message
        {
            state.queued.insert(message);
            state.sent.insert(message);
        }
        if fresh && !duplicate {
            agent.out.update(
                &self.id,
                json!({"sessionUpdate": "user_message", "messageId": inserted, "content": shown}),
            );
        }
        Ok(json!({"messageId": inserted}))
    }

    /// A message Sonny takes in itself, as a command it answers: shown,
    /// with what Sonny says to it, as work of its own when the bot is idle.
    async fn handled(&self, agent: &Arc<Agent>, content: &Value, said: Option<&str>) -> Value {
        let message = crate::unique(&format!("{}-said", self.id));
        let mut state = self.state.lock().await;
        let out = &agent.out;
        out.update(
            &self.id,
            json!({"sessionUpdate": "user_message", "messageId": message, "content": content}),
        );
        let idle = state.running.is_none();
        if idle {
            out.update(
                &self.id,
                json!({"sessionUpdate": "state_update", "state": "running",
                "background": state.snapshot(&self.children())["background"]}),
            );
        }
        if let Some(said) = said {
            out.update(
                &self.id,
                json!({"sessionUpdate": "agent_message_chunk",
                "messageId": format!("{message}-reply"), "content": {"type": "text", "text": said}}),
            );
        }
        if idle {
            let mut now = state.snapshot(&self.children());
            now["stopReason"] = json!("end_turn");
            self.say_state(agent, &mut state, now);
        }
        json!({"messageId": message})
    }

    /// The first prompt of the thread T3 Code opened for a task: its
    /// brief, which the task's first turn took in already. It is that
    /// message, and its work is that turn's.
    async fn claimed(&self, agent: &Arc<Agent>, shown: &Value) -> Reply {
        let (client, bot) = {
            let state = self.state.lock().await;
            let bound = state
                .bound
                .as_ref()
                .ok_or((FAILED, "the thread has no bot".to_owned()))?;
            (bound.client.clone(), bound.bot.clone())
        };
        let first = client
            .request("messages", json!({"bot": bot, "after": 0, "limit": 1}))
            .await
            .ok()
            .and_then(|page| page["messages"][0]["message"].as_i64());
        let message = user_id(&bot, first);
        let mut state = self.state.lock().await;
        if let Some(first) = first {
            state.sent.insert(first);
        }
        agent.out.update(
            &self.id,
            json!({"sessionUpdate": "user_message", "messageId": message, "content": shown}),
        );
        if state.running.is_some() {
            self.report(agent, &mut state);
        } else {
            // Its turn ended before the thread came: its reply, as the
            // work this prompt stands for.
            let mut now = state.snapshot(&self.children());
            now["state"] = json!("running");
            self.say_state(agent, &mut state, now);
            drop(state);
            self.say_last(agent).await;
            let mut state = self.state.lock().await;
            let mut now = state.snapshot(&self.children());
            now["stopReason"] = json!("end_turn");
            self.say_state(agent, &mut state, now);
        }
        Ok(json!({"messageId": message}))
    }

    /// `session/cancel`: the work running stops, and with it what it asks
    /// of the person; a prompt still being sent stops once it is in.
    async fn cancel(&self, out: &Out) {
        let mut state = self.state.lock().await;
        let running = state.running;
        let stopped: Vec<i64> = (state.asking.iter())
            .filter(|(_, w)| w.holds.is_some() && w.holds == running)
            .map(|(request, _)| *request)
            .collect();
        for request in stopped {
            state.asking.remove(&request);
            out.withdraw(request);
        }
        if state.sending > 0 {
            state.cancelled = true;
        }
        // The turn running, and the session's messages waiting in line,
        // each stopped by its number, which a turn takes from its opener.
        let stopping: Vec<i64> = running.into_iter().chain(state.queued.drain()).collect();
        if let Some(bound) = &state.bound {
            for turn in stopping {
                let _ = (bound.client)
                    .request("interrupt", json!({"bot": bound.bot, "turn": turn}))
                    .await;
            }
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
            let mut lost = false;
            events = loop {
                if !open() {
                    return;
                }
                match self.reattach(agent).await {
                    Ok(events) => break events,
                    Err(_) => {
                        // While the daemon comes back, the session's work
                        // is not known: said once, not left as running.
                        if !lost {
                            let mut state = self.state.lock().await;
                            let mut now = state.snapshot(&self.children());
                            now["state"] = json!("unknown");
                            self.say_state(agent, &mut state, now);
                        }
                        lost = true;
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
            // (its events gone) ended if the daemon runs another, how is
            // not known.
            let was = state.running;
            if was.is_some() && running != was {
                state.end_calls(&agent.out, &self.id, Some("failed"));
                state.running = None;
                let mut now = state.snapshot(&self.children());
                now["stopReason"] = Value::Null;
                self.say_state(agent, &mut state, now);
            }
            state.running = running;
            // Where it stands now, which was not known while it was away.
            state.reported = None;
            self.report(agent, &mut state);
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
        let workspace = bound.workspace.clone();
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
                    state.end_calls(out, id, None);
                }
                state.running = turn;
                if let Some(turn) = turn {
                    state.queued.remove(&turn);
                }
                self.report(agent, &mut state);
                // A turn the bot took on its own, as one the lookout woke
                // it for: what started it, as the thread's next message.
                // Shown once: a prompt of this session's, whose answer is
                // still on its way, is not shown again.
                if let Some(message) = turn
                    && state.unseen(message)
                    && let Some(node) = data["node"].as_i64()
                {
                    self.show_message(out, &client, &bot, message, node, data)
                        .await;
                }
            }
            "steered" => {
                if let Some(message) = data["message"].as_i64() {
                    state.queued.remove(&message);
                }
                // A message the running turn took in from elsewhere, as
                // from another window: the thread's next message too.
                if data["attached"] != true
                    && let Some(message) = data["message"].as_i64()
                    && state.unseen(message)
                    && let Some(node) = data["node"].as_i64()
                {
                    self.show_message(out, &client, &bot, message, node, data)
                        .await;
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
                    out.update(id, plan_update(&bot, &arguments));
                    return;
                }
                out.update(
                    id,
                    tool_call(&shown_id(turn, &call), &name, &arguments, "in_progress"),
                );
                state.gated.remove(&(turn, call.clone()));
                // Allowed elsewhere: what this session asked is moot.
                state.settle_asks(out, &(turn, call.clone()));
                self.report(agent, &mut state);
                state.calls.insert((turn, call), (name, arguments));
            }
            // A call a message lifted goes on in the background: its
            // turn's answer to it is its handle, and it ends later.
            "call_lifted" => {
                let key = (turn, data["call_id"].as_str().unwrap_or("").to_owned());
                // Its question to the person, if any, waits on with it.
                for waiting in state.asking.values_mut() {
                    if waiting.call == key {
                        waiting.holds = None;
                    }
                }
                if state.calls.contains_key(&key) || state.gated.contains_key(&key) {
                    state.lifted.insert(key);
                }
                self.report(agent, &mut state);
            }
            "tool_completed" | "lifted_ended" => {
                let call = data["call_id"].as_str().unwrap_or("").to_owned();
                let key = (turn, call.clone());
                let lifted = event["event"] == "lifted_ended";
                if !lifted && state.lifted.contains(&key) {
                    return;
                }
                let gated = state.gated.remove(&key);
                // Decided, here or elsewhere: what this session asked is
                // moot.
                if state.settle_asks(out, &key) {
                    self.report(agent, &mut state);
                }
                // Its approval is decided: a call of a later turn under the
                // same id is put to the person afresh.
                let prompted = asked_key(turn, &call, "");
                state.asked.retain(|asked| !asked.starts_with(&prompted));
                match lifted {
                    // Allowed, one its client answers waits on for that
                    // answer. One lifted while it waited on its gate had
                    // no `tool_started`: it starts here, and its answer
                    // ends it as any call's does.
                    true if data["waits_for"] == "answer" => {
                        if data["outcome"] == "allowed"
                            && !state.calls.contains_key(&key)
                            && let Some(node) = gated
                            && let Some((name, arguments)) =
                                planned_call(&client, &bot, node, &call).await
                            && name != reactions::TOOL
                            && name != plan::TOOL
                        {
                            let shown = shown_id(turn, &call);
                            out.update(id, tool_call(&shown, &name, &arguments, "in_progress"));
                            state.calls.insert(key, (name, arguments));
                        }
                        self.report(agent, &mut state);
                        return;
                    }
                    true if !state.lifted.remove(&key) => return,
                    _ => {}
                }
                // One denied at its gate never started: it ends here, as a
                // replay or a client that saw it ask shows it.
                let unstarted = !lifted && !state.calls.contains_key(&key);
                let ended = match (state.calls.remove(&key), gated) {
                    (Some(known), _) => Some(known),
                    (None, Some(node)) if unstarted && data["denied"] == true => {
                        planned_call(&client, &bot, node, &call)
                            .await
                            .filter(|(name, _)| name != reactions::TOOL && name != plan::TOOL)
                    }
                    _ => None,
                };
                let Some((name, arguments)) = ended else {
                    if lifted {
                        self.report(agent, &mut state);
                    }
                    return;
                };
                // The client knows the call by its turn too: a later turn's
                // call of the same id is another.
                let shown_call = shown_id(turn, &call);
                let (mut shown, mut failed) = (String::new(), data["failed"] == true);
                let mut result = None;
                if lifted {
                    // Allowed and run as a background command, its end is
                    // the bot's to hear: the call's own part is over.
                    let ended = &data["result"];
                    shown = (ended["answer"].as_str())
                        .or(ended["output"].as_str())
                        .or(ended["detail"].as_str())
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
                    result = Some(item);
                }
                if data["denied"] == true {
                    failed = true;
                }
                let status = if failed { "failed" } else { "completed" };
                let ran = Ran {
                    call: &shown_call,
                    name: &name,
                    arguments: &arguments,
                    shown: &shown,
                    result: result.as_ref(),
                    failed,
                    workspace: &workspace,
                };
                let content = ended_content(out, id, &ran, true);
                let mut update = match unstarted {
                    // Named in full: the client may know it from nowhere else.
                    true => tool_call(&shown_call, &name, &arguments, status),
                    false => json!({"sessionUpdate": "tool_call_update",
                        "toolCallId": shown_call, "status": status}),
                };
                update["content"] = json!(content);
                out.update(id, update);
                if lifted {
                    self.report(agent, &mut state);
                }
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
                    let asked = out.asking("elicitation/create", form(id, &ask, Some(&shown_call)));
                    let lead = (bot, state.bound.as_ref().and_then(|b| b.bot_id));
                    let (agent, session) = (agent.clone(), self.id.clone());
                    tokio::spawn(async move { elicit(asked, &agent, &session, lead, &ask).await });
                    return;
                }
                let started =
                    name == crate::served::START && !failed && shown.starts_with("Started ");
                let project = state.bound.as_ref().and_then(|b| b.project.clone());
                let this = (agent.sessions.lock().ok())
                    .and_then(|sessions| sessions.get(&self.id).cloned());
                // A task the lead started: the lead session's subagent, for
                // a client that shows them.
                if started
                    && agent.t3.is_none()
                    && let Some(project) = &project
                    && let Some(task) = task_bot(project, &arguments)
                    && let Some(lead) = this
                {
                    let agent = agent.clone();
                    let linked = Some((shown_call, cut(&shown)));
                    tokio::spawn(async move { lead.adopt(&agent, &client, &task, linked).await });
                    return;
                }
                // A task the lead started opens as a thread of its own.
                if started
                    && let Some(project) = project
                    && agent.t3.is_some()
                    && let Some(session) = this
                {
                    state.opening += 1;
                    let agent = agent.clone();
                    tokio::spawn(async move {
                        let (opener, thread) = (agent.clone(), session.cwd.clone());
                        let session_id = session.id.clone();
                        let opening = tokio::spawn(async move {
                            let at = Where {
                                project: &project,
                                workspace: &workspace,
                                thread: &thread,
                            };
                            let session = &session_id;
                            let call = &shown_call;
                            open_task(&opener, session, &client, &at, &arguments, call).await;
                        });
                        // The work waits for its tasks' threads, not for
                        // ever: one still opening opens on its own.
                        let _ = tokio::time::timeout(OPENING, opening).await;
                        let mut state = session.state.lock().await;
                        state.opening -= 1;
                        if state.opening == 0 {
                            for update in std::mem::take(&mut state.held) {
                                agent.out.update(&session.id, update);
                            }
                        }
                    });
                }
            }
            "approval_requested" | "approval_asked" => {
                if event["event"] == "approval_requested" {
                    state.gated.extend(planned(event));
                }
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
                if state.running.is_some() && state.running != turn {
                    return;
                }
                let (reason, error) = stop_reason(data);
                let unfinished = if reason == "cancelled" {
                    "cancelled"
                } else {
                    "failed"
                };
                state.end_calls(out, id, Some(unfinished));
                // What the work asked of the person and was not answered
                // is taken back with it.
                let stale: Vec<i64> = (state.asking.iter())
                    .filter(|(_, w)| w.holds.is_some() && w.holds == turn)
                    .map(|(request, _)| *request)
                    .collect();
                for request in stale {
                    state.asking.remove(&request);
                    out.withdraw(request);
                }
                state.running = None;
                let mut now = state.snapshot(&self.children());
                now["stopReason"] = json!(reason);
                if let Some(error) = error {
                    now["error"] = error;
                }
                self.say_state(agent, &mut state, now);
            }
            "message_ended" => {
                // A message this session sent that ended in line, withdrawn
                // or failed before any turn took it in: the work it was
                // to start ended so. One queued behind a running turn is
                // that turn's to end.
                let ended = event["message"].as_i64();
                if let Some(ended) = ended {
                    state.queued.remove(&ended);
                }
                if ended.is_some_and(|ended| state.sent.remove(&ended)) && state.running.is_none() {
                    let mut now = state.snapshot(&self.children());
                    match data["status"].as_str() {
                        Some("withdrawn") => now["stopReason"] = json!("cancelled"),
                        _ => {
                            now["stopReason"] = json!("error");
                            now["error"] = json!({"code": FAILED,
                                "message": data["error"].as_str().unwrap_or("the message failed")});
                        }
                    }
                    self.say_state(agent, &mut state, now);
                }
            }
            "deleted" => {
                let gone = format!("{bot} was deleted");
                if state.running.take().is_some() {
                    state.end_calls(out, id, Some("failed"));
                    let mut now = state.snapshot(&self.children());
                    now["stopReason"] = json!("error");
                    now["error"] = json!({"code": FAILED, "message": gone});
                    self.say_state(agent, &mut state, now);
                } else {
                    out.update(
                        id,
                        json!({"sessionUpdate": "notice", "severity": "warning", "title": gone}),
                    );
                }
                // A task removed is done: its session is not followed on,
                // so another bot made under its name is not taken for it.
                if self.parent.is_some() {
                    let (agent, child) = (agent.clone(), self.id.clone());
                    tokio::spawn(async move { agent.forget(&child) });
                }
            }
            _ => {}
        }
    }

    /// A message the session did not send, by its number and node: from
    /// the lead, for a task's session, as the lead's message to it; else
    /// as the session's next message, as one the lookout sent.
    async fn show_message(
        &self,
        out: &Out,
        client: &Client,
        bot: &str,
        message: i64,
        node: i64,
        data: &Value,
    ) {
        let Some(item) = items(client, bot, &[node]).await.into_iter().next() else {
            return;
        };
        let text = crate::text(&item);
        if reactions::is_reaction(&text) {
            return;
        }
        let content = json!([{"type": "text", "text": text}]);
        let messaged = user_id(bot, Some(message));
        let update = match &self.parent {
            Some((parent, lead)) if data["from"]["bot"] == lead.as_str() => {
                json!({"sessionUpdate": "session_message", "messageId": messaged,
                    "senderSessionId": parent, "content": content})
            }
            _ => {
                json!({"sessionUpdate": "user_message", "messageId": messaged, "content": content})
            }
        };
        out.update(&self.id, update);
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
            let Some(session) = (agent.sessions.lock().ok()).and_then(|s| s.get(&self.id).cloned())
            else {
                return;
            };
            let (agent, client, bot, call) =
                (agent.clone(), client.clone(), bot.to_owned(), call.clone());
            tokio::spawn(async move { ask(&agent, &session, &client, &bot, &call).await });
        }
    }
}

impl Session {
    /// The lead's questions still open from before this thread followed
    /// it, each put to the person as a form. Read back from its history,
    /// newest first, as far back as every surface reads it.
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
    /// The lead's tasks that still matter when its session comes, as its
    /// subagents: each running, or with a question open, from tasks that
    /// ran in the last week.
    async fn adopt_tasks(self: &Arc<Self>, agent: &Arc<Agent>, client: &Client, lead_id: i64) {
        let Ok(tasks) = tasks_of(client, lead_id).await else {
            return;
        };
        let now = now_ms();
        for task in tasks.iter().filter(|task| lately(task, now)) {
            let Some(name) = task["name"].as_str() else {
                continue;
            };
            let running = task["running_turn"].as_i64().is_some();
            // One closed, by itself or its lead, with no turn since, is
            // done: it has nothing open.
            if crate::resolved::done(crate::resolved::closed(task), running) {
                continue;
            }
            if running || !open_asks(client, name).await.is_empty() {
                self.adopt(agent, client, name, None).await;
            }
        }
    }

    /// Show `task`, a task the lead started, as the lead session's
    /// subagent: announced, its brief as the lead's message to it, then
    /// followed in a session of its own, which takes only `session/cancel`
    /// and answers to what Sonny asks there. `call`, the lead's
    /// `start_task` call that started it, with what it said, links to it.
    async fn adopt(
        self: &Arc<Self>,
        agent: &Arc<Agent>,
        client: &Client,
        task: &str,
        call: Option<(String, String)>,
    ) {
        let Ok(record) = client.request("inspect", json!({"bot": task})).await else {
            return;
        };
        let Some(bot_id) = record["bot_id"].as_i64() else {
            return;
        };
        // Its own under this session: another session on the same lead
        // follows the task in a session of its own.
        let child = format!("{}/{}", self.id, lead_session(task, bot_id, client.store()));
        let workspace = record["workspace"].as_str().unwrap_or_default().to_owned();
        let Some(lead) = (self.state.lock().await.bound.as_ref()).map(|b| b.bot.clone()) else {
            return;
        };
        let known = self
            .children
            .lock()
            .map_or(true, |c| c.contains_key(&child));
        let out = &agent.out;
        if !known {
            let cwd = Some(&workspace).filter(|w| Path::new(w).is_absolute());
            let announced = json!({"sessionId": child, "title": task, "cwd": cwd,
                "capabilities": {"cancel": {}}});
            if let Ok(mut children) = self.children.lock() {
                children.insert(child.clone(), announced.clone());
            }
            let mut update = announced;
            update["sessionUpdate"] = json!("subagent_update");
            out.update(&self.id, update);
        }
        if let Some((call, said)) = call {
            out.update(
                &self.id,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": call,
                "content": [text_content(&said), {"type": "session", "sessionId": child}]}),
            );
        }
        if known {
            return;
        }
        let session = Arc::new(Session {
            id: child.clone(),
            cwd: PathBuf::from(&workspace),
            parent: Some((self.id.clone(), lead)),
            state: AsyncMutex::new(State::default()),
            follower: Mutex::new(None),
            switching: AsyncMutex::new(()),
            children: Mutex::new(HashMap::new()),
        });
        if let Ok(mut sessions) = agent.sessions.lock() {
            sessions.insert(child.clone(), session.clone());
        }
        // Its brief, the first message it was sent, as the lead's to it.
        if let Some((first, brief)) = brief(client, task).await {
            session.state.lock().await.sent.insert(first);
            out.update(
                &child,
                json!({"sessionUpdate": "session_message", "messageId": user_id(task, Some(first)),
                "senderSessionId": self.id, "content": [{"type": "text", "text": brief}]}),
            );
        }
        let pinned = t3::Pinned {
            bot: task.to_owned(),
            bot_id: Some(bot_id),
            store: client.store().map(str::to_owned),
        };
        if bind_task(session.clone(), agent.clone(), pinned)
            .await
            .is_err()
        {
            // Not followed: its state is not known.
            let mut state = session.state.lock().await;
            let mut now = state.snapshot(&HashMap::new());
            now["state"] = json!("unknown");
            session.say_state(agent, &mut state, now);
            return;
        }
        // One that worked before it was shown: its last reply.
        if session.state.lock().await.running.is_none() {
            session.say_last(agent).await;
        }
        let mut state = session.state.lock().await;
        session.report(agent, &mut state);
    }

    /// Replay the bot's history, from its start, as the session's: the
    /// messages, the bot's replies and its calls as they ended. What only
    /// described its state then (its states, notices, options) is not
    /// history.
    async fn replay(&self, out: &Out) {
        let (client, bot) = {
            let state = self.state.lock().await;
            let Some(bound) = &state.bound else { return };
            (bound.client.clone(), bound.bot.clone())
        };
        // Its nodes up to where the live follow took over, newest first as
        // the daemon lists them: only their ids are kept, and the items
        // are read a page at a time, oldest first.
        let upto = self.state.lock().await.upto;
        let mut nodes: Vec<(i64, i64)> = Vec::new();
        let mut from = Value::Null;
        loop {
            let Ok(page) = client
                .request(
                    "history_nodes",
                    json!({"bot": bot, "from": from, "limit": 200}),
                )
                .await
            else {
                break;
            };
            nodes.extend(
                (page["nodes"].as_array().into_iter().flatten())
                    .filter_map(|n| Some((n["node"].as_i64()?, n["turn"].as_i64()?)))
                    .filter(|(node, _)| upto.is_none_or(|upto| *node <= upto)),
            );
            from = page["next_from"].clone();
            if from.is_null() {
                break;
            }
        }
        nodes.reverse();
        let turns: HashMap<i64, i64> = nodes.iter().copied().collect();
        let workspace = (self.state.lock().await.bound.as_ref())
            .map(|b| b.workspace.clone())
            .unwrap_or_default();
        let mut history = History {
            out,
            session: &self.id,
            bot: &bot,
            workspace: &workspace,
            calls: HashMap::new(),
            parts: HashMap::new(),
        };
        let mut next = 0;
        while next < nodes.len() {
            let chunk: Vec<i64> = (nodes[next..(next + 100).min(nodes.len())].iter())
                .map(|(node, _)| *node)
                .collect();
            let Ok(read) = client
                .request("history_items", json!({"bot": bot, "nodes": chunk}))
                .await
            else {
                return;
            };
            // Fewer than asked when they are large.
            let items = read["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                return;
            }
            next += items.len();
            for entry in &items {
                let node = entry["node"].as_i64();
                let Some(turn) = node.and_then(|node| turns.get(&node)) else {
                    continue;
                };
                // The message it holds by its handle, the bot it was sent
                // to and its number: a fork's history keeps its source's.
                let handle = entry["handle"].as_str();
                for item in crate::responses(&entry["item"]) {
                    history.item(*turn, &item, handle);
                }
            }
        }
    }
}

/// A task's session bound to its task, behind a box: a lead's session
/// that binds adopts its tasks, whose sessions bind.
fn bind_task(
    session: Arc<Session>,
    agent: Arc<Agent>,
    task: t3::Pinned,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> {
    Box::pin(async move { session.bind(&agent, Some(task), &project::Pick::Last).await })
}

/// A bot's history as it is replayed: its calls not ended yet, and the
/// part of each turn its words are in.
struct History<'a> {
    out: &'a Out,
    session: &'a str,
    bot: &'a str,
    workspace: &'a str,
    calls: HashMap<String, (i64, String, Value)>,
    parts: HashMap<i64, u32>,
}

impl History<'_> {
    fn item(&mut self, turn: i64, item: &Value, message: Option<&str>) {
        let part = *self.parts.get(&turn).unwrap_or(&0);
        match item["type"].as_str() {
            Some("function_call") => {
                self.parts.insert(turn, part + 1);
                let (Some(call), Some(name)) = (item["call_id"].as_str(), item["name"].as_str())
                else {
                    return;
                };
                let arguments = (item["arguments"].as_str())
                    .and_then(|arguments| serde_json::from_str(arguments).ok())
                    .unwrap_or(Value::Null);
                if name == reactions::TOOL {
                    return;
                }
                if name == plan::TOOL {
                    self.out
                        .replayed(self.session, plan_update(self.bot, &arguments));
                    return;
                }
                let shown = shown_id(Some(turn), call);
                self.out.replayed(
                    self.session,
                    tool_call(&shown, name, &arguments, "in_progress"),
                );
                self.calls
                    .insert(call.to_owned(), (turn, name.to_owned(), arguments));
            }
            Some("function_call_output") => {
                let Some(call) = item["call_id"].as_str() else {
                    return;
                };
                let Some((turn, name, arguments)) = self.calls.remove(call) else {
                    return;
                };
                let (shown, failed) = crate::output(item);
                let shown_call = shown_id(Some(turn), call);
                let ran = Ran {
                    call: &shown_call,
                    name: &name,
                    arguments: &arguments,
                    shown: &shown,
                    result: Some(item),
                    failed,
                    workspace: self.workspace,
                };
                // The file as it is now is not the one the call left: an
                // edit replays as what it said.
                let content = ended_content(self.out, self.session, &ran, false);
                let status = if failed { "failed" } else { "completed" };
                self.out.replayed(
                    self.session,
                    json!({"sessionUpdate": "tool_call_update", "toolCallId": shown_call,
                    "status": status, "content": content}),
                );
            }
            _ if item["role"] == "user" => {
                let text = crate::text(item);
                if text.is_empty() || reactions::is_reaction(&text) {
                    return;
                }
                let Some(message) = message else { return };
                self.out.replayed(
                    self.session,
                    json!({"sessionUpdate": "user_message",
                    "messageId": message,
                    "content": [{"type": "text", "text": text}]}),
                );
            }
            _ if item["role"] == "assistant" => {
                let text = crate::text(item);
                if text.is_empty() {
                    return;
                }
                self.out.replayed(
                    self.session,
                    json!({"sessionUpdate": "agent_message",
                    "messageId": message_id(self.bot, Some(turn), part),
                    "content": [{"type": "text", "text": text}]}),
                );
            }
            _ => {}
        }
    }
}

/// The questions `bot` has open with options to pick from, the oldest
/// first, as they were asked. Read back from its whole history, newest
/// first: a question stays open until a pick or the agent's close, however
/// much the agent did since.
async fn open_asks(client: &Client, bot: &str) -> Vec<asks::Ask> {
    let (mut asked, mut open) = (asks::Asked::default(), Vec::new());
    let _ = crate::said::walk(client, bot, 0, |at, entry| {
        open.extend(
            (asked.entry(at, entry).into_iter())
                .filter(|(ask, state)| *state == asks::State::Open && !ask.options.is_empty())
                .map(|(ask, _)| ask),
        );
        false
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
        message = format!("{message}\n\nRecommended: {}.", choice.label);
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
    let _ = send_pick(&client, (&bot, bot_id), ask, picked, person).await;
}

/// The pick `picked` of `person` (the person when none) on `bot`'s
/// question `ask`, sent as their answer: whether it went. A form may wait
/// in the client for days, and the person may have answered the question
/// since, in a window or another client: read as it goes, nothing goes to
/// one answered already ([`crate::repick`]). Each goes once, under one
/// request id however often a client sends it: the person's the one every
/// window sends theirs under, anyone else's their own, so another's pick
/// never takes the person's place.
async fn send_pick(
    client: &Client,
    (bot, bot_id): (&str, Option<i64>),
    ask: &asks::Ask,
    picked: &str,
    person: Option<&str>,
) -> Result<bool, String> {
    let ask = match crate::repick::check(client, bot, ask).await {
        crate::repick::Verdict::Send(ask) => *ask,
        _ => return Ok(false),
    };
    let mut submitted = json!({"bot": bot, "bot_id": bot_id, "delivery": "steer",
        "prompt": asks::answer(&ask, picked)});
    match person {
        Some(person) => {
            submitted["person"] = json!(person);
            let key = format!("{} {person}", ask.id);
            (crate::submit_once(client, &key, submitted).await).map_err(|e| e.to_string())?;
        }
        None => {
            crate::answer_once(client, &ask.id, submitted).await?;
        }
    }
    Ok(true)
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

/// Put one call to the person, and answer it with their choice. While
/// they choose, the work it holds up waits on them (`requires_action`);
/// one a message lifted is background work, which waits on no one.
async fn ask(agent: &Arc<Agent>, session: &Arc<Session>, client: &Client, bot: &str, call: &Value) {
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
    let turn = call["turn"].as_i64();
    let shown = shown_id(turn, id);
    let mut update = tool_call(
        &shown,
        name,
        arguments.as_ref().unwrap_or(&Value::Null),
        "pending",
    );
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
    let holds = (!call["handle"].is_string()).then_some(turn).flatten();
    let (request, answer) = agent.out.reserve();
    {
        let mut state = session.state.lock().await;
        let call = (turn, id.to_owned());
        state.asking.insert(request, Waiting { holds, call });
        session.report(agent, &mut state);
    }
    agent.out.request(
        request,
        "session/request_permission",
        json!({"sessionId": session.id, "title": title,
            "subject": {"type": "tool_call", "toolCall": update}, "options": options}),
    );
    let answer = Out::answer_to(answer).await;
    {
        let mut state = session.state.lock().await;
        if state.asking.remove(&request).is_some() {
            session.report(agent, &mut state);
        }
    }
    // No choice, as when the client gave up on it or it was taken back:
    // the call waits for an answer from anywhere else.
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
    crate::start_companions(client, crate::Companions::Add).await?;
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
    // Its profile is on it before its instructions are composed from it.
    crate::upgrade::profiles_on_agents(client, &agent.options.store).await;
    let record = client
        .request("inspect", json!({"bot": lead.bot}))
        .await
        .map_err(|e| e.to_string())?;
    let workspace = moving
        .or(record["workspace"].as_str())
        .unwrap_or_default()
        .to_owned();
    let mut asked = json!({"bot": lead.bot});
    let mut left_out = Vec::new();
    if let Ok((taken, left)) = project::review(
        Path::new(&workspace),
        record["instructions"].as_str().unwrap_or_default(),
        record["profile"].as_str(),
    ) {
        left_out = left;
        if let Some(text) = taken {
            asked["instructions"] = json!(text);
        }
    }
    let mut declare = declared::declare(&record, crate::served::Role::LEAD);
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
    // Its `exec` shows the shell's fields for leaving a sandbox only while
    // its row puts it in one.
    if let Some(mode) = gating
        && let Some(held) = declared::held(
            declare.as_ref().unwrap_or(&record["declared"]),
            &json!(sandbox(mode)),
        )
    {
        declare = Some(held);
    }
    if let Some(declare) = &declare {
        asked["declare"] = declare.clone();
    }
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
            Ok(_) => {}
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
    crate::start_companions(client, crate::Companions::Add).await?;
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
        let (_, brief) = brief(client, &bot).await.ok_or("the task has no brief")?;
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

/// The id a client knows call `call` of `turn` by: a later turn may use
/// the same call id, so the turn is part of it. The daemon is asked by the
/// call's own id.
fn shown_id(turn: Option<i64>, call: &str) -> String {
    match turn {
        Some(turn) => format!("{turn}/{call}"),
        None => call.to_owned(),
    }
}

/// The update that shows a call, created or brought to `status`: what it
/// does, in a few words.
fn tool_call(call: &str, name: &str, arguments: &Value, status: &str) -> Value {
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
    let mut update = json!({"sessionUpdate": "tool_call_update", "toolCallId": call,
        "title": title.trim(), "name": name, "kind": kind, "status": status, "rawInput": arguments});
    if let Some(path) = path {
        update["locations"] = json!([{"path": path}]);
    }
    update
}

/// A `plan` call's steps, as the protocol's plan: the bot's one plan,
/// which each call replaces.
fn plan_update(bot: &str, arguments: &Value) -> Value {
    json!({"sessionUpdate": "plan_update", "plan": {"type": "items",
        "planId": format!("{bot}-plan"), "entries": plan_entries(arguments)}})
}

/// A `plan` call's steps, as the protocol's plan entries.
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

/// How a turn ended, as the protocol's stop reason, and the error it
/// carries when it failed.
fn stop_reason(data: &Value) -> (&'static str, Option<Value>) {
    match data["status"].as_str().unwrap_or("") {
        "interrupted" | "cancelled" => ("cancelled", None),
        "failed" if data["error"] == "provider_refusal" => ("refusal", None),
        "failed" => {
            let message = (data["detail"].as_str())
                .or(data["error"].as_str())
                .unwrap_or("the turn failed");
            ("error", Some(json!({"code": FAILED, "message": message})))
        }
        _ if data["limit"] == "rounds" => ("max_turn_requests", None),
        _ => ("end_turn", None),
    }
}

/// The id of a message the bot was sent, by its number, as the daemon
/// names it (`BOT/N`): a prompt's, and the same one when it is replayed.
fn user_id(bot: &str, message: Option<i64>) -> String {
    match message {
        Some(message) => format!("{bot}/{message}"),
        None => crate::unique(&format!("{bot}/sent")),
    }
}

/// A prompt's `idempotencyKey`, when it has one: 1 to 255 characters.
fn idempotency(params: &Value) -> Result<Option<&str>, String> {
    match &params["idempotencyKey"] {
        Value::Null => Ok(None),
        Value::String(key) if (1..=255).contains(&key.chars().count()) => Ok(Some(key)),
        _ => Err("idempotencyKey must be 1 to 255 characters".into()),
    }
}

/// The daemon's request id for a prompt's idempotency key: one way, so
/// the key itself is kept nowhere.
fn keyed(key: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(key.as_bytes());
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("acp-{hex}")
}

/// Whether a prompt holds only what Sonny takes: words and links, which
/// every agent takes, and the pictures and files it says it takes.
fn accepted_blocks(prompt: &Value) -> Result<(), String> {
    let blocks = prompt.as_array().filter(|blocks| !blocks.is_empty());
    let Some(blocks) = blocks else {
        return Err("prompt must be a list of one or more blocks".into());
    };
    for block in blocks {
        match block["type"].as_str() {
            Some("text" | "resource_link" | "image" | "resource") => {}
            other => {
                return Err(format!(
                    "Sonny takes no {} block",
                    other.unwrap_or("untyped")
                ));
            }
        }
    }
    Ok(())
}

/// A call that ended, as its final update shows it.
struct Ran<'a> {
    /// The id the client knows it by.
    call: &'a str,
    name: &'a str,
    arguments: &'a Value,
    /// What it said, as the person reads it.
    shown: &'a str,
    /// Its result as the bot's history keeps it.
    result: Option<&'a Value>,
    failed: bool,
    /// The bot's folder, which its paths are in.
    workspace: &'a str,
}

/// What an ended call shows: a command that exited as a terminal, with
/// its output and exit status; an edit as the diff it made, read from the
/// file as it is now when `now` says the call is the last to touch it;
/// anything else as what it said.
fn ended_content(out: &Out, session: &str, ran: &Ran<'_>, now: bool) -> Vec<Value> {
    if ran.name == "shell"
        && let Some(exit) = ran.result.and_then(exit_status)
    {
        let command = ran.arguments["command"].as_str();
        let cwd = Path::new(ran.workspace).join(ran.arguments["workdir"].as_str().unwrap_or(""));
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD.encode(cut(ran.shown));
        let mut terminal = json!({"sessionUpdate": "terminal_update", "terminalId": ran.call,
            "command": command, "output": {"data": data}, "exitStatus": exit});
        if cwd.is_absolute() {
            terminal["cwd"] = json!(cwd);
        }
        // A replayed one goes with the replay, ahead of what is held.
        match now {
            true => out.update(session, terminal),
            false => out.replayed(session, terminal),
        }
        return vec![json!({"type": "terminal", "terminalId": ran.call})];
    }
    if !ran.failed
        && let Some(diff) = edit_diff(ran.name, ran.arguments, ran.workspace, now)
    {
        return vec![diff];
    }
    vec![text_content(&cut(ran.shown))]
}

/// A command's exit status from its result, when it ran to its end: one
/// still running, or left running in the background, has none yet.
fn exit_status(result: &Value) -> Option<Value> {
    let raw = match &result["output"] {
        Value::String(raw) => raw.clone(),
        _ => return None,
    };
    let ended: Value = serde_json::from_str(&raw).ok()?;
    if ended["running"] == true {
        return None;
    }
    let code = ended.get("exit_code")?;
    Some(json!({"exitCode": code}))
}

/// The diff an edit made, as the protocol's diff: the files it changed,
/// and as `git diff` would show them, every path whole. `now`: the file
/// as it is now is the one the edit left, so the lines it changed are
/// placed in it; else only which files it changed.
fn edit_diff(name: &str, arguments: &Value, workspace: &str, now: bool) -> Option<Value> {
    let whole = |path: &str| Path::new(workspace).join(path).display().to_string();
    match name {
        "edit" => {
            let path = whole(arguments["path"].as_str()?);
            let mut diff = json!({"type": "diff",
                "changes": [{"operation": "modify", "path": path, "fileType": "text"}]});
            if now {
                let edit = crate::transcript::edit_lines(arguments, workspace);
                if let Some(start) = edit.start {
                    let (old, new): (Vec<&str>, Vec<&str>) =
                        (edit.old.lines().collect(), edit.new.lines().collect());
                    let quoted = quoted_path(&path);
                    let mut text = format!(
                        "diff --git {quoted} {quoted}\n--- {quoted}\n+++ {quoted}\n@@ -{} +{} @@\n",
                        span(start, old.len()),
                        span(start, new.len())
                    );
                    for line in old {
                        text.push_str(&format!("-{line}\n"));
                    }
                    for line in new {
                        text.push_str(&format!("+{line}\n"));
                    }
                    diff["patch"] = json!({"format": "git_patch", "text": text});
                }
            }
            Some(diff)
        }
        "apply_patch" => {
            let patch = arguments["input"].as_str()?;
            let files = crate::transcript::patch_sections(patch, workspace);
            if files.is_empty() {
                return None;
            }
            let mut changes = Vec::new();
            let mut text = String::new();
            for file in &files {
                let path = whole(&file.path);
                let old = file.moved_from.as_deref().map(whole);
                let mut change = json!({"operation": file.operation(), "path": path});
                if let Some(old) = &old {
                    change["oldPath"] = json!(old);
                }
                changes.push(change);
                let (before, after) = (old.as_deref().unwrap_or(&path), path.as_str());
                let (before, after) = (quoted_path(before), quoted_path(after));
                text.push_str(&format!("diff --git {before} {after}\n"));
                match file.operation() {
                    "add" => text.push_str(&format!(
                        "new file mode 100644\n--- /dev/null\n+++ {after}\n"
                    )),
                    "delete" => text.push_str(&format!(
                        "deleted file mode 100644\n--- {before}\n+++ /dev/null\n"
                    )),
                    "move" => text.push_str(&format!(
                        "rename from {before}\nrename to {after}\n--- {before}\n+++ {after}\n"
                    )),
                    _ => text.push_str(&format!("--- {before}\n+++ {after}\n")),
                }
                text.push_str(&file.hunks);
            }
            let mut diff = json!({"type": "diff", "changes": changes});
            // Its chunks carry no line numbers: placed in the files as they
            // are now, which only the last call to touch them left.
            if now {
                diff["patch"] = json!({"format": "git_patch", "text": text});
            }
            Some(diff)
        }
        _ => None,
    }
}

/// A hunk's side as a unified diff numbers it: `START,COUNT`, an empty
/// side placed after the line before it.
fn span(start: usize, count: usize) -> String {
    match count {
        0 => format!("{},0", start.saturating_sub(1)),
        _ => format!("{start},{count}"),
    }
}

/// A path as Git writes one in a patch: quoted, with C escapes, when it
/// has a quote, a backslash, a control character or a byte past ASCII.
fn quoted_path(path: &str) -> String {
    let plain = path
        .bytes()
        .all(|b| (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\');
    if plain {
        return path.to_owned();
    }
    let mut quoted = String::from("\"");
    for b in path.bytes() {
        match b {
            b'"' => quoted.push_str("\\\""),
            b'\\' => quoted.push_str("\\\\"),
            b'\t' => quoted.push_str("\\t"),
            b'\n' => quoted.push_str("\\n"),
            0x20..0x7f => quoted.push(b as char),
            _ => quoted.push_str(&format!("\\{b:03o}")),
        }
    }
    quoted.push('"');
    quoted
}

/// A task's brief, the first message it was sent, by its number: whether
/// or not a turn has taken it in yet, as a task may wait for room.
async fn brief(client: &Client, bot: &str) -> Option<(i64, String)> {
    let messages = client
        .request("messages", json!({"bot": bot, "after": 0, "limit": 1}))
        .await
        .ok()?;
    let first = messages["messages"][0]["message"].as_i64()?;
    let prompts = client
        .request("prompts", json!({"bot": bot, "turn": first}))
        .await
        .ok()?;
    Some((first, prompts["prompts"][0]["text"].as_str()?.to_owned()))
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

/// The calls an `approval_requested` event puts to their gates, by turn
/// and call, with the node that planned each. A script's steps are not
/// calls a turn planned: they end with `step_decided`, never lifted.
fn planned(event: &Value) -> Vec<((Option<i64>, String), i64)> {
    let turn = event["turn"].as_i64();
    (event["data"]["calls"].as_array().into_iter().flatten())
        .filter(|call| call["step_of"].is_null())
        .filter_map(|call| {
            let id = call["call_id"].as_str()?.to_owned();
            Some(((turn, id), call["node"].as_i64()?))
        })
        .collect()
}

/// Call `id`'s name and arguments, from `node`, which planned it.
async fn planned_call(client: &Client, bot: &str, node: i64, id: &str) -> Option<(String, Value)> {
    let call = items(client, bot, &[node])
        .await
        .into_iter()
        .find(|item| item["type"] == "function_call" && item["call_id"] == id)?;
    let arguments = (call["arguments"].as_str())
        .and_then(|arguments| serde_json::from_str(arguments).ok())
        .unwrap_or(Value::Null);
    Some((call["name"].as_str()?.to_owned(), arguments))
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

    /// A pick from a client goes once, and never to a question the person
    /// answered already: read as it goes, one answered sends nothing; one
    /// open goes under the id every window sends the person's answer
    /// under, so a second from any surface is refused; someone the client
    /// names goes under one of their own, never taking the person's place.
    #[tokio::test]
    async fn a_pick_goes_once_and_never_to_a_question_answered_already() {
        let ask = asks::call(
            "ask:4/c1",
            &json!({"question": "Ship it?", "why": "w",
            "options": [{"label": "Now"}, {"label": "Later"}]}),
        )
        .unwrap();
        for answered in [true, false] {
            let submitted = Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = submitted.clone();
            let history = crate::tests::asked_history(answered, false);
            let client = crate::tests::daemon_answering(move |request| {
                if let Some(read) = crate::tests::history_read(&history, request) {
                    return read;
                }
                let mut seen = seen.lock().unwrap();
                let again =
                    (seen.iter()).any(|was: &Value| was["request_id"] == request["request_id"]);
                seen.push(request.clone());
                match again {
                    true => json!({"error": "idempotency_conflict", "message": "taken"}),
                    false => json!({"message": 9, "turn": 9}),
                }
            })
            .await;
            let went = send_pick(&client, ("shop.lead", Some(1)), &ask, "2", None).await;
            if answered {
                assert_eq!(went, Ok(false));
                assert!(submitted.lock().unwrap().is_empty());
                continue;
            }
            assert_eq!(went, Ok(true));
            let again = send_pick(&client, ("shop.lead", Some(1)), &ask, "1", None).await;
            assert_eq!(again, Err("that question was answered already".to_owned()));
            let named = send_pick(&client, ("shop.lead", Some(1)), &ask, "1", Some("ana")).await;
            assert_eq!(named, Ok(true));
            let twice = send_pick(&client, ("shop.lead", Some(1)), &ask, "1", Some("ana")).await;
            assert!(twice.is_err());
            let ids: Vec<Value> = (submitted.lock().unwrap().iter())
                .map(|request| request["request_id"].clone())
                .collect();
            let person = json!(asks::request_id("ask:4/c1", 0));
            assert_eq!(ids[..2], [person.clone(), person]);
            assert_ne!(ids[2], ids[0]);
            assert_eq!(ids[3], ids[2]);
        }
    }

    /// A named person's pick withdrawn from the queue before the agent
    /// read it delivered nothing: their next goes as the next attempt,
    /// as the person's own does.
    #[tokio::test]
    async fn a_named_pick_taken_back_unread_goes_again() {
        let ask = asks::call(
            "ask:4/c1",
            &json!({"question": "Ship it?", "why": "w",
            "options": [{"label": "Now"}, {"label": "Later"}]}),
        )
        .unwrap();
        let submitted = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let seen = submitted.clone();
        let history = crate::tests::asked_history(false, false);
        let client = crate::tests::daemon_answering(move |request| {
            if let Some(read) = crate::tests::history_read(&history, request) {
                return read;
            }
            match request["op"].as_str().unwrap_or("") {
                "messages" => json!({"messages": [{"message": 9, "status": "withdrawn",
                    "turn": null}]}),
                _ => {
                    let mut seen = seen.lock().unwrap();
                    let again = (seen.iter()).any(|was| was["request_id"] == request["request_id"]);
                    seen.push(request.clone());
                    match again {
                        true => json!({"duplicate": true, "message": 9}),
                        false => json!({"message": 9}),
                    }
                }
            }
        })
        .await;
        for _ in 0..2 {
            let went = send_pick(&client, ("shop.lead", Some(1)), &ask, "1", Some("ana")).await;
            assert_eq!(went, Ok(true));
        }
        let ids: Vec<Value> = (submitted.lock().unwrap().iter())
            .map(|request| request["request_id"].clone())
            .collect();
        let key = "ask:4/c1 ana";
        assert_eq!(
            ids,
            [
                json!(asks::request_id(key, 0)),
                json!(asks::request_id(key, 0)),
                json!(asks::request_id(key, 1)),
            ]
        );
    }

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
        // The bot's one plan, which each call replaces.
        let update = plan_update("calc.lead", &json!({"steps": ["[x] Read"]}));
        assert_eq!(update["sessionUpdate"], "plan_update");
        assert_eq!(
            (&update["plan"]["type"], &update["plan"]["planId"]),
            (&json!("items"), &json!("calc.lead-plan"))
        );
    }

    #[test]
    fn a_call_says_what_it_does() {
        let shell = tool_call(
            "c1",
            "shell",
            &json!({"command": "cargo test\necho done"}),
            "pending",
        );
        assert_eq!(
            (shell["kind"].as_str(), shell["title"].as_str()),
            (Some("execute"), Some("cargo test"))
        );
        // Version 2 has no `tool_call`: the first update makes the call.
        assert_eq!(
            (&shell["sessionUpdate"], &shell["status"]),
            (&json!("tool_call_update"), &json!("pending"))
        );
        let edit = tool_call("c2", "edit", &json!({"path": "/src/a.rs"}), "in_progress");
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
            (&picker["configId"], &picker["currentValue"]),
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
                "recommended": 2, "one_way": true}),
        )
        .unwrap();
        let form = form("s1", &ask, Some("c1"));
        assert_eq!(
            form["message"],
            "Which database?\n\nIt decides the schema.\n\nRecommended: Postgres."
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
                "one_way": true}),
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
    fn only_tasks_that_ran_lately_are_adopted() {
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

    /// An agent for tests, its daemon nowhere: what it writes is kept.
    fn test_agent() -> Arc<Agent> {
        let dir = std::env::temp_dir().join(crate::unique("acp-test"));
        Agent::new(dir.join("store"), dir.join("socket"), None)
    }

    fn written(agent: &Agent) -> Vec<Value> {
        std::mem::take(&mut *agent.out.written.lock().unwrap())
    }

    /// The updates among what was written, each with its session.
    fn updates(written: &[Value]) -> Vec<(String, Value)> {
        (written.iter())
            .filter(|m| m["method"] == "session/update")
            .map(|m| {
                let session = m["params"]["sessionId"].as_str().unwrap_or("").to_owned();
                (session, m["params"]["update"].clone())
            })
            .collect()
    }

    /// A session of `bot`, on `client`, in the agent's list.
    fn bound_session(
        agent: &Agent,
        id: &str,
        client: Arc<Client>,
        bot: &str,
        parent: Option<(&str, &str)>,
    ) -> Arc<Session> {
        let session = Arc::new(Session {
            id: id.to_owned(),
            cwd: PathBuf::from("/w"),
            parent: parent.map(|(s, b)| (s.to_owned(), b.to_owned())),
            state: AsyncMutex::new(State {
                bound: Some(Bound {
                    bot: bot.to_owned(),
                    bot_id: Some(1),
                    client,
                    project: None,
                    workspace: "/w".into(),
                    moving: None,
                    model: "openai/gpt-6".into(),
                    effort: None,
                }),
                ..State::default()
            }),
            follower: Mutex::new(None),
            switching: AsyncMutex::new(()),
            children: Mutex::new(HashMap::new()),
        });
        (agent.sessions.lock().unwrap()).insert(id.to_owned(), session.clone());
        session
    }

    fn event(kind: &str, bot: &str, turn: i64, data: Value) -> Value {
        json!({"event": kind, "bot": bot, "turn": turn, "data": data})
    }

    #[tokio::test]
    async fn the_handshake_is_version_2_or_none() {
        let agent = test_agent();
        let request = |method: &str, params: Value| json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        // Nothing before `initialize`.
        agent
            .clone()
            .request(request("session/list", json!({})))
            .await;
        assert_eq!(written(&agent)[0]["error"]["code"], INVALID_REQUEST);
        // An older version is refused with the versions Sonny speaks.
        agent
            .clone()
            .request(request("initialize", json!({"protocolVersion": 1})))
            .await;
        let refused = &written(&agent)[0]["error"];
        assert_eq!(refused["code"], UNSUPPORTED_VERSION);
        assert_eq!(refused["data"], json!({"requested": 1, "supported": [2]}));
        for bad in [json!("2"), json!(70000), Value::Null] {
            agent
                .clone()
                .request(request("initialize", json!({"protocolVersion": bad})))
                .await;
            assert_eq!(written(&agent)[0]["error"]["code"], INVALID_PARAMS);
        }
        // A newer one gets version 2, in its shape.
        let asked = json!({"protocolVersion": 3, "info": {"name": "c", "version": "1"},
            "capabilities": {"elicitation": {"form": {}}}});
        agent.clone().request(request("initialize", asked)).await;
        let result = written(&agent)[0]["result"].clone();
        assert_eq!(result["protocolVersion"], 2);
        assert_eq!(result["info"]["name"], "sonny");
        assert_eq!(
            result["capabilities"],
            json!({"session": {"prompt": {"image": {}, "embeddedContext": {}}}})
        );
        assert!(agent.forms.load(Ordering::Relaxed));
        // A session not open is the protocol's not-found, naming it; one
        // closed already closes again; an unknown method is not found.
        let prompt = json!({"sessionId": "s9", "prompt": [{"type": "text", "text": "hi"}]});
        agent
            .clone()
            .request(request("session/prompt", prompt))
            .await;
        let missing = &written(&agent)[0]["error"];
        assert_eq!(
            (&missing["code"], &missing["data"]),
            (&json!(SESSION_NOT_FOUND), &json!({"sessionId": "s9"}))
        );
        agent
            .clone()
            .request(request("session/close", json!({"sessionId": "s9"})))
            .await;
        assert_eq!(written(&agent)[0]["result"], json!({}));
        agent
            .clone()
            .request(request("session/load", json!({})))
            .await;
        assert_eq!(written(&agent)[0]["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn a_tasks_session_takes_only_a_cancel() {
        let agent = test_agent();
        agent.initialized.store(true, Ordering::Relaxed);
        let client = crate::tests::daemon_answering(|_| json!({})).await;
        bound_session(
            &agent,
            "calc.fix#9",
            client,
            "calc.fix",
            Some(("s1", "calc.lead")),
        );
        for method in [
            "session/prompt",
            "session/set_config_option",
            "session/close",
        ] {
            let asked = json!({"jsonrpc": "2.0", "id": 1, "method": method,
                "params": {"sessionId": "calc.fix#9", "prompt": [{"type": "text", "text": "x"}]}});
            agent.clone().request(asked).await;
            assert_eq!(
                written(&agent)[0]["error"]["code"],
                INVALID_PARAMS,
                "{method}"
            );
        }
    }

    #[tokio::test]
    async fn a_turn_reports_its_work_as_the_sessions_state() {
        let agent = test_agent();
        let shell = json!({"stdout": "a\n", "stderr": "", "exit_code": 0, "success": true});
        let history = vec![
            (
                6,
                7,
                json!({"type": "function_call_output", "call_id": "c1",
                "output": shell.to_string()}),
            ),
            (
                5,
                7,
                json!({"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "calc.fix finished"}]}),
            ),
        ];
        let client = crate::tests::daemon_answering(move |asked| {
            crate::tests::history_read(&history, asked).unwrap_or(json!({}))
        })
        .await;
        let session = bound_session(&agent, "s1", client, "calc.lead", None);
        let bot = "calc.lead";
        // A turn the bot took on its own, as the lookout woke it.
        session
            .on(&agent, &event("accepted", bot, 7, json!({"node": 5})))
            .await;
        let arguments = json!({"command": "ls"}).to_string();
        let started = json!({"call_id": "c1", "name": "shell", "arguments": arguments});
        session
            .on(&agent, &event("tool_started", bot, 7, started))
            .await;
        let done = json!({"call_id": "c1", "node": 6});
        session
            .on(&agent, &event("tool_completed", bot, 7, done))
            .await;
        let ended = json!({"status": "completed"});
        session
            .on(&agent, &event("turn_finished", bot, 7, ended))
            .await;
        let updates: Vec<Value> = updates(&written(&agent))
            .into_iter()
            .map(|(_, u)| u)
            .collect();
        assert_eq!(
            updates[0],
            json!({"sessionUpdate": "state_update", "state": "running", "background": []})
        );
        assert_eq!(
            updates[1],
            json!({"sessionUpdate": "user_message", "messageId": "calc.lead/7",
                "content": [{"type": "text", "text": "calc.fix finished"}]})
        );
        assert_eq!(
            (
                &updates[2]["sessionUpdate"],
                &updates[2]["toolCallId"],
                &updates[2]["status"]
            ),
            (
                &json!("tool_call_update"),
                &json!("7/c1"),
                &json!("in_progress")
            )
        );
        // A command that exited is a terminal: its output and exit status.
        assert_eq!(
            updates[3],
            json!({"sessionUpdate": "terminal_update", "terminalId": "7/c1", "command": "ls",
                "cwd": "/w/", "output": {"data": "YQo="}, "exitStatus": {"exitCode": 0}})
        );
        assert_eq!(
            updates[4],
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "7/c1",
                "status": "completed", "content": [{"type": "terminal", "terminalId": "7/c1"}]})
        );
        assert_eq!(
            updates[5],
            json!({"sessionUpdate": "state_update", "state": "idle", "background": [],
                "stopReason": "end_turn"})
        );
        assert_eq!(updates.len(), 6);
    }

    #[tokio::test]
    async fn a_message_the_session_sent_is_not_shown_again() {
        let agent = test_agent();
        let client = crate::tests::daemon_answering(|_| json!({})).await;
        let session = bound_session(&agent, "s1", client, "calc.lead", None);
        session.state.lock().await.sent.insert(7);
        session
            .on(
                &agent,
                &event("accepted", "calc.lead", 7, json!({"node": 5})),
            )
            .await;
        let first = updates(&written(&agent));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].1["state"], "running");
        // A turn that failed ends with its error; a stopped one cancelled,
        // its calls still shown running with it.
        session.state.lock().await.calls.insert(
            (Some(7), "c2".into()),
            ("read".into(), json!({"path": "a"})),
        );
        let failed = json!({"status": "failed", "error": "provider_error", "detail": "boom"});
        session
            .on(&agent, &event("turn_finished", "calc.lead", 7, failed))
            .await;
        let updates = updates(&written(&agent));
        assert_eq!(
            updates[0].1,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "7/c2", "status": "failed"})
        );
        assert_eq!(updates[1].1["stopReason"], "error");
        assert_eq!(
            updates[1].1["error"],
            json!({"code": FAILED, "message": "boom"})
        );
    }

    #[tokio::test]
    async fn a_call_a_message_lifted_is_still_asked_after_its_turn() {
        let agent = test_agent();
        let client = crate::tests::daemon_answering(|_| json!({})).await;
        let session = bound_session(&agent, "s1", client, "calc.lead", None);
        {
            let mut state = session.state.lock().await;
            state.running = Some(7);
            state.gated.insert((Some(7), "c1".into()), 4);
            state.asking.insert(
                1,
                Waiting {
                    holds: Some(7),
                    call: (Some(7), "c1".into()),
                },
            );
        }
        let lifted = json!({"call_id": "c1"});
        session
            .on(&agent, &event("call_lifted", "calc.lead", 7, lifted))
            .await;
        session
            .on(
                &agent,
                &event(
                    "turn_finished",
                    "calc.lead",
                    7,
                    json!({"status": "completed"}),
                ),
            )
            .await;
        let state = session.state.lock().await;
        assert_eq!(state.asking[&1].holds, None);
        let said = written(&agent);
        assert!(!said.iter().any(|m| m["method"] == "$/cancel_request"));
        let [.., (_, end)] = &updates(&said)[..] else {
            panic!("no update")
        };
        assert_eq!(end["stopReason"], "end_turn");
        assert_eq!(
            end["background"],
            json!([{"type": "tool_call", "toolCallId": "7/c1"}])
        );
    }

    #[tokio::test]
    async fn a_call_denied_at_its_gate_ends_failed() {
        let agent = test_agent();
        let client = crate::tests::daemon_answering(|asked| {
            let item = match asked["nodes"][0].as_i64() {
                Some(4) => json!({"type": "function_call", "call_id": "c1", "name": "shell",
                    "arguments": "{\"command\":\"rm -rf build\"}"}),
                _ => json!({"type": "function_call_output", "call_id": "c1",
                    "output": "{\"error\":\"denied\"}"}),
            };
            json!({"items": [{"node": asked["nodes"][0], "item": item}]})
        })
        .await;
        let session = bound_session(&agent, "s1", client, "calc.lead", None);
        {
            let mut state = session.state.lock().await;
            state.running = Some(7);
            state.gated.insert((Some(7), "c1".into()), 4);
        }
        let denied = json!({"call_id": "c1", "node": 5, "denied": true});
        session
            .on(&agent, &event("tool_completed", "calc.lead", 7, denied))
            .await;
        let ended = (updates(&written(&agent)).into_iter())
            .map(|(_, u)| u)
            .find(|u| u["toolCallId"] == "7/c1")
            .expect("the call ends");
        assert_eq!(ended["status"], "failed");
        assert_eq!(ended["title"], "rm -rf build");
    }

    #[test]
    fn a_held_lead_holds_its_tasks_sessions_in_the_order_sent() {
        let agent = test_agent();
        agent.out.hold("s1");
        agent
            .out
            .update("s1", json!({"sessionUpdate": "subagent_update"}));
        agent
            .out
            .update("s1/t#1", json!({"sessionUpdate": "session_message"}));
        agent
            .out
            .update("s10", json!({"sessionUpdate": "agent_message"}));
        let order = |written: Vec<Value>| -> Vec<String> {
            (updates(&written).into_iter())
                .map(|(session, _)| session)
                .collect()
        };
        // Another session whose id only begins the same goes on.
        assert_eq!(order(written(&agent)), ["s10"]);
        agent.out.release("s1");
        assert_eq!(order(written(&agent)), ["s1", "s1/t#1"]);
    }

    #[test]
    fn a_message_is_kept_only_until_its_other_sighting() {
        let mut state = State::default();
        // Sent and shown here, then taken in by a turn: shown once, and
        // nothing is kept after.
        state.sent.insert(5);
        assert!(!state.unseen(5));
        assert!(state.sent.is_empty() && state.early.is_empty());
        // Taken in while a prompt is on its way: kept for that prompt's
        // answer to find.
        state.sending = 1;
        assert!(state.unseen(6));
        assert!(state.early.contains(&6));
        // From elsewhere with nothing on its way: shown, nothing kept.
        state.sending = 0;
        state.early.clear();
        assert!(state.unseen(7));
        assert!(state.early.is_empty());
    }

    #[tokio::test]
    async fn a_call_allowed_elsewhere_is_no_longer_asked_here() {
        let agent = test_agent();
        let client = crate::tests::daemon_answering(|_| json!({})).await;
        let session = bound_session(&agent, "s1", client, "calc.lead", None);
        {
            let mut state = session.state.lock().await;
            state.running = Some(7);
            state.asking.insert(
                1,
                Waiting {
                    holds: Some(7),
                    call: (Some(7), "c2".into()),
                },
            );
        }
        let started = json!({"call_id": "c2", "name": "read", "arguments": "{\"path\":\"a\"}"});
        session
            .on(&agent, &event("tool_started", "calc.lead", 7, started))
            .await;
        assert!(session.state.lock().await.asking.is_empty());
        let said = written(&agent);
        assert!(said.iter().any(|m| m["method"] == "$/cancel_request"));
        let states: Vec<Value> = (updates(&said).into_iter())
            .filter(|(_, u)| u["sessionUpdate"] == "state_update")
            .map(|(_, u)| u["state"].clone())
            .collect();
        assert_eq!(states, [json!("running")]);
    }

    #[tokio::test]
    async fn a_tasks_work_shows_on_its_leads_session_too() {
        let agent = test_agent();
        let history = vec![(
            5,
            3,
            json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "Fix the cart"}]}),
        )];
        let client = crate::tests::daemon_answering(move |asked| {
            crate::tests::history_read(&history, asked).unwrap_or(json!({}))
        })
        .await;
        let lead = bound_session(&agent, "s1", client.clone(), "calc.lead", None);
        (lead.children.lock().unwrap()).insert(
            "calc.fix#9".into(),
            json!({"sessionId": "calc.fix#9", "title": "calc.fix"}),
        );
        let task = bound_session(
            &agent,
            "calc.fix#9",
            client,
            "calc.fix",
            Some(("s1", "calc.lead")),
        );
        let accepted = json!({"node": 5, "from": {"bot": "calc.lead", "turn": 2}});
        task.on(&agent, &event("accepted", "calc.fix", 3, accepted))
            .await;
        // The lead's session's state follows, with the task as its
        // background work.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let updates = updates(&written(&agent));
        let of = |session: &str| -> Vec<Value> {
            (updates.iter())
                .filter(|(s, _)| s == session)
                .map(|(_, u)| u.clone())
                .collect()
        };
        let task = of("calc.fix#9");
        assert_eq!(task[0]["state"], "running");
        // The lead's message to it is the lead's, not the person's.
        assert_eq!(
            task[1],
            json!({"sessionUpdate": "session_message", "messageId": "calc.fix/3",
                "senderSessionId": "s1", "content": [{"type": "text", "text": "Fix the cart"}]})
        );
        assert_eq!(
            of("s1"),
            [
                json!({"sessionUpdate": "subagent_update", "sessionId": "calc.fix#9",
                    "state": {"state": "running", "background": []}}),
                json!({"sessionUpdate": "state_update", "state": "idle",
                    "background": [{"type": "subagent", "sessionId": "calc.fix#9"}]}),
            ]
        );
    }

    #[test]
    fn a_call_waiting_on_the_person_holds_the_work_up() {
        let mut state = State {
            running: Some(7),
            ..State::default()
        };
        let none = HashMap::new();
        assert_eq!(state.snapshot(&none)["state"], "running");
        state.asking.insert(
            1,
            Waiting {
                holds: Some(7),
                call: (Some(7), "c0".into()),
            },
        );
        assert_eq!(state.snapshot(&none)["state"], "requires_action");
        // One a message lifted is background work: it holds nothing up,
        // and is listed once it is shown.
        state.asking.clear();
        state.asking.insert(
            2,
            Waiting {
                holds: None,
                call: (Some(6), "c1".into()),
            },
        );
        let lifted = (Some(6), "c1".to_owned());
        state.lifted.insert(lifted.clone());
        state.calls.insert(lifted, ("shell".into(), json!({})));
        state.running = None;
        let children = HashMap::from([
            ("t#1".to_owned(), json!({"state": {"state": "running"}})),
            ("t#2".to_owned(), json!({"state": {"state": "idle"}})),
        ]);
        assert_eq!(
            state.snapshot(&children),
            json!({"state": "idle", "background": [
                {"type": "tool_call", "toolCallId": "6/c1"},
                {"type": "subagent", "sessionId": "t#1"}]})
        );
    }

    #[tokio::test]
    async fn a_resumed_session_replays_its_history_as_it_ended() {
        let agent = test_agent();
        let said = |role: &str, text: &str| {
            let kind = if role == "user" {
                "input_text"
            } else {
                "output_text"
            };
            json!({"type": "message", "role": role, "content": [{"type": kind, "text": text}]})
        };
        // Newest first, as the daemon reads it.
        let history = vec![
            (14, 3, said("assistant", "Done")),
            (
                13,
                3,
                json!({"type": "function_call_output", "call_id": "c1", "output": "fn a"}),
            ),
            (
                12,
                3,
                json!({"type": "function_call", "call_id": "c1", "name": "read",
                "arguments": "{\"path\":\"a.rs\"}"}),
            ),
            (11, 3, said("assistant", "On it")),
            (10, 3, said("user", "Fix it")),
            // Forked from another lead: its prompt is that lead's message.
            (9, 2, said("user", "Start")),
        ];
        let client = crate::tests::daemon_answering(move |asked| {
            let mut read = crate::tests::history_read(&history, asked).unwrap_or(json!({}));
            for item in read["items"].as_array_mut().into_iter().flatten() {
                match item["node"].as_i64() {
                    Some(10) => item["handle"] = json!("calc.lead/3"),
                    Some(9) => item["handle"] = json!("calc.old/2"),
                    _ => {}
                }
            }
            read
        })
        .await;
        let session = bound_session(&agent, "s1", client, "calc.lead", None);
        agent.out.hold("s1");
        // Live work meanwhile waits for the replay.
        agent
            .out
            .update("s1", json!({"sessionUpdate": "agent_message_chunk"}));
        session.replay(&agent.out).await;
        agent
            .out
            .replayed("s1", json!({"sessionUpdate": "replay_complete"}));
        agent.out.release("s1");
        let kinds: Vec<Value> = updates(&written(&agent))
            .into_iter()
            .map(|(_, u)| {
                json!([
                    u["sessionUpdate"],
                    u.get("messageId").or(u.get("toolCallId")),
                    u.get("status")
                ])
            })
            .collect();
        assert_eq!(
            kinds,
            [
                json!(["user_message", "calc.old/2", null]),
                json!(["user_message", "calc.lead/3", null]),
                json!(["agent_message", "calc.lead-3-0", null]),
                json!(["tool_call_update", "3/c1", "in_progress"]),
                json!(["tool_call_update", "3/c1", "completed"]),
                json!(["agent_message", "calc.lead-3-1", null]),
                json!(["replay_complete", null, null]),
                json!(["agent_message_chunk", null, null]),
            ]
        );
        // Only up to where the live follow began: what came after is live.
        session.state.lock().await.upto = Some(13);
        session.replay(&agent.out).await;
        let last = updates(&written(&agent)).pop().map(|(_, u)| u);
        assert_eq!(last.map(|u| u["status"].clone()), Some(json!("completed")));
    }

    #[test]
    fn a_turn_ends_as_the_protocol_names_it() {
        let ends = |data: Value| stop_reason(&data).0;
        assert_eq!(ends(json!({"status": "completed"})), "end_turn");
        assert_eq!(
            ends(json!({"status": "completed", "limit": "rounds"})),
            "max_turn_requests"
        );
        assert_eq!(ends(json!({"status": "interrupted"})), "cancelled");
        assert_eq!(
            ends(json!({"status": "failed", "error": "provider_refusal"})),
            "refusal"
        );
        let (reason, error) = stop_reason(&json!({"status": "failed", "error": "stale_turn"}));
        assert_eq!(
            (reason, error),
            (
                "error",
                Some(json!({"code": FAILED, "message": "stale_turn"}))
            )
        );
    }

    #[test]
    fn a_prompt_is_checked_before_it_goes() {
        assert_eq!(idempotency(&json!({})), Ok(None));
        assert_eq!(idempotency(&json!({"idempotencyKey": "k"})), Ok(Some("k")));
        for refused in [json!(""), json!("k".repeat(256)), json!(7)] {
            assert!(idempotency(&json!({"idempotencyKey": refused})).is_err());
        }
        // The same key is the same request, and the key is not in it.
        let key = "4f1c-secret";
        assert_eq!(keyed(key), keyed(key));
        assert_ne!(keyed(key), keyed("other"));
        assert!(!keyed(key).contains("secret"));
        assert!(
            accepted_blocks(&json!([{"type": "text", "text": "a"},
            {"type": "resource_link", "uri": "file:///a"}]))
            .is_ok()
        );
        assert!(accepted_blocks(&json!([])).is_err());
        assert!(accepted_blocks(&json!([{"type": "audio", "data": "AA=="}])).is_err());
        assert_eq!(user_id("calc.lead", Some(12)), "calc.lead/12");
    }

    #[test]
    fn a_command_has_an_exit_status_once_it_ended() {
        let result = |output: Value| json!({"output": output.to_string()});
        assert_eq!(
            exit_status(&result(json!({"exit_code": 2, "stdout": ""}))),
            Some(json!({"exitCode": 2}))
        );
        assert_eq!(
            exit_status(&result(json!({"exit_code": null, "timed_out": true}))),
            Some(json!({"exitCode": null}))
        );
        assert_eq!(
            exit_status(&result(json!({"running": true, "output": "x"}))),
            None
        );
        assert_eq!(exit_status(&result(json!({"handle": "proc:3"}))), None);
    }

    #[test]
    fn an_edit_is_the_diff_it_made() {
        let dir = std::env::temp_dir().join(crate::unique("acp-diff"));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.to_str().unwrap();
        std::fs::write(dir.join("a.rs"), "one\ntwo changed\nthree\n").unwrap();
        let edit = json!({"path": "a.rs", "old": "two", "new": "two changed"});
        let diff = edit_diff("edit", &edit, root, true).unwrap();
        let path = format!("{root}/a.rs");
        assert_eq!(
            diff["changes"],
            json!([{"operation": "modify", "path": path, "fileType": "text"}])
        );
        assert_eq!(
            diff["patch"]["text"],
            format!(
                "diff --git {path} {path}\n--- {path}\n+++ {path}\n@@ -2,1 +2,1 @@\n-two\n+two changed\n"
            )
        );
        // Replayed, the file is not the one it left: which file, no lines.
        assert!(edit_diff("edit", &edit, root, false).unwrap()["patch"].is_null());
        std::fs::write(dir.join("b.rs"), "fn b() {}\nfn c() {}\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: b.rs\n@@\n-fn b() {old}\n+fn b() {}\n fn c() {}\n\
            *** Add File: n.rs\n+new\n*** Delete File: gone.rs\n\
            *** Update File: old.rs\n*** Move to: moved.rs\n@@\n x\n*** End Patch";
        let diff = edit_diff("apply_patch", &json!({"input": patch}), root, true).unwrap();
        assert_eq!(
            diff["changes"],
            json!([{"operation": "modify", "path": format!("{root}/b.rs")},
                {"operation": "add", "path": format!("{root}/n.rs")},
                {"operation": "delete", "path": format!("{root}/gone.rs")},
                {"operation": "move", "path": format!("{root}/moved.rs"),
                    "oldPath": format!("{root}/old.rs")}])
        );
        let text = diff["patch"]["text"].as_str().unwrap();
        assert!(text.starts_with(&format!(
            "diff --git {root}/b.rs {root}/b.rs\n--- {root}/b.rs\n+++ {root}/b.rs\n@@ -1,2 +1,2 @@\n"
        )));
        assert!(text.contains(&format!(
            "new file mode 100644\n--- /dev/null\n+++ {root}/n.rs\n"
        )));
        assert!(text.contains(&format!("--- {root}/gone.rs\n+++ /dev/null\n")));
        assert!(text.contains(&format!(
            "rename from {root}/old.rs\nrename to {root}/moved.rs\n"
        )));
        assert!(!text.contains(" a/") && !text.contains(" b/"));
        assert_eq!(quoted_path("/a b/c.rs"), "/a b/c.rs");
        assert_eq!(quoted_path("/a\"b/é"), "\"/a\\\"b/\\303\\251\"");
        let _ = std::fs::remove_dir_all(dir);
    }
}
