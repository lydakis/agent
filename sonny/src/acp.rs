//! `sonny acp`: Sonny as an agent of the Agent Client Protocol, for T3
//! Code, which runs it as a provider ("Local command"), a process for each
//! thread. A thread in a folder is that folder's lead, as `sonny` there
//! finds or makes it, set up the same way: its rules and skills, its tools,
//! its approval mode, and the lookout. Each task the lead starts opens as a
//! thread through T3 Code's tools, the lead thread's child while the
//! person's prompt runs, and that thread is the task's.
//!
//! It speaks the protocol's version 2 draft as T3 Code does: the turns a
//! bot takes on its own, as when the lookout wakes a lead because a task
//! finished, reach the thread between the person's prompts, as work the
//! agent started.
use crate::{approvals, declared, lookout, plan, project, reactions, t3};
use agent_client::{Client, Events};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
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

/// The model and effort pickers' first choice: what the thread's bot
/// runs, or what Sonny gives a new lead.
const DEFAULT: &str = "default";

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
        lead: project::Pick::Last,
    };
    let agent = Arc::new(Agent {
        out: Arc::new(Out::default()),
        options,
        t3,
        sessions: Mutex::new(HashMap::new()),
        ready: OnceCell::new(),
        offer: Mutex::new(Offer::default()),
        version: AtomicI64::new(VERSION),
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
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut waiting) = self.waiting.lock() {
            waiting.insert(id, tx);
        }
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let answer = rx.await.ok()?;
        answer.get("result").cloned()
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
        json!({
            "protocolVersion": version,
            "agentCapabilities": {
                "loadSession": false,
                "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
                "mcpCapabilities": {"http": false, "sse": false, "acp": false},
                "sessionCapabilities": {"resume": {}, "close": {}}
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
    /// as `/model` lists it, with `~/.agent/models`' notes and the models
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
    /// `~/.agent/models` says of them and the models only it names. A
    /// listing that does not come in time leaves the file's.
    async fn list(&self) {
        let file = agent_client::models::path()
            .and_then(|path| agent_client::models::read(&path).ok())
            .unwrap_or_default();
        let mut offer = Offer::default();
        if let Ok((client, _events)) = Client::connect(&self.options.socket).await {
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
                        if let Some(id) = model["id"].as_str() {
                            offer.models.push(Listed {
                                id: format!("{name}/{id}"),
                                name: model["name"].as_str().map(str::to_owned),
                                note: None,
                            });
                        }
                    }
                }
            }
            client.close().await;
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
                crate::connect(&self.options).await.map(drop)
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
        self.list().await;
        let id = match resumed {
            Some(id) => id.to_owned(),
            None => crate::unique("sonny"),
        };
        let session = Arc::new(Session {
            id: id.clone(),
            cwd,
            state: AsyncMutex::new(State::default()),
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
            Some(id) => match t3::recall(id) {
                Some(bot) => {
                    session
                        .bind(self, Some(bot), false, &project::Pick::Last)
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
    calls: HashMap<String, (String, Value)>,
    /// Calls put to the person, by call and request.
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
    /// Tasks whose threads are opening, as children of the person's prompt
    /// while it runs, and the answers of the prompts whose turns ended
    /// meanwhile, held until they are open.
    opening: usize,
    held: Vec<(Value, Value)>,
}

struct Bound {
    bot: String,
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
/// listing gives it, and `~/.agent/models`' note.
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
    let label = match id.split_once('/') {
        Some((provider, model)) => format!("{} · {provider}", name.unwrap_or(model)),
        None => name.unwrap_or(id).to_owned(),
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
    async fn bind(
        self: &Arc<Self>,
        agent: &Arc<Agent>,
        bot: Option<String>,
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
        let (bot, project, moving, made) = match bot {
            Some(bot) => {
                let project = project::lead_project(&bot).map(str::to_owned);
                services(agent, &client, &bot, project.as_deref()).await?;
                (bot, project, None, false)
            }
            None => {
                let picked = self.state.lock().await.picked.clone();
                let lead = project::open(&client, &self.cwd, picked.as_deref(), pick).await?;
                let moving = lead.moving.as_ref().map(|dir| dir.display().to_string());
                setup_lead(agent, &self.id, &client, &lead, moving.as_deref()).await?;
                (lead.bot, Some(lead.project), moving, lead.new)
            }
        };
        client
            .request("follow", json!({"bot": bot, "after": 0}))
            .await
            .map_err(|e| e.to_string())?;
        // Calls started before this thread came and not done yet: a task a
        // lead is starting still opens its thread when it is done.
        let mut started = HashMap::new();
        let cursor = live(&mut events, |event| {
            let call = event["data"]["call_id"].as_str().unwrap_or("").to_owned();
            match event["event"].as_str() {
                Some("tool_started") => {
                    started.insert(call, (event["turn"].as_i64(), event["data"].clone()));
                }
                Some("tool_completed") => {
                    started.remove(&call);
                }
                _ => {}
            }
            async {}
        })
        .await;
        let record = client
            .request("inspect", json!({"bot": bot}))
            .await
            .map_err(|e| e.to_string())?;
        let workspace = record["workspace"].as_str().unwrap_or_default().to_owned();
        let running = record["running_turn"].as_i64();
        let model = format!(
            "{}/{}",
            record["provider"].as_str().unwrap_or(""),
            record["model"].as_str().unwrap_or("")
        );
        let effort = record["effort"].as_str().map(str::to_owned);
        let mut calls = HashMap::new();
        for (turn, data) in started.into_values() {
            if running.is_some() && turn == running {
                let (call, name, arguments) = started_call(&client, &bot, &data).await;
                if name != reactions::TOOL && name != plan::TOOL {
                    calls.insert(call, (name, arguments));
                }
            }
        }
        t3::remember(&self.id, &bot);
        {
            let mut state = self.state.lock().await;
            state.running = running;
            state.calls = calls;
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
        let handle = tokio::spawn(async move {
            if let Some((client, bot, turn @ Some(_))) = waiting {
                follower.approvals(&agent, &client, &bot, turn).await;
            }
            follower.follow(&agent, events).await
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
        agent.options(
            model,
            effort,
            bound.map(|bound| (bound.model.as_str(), bound.client.providers())),
        )
    }

    async fn show_options(&self, agent: &Agent) {
        let options = self.options(agent).await;
        agent.out.update(
            &self.id,
            json!({"sessionUpdate": "config_option_update", "configOptions": options}),
        );
    }

    /// The person picks a model or an effort. A bot's thread takes a
    /// model of its family only, at once when the bot is idle and with its
    /// next turn when it is not; a thread that is no bot yet keeps it for
    /// the lead it makes.
    async fn set_option(&self, agent: &Agent, params: &Value) -> Reply {
        let option = params.get("configId").unwrap_or(&params["id"]);
        let Some(value) = params["value"].as_str() else {
            return Err((INVALID_PARAMS, "an option is picked by its value".into()));
        };
        let _one = self.switching.lock().await;
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
            // words, unless they are `/lead NAME`, which opens one there.
            let chosen = lead_asked(&text);
            let pick = chosen
                .clone()
                .unwrap_or_else(|| project::Pick::Thread(text.clone()));
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
        let steer = state.running.is_some();
        let mut asked = json!({"bot": bound.bot, "request_id": crate::unique("acp"), "prompt": text,
            "delivery": if steer { json!("steer") } else { Value::Null },
            "images": if images.is_empty() { Value::Null } else { json!(images) }});
        if !steer && let Some(dir) = &bound.moving {
            asked["workspace"] = json!(dir);
        }
        let client = bound.client.clone();
        // The lead its folder opens next in Sonny.
        project::wrote_to(&bound.workspace, &bound.bot);
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
        let state = self.state.lock().await;
        let Some(bound) = &state.bound else { return };
        let Ok(page) = (bound.client)
            .request("history_nodes", json!({"bot": bound.bot, "limit": 64}))
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
        // Newest first, as the nodes were asked for.
        let reply = items(&bound.client, &bound.bot, &nodes)
            .await
            .into_iter()
            .filter(|item| item["role"] == "assistant")
            .map(|item| crate::text(&item))
            .find(|text| !text.is_empty());
        if let Some(reply) = reply {
            agent.out.update(&self.id, json!({"sessionUpdate": "agent_message_chunk",
                "messageId": format!("{}-last", bound.bot), "content": {"type": "text", "text": reply}}));
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
                state.calls.clear();
            }
            state.running = running;
        }
        if running.is_some() {
            self.approvals(agent, &client, &bot, running).await;
        }
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
                    state.calls.clear();
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
                if let Some(rest) = text.strip_prefix(streamed.as_str())
                    && !rest.is_empty()
                {
                    out.update(
                        id,
                        json!({"sessionUpdate": "agent_message_chunk",
                        "messageId": message_id(&bot, turn, state.part),
                        "content": {"type": "text", "text": rest}}),
                    );
                }
            }
            "tool_started" => {
                let (call, name, arguments) = started_call(&client, &bot, data).await;
                state.part += 1;
                state.streamed.clear();
                if name == reactions::TOOL {
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
                state.calls.insert(call, (name, arguments));
            }
            "tool_completed" => {
                let call = data["call_id"].as_str().unwrap_or("").to_owned();
                state
                    .asked
                    .retain(|asked| !asked.starts_with(&format!("{call}#")));
                let Some((name, arguments)) = state.calls.remove(&call) else {
                    return;
                };
                let (mut shown, mut failed) = (String::new(), data["failed"] == true);
                if let Some(node) = data["node"].as_i64()
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
                state.calls.clear();
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

    /// The calls of `turn` waiting for the person, each put to them once:
    /// the gate it waits on is the person's (`ask`), or the judge handed
    /// it over (`auto`).
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
            let person = !call["asked"].is_null()
                || (call["gates"].as_array())
                    .is_some_and(|gates| gates.iter().any(|g| g == "manual"));
            if call["turn"].as_i64() != turn || !person {
                continue;
            }
            let key = format!(
                "{}#{}",
                call["call_id"].as_str().unwrap_or(""),
                call["request"]
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
                    "tag": crate::person_gate(call), "decision": decision, "by": "acp"}),
            )
            .await
            .is_ok(),
        None => false,
    };
    // Not delivered: the call is put to the person again once the thread
    // follows its bot again.
    if !sent {
        let key = format!("{id}#{}", call["request"]);
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
/// was set up: the lookout watching the lead's tasks and answering
/// Sonny's tools, and a judge for an `auto` gate.
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
    crate::start_lookout(client).await?;
    if approvals::mode(&record) == Some("auto") {
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
/// the approval mode a new lead starts in, the lookout watching its tasks,
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
    if let Ok(project::Instructions::Stale(text)) =
        project::review(Path::new(&workspace), &record, &key, lead.new)
    {
        asked["instructions"] = json!(text);
    }
    let declare = declared::declare(&record, true);
    if let Some(declare) = &declare {
        asked["declare"] = declare.clone();
    }
    let mode = approvals::mode(&record);
    let mut gating = lead.new.then_some(approvals::MODES[0].0);
    if gating.is_none()
        && let Some(mode) = mode.filter(|mode| *mode != "full")
        && !approvals::sandboxed_as(&record, &approvals::sandbox(mode, Path::new(&workspace)))
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
        fields.extend(approvals::sandbox(mode, Path::new(&workspace)));
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
    lookout::subscribe_lead(&agent.options.store, &lead.bot, &lead.project)?;
    crate::start_lookout(client).await?;
    if gating.or(mode) == Some("auto") {
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
    let Some(name) = arguments["name"].as_str() else {
        return;
    };
    // Named as `start_task` names it.
    let short = name.trim();
    let bot = match short.strip_prefix(&format!("{project}.")) {
        Some(_) => short.to_owned(),
        None => format!("{project}.{short}"),
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

/// The lead a thread's first words ask for: `/lead NAME` one of the
/// folder's, `/lead new [NAME]` a new one.
fn lead_asked(text: &str) -> Option<project::Pick> {
    let rest = text.trim().strip_prefix("/lead")?;
    let mut words = rest.split_whitespace();
    match (words.next()?, words.next(), words.next()) {
        ("new", name, None) => Some(project::Pick::New(name.map(str::to_owned))),
        (name, None, _) => Some(project::Pick::Named(name.to_owned())),
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

#[cfg(test)]
mod tests {
    use super::*;

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
            json!({"value": "openai/gpt-6-sol", "name": "gpt-6-sol · openai",
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
    fn long_output_is_cut_on_a_character() {
        let long = "é".repeat(OUTPUT_BYTES);
        let cut = cut(&long);
        assert!(cut.len() <= OUTPUT_BYTES + 4);
        assert!(cut.ends_with('…'));
    }
}
