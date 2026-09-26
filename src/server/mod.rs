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
    output::{self, Output},
    provider::{Provider, STREAMS_PER_CONNECTION, Transport},
    store::{Answer, Binding, Bot, Delivery, Fork, Publication, Started, Store, TurnOptions},
    tools::Registry,
};
use handles::{Completion, Handles, Waiter, now_ms};
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

/// Most admissions (submissions and creations) whose commits may be pending
/// at once. The service queues each with the storage worker and goes on to
/// the next request, so admissions that arrive together share a commit; an
/// idle one waits for no company. Anything else waits for those already
/// queued, and they are answered in the order they arrived.
const ADMISSION_WINDOW: usize = 32;
/// A canonical workspace in JSON: the path limit, every byte escaped.
const PATH_JSON: usize = 6 * 4096 + 2;
/// A model reference in JSON, at the limits `split_model` enforces.
const MODEL_JSON: usize = 64 + 1 + 6 * 256 + 2;
/// The rest of a reply or an event: its envelope, keys, numbers, and short
/// fixed strings.
const FIELDS_JSON: usize = 1024;

/// Bytes and packets sent to one session.
#[derive(Clone, Copy, Default)]
struct Sends {
    bytes: usize,
    packets: usize,
}
impl std::ops::Add for Sends {
    type Output = Sends;
    fn add(self, other: Sends) -> Sends {
        Sends {
            bytes: self.bytes + other.bytes,
            packets: self.packets + other.packets,
        }
    }
}
/// What an admission sends its own session: its reply when it is answered,
/// and its event copies when the publisher delivers them, which can be later.
#[derive(Clone, Copy, Default)]
struct Bound {
    reply: Sends,
    events: Sends,
}
impl Bound {
    fn total(self) -> Sends {
        self.reply + self.events
    }
}

/// At least what an admission sends its own session once its commit lands:
/// its reply, and its event once per `deliveries`, each way the session
/// follows the bot. Queued admissions are answered back to back, faster than
/// any client reads, so what they send one session must fit its output queue
/// together. A creation answers with the bot's record, which repeats the
/// request's strings (escaped no longer than the request wrote them) and the
/// canonical workspace; a submission names its bot twice. A refusal says no
/// more, and may name what it refused. The event names the bot, the request
/// or creator, a workspace and a model.
fn admission_bound(command: &Command, id: &Value, deliveries: usize) -> Bound {
    let (reply, named, reply_workspace) = match command {
        Command::Create {
            bot,
            workspace,
            model,
            instructions,
            reasoning,
            tools,
            created_by,
            compaction_instructions,
            compaction_model,
            ..
        } => (
            output::encoded_len(&(
                id,
                bot,
                model,
                instructions,
                reasoning,
                tools,
                created_by,
                compaction_instructions,
                compaction_model,
            )),
            output::encoded_len(&(bot, created_by)),
            if workspace.is_some() { PATH_JSON } else { 0 },
        ),
        Command::Submit {
            bot,
            request_id,
            model,
            delivery,
            ..
        } => (
            output::encoded_len(&(id, bot, bot, request_id, model, delivery)),
            output::encoded_len(&(bot, request_id)),
            0,
        ),
        _ => return Bound::default(),
    };
    let reply = reply.unwrap_or(output::MAX_EVENT) + reply_workspace + FIELDS_JSON;
    let event = named.unwrap_or(output::MAX_EVENT) + PATH_JSON + MODEL_JSON + FIELDS_JSON;
    Bound {
        reply: Sends {
            bytes: reply,
            packets: 1,
        },
        events: Sends {
            bytes: deliveries * event,
            packets: deliveries,
        },
    }
}

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
        /// The tools this bot may call, from the daemon's registered set.
        tools: Option<Vec<String>>,
        /// The bot on whose behalf the client creates this one, if any.
        created_by: Option<String>,
        created_by_id: Option<i64>,
        /// Compaction instructions and an optional summarizer model, both
        /// the client's; without instructions the bot never compacts.
        compaction_instructions: Option<String>,
        compaction_model: Option<String>,
        /// Anthropic server-side fallbacks for this bot; off unless asked.
        #[serde(default)]
        fallbacks: bool,
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
        created_by: Option<String>,
        created_by_id: Option<i64>,
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
        /// The identity `bot` had when this request was first made; a retry
        /// carrying it is refused if the name has since changed hands.
        bot_id: Option<i64>,
        request_id: String,
        prompt: String,
        workspace: Option<String>,
        model: Option<String>,
        /// `reject` (default), `queue`, or `steer`.
        delivery: Option<String>,
        /// With `steer`: the running turn this message is for, or `stale_turn`.
        expected_turn: Option<i64>,
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
    HistoryNodes {
        bot: String,
        from: Option<i64>,
        limit: Option<usize>,
        min_node: Option<i64>,
        #[serde(default)]
        oldest_first: bool,
    },
    HistoryItems {
        bot: String,
        nodes: Vec<i64>,
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
    /// Stop the daemon. With `grace_ms`, running turns first get up to that
    /// long to finish while nothing new starts; whatever still runs is then
    /// cancelled with `daemon_shutdown`.
    Shutdown {
        #[serde(default)]
        grace_ms: u64,
    },
}
pub struct ProviderSpec {
    pub name: String,
    pub family: Family,
    pub url: String,
    pub key_env: Option<String>,
    /// Authenticate with the ChatGPT login Codex saved, not a key variable.
    pub chatgpt_login: bool,
    /// Carry Responses calls over WebSocket (family `responses-ws`).
    pub socket: bool,
    /// A Bedrock endpoint with no key variable signs with SigV4 and the
    /// AWS credential chain.
    pub sigv4: bool,
}
impl ProviderSpec {
    /// How calls reach the provider, as `ready.providers` reports it.
    pub fn transport(&self) -> &'static str {
        if self.socket { "websocket" } else { "http" }
    }
    /// `NAME[=FAMILY[,URL[,KEY_ENV]]]`. Known names have defaults; the key
    /// variable is read only when named here or implied by a default endpoint.
    /// `chatgpt` at its default endpoint without a key variable uses Codex's
    /// saved ChatGPT login; the login never goes to a caller-chosen URL.
    /// `bedrock` (Claude) and `bedrock-openai` default to Bedrock Mantle in
    /// `AWS_REGION`, or `AWS_DEFAULT_REGION`; any Bedrock URL without a key
    /// variable signs with SigV4, and one with a key variable sends it as a
    /// Bedrock API key.
    pub fn parse(spec: &str) -> Result<Self> {
        Self::parse_with(spec, &|name| std::env::var(name).ok())
    }
    fn parse_with(spec: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
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
            "chatgpt" => ("responses", "https://chatgpt.com/backend-api/codex", None),
            "bedrock" => (
                "anthropic",
                "https://bedrock-mantle.{region}.api.aws/anthropic/v1",
                None,
            ),
            "bedrock-openai" => (
                "responses",
                "https://bedrock-mantle.{region}.api.aws/openai/v1",
                None,
            ),
            _ => ("", "", None),
        };
        let default_url = if default_url.contains("{region}") && url.is_none() {
            let region = env("AWS_REGION")
                .or_else(|| env("AWS_DEFAULT_REGION"))
                .filter(|region| !region.is_empty())
                .ok_or_else(|| {
                    Error::with(
                        "invalid_provider_spec",
                        format!("{spec}: set AWS_REGION or give the endpoint URL"),
                    )
                })?;
            default_url.replace("{region}", &region)
        } else {
            default_url.to_owned()
        };
        let default_url = default_url.as_str();
        let family = family.as_deref().unwrap_or(default_family);
        let (family, socket) = match family {
            "responses-ws" => ("responses", true),
            family => (family, false),
        };
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
        let parsed = reqwest::Url::parse(&url).ok();
        let bedrock = parsed
            .as_ref()
            .and_then(agent_runtime::provider::aws::endpoint)
            .is_some();
        // Bedrock serves Responses over HTTP only.
        if bedrock && socket {
            return fail_with("invalid_provider_spec", spec);
        }
        // Every Bedrock request carries a signature or key and the whole
        // conversation, so none leaves over cleartext.
        if bedrock && parsed.is_some_and(|url| url.scheme() != "https") {
            return Err(Error::with(
                "invalid_provider_spec",
                format!("{spec}: Bedrock endpoints need https"),
            ));
        }
        Ok(Self {
            sigv4: bedrock && key_env.is_none(),
            chatgpt_login: name == "chatgpt" && key_env.is_none() && url == default_url,
            name: name.to_owned(),
            family,
            url,
            key_env,
            socket,
        })
    }
}

/// Codex's `auth.json`: `$CODEX_HOME`, else `~/.codex`.
fn codex_auth() -> Result<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .map(|home| home.join("auth.json"))
        .ok_or(Error::new("provider_login_unavailable"))
}

/// The access token and workspace from Codex's `auth.json`. Codex refreshes
/// the token when it runs; the daemon re-reads the file when the token
/// expires or is refused, and never refreshes it itself.
#[cfg(test)]
fn chatgpt_login(path: &Path) -> Result<(String, String)> {
    agent_runtime::provider::login::read(path).map(|s| (s.token, s.account))
}

pub struct Configuration {
    pub store: PathBuf,
    pub socket: Option<PathBuf>,
    pub providers: Vec<ProviderSpec>,
    /// Concurrent child processes; default 64 per logical CPU; zero unbounded.
    pub max_processes: Option<usize>,
    /// Detached commands still running, daemon-wide; default 16; zero unbounded.
    pub max_detached: Option<usize>,
    /// Turns with a live task (model call or foreground tool); default 4,096; zero unbounded.
    pub max_active: Option<usize>,
    /// Provider requests awaiting response headers; unbounded by default,
    /// since providers hold headers until the first token and the bound
    /// would cap throughput at permits per first-token latency.
    pub max_connecting: Option<usize>,
    /// Submissions waiting to start, as a count and as prompt bytes; both
    /// unbounded by default, since waiting work is durable rows.
    pub max_pending: Option<usize>,
    pub max_pending_bytes: Option<usize>,
    /// Generated tokens per Responses call, including reasoning; none by default.
    pub max_output_tokens: Option<u32>,
    /// Seconds an established provider stream may go without a content
    /// frame before the attempt fails and is retried; default 120.
    pub stall_timeout: Option<u64>,
    /// Seconds an Anthropic prompt cache may sit unread while a tool runs
    /// before it is refreshed; default 240, 0 disables.
    pub keep_warm: Option<u64>,
    /// Anthropic prompt caches last an hour instead of five minutes; their
    /// writes bill twice the input rate instead of 1.25 times, and no
    /// refresh is sent. Responses providers are unaffected.
    pub cache_hour: bool,
    /// Exit a socket daemon after this many seconds with no sessions, no
    /// active turns, and no running background commands; none by default.
    pub idle_exit: Option<u64>,
    /// Model context per request: newest turns within these bounds. Stored
    /// history itself is unbounded. Defaults 8 MiB and 4,096 items.
    pub context_bytes: Option<usize>,
    pub context_items: Option<usize>,
    /// Omitted turns the context note lists, newest first; zero lists none.
    /// Default 48.
    pub note_turns: Option<usize>,
    /// Compaction threshold and verbatim tail as percentages of the context
    /// budget; defaults 75 and 25.
    pub compact_at: Option<usize>,
    pub compact_keep: Option<usize>,
    /// Prune every bot to this many turns' records after each of its turns
    /// finishes; none by default.
    pub retain_turns: Option<usize>,
}

#[derive(Clone, Copy)]
pub struct Limits {
    pub processes: usize,
    pub detached: usize,
    pub active: usize,
    pub connecting: usize,
    pub pending: usize,
    pub pending_bytes: usize,
    /// HTTP/2 connections per provider, enough for `active` turns to stream
    /// at once at the providers' advertised streams per connection.
    pub connections: usize,
    pub context_bytes: usize,
    pub context_items: usize,
    pub note_turns: usize,
    /// Compaction fires at a round boundary once the turns since the last
    /// summary hold this percentage of `context_bytes`, keeping
    /// `compact_keep` percent verbatim.
    pub compact_at: usize,
    pub compact_keep: usize,
}
pub const MIN_CONTEXT_BYTES: usize = 1024;
pub const MIN_CONTEXT_ITEMS: usize = 2;

impl Limits {
    pub fn resolve(config: &Configuration) -> Limits {
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        let active = config.max_active.unwrap_or(4096);
        Limits {
            processes: config.max_processes.unwrap_or(64 * cpus),
            detached: config
                .max_detached
                .unwrap_or(agent_runtime::tools::DEFAULT_DETACHED_BUDGET),
            active,
            connecting: config.max_connecting.unwrap_or(0),
            pending: config.max_pending.unwrap_or(0),
            pending_bytes: config.max_pending_bytes.unwrap_or(0),
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
            note_turns: config.note_turns.unwrap_or(48),
            compact_at: config.compact_at.unwrap_or(75),
            compact_keep: config.compact_keep.unwrap_or(25),
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
    handles: Handles,
    background_failures: mpsc::UnboundedSender<Error>,
    limits: Limits,
    /// The store's identity, announced in `ready` and used for cache keys.
    identity: u128,
    retain_turns: Option<usize>,
    /// Open client sessions, kept by the run loop for `stats`.
    sessions: usize,
    limit_active: usize,
    /// Per bot: the live turn. A parked turn's task ends while a resumed
    /// task may already own the slot.
    active: HashMap<String, Active>,
    next_task: u64,
    jobs: JoinSet<(String, i64, u64, Result<turn::Exit>)>,
    replays: JoinSet<()>,
    /// Retention tasks own outputs and must release them before stdout joins.
    retention: JoinSet<()>,
    /// A turn may be ready to start: set whenever a slot opens or a turn
    /// is queued, cleared when the store has none. Keeps the idle loop free
    /// of a store read per iteration.
    ready_hint: bool,
    /// Provider-reported tokens since this daemon started, for `stats`.
    tokens: Arc<turn::TokenTotals>,
    /// Turns parked on a rate-limited pool, by resume time. Each holds no
    /// task and no slot; the run loop resumes them as they come due.
    paced: std::collections::BinaryHeap<std::cmp::Reverse<(u64, String, i64)>>,
    /// Shutting down with a grace period: running turns go on, no turn
    /// starts, and accepted submissions wait durably for the next start.
    draining: bool,
    /// Admissions queued with the storage worker and not yet answered,
    /// oldest first: the worker answers in queue order.
    admissions: std::collections::VecDeque<Admission>,
    /// Active slots promised to queued admissions told they may start a
    /// turn; each is released when its admission is answered.
    reserved: usize,
    /// The wait before trying a refused turn start again, doubling from
    /// 10 ms to a second; zero once the store takes one.
    storage_backoff: Duration,
    /// When the ready turn a refused store left waiting is tried again.
    ready_retry_at: Option<tokio::time::Instant>,
    /// Events of answered admissions that the publisher has not delivered
    /// yet, oldest first. Each keeps its room in its session's queue.
    unpublished: std::collections::VecDeque<Unpublished>,
}

struct Unpublished {
    session: u64,
    cursor: i64,
    events: Sends,
}

/// A queued admission and whom to answer.
struct Admission {
    session: u64,
    output: Output,
    id: Value,
    /// `admission_bound` of its request: room it holds in its session's queue.
    bound: Bound,
    pending: Pending,
}
enum Pending {
    Submit {
        bot: String,
        request_id: String,
        delivery: Delivery,
        /// Holds one of `reserved`.
        reserved: bool,
        draining: bool,
        answer: Answer<(i64, Started)>,
        answered: Option<Result<(i64, Started)>>,
    },
    /// The bot's record, and the cursor of its `created` event.
    Create {
        answer: Answer<(Value, Option<i64>)>,
        answered: Option<Result<(Value, Option<i64>)>>,
    },
    /// Settled before queueing (a refused request), kept in line so replies
    /// stay in request order.
    Settled(Result<Value>),
}

/// Wait for the oldest admission's answer and keep it with the admission.
/// Cancel-safe: the answer is stored in the same poll that receives it.
/// Borrows only the queue, so the run loop can select on it.
async fn arrived(admissions: &mut std::collections::VecDeque<Admission>) {
    let front = admissions.front_mut().expect("an admission is queued");
    match &mut front.pending {
        Pending::Submit {
            answer, answered, ..
        } if answered.is_none() => *answered = Some(answer.await),
        Pending::Create { answer, answered } if answered.is_none() => {
            *answered = Some(answer.await)
        }
        _ => {}
    }
}

/// A bot's live turn: which turn, the task owning it, its cancel signal,
/// and whether a steer may be waiting for it. The flag is the only state a
/// steer submission touches on the running side; idle bots hold nothing.
struct Active {
    turn: i64,
    task: u64,
    /// Set to the error the cancelled turn ends with.
    cancel: watch::Sender<Option<&'static str>>,
    steers: Arc<std::sync::atomic::AtomicBool>,
}
impl Active {
    /// Cancel the turn with `code` unless a cause is already set: the first
    /// cause wins, so shutdown never relabels a client's pending interrupt.
    /// False when the task has already dropped its receiver.
    fn cancel(&self, code: &'static str) -> bool {
        self.cancel.send_if_modified(|cause| {
            cause.is_none() && {
                *cause = Some(code);
                true
            }
        });
        !self.cancel.is_closed()
    }
}

/// The one path durable events take to followers and waiters. The storage
/// worker hands over what each job committed, in commit order, so a
/// follower's cursors only ever rise and a committed batch is published
/// whether or not the task that asked for it still lives.
pub(crate) async fn publish(
    mut publications: mpsc::Receiver<Publication>,
    hub: Hub,
    handles: Handles,
) {
    while let Some(publication) = publications.recv().await {
        match publication {
            Publication::Event(entry) => {
                let bot = entry["bot"].as_str().unwrap_or_default().to_owned();
                // A closed stdio owner ends the daemon through its own signal.
                let _ = hub.durable(&bot, entry).await;
            }
            Publication::Through(cursor) => hub.published_to(cursor),
            Publication::Finished { bot, turn, outcome } => {
                handles.turn_finished(&bot, turn, outcome);
            }
        }
    }
}

pub async fn run(config: Configuration) -> Result<()> {
    let limits = Limits::resolve(&config);
    let transport = Transport::new(limits.connecting, limits.connections)?;
    let stall_timeout = config
        .stall_timeout
        .map_or(agent_runtime::provider::STALL_TIMEOUT, Duration::from_secs);
    let keep_warm = match config.keep_warm {
        Some(0) => None,
        Some(seconds) => Some(Duration::from_secs(seconds)),
        None => Some(agent_runtime::provider::KEEP_WARM),
    };
    let registry = Registry::all()?;
    let mut providers = HashMap::new();
    let credentials = agent_runtime::tools::Credentials::default();
    let mut bindings = serde_json::Map::new();
    for spec in &config.providers {
        let (key, login) = if spec.chatgpt_login {
            // No variable carries the token; the shared set redacts it, and
            // every token the login re-reads later.
            let login = agent_runtime::provider::login::Login::open(
                &codex_auth()?,
                Some(credentials.clone()),
            )?;
            (None, Some(Arc::new(login)))
        } else {
            let key = spec
                .key_env
                .as_ref()
                .map(|env| {
                    std::env::var(env)
                        .map_err(|_| Error::with("provider_key_unavailable", env.as_str()))
                })
                .transpose()?;
            if let (Some(env), Some(value)) = (&spec.key_env, &key) {
                credentials.set(env, value);
            }
            (key, None)
        };
        let mut provider = Provider::new(transport.clone(), spec.family, &spec.url, key)?;
        if let Some(login) = login {
            provider = provider.with_login(login)?;
        }
        let mut auth = None;
        if spec.sigv4 {
            let url =
                reqwest::Url::parse(&spec.url).map_err(|_| Error::new("invalid_provider_url"))?;
            let aws =
                agent_runtime::provider::aws::Aws::open(&url, Some(credentials.clone())).await?;
            auth = Some(json!({"auth":"sigv4","region":aws.region(),
                "credentials":aws.source()}));
            provider = provider.with_aws(Arc::new(aws))?;
        }
        if let Some(cap) = config.max_output_tokens {
            provider = provider.with_max_output_tokens(cap)?;
        }
        if spec.socket {
            provider = provider.with_socket()?;
        }
        let provider = provider
            .with_stall_timeout(stall_timeout)?
            .with_keep_warm(keep_warm)?
            .with_cache_hour(config.cache_hour);
        if providers.insert(spec.name.clone(), provider).is_some() {
            return fail_with("duplicate_provider", spec.name.as_str());
        }
        let mut binding = json!({"family":spec.family.name(),"url":spec.url,
            "transport":spec.transport()});
        if let Some(Value::Object(auth)) = auth {
            binding.as_object_mut().expect("object").extend(auth);
        }
        bindings.insert(spec.name.clone(), binding);
    }
    if providers.is_empty() {
        return fail("no_providers");
    }
    let (store, publications) = Store::open(&config.store).await?;
    let (pending, pending_bytes) = (limits.pending, limits.pending_bytes);
    store
        .call(move |db| {
            db.set_pending_limits(pending, pending_bytes);
            Ok(())
        })
        .await?;
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
        .with_credentials(credentials)
        .with_environment(environment)
        .with_process_budget(limits.processes)
        .with_detached_budget(limits.detached);
    let hub = Hub::default();
    let lineage = store.op("store_identity", |db| db.store_identity()).await?;
    let identity = store.instance_identity(lineage)?;
    let ready = json!({"event":"ready","protocol":3,"pid":std::process::id(),
        "store":{"identity":format!("{identity:032x}"),"lineage":format!("{:016x}", lineage as u64)},
        "capabilities":["create","resume","fork_any_node","context_window","submit","bot_identity","delivery","interrupt","events","item","history_nodes","history_items","artifact","follow","follow_all","bots","wait","wait_any","stats","turns","result","budgets","delete","prune","shutdown_grace"],
        "limits":{"processes":limits.processes,"detached":limits.detached,"active":limits.active,"connecting":limits.connecting,
            "pending":limits.pending,"pending_bytes":limits.pending_bytes,
            "connections":limits.connections,
            "output_tokens":config.max_output_tokens,"idle_exit_seconds":config.idle_exit,
            "stall_timeout_seconds":stall_timeout.as_secs(),
            "keep_warm_seconds":keep_warm.map_or(0, |after| after.as_secs()),
            "cache_ttl":if config.cache_hour { "1h" } else { "5m" },
            "context_bytes":limits.context_bytes,"context_items":limits.context_items,
            "note_turns":limits.note_turns,"compact_at":limits.compact_at,"compact_keep":limits.compact_keep,
            "retain_turns":config.retain_turns},
        "schema":agent_runtime::store::Database::SCHEMA,
        "tools":registry.names(),"providers":bindings,
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
    let mut publisher = tokio::spawn(publish(publications, hub.clone(), handles.clone()));
    // Turns parked before a restart keep waiting; their processes are gone.
    // Those parked on a closed pool wait for their time, not for handles.
    let mut paced_at_start = Vec::new();
    for waiting in store.op("waiting_turns", |db| db.waiting_turns()).await? {
        if waiting.paced_since_ms.is_some() {
            paced_at_start.push((waiting.deadline_ms.unwrap_or(0), waiting.bot, waiting.turn));
            continue;
        }
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
        identity,
        transport,
        providers: Arc::new(providers),
        registry,
        hub,
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
        retention: JoinSet::new(),
        // Queued turns that survived a restart start as capacity allows.
        ready_hint: true,
        tokens: Arc::default(),
        paced: std::collections::BinaryHeap::new(),
        draining: false,
        admissions: std::collections::VecDeque::with_capacity(ADMISSION_WINDOW),
        reserved: 0,
        storage_backoff: Duration::ZERO,
        ready_retry_at: None,
        unpublished: std::collections::VecDeque::new(),
    };
    for (at, bot, turn) in paced_at_start {
        service.paced.push(std::cmp::Reverse((at, bot, turn)));
    }
    let idle_exit = config
        .idle_exit
        .filter(|_| config.socket.is_some())
        .map(Duration::from_secs);
    let mut last_activity = std::time::Instant::now();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    // A shutdown's grace deadline: running turns may finish until then. The
    // placeholder only fills the disabled select arm, never polled.
    let mut drain_until: Option<tokio::time::Instant> = None;
    let undrained = tokio::time::Instant::now();
    // A fatal error still ends through the cleanup below, so queued
    // admissions are answered and their turns end like any other, unless
    // the stdio owner's output is what failed.
    let served: Result<()> = async {
        loop {
            if service.draining && service.active.is_empty() {
                break;
            }
            // Computed before the select so its arms borrow the service freely.
            let (paced_due, paced_delay) = (service.paced.peek().is_some(), service.paced_delay());
            let ready_retry = service.ready_retry_at;
            tokio::select! {
                _ = stdout_closed.wait_for(|closed| *closed), if stdio_owner => return fail("output_closed"),
                Some(error) = failures.recv() => return Err(error),
                _ = service.replays.join_next(), if !service.replays.is_empty() => {}
                _ = service.retention.join_next(), if !service.retention.is_empty() => {}
                joined = service.jobs.join_next(), if !service.jobs.is_empty() => {
                    let (bot, turn, task, exit) = joined.unwrap().map_err(|_| Error::new("turn_task_failed"))?;
                    service.complete(bot, turn, task, exit).await?;
                }
                _ = terminate.recv() => break,
                _ = interrupt.recv() => break,
                _ = tokio::time::sleep_until(drain_until.unwrap_or(undrained)), if drain_until.is_some() => break,
                _ = tokio::time::sleep(idle_exit.unwrap_or(Duration::MAX).min(Duration::from_secs(3600))), if idle_exit.is_some() => {
                    // Idle means no client, no live turn, and no running command.
                    // Parked turns are durable and resume on the next start.
                    let running = service.store.op("running_processes", |db| db.running_processes()).await?;
                    if sessions.is_empty() && service.active.is_empty() && service.admissions.is_empty() && running == 0 {
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
                _ = std::future::ready(()), if service.ready_hint && ready_retry.is_none() && service.has_capacity() => {
                    service.dispatch_ready().await?;
                }
                _ = tokio::time::sleep_until(ready_retry.unwrap_or_else(tokio::time::Instant::now)), if ready_retry.is_some() => {
                    service.ready_retry_at = None;
                }
                // A paced turn comes due: resume it like any parked turn, capacity
                // permitting; a stale entry (interrupted, deleted) is dropped.
                _ = tokio::time::sleep(paced_delay), if paced_due && service.has_capacity() => {
                    if let Some(std::cmp::Reverse((_, bot, turn))) = service.paced.pop() {
                        service.resume(bot, turn).await?;
                    }
                }
                // Queued admissions are answered as their commits land, in order.
                _ = arrived(&mut service.admissions), if !service.admissions.is_empty() => {
                    let (session, output, id, result) = service.settled();
                    reply(&mut service, &mut sessions, stdio_owner, session, &output, id, result).await?;
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
                            // A bad id is refused like a malformed line, after the
                            // admissions queued before it.
                            let request = request.and_then(|request| match &request.id {
                                Value::Number(number) if number.is_u64() => Ok(request),
                                Value::String(text) if text.len() <= 128 => Ok(request),
                                _ => fail("invalid_request_id"),
                            });
                            let bound = request.as_ref().map_or(Bound::default(), |r| service.admission_sends(&r.command, &r.id, id));
                            // Everything but an admission with room waits for the
                            // admissions already queued, keeping request order.
                            while service.must_settle(request.as_ref().ok().map(|r| &r.command), id, &output, bound) {
                                let (session, output, id, result) = service.settle().await;
                                reply(&mut service, &mut sessions, stdio_owner, session, &output, id, result).await?;
                            }
                            // Even with none queued, an admission waits for the events
                            // of those answered before it when they and it would not
                            // fit together: a socket whose queue overflows is closed,
                            // and its admission must not commit first. The stdio
                            // owner's output waits for room instead.
                            if !(id == 0 && stdio_owner)
                                && let Some(cursor) = service.awaits_publication(request.as_ref().ok().map(|r| &r.command), id, &output, bound)
                            {
                                service.hub.published_through(cursor).await;
                                if *output.subscribe_closed().borrow() { continue; }
                            }
                            let (request_id, result, shutdown) = match request {
                                Ok(request) => {
                                    let shutdown = match request.command {
                                        Command::Shutdown { grace_ms } => Some(grace_ms),
                                        _ => None,
                                    };
                                    let admission = matches!(request.command, Command::Create { .. } | Command::Submit { .. });
                                    let result = service.dispatch(request.command, id, &output, request.id.clone(), bound).await;
                                    if result.as_ref().is_err_and(|e| e.code == "deferred") { continue; }
                                    // A refused admission still answers after those queued before it.
                                    if admission && !service.admissions.is_empty() {
                                        service.admissions.push_back(Admission { session: id, output, id: request.id, bound, pending: Pending::Settled(result) });
                                        continue;
                                    }
                                    (request.id, result, shutdown)
                                }
                                Err(error) => (Value::Null, Err(error), None),
                            };
                            let shutdown = shutdown.filter(|_| result.is_ok());
                            reply(&mut service, &mut sessions, stdio_owner, id, &output, request_id, result).await?;
                            if let Some(grace_ms) = shutdown {
                                // A later shutdown can only bring the deadline closer.
                                let until = tokio::time::Instant::now() + Duration::from_millis(grace_ms);
                                drain_until = Some(drain_until.map_or(until, |current| current.min(until)));
                                service.draining = true;
                                if grace_ms == 0 || service.active.is_empty() {
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
        }
        Ok(())
    }
    .await;
    // The stdio owner's output is gone: no one is left to answer, and a
    // blocked stdout write cannot be cancelled. The next start ends the rest.
    if stdio_owner
        && served
            .as_ref()
            .is_err_and(|error| error.code == "output_closed")
    {
        return served;
    }
    // Committed pieces survive cancellation. Deletion resumes at next open;
    // explicit pruning can be reissued. Await drops before joining stdout.
    service.retention.abort_all();
    while service.retention.join_next().await.is_some() {}
    answer_queued(&mut service, &mut sessions, stdio_owner).await;
    // Release every slot as it is cancelled: a turn interrupted earlier keeps
    // its first cause, and its dropped sender still stops a finish retry.
    for active in std::mem::take(&mut service.active).into_values() {
        active.cancel(turn::SHUTDOWN);
    }
    // A completion that could not commit leaves its turn for the next start
    // to end as interrupted; the rest still drain, and the exit reports it.
    let mut failed = None;
    while let Some(result) = service.jobs.join_next().await {
        let completed = match result.map_err(|_| Error::new("turn_task_failed")) {
            Ok((bot, turn, task, exit)) => service.complete(bot, turn, task, exit).await,
            Err(error) => Err(error),
        };
        if let Err(error) = completed {
            failed.get_or_insert(error);
        }
    }
    service.replays.abort_all();
    while service.replays.join_next().await.is_some() {}
    // Every completion is committed. Allow publication to drain before
    // releasing waiters. Background result tasks may still own Store clones,
    // keeping the stream open even after the service is dropped.
    let handles = service.handles.clone();
    let firehose_hub = service.hub.clone();
    drop(service);
    if tokio::time::timeout(Duration::from_secs(5), &mut publisher)
        .await
        .is_err()
    {
        // A timed-out JoinHandle would detach its task and retain stdout.
        // Await cancellation before the blocking writer join below: this
        // current-thread runtime cannot finish dropping the task during it.
        publisher.abort();
        let _ = publisher.await;
    }
    drop(firehose_hub);
    handles.shutdown();
    // Socket writers need runtime time to flush terminal events and wait
    // results. Drain concurrently under one deadline, so slow clients cannot
    // multiply shutdown latency. Stdout also drains through its worker below.
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        futures_util::future::join_all(sessions.values().map(Output::drain)),
    )
    .await;
    drop(sessions);
    // The stdio firehose's senders went with the service and the publisher;
    // the output worker can now drain and exit, including shutdown without EOF.
    let written = stdout_writer
        .join()
        .map_err(|_| Error::new("output_worker_failed"))
        .flatten();
    drop(socket_owner);
    // The first failure is the cause: a closed stdout also fails its writer.
    served.and(written).and(failed.map_or(Ok(()), Err))
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
        !self.draining
            && (self.limit_active == 0 || self.active.len() + self.reserved < self.limit_active)
    }

    /// What an admission will send `session`: its reply, and its event once
    /// for each way the session follows the bot.
    fn admission_sends(&self, command: &Command, id: &Value, session: u64) -> Bound {
        let deliveries = match command {
            Command::Create { bot, .. } | Command::Submit { bot, .. } => {
                self.hub.deliveries(bot, session)
            }
            _ => 0,
        };
        admission_bound(command, id, deliveries)
    }

    /// Whether a request must wait for queued admissions to be answered
    /// before it is handled. Only an admission may join them, and only while
    /// the window has room, its session's output queue can take what it,
    /// the admissions queued for that session, and the events of those
    /// answered but not yet published will send, and its capacity answer is
    /// the one the serial order would give: with no slot left unpromised,
    /// whether one frees up depends on how the queued admissions end.
    fn must_settle(
        &self,
        command: Option<&Command>,
        session: u64,
        output: &Output,
        bound: Bound,
    ) -> bool {
        if self.admissions.is_empty() {
            return false;
        }
        let full = || {
            if self.admissions.len() >= ADMISSION_WINDOW {
                return true;
            }
            let (bytes, packets) = output.room();
            let queued = self
                .admissions
                .iter()
                .filter(|admission| admission.session == session)
                .map(|admission| admission.bound.total());
            let answered = self.pending_events(session).map(|events| events.events);
            let promised = queued
                .chain(answered)
                .fold(bound.total(), |sum, sends| sum + sends);
            promised.bytes > bytes || promised.packets > packets
        };
        match command {
            Some(Command::Create { .. }) => full(),
            Some(Command::Submit { .. }) => {
                full()
                    || (!self.draining
                        && self.limit_active != 0
                        && self.reserved > 0
                        && self.active.len() + self.reserved >= self.limit_active)
            }
            _ => true,
        }
    }

    /// Events answered for `session` that the publisher has not delivered.
    fn pending_events(&self, session: u64) -> impl Iterator<Item = &Unpublished> {
        let published = self.hub.published();
        self.unpublished
            .iter()
            .filter(move |events| events.session == session && events.cursor > published)
    }
    /// For an admission that finds the window empty: the newest of its
    /// session's events answered but not yet delivered, when those events
    /// and the admission would not fit the session's queue together.
    fn awaits_publication(
        &self,
        command: Option<&Command>,
        session: u64,
        output: &Output,
        bound: Bound,
    ) -> Option<i64> {
        if !self.admissions.is_empty()
            || !matches!(
                command,
                Some(Command::Create { .. } | Command::Submit { .. })
            )
        {
            return None;
        }
        let (promised, newest) = self
            .pending_events(session)
            .fold((bound.total(), None), |(sum, _), events| {
                (sum + events.events, Some(events.cursor))
            });
        let newest = newest?;
        let (bytes, packets) = output.room();
        (promised.bytes > bytes || promised.packets > packets).then_some(newest)
    }

    /// Answer the oldest queued admission once its commit is known.
    async fn settle(&mut self) -> (u64, Output, Value, Result<Value>) {
        arrived(&mut self.admissions).await;
        self.settled()
    }
    /// Answer the oldest admission, whose commit `arrived` has seen, applying
    /// what the serial path did after its store call: start the turn, flag a
    /// steer, release the reserved slot. Its events keep their room in the
    /// session's queue until the publisher has delivered them.
    fn settled(&mut self) -> (u64, Output, Value, Result<Value>) {
        let Admission {
            session,
            output,
            id,
            bound,
            pending,
        } = self.admissions.pop_front().expect("an admission is queued");
        let (result, cursor) = match pending {
            Pending::Submit {
                bot,
                request_id,
                delivery,
                reserved,
                draining,
                answered,
                ..
            } => {
                self.reserved -= usize::from(reserved);
                let answered = answered.expect("the submission was answered");
                let result = self.admitted(bot, request_id, delivery, draining, answered);
                let cursor = result.as_ref().ok().and_then(|r| r["cursor"].as_i64());
                (result, cursor)
            }
            Pending::Create { answered, .. } => {
                match answered.expect("the creation was answered") {
                    Ok((created, cursor)) => (Ok(created), cursor),
                    Err(error) => (Err(error), None),
                }
            }
            Pending::Settled(result) => (result, None),
        };
        // Commits are answered in order, so cursors only grow.
        let published = self.hub.published();
        while self
            .unpublished
            .front()
            .is_some_and(|events| events.cursor <= published)
        {
            self.unpublished.pop_front();
        }
        if let Some(cursor) = cursor.filter(|cursor| *cursor > published)
            && bound.events.packets > 0
        {
            self.unpublished.push_back(Unpublished {
                session,
                cursor,
                events: bound.events,
            });
        }
        (session, output, id, result)
    }

    /// After a submission's commit: start its turn if it runs now, and
    /// make its reply.
    fn admitted(
        &mut self,
        bot: String,
        request_id: String,
        delivery: Delivery,
        draining: bool,
        result: Result<(i64, Started)>,
    ) -> Result<Value> {
        // While draining no turn starts: `reject` work is refused for the
        // next daemon, and queued work waits durably for it.
        let (identity, started) = result.map_err(|error| match error.code.as_str() {
            "active_agent_limit" if draining => Error::new("daemon_draining"),
            _ => error,
        })?;
        let cursor = started.entry.as_ref().and_then(|e| e["cursor"].as_i64());
        if started.fresh && started.status == "running" {
            // A steer queued behind it in the window is its steer once
            // committed: arm the turn's first round boundary for it.
            let steered = self.admissions.iter().any(|admission| {
                matches!(&admission.pending, Pending::Submit { bot: queued, delivery: Delivery::Steer, .. } if *queued == bot)
            });
            self.spawn(bot.clone(), started.turn, false, steered);
        }
        // The running turn may have started after the steer was queued, from
        // an admission answered in between.
        if started.fresh
            && started.status == "queued"
            && delivery == Delivery::Steer
            && let Some(active) = self.active.get(&bot)
        {
            active
                .steers
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if started.status == "ready" {
            self.ready_hint = true;
        }
        Ok(
            json!({"bot":bot,"bot_id":identity,"turn":started.turn,"request_id":request_id,
            "duplicate":!started.fresh,"status":started.status,"cursor":cursor,
            "handle":format!("turn:{bot}/{}", started.turn)}),
        )
    }

    /// How long until the earliest paced turn is due.
    fn paced_delay(&self) -> Duration {
        self.paced
            .peek()
            .map(|std::cmp::Reverse((at, _, _))| Duration::from_millis(at.saturating_sub(now_ms())))
            .unwrap_or(Duration::MAX)
    }

    async fn resume(&mut self, bot: String, turn: i64) -> Result<()> {
        // A wake-up can outlive an interrupt, deletion, or reuse of the name.
        // Check only its identity and state, without loading the full bot.
        let check = bot.clone();
        let pending = match self
            .store
            .op("can_resume", move |db| db.can_resume(&check, turn))
            .await
        {
            // A lost group (a full disk, say) leaves the turn parked: the
            // wake-up comes due again after a backoff, as a paced one does.
            Err(error) if error.code == "storage_error" => {
                let at = now_ms() + self.storage_retry().as_millis() as u64;
                self.paced.push(std::cmp::Reverse((at, bot, turn)));
                return Ok(());
            }
            pending => pending?,
        };
        if pending {
            self.spawn(bot, turn, true, false);
        }
        Ok(())
    }

    /// The wait after the store refused to start a turn: doubling from
    /// 10 ms up to a second, until one starts.
    fn storage_retry(&mut self) -> Duration {
        let delay = self.storage_backoff.max(Duration::from_millis(10));
        self.storage_backoff = (delay * 2).min(Duration::from_secs(1));
        delay
    }

    /// Start the oldest turn waiting for a slot, or learn there is none.
    /// A turn that cannot start (for example, its bot's budget is spent)
    /// ends with that error; the bot's next queued turn takes its place.
    /// A store that refuses writes (a full disk, say) leaves the turn ready,
    /// and the service tries again after a backoff while the rest goes on.
    async fn dispatch_ready(&mut self) -> Result<()> {
        match self.start_ready().await {
            Err(error) if error.code == "storage_error" => {
                let delay = self.storage_retry();
                self.ready_retry_at = Some(tokio::time::Instant::now() + delay);
                Ok(())
            }
            started => {
                self.storage_backoff = Duration::ZERO;
                started
            }
        }
    }
    async fn start_ready(&mut self) -> Result<()> {
        let Some((bot, turn)) = self.store.op("next_ready", |db| db.next_ready()).await? else {
            self.ready_hint = false;
            return Ok(());
        };
        let providers = self.providers.clone();
        match self
            .store
            .op("start", move |db| {
                db.start(turn, |bot, model| validate_provider(&providers, bot, model))
            })
            .await
        {
            Ok((_, steers)) => self.spawn(bot, turn, false, steers),
            Err(error) if error.code == "bot_busy" => {}
            // Nothing was written: the turn is still ready.
            Err(error) if error.code == "storage_error" => return Err(error),
            // A strict steer whose turn is over ends as stale, like any
            // other queued turn that cannot start; an already-ended row is
            // simply gone from the line.
            Err(error) if error.code == "stale_turn" => {
                let check = bot.clone();
                if matches!(
                    self.store
                        .op("turn_status", move |db| db.turn_status(&check, turn))
                        .await
                        .as_deref(),
                    Ok("queued" | "ready")
                ) {
                    self.end_queued(bot, turn, error).await?;
                }
            }
            Err(error) => self.end_queued(bot, turn, error).await?,
        }
        Ok(())
    }

    /// End a turn that never started; the worker answers its waiters.
    async fn end_queued(&mut self, bot: String, turn: i64, error: Error) -> Result<()> {
        let keep = self.retain_turns;
        let steers = self.active.get(&bot).map(|active| active.steers.clone());
        self.store
            .op_pruning("end_queued", bot.clone(), move |db| {
                db.end_queued(turn, &error)?;
                // Removing an incompatible head can expose eligible steers.
                // Re-arm this bot inside the committing job, before another
                // boundary can observe the updated queue with a cleared hint.
                if let Some(steers) = steers {
                    steers.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(keep) = keep {
                    db.prune_except(&bot, keep, Some(turn))?;
                }
                Ok(())
            })
            .await?;
        self.ready_hint = true;
        Ok(())
    }

    /// Run a turn as a task: fresh after submission, or resuming a parked one.
    fn spawn(&mut self, bot: String, turn: i64, resume: bool, steers: bool) {
        let (cancel, cancelled) = watch::channel(None);
        self.next_task += 1;
        let task_id = self.next_task;
        // The job that started this turn said whether a steer waits for it;
        // afterwards only a steer for this bot sets the flag.
        let steers = Arc::new(std::sync::atomic::AtomicBool::new(steers));
        self.active.insert(
            bot.clone(),
            Active {
                turn,
                task: task_id,
                cancel,
                steers: steers.clone(),
            },
        );
        let task = Turn {
            bot,
            turn,
            store: self.store.clone(),
            identity: self.identity,
            providers: self.providers.clone(),
            registry: self.registry.clone(),
            hub: self.hub.clone(),
            handles: self.handles.clone(),
            background_failures: self.background_failures.clone(),
            context_bytes: self.limits.context_bytes,
            context_items: self.limits.context_items,
            note_turns: self.limits.note_turns,
            compact_at: self.limits.compact_at,
            compact_keep: self.limits.compact_keep,
            resume,
            steers,
            tokens: self.tokens.clone(),
        };
        let keep = self.retain_turns;
        self.jobs.spawn(async move {
            let (bot, id) = (task.bot.clone(), task.turn);
            // Keep the cancellation receiver alive through completion so an
            // interrupt cannot mistake this task for an unreaped parked turn.
            let mut cancelled = cancelled;
            let exit = task.execute(cancelled.clone()).await;
            let result = match &exit {
                turn::Exit::Finished(error) => {
                    commit_finish(&task.store, &bot, id, error.as_ref(), keep, &mut cancelled)
                        .await
                        .map(|()| exit)
                }
                _ => Ok(exit),
            };
            drop(cancelled);
            (bot, id, task_id, result)
        });
    }

    /// Reap a task after its completion has committed. Independent turns send
    /// their finishes concurrently, letting the worker group their syncs.
    /// The durable busy state and worker publication order keep a successor's
    /// accepted event behind its predecessor's terminal event.
    async fn complete(
        &mut self,
        bot: String,
        turn: i64,
        task: u64,
        exit: Result<turn::Exit>,
    ) -> Result<()> {
        let exit = exit?;
        if self.active.get(&bot).is_some_and(|a| a.task == task) {
            self.active.remove(&bot);
            // A slot opened, and a finish may promote the bot's next turn.
            self.ready_hint = true;
        }
        if !matches!(exit, turn::Exit::Unresumed) {
            self.storage_backoff = Duration::ZERO;
        }
        match exit {
            turn::Exit::Finished(_) => {
                // Interrupt may already have released the task's active slot.
                // Finishing still promotes queued work in that case.
                self.ready_hint = true;
            }
            turn::Exit::Paced(at) => self.paced.push(std::cmp::Reverse((at, bot, turn))),
            turn::Exit::Parked => {}
            // The store refused its resume, so it is still parked: its
            // wake-up comes due again after a backoff.
            turn::Exit::Unresumed => {
                let at = now_ms() + self.storage_retry().as_millis() as u64;
                self.paced.push(std::cmp::Reverse((at, bot, turn)));
            }
        }
        Ok(())
    }

    async fn dispatch(
        &mut self,
        command: Command,
        session: u64,
        output: &Output,
        id: Value,
        bound: Bound,
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
                tools,
                created_by,
                created_by_id,
                compaction_instructions,
                compaction_model,
                fallbacks,
            } => {
                if budget_tokens == Some(0) {
                    return fail("invalid_budget");
                }
                name(&bot)?;
                if let Some(creator) = &created_by {
                    name(creator)?;
                }
                let path = path.as_deref().map(workspace).transpose()?;
                // The daemon supplies no agent behavior: who creates a bot
                // says what it runs and what it is told, and the bot keeps both.
                let reference = model.ok_or(Error::new("model_required"))?;
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
                let instructions = instructions.ok_or(Error::new("instructions_required"))?;
                if instructions.len() > 64 * 1024 {
                    return fail("instructions_limit");
                }
                let tools = tools.ok_or(Error::new("tools_required"))?;
                self.registry.validate(&tools)?;
                if compaction_instructions
                    .as_ref()
                    .is_some_and(|text| text.len() > 64 * 1024)
                {
                    return fail("instructions_limit");
                }
                // The summarizer must speak the bot's family: its items are
                // stored in that encoding and go to the summarizer as they are.
                if let Some(reference) = &compaction_model {
                    let (summarizer, _) = split_model(reference)?;
                    let known = self
                        .providers
                        .get(summarizer)
                        .ok_or(Error::with("provider_unavailable", summarizer))?;
                    if known.family() != family {
                        return fail_with("provider_family_mismatch", reference.clone());
                    }
                }
                let (provider, model) = (provider.to_owned(), model.to_owned());
                let answer = store
                    .queue("create", move |db| {
                        let (created, event) = db.create(
                            &bot,
                            path.as_deref(),
                            Binding {
                                provider: &provider,
                                family,
                                model: &model,
                                instructions: &instructions,
                                reasoning: reasoning.as_deref(),
                                budget_tokens,
                                tools: &tools,
                                created_by: created_by.as_deref(),
                                created_by_id,
                                compaction_instructions: compaction_instructions.as_deref(),
                                compaction_model: compaction_model.as_deref(),
                                fallbacks,
                            },
                        )?;
                        Ok((serde_json::to_value(created)?, event["cursor"].as_i64()))
                    })
                    .await?;
                self.admissions.push_back(Admission {
                    session,
                    output: output.clone(),
                    id,
                    bound,
                    pending: Pending::Create {
                        answer,
                        answered: None,
                    },
                });
                Err(Error::new("deferred"))
            }
            Command::Turns { bot, after, limit } => {
                store
                    .op("turns", move |db| {
                        db.turns(&bot, after, limit.unwrap_or(64))
                    })
                    .await
            }
            Command::Delete { bot } => {
                if self.active.contains_key(&bot) {
                    return fail("bot_busy");
                }
                // One bounded piece per job, on a task of its own, so other
                // bots' requests and commits interleave with a large deletion
                // instead of waiting behind this loop; the bot refuses work
                // from the first piece. The response is sent when it is done.
                let name = bot.clone();
                // Capture identity before spawning, but leave artifact deletion
                // off the dispatch path so other clients are not held behind it.
                let bot_id = store
                    .op("inspect", move |db| Ok(db.inspect(&name)?.id))
                    .await?;
                let (store, hub, output) = (store.clone(), self.hub.clone(), output.clone());
                let providers = self.providers.clone();
                self.retention.spawn(async move {
                    let result = async {
                        let mut deleted = json!({"turns":0,"events":0,"nodes":0});
                        loop {
                            let name = bot.clone();
                            let piece = store
                                .op_pruning("delete_bot", name.clone(), move |db| {
                                    db.delete_bot_piece(
                                        &name,
                                        bot_id,
                                        agent_runtime::store::Database::RETENTION_PIECE,
                                    )
                                })
                                .await?;
                            for key in ["turns", "events", "nodes"] {
                                deleted[key] = json!(
                                    deleted[key].as_i64().unwrap_or(0)
                                        + piece[key].as_i64().unwrap_or(0)
                                );
                            }
                            if piece["done"] == true {
                                break;
                            }
                        }
                        // Its connections go with it, and a later bot of the
                        // same name starts on a fresh one.
                        for provider in providers.values() {
                            provider.forget(&bot);
                        }
                        // Followers learn the bot is gone; nothing durable remains to replay.
                        hub.live(&bot, json!({"event":"deleted","bot":bot,"durable":false}))
                            .await?;
                        Ok(deleted)
                    }
                    .await;
                    retention_reply(session, &output, id, result).await;
                });
                Err(Error::new("deferred"))
            }
            Command::Prune { bot, keep_turns } => {
                // Pieces of turns, oldest first, each its own job, on a task
                // of its own for the same reason as deletion.
                let name = bot.clone();
                let bot_id = store
                    .op("identity", move |db| db.identity(&name, None))
                    .await?;
                let (store, output) = (store.clone(), output.clone());
                self.retention.spawn(async move {
                    let result = async {
                        let mut after = 0;
                        let mut events = 0;
                        loop {
                            let name = bot.clone();
                            let piece = store
                                .op_pruning("prune", name.clone(), move |db| {
                                    db.prune_piece(
                                        &name,
                                        bot_id,
                                        keep_turns,
                                        after,
                                        agent_runtime::store::Database::RETENTION_PIECE,
                                    )
                                })
                                .await?;
                            events += piece["events"].as_i64().unwrap_or(0);
                            match piece["next_after"].as_i64() {
                                Some(next) => after = next,
                                None => {
                                    return Ok(json!({"events":events,
                                        "pruned_cursor":piece["pruned_cursor"]}));
                                }
                            }
                        }
                    }
                    .await;
                    retention_reply(session, &output, id, result).await;
                });
                Err(Error::new("deferred"))
            }
            Command::Result { bot, turn } => {
                store
                    .op("turn_outcome", move |db| {
                        match db.turn_outcome(&bot, turn)? {
                            Some(outcome) => Ok(outcome),
                            None => {
                                let status = db.turn_status(&bot, turn)?;
                                Ok(json!({"turn":turn,"status":status,"finished":false}))
                            }
                        }
                    })
                    .await
            }
            Command::Resume { bot } => {
                store
                    .op("inspect", move |db| {
                        Ok(serde_json::to_value(db.inspect(&bot)?)?)
                    })
                    .await
            }
            Command::Stats => {
                let (waiting, running, queued, paced, pending_bytes) = store
                    .op("counts", |db| {
                        let (waiting, running, queued, paced) = db.counts()?;
                        Ok((waiting, running, queued, paced, db.pending()?.1))
                    })
                    .await?;
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
                    "paced_turns": paced,
                    "queued_turns": queued,
                    "pending_bytes": pending_bytes,
                    "pending_limit": self.limits.pending,
                    "pending_bytes_limit": self.limits.pending_bytes,
                    "running_processes": running,
                    "queued_processes": self.registry.pending(),
                    "process_limit": self.limits.processes,
                    "transport": {"in_flight_by_shard": self.transport.loads()},
                    "providers": providers,
                    "store": store.stats(),
                    "tokens": self.tokens.snapshot(),
                    "handles": {"waiters": waiters, "retained": retained},
                    "draining": self.draining,
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
                            request: id,
                        },
                    )
                    .await;
                // The response is sent by the completion, not by this dispatch.
                Err(Error::new("deferred"))
            }
            Command::Bots { after, limit } => {
                store
                    .op("list", move |db| {
                        db.list(after.as_deref(), limit.unwrap_or(64))
                    })
                    .await
            }
            Command::Fork {
                source,
                checkpoint,
                bot,
                workspace: path,
                budget_tokens,
                created_by,
                created_by_id,
            } => {
                if budget_tokens == Some(0) {
                    return fail("invalid_budget");
                }
                name(&bot)?;
                if let Some(creator) = &created_by {
                    name(creator)?;
                }
                let path = path.as_deref().map(workspace).transpose()?;
                let (created, event) = store
                    .op("fork", move |db| {
                        db.fork(
                            &source,
                            &bot,
                            Fork {
                                checkpoint,
                                workspace: path.as_deref(),
                                budget_tokens,
                                created_by: created_by.as_deref(),
                                created_by_id,
                            },
                        )
                    })
                    .await?;
                let _ = event;
                Ok(serde_json::to_value(created)?)
            }
            Command::Events { bot, after, limit } => {
                store
                    .op("events", move |db| db.events(&bot, after, limit))
                    .await
            }
            Command::HistoryNodes {
                bot,
                from,
                limit,
                min_node,
                oldest_first,
            } => {
                store
                    .read("history_nodes", move |db| {
                        db.history_nodes(&bot, from, limit.unwrap_or(400), min_node, oldest_first)
                    })
                    .await
            }
            Command::HistoryItems { bot, nodes } => {
                store
                    .read("history_items", move |db| db.history_items(&bot, &nodes))
                    .await
            }
            Command::Item { bot, node } => store.read("item", move |db| db.item(&bot, node)).await,
            Command::Artifact {
                bot,
                turn,
                call_id,
                stream,
                offset,
                limit,
            } => {
                store
                    .op("artifact_page", move |db| match stream {
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
                    store.op("inspect", move |db| db.inspect(&check)).await?;
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
                bot_id,
                request_id,
                prompt,
                workspace: path,
                model,
                delivery,
                expected_turn,
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
                if expected_turn.is_some() && delivery != Delivery::Steer {
                    return fail_with("invalid_delivery", "expected_turn needs delivery steer");
                }
                // A turn may run in another checkout or on another model of
                // the same family; the conversation encoding never changes.
                let options = TurnOptions {
                    workspace: path.as_deref().map(workspace).transpose()?,
                    model,
                    delivery,
                    expected_turn,
                };
                // A slot is promised before the commit that may take it, so
                // admissions queued together cannot start more turns than
                // the limit allows.
                let capacity = self.has_capacity();
                let reserved = capacity && self.limit_active != 0;
                self.reserved += usize::from(reserved);
                // A steer arms the running turn inside its committing job,
                // as `end_queued` does, so no round boundary before the
                // answer finds it waiting behind a cleared flag.
                let steers = self
                    .active
                    .get(&bot)
                    .filter(|_| delivery == Delivery::Steer)
                    .map(|active| active.steers.clone());
                let (b, r) = (bot.clone(), request_id.clone());
                let providers = self.providers.clone();
                let queued = store
                    .queue("begin", move |db| {
                        let identity = db.identity(&b, bot_id)?;
                        let started =
                            db.begin(&b, &r, &prompt, capacity, &options, |bot, model| {
                                validate_provider(&providers, bot, model)
                            })?;
                        if started.fresh
                            && started.status == "queued"
                            && let Some(steers) = &steers
                        {
                            steers.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        Ok((identity, started))
                    })
                    .await;
                let answer = match queued {
                    Ok(answer) => answer,
                    Err(error) => {
                        self.reserved -= usize::from(reserved);
                        return Err(error);
                    }
                };
                self.admissions.push_back(Admission {
                    session,
                    output: output.clone(),
                    id,
                    bound,
                    pending: Pending::Submit {
                        bot,
                        request_id,
                        delivery,
                        reserved,
                        draining: self.draining,
                        answer,
                        answered: None,
                    },
                });
                Err(Error::new("deferred"))
            }
            Command::Interrupt { bot, turn } => {
                if let Some(active) = self.active.get(&bot).filter(|a| a.turn == turn) {
                    if active.cancel(turn::INTERRUPTED) {
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
                    .op("turn_status", move |db| db.turn_status(&check, turn))
                    .await
                    .unwrap_or_default();
                if matches!(status.as_str(), "queued" | "ready") {
                    self.end_queued(bot, turn, Error::new(turn::INTERRUPTED))
                        .await?;
                    return Ok(json!({"interrupt_requested":true,"turn":turn,"queued":true}));
                }
                if self.active.contains_key(&bot) {
                    return fail("stale_turn");
                }
                // A parked turn has no task; confirm durable state and end it.
                let check = bot.clone();
                let state = store.op("inspect", move |db| db.inspect(&check)).await?;
                if state.running_turn.is_none() {
                    return fail("no_active_turn");
                }
                if state.running_turn != Some(turn)
                    || (state.status != "waiting" && state.status != "paced")
                {
                    return fail("stale_turn");
                }
                let name = bot.clone();
                let keep = self.retain_turns;
                store
                    .op_pruning("finish", name.clone(), move |db| {
                        turn::Finished::record(
                            db,
                            &name,
                            turn,
                            Some(&Error::new(turn::INTERRUPTED)),
                            keep,
                        )
                    })
                    .await?;
                // Only once the end commits: a refused one leaves the turn
                // parked, and its wait must still wake it.
                self.handles.forget(Waiter::Turn(turn));
                self.ready_hint = true;
                Ok(json!({"interrupt_requested":true,"turn":turn,"parked":true}))
            }
            Command::Shutdown { grace_ms } => {
                if grace_ms > 86_400_000 {
                    return fail("invalid_timeout");
                }
                Ok(json!({"shutting_down":true}))
            }
        }
    }
}

/// Commit a finished turn. A group that cannot commit (a full disk, say)
/// leaves nothing durable, so the completion is tried again, with backoff,
/// while the bot stays busy and other bots go on. An interrupt that arrives
/// meanwhile is honored: the next attempt ends the turn as interrupted.
/// Only shutdown stops it (its cause, or the service dropping the turn's
/// slot): the turn then stays running in the store, and the next start ends
/// it as interrupted.
async fn commit_finish(
    store: &Store,
    bot: &str,
    turn: i64,
    error: Option<&Error>,
    keep: Option<usize>,
    cancelled: &mut watch::Receiver<Option<&'static str>>,
) -> Result<()> {
    let mut error = error.cloned();
    let mut backoff = Duration::from_millis(10);
    loop {
        let (name, attempt) = (bot.to_owned(), error.clone());
        let failure = match store
            .op_pruning("finish", bot.to_owned(), move |db| {
                turn::Finished::record(db, &name, turn, attempt.as_ref(), keep)
            })
            .await
        {
            Err(failure) if failure.code == "storage_error" => failure,
            done => return done,
        };
        let interrupted = error.as_ref().is_some_and(|e| e.code == turn::INTERRUPTED);
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            cause = cancelled.wait_for(|cause| {
                *cause == Some(turn::SHUTDOWN) || (!interrupted && *cause == Some(turn::INTERRUPTED))
            }) => match cause.map(|cause| *cause) {
                Ok(Some(turn::INTERRUPTED)) => {
                    error = Some(Error::new(turn::INTERRUPTED));
                    continue;
                }
                _ => return Err(failure),
            },
        }
        backoff = (backoff * 2).min(Duration::from_secs(1));
    }
}

/// Answer the admissions still queued at shutdown and start their turns, so
/// shutdown ends those turns like any other running turn. A stdio owner that
/// cannot take a reply within its timeout gets no more: the rest settle
/// without waiting on it again.
async fn answer_queued(
    service: &mut Service,
    sessions: &mut HashMap<u64, Output>,
    stdio_owner: bool,
) {
    let mut stdout_lost = false;
    while !service.admissions.is_empty() {
        let (session, output, id, result) = service.settle().await;
        if stdout_lost && session == 0 && stdio_owner {
            continue;
        }
        // Only the stdio owner's reply can fail.
        stdout_lost |= reply(service, sessions, stdio_owner, session, &output, id, result)
            .await
            .is_err();
    }
}

/// Answer a request: the stdio owner gets bounded backpressure, and losing
/// it ends the daemon; a socket client that cannot take the reply is dropped.
async fn reply(
    service: &mut Service,
    sessions: &mut HashMap<u64, Output>,
    stdio_owner: bool,
    session: u64,
    output: &Output,
    id: Value,
    result: Result<Value>,
) -> Result<()> {
    if session == 0 && stdio_owner {
        if !matches!(
            tokio::time::timeout(Duration::from_secs(5), output.respond(id, result)).await,
            Ok(Ok(()))
        ) {
            return fail("output_closed");
        }
    } else if output.try_respond(id, result).is_err() {
        if sessions.remove(&session).is_some() {
            service.sessions = sessions.len();
            service.hub.close_session(session);
            service.handles.close_session(session);
        }
        output.close();
    }
    Ok(())
}

/// Match ordinary replies: the stdio owner (session zero) gets bounded
/// backpressure; socket clients cannot hold up a reply task when they lag.
async fn retention_reply(session: u64, output: &Output, id: Value, result: Result<Value>) {
    let sent = if session == 0 {
        matches!(
            tokio::time::timeout(Duration::from_secs(5), output.respond(id, result)).await,
            Ok(Ok(()))
        )
    } else {
        output.try_respond(id, result).is_ok()
    };
    if !sent {
        output.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_cancel_cause_wins() {
        let (cancel, cancelled) = watch::channel(None);
        let active = Active {
            turn: 1,
            task: 1,
            cancel,
            steers: Arc::default(),
        };
        assert!(active.cancel(turn::INTERRUPTED));
        assert!(active.cancel(turn::SHUTDOWN));
        assert_eq!(*cancelled.borrow(), Some(turn::INTERRUPTED));
        drop(cancelled);
        assert!(!active.cancel(turn::SHUTDOWN));
    }

    #[tokio::test]
    async fn retention_replies_backpressure_stdio_but_evict_lagged_sockets() {
        use tokio::io::AsyncBufReadExt;
        for session in [0, 1] {
            let (writer, reader) = tokio::io::duplex(1);
            let output = Output::writer(writer);
            let closed = output.subscribe_closed();
            output.send(json!({"first":true})).await.unwrap();
            // Block the writer on its first packet, then fill the queue.
            tokio::task::yield_now().await;
            while output.try_send(json!({"filler":true})).is_ok() {}
            let response = output.clone();
            let mut task = tokio::spawn(async move {
                retention_reply(
                    session,
                    &response,
                    json!("retention"),
                    Ok(json!({"done":true})),
                )
                .await;
            });
            if session == 0 {
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), &mut task)
                        .await
                        .is_err()
                );
                assert!(!*closed.borrow());
                let mut lines = BufReader::new(reader).lines();
                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        let line = lines.next_line().await.unwrap().unwrap();
                        let value: Value = serde_json::from_str(&line).unwrap();
                        if value["id"] == "retention" {
                            assert_eq!(value["result"]["done"], true);
                            break;
                        }
                    }
                    task.await.unwrap();
                })
                .await
                .unwrap();
                assert!(!*closed.borrow());
            } else {
                tokio::time::timeout(Duration::from_secs(1), task)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(*closed.borrow());
            }
            output.close();
        }
    }

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
    fn bedrock_specs_default_to_mantle_in_the_aws_region_and_sign() {
        let region = |name: &str| (name == "AWS_REGION").then(|| "us-east-1".to_owned());
        let parse = |spec: &str| ProviderSpec::parse_with(spec, &region).unwrap();
        let claude = parse("bedrock");
        assert_eq!(claude.family, Family::Anthropic);
        assert_eq!(
            claude.url,
            "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1"
        );
        assert!(claude.sigv4 && claude.key_env.is_none());
        let openai = parse("bedrock-openai");
        assert_eq!(openai.family, Family::Responses);
        assert_eq!(
            openai.url,
            "https://bedrock-mantle.us-east-1.api.aws/openai/v1"
        );
        assert!(openai.sigv4);
        // AWS_DEFAULT_REGION is the fallback name the AWS CLI also reads.
        let fallback = |name: &str| (name == "AWS_DEFAULT_REGION").then(|| "eu-west-1".to_owned());
        assert_eq!(
            ProviderSpec::parse_with("bedrock", &fallback).unwrap().url,
            "https://bedrock-mantle.eu-west-1.api.aws/anthropic/v1"
        );
        let error = ProviderSpec::parse_with("bedrock", &|_| None)
            .err()
            .unwrap();
        assert_eq!(error.code, "invalid_provider_spec");
        // Any Bedrock URL signs, runtime included; a key variable is sent as
        // a Bedrock API key instead.
        let runtime =
            parse("br=anthropic,https://bedrock-runtime.us-west-2.amazonaws.com/anthropic/v1");
        assert!(runtime.sigv4);
        let keyed = parse(
            "mantle=anthropic,https://bedrock-mantle.us-east-1.api.aws/anthropic/v1,AWS_BEARER_TOKEN_BEDROCK",
        );
        assert!(!keyed.sigv4);
        assert_eq!(keyed.key_env.as_deref(), Some("AWS_BEARER_TOKEN_BEDROCK"));
        assert!(!parse("anthropic").sigv4);
        // Bedrock has no WebSocket transport.
        assert!(
            ProviderSpec::parse_with(
                "b=responses-ws,https://bedrock-mantle.us-east-1.api.aws/openai/v1",
                &region
            )
            .is_err()
        );
        // Nor any cleartext one, signed or keyed.
        for spec in [
            "b=anthropic,http://bedrock-runtime.us-west-2.amazonaws.com/anthropic/v1",
            "b=responses,http://bedrock-mantle.us-east-1.api.aws/openai/v1,AWS_BEARER_TOKEN_BEDROCK",
        ] {
            let error = ProviderSpec::parse_with(spec, &region).err().unwrap();
            assert_eq!(error.code, "invalid_provider_spec");
        }
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

    #[test]
    fn chatgpt_uses_the_codex_login_unless_given_a_key() {
        let login = ProviderSpec::parse("chatgpt").unwrap();
        assert_eq!(login.url, "https://chatgpt.com/backend-api/codex");
        assert!(login.chatgpt_login && login.key_env.is_none());
        // Another endpoint never receives the login, even a local one.
        let local = ProviderSpec::parse("chatgpt=responses,http://127.0.0.1:9/backend-api/codex");
        let local = local.unwrap();
        assert!(!local.chatgpt_login && local.key_env.is_none());
        let keyed = ProviderSpec::parse("chatgpt=responses,https://gw.example.test/v1,GW_KEY");
        assert!(!keyed.unwrap().chatgpt_login);
        assert!(!ProviderSpec::parse("openai").unwrap().chatgpt_login);
    }

    #[test]
    fn the_login_is_the_access_token_and_workspace_only() {
        let dir = std::env::temp_dir().join(format!("agent-codex-login-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{"OPENAI_API_KEY":null,"tokens":{"id_token":"i","access_token":"a","refresh_token":"r","account_id":"w"}}"#,
        )
        .unwrap();
        assert_eq!(chatgpt_login(&path).unwrap(), ("a".into(), "w".into()));
        std::fs::write(&path, r#"{"tokens":{"access_token":"a"}}"#).unwrap();
        let missing = chatgpt_login(&path).unwrap_err();
        assert_eq!(missing.code, "provider_login_unavailable");
        std::fs::remove_dir_all(dir).unwrap();
        let absent = chatgpt_login(&path).unwrap_err();
        assert_eq!(absent.detail, Some(path.display().to_string()));
    }

    #[tokio::test]
    async fn interrupt_reconciles_a_parked_task_before_it_is_reaped() {
        let dir = std::env::temp_dir().join(format!(
            "agent-parked-interrupt-test-{}",
            std::process::id()
        ));
        let (store, _publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let turn = store
            .op("create", |db| {
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
                        tools: &[],
                        created_by: None,
                        created_by_id: None,
                        compaction_instructions: None,
                        compaction_model: None,
                        fallbacks: false,
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
                db.suspend(
                    turn,
                    &call.call_id,
                    &["proc:1".into()],
                    None,
                    false,
                    &[],
                    None,
                )?;
                Ok(turn)
            })
            .await
            .unwrap();
        let (cancel, cancelled) = watch::channel(None);
        let mut service = Service {
            store: store.clone(),
            identity: 0,
            transport: Transport::new(64, 1).unwrap(),
            providers: Arc::new(HashMap::new()),
            registry: Registry::new("wait").unwrap(),
            hub: Hub::default(),
            handles: Handles::new(mpsc::unbounded_channel().0),
            limits: Limits {
                pending: 0,
                pending_bytes: 0,
                processes: 16,
                detached: 16,
                active: 1024,
                connecting: 64,
                connections: 11,
                context_bytes: 8 << 20,
                context_items: 4096,
                note_turns: 48,
                compact_at: 75,
                compact_keep: 25,
            },
            retain_turns: None,
            sessions: 0,
            background_failures: mpsc::unbounded_channel().0,
            limit_active: 1,
            active: HashMap::from([(
                "Bob".into(),
                Active {
                    turn,
                    task: 1,
                    cancel,
                    steers: Arc::default(),
                },
            )]),
            next_task: 1,
            jobs: JoinSet::new(),
            replays: JoinSet::new(),
            retention: JoinSet::new(),
            ready_hint: false,
            tokens: Arc::default(),
            paced: std::collections::BinaryHeap::new(),
            draining: false,
            admissions: std::collections::VecDeque::new(),
            unpublished: std::collections::VecDeque::new(),
            reserved: 0,
            storage_backoff: Duration::ZERO,
            ready_retry_at: None,
        };
        service.jobs.spawn(async move {
            drop(cancelled);
            ("Bob".into(), turn, 1, Ok(turn::Exit::Parked))
        });
        service.active["Bob"].cancel.closed().await;
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
                Bound::default(),
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
                Bound::default(),
            )
            .await
            .unwrap();
        assert_eq!(reply["parked"], true);
        assert!(service.has_capacity());
        assert_eq!(service.jobs.join_next().await.unwrap().unwrap().1, turn);
        let next = store
            .op("inspect", move |db| {
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
        store
            .op("finish", move |db| {
                turn::Finished::record(db, "Bob", next, None, None)
            })
            .await
            .unwrap();
        service
            .complete("Bob".into(), next, 2, Ok(turn::Exit::Finished(None)))
            .await
            .unwrap();
        assert!(service.ready_hint);
        assert!(
            store
                .op("next_ready", |db| db.next_ready())
                .await
                .unwrap()
                .is_some()
        );
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn saturated_dispatch_reconciles_duplicates_but_rejects_new_work() {
        let dir = std::env::temp_dir().join(format!("agent-admission-test-{}", std::process::id()));
        let (store, _publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let binding = || Binding {
            provider: "openai",
            family: Family::Responses,
            model: "synthetic-model",
            instructions: "test",
            reasoning: None,
            budget_tokens: None,
            tools: &[],
            created_by: None,
            created_by_id: None,
            compaction_instructions: None,
            compaction_model: None,
            fallbacks: false,
        };
        let turn = store
            .op("create", move |db| {
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
        )
        .unwrap();
        let (output, writer) = Output::stdout();
        let mut service = Service {
            identity: 0,
            store: store.clone(),
            transport,
            providers: Arc::new(HashMap::from([("openai".to_owned(), provider)])),
            registry,
            hub: Hub::default(),
            handles: Handles::new(mpsc::unbounded_channel().0),
            limits: Limits {
                pending: 0,
                pending_bytes: 0,
                processes: 16,
                detached: 16,
                active: 1024,
                connecting: 64,
                connections: 11,
                context_bytes: 8 << 20,
                context_items: 4096,
                note_turns: 48,
                compact_at: 75,
                compact_keep: 25,
            },
            retain_turns: None,
            sessions: 0,
            background_failures: mpsc::unbounded_channel().0,
            limit_active: 1024,
            active: (0..1024)
                .map(|index| {
                    (
                        index.to_string(),
                        Active {
                            turn,
                            task: 0,
                            cancel: watch::channel(None).0,
                            steers: Arc::default(),
                        },
                    )
                })
                .collect(),
            next_task: 0,
            jobs: JoinSet::new(),
            replays: JoinSet::new(),
            retention: JoinSet::new(),
            ready_hint: false,
            tokens: Arc::default(),
            paced: std::collections::BinaryHeap::new(),
            draining: false,
            admissions: std::collections::VecDeque::new(),
            unpublished: std::collections::VecDeque::new(),
            reserved: 0,
            storage_backoff: Duration::ZERO,
            ready_retry_at: None,
        };
        let duplicate = request(&mut service, submit("Bob", "same"), &output)
            .await
            .unwrap();
        assert_eq!(duplicate["turn"], turn);
        assert_eq!(duplicate["duplicate"], true);
        let error = request(&mut service, submit("Other", "new"), &output)
            .await
            .unwrap_err();
        assert_eq!(error.code, "active_agent_limit");
        assert!(service.jobs.is_empty());
        assert!(
            store
                .op("inspect", |db| Ok(db.inspect("Other")?.head.is_none()))
                .await
                .unwrap()
        );
        drop(output);
        drop(service);
        writer.join().unwrap().unwrap();
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn finished_turns_commit_without_service_reaping() {
        let dir =
            std::env::temp_dir().join(format!("agent-concurrent-finish-{}", std::process::id()));
        let (store, mut publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let names: Vec<String> = (0..8).map(|n| format!("bot{n}")).collect();
        let turns = running(&store, &names).await;
        let mut service = bare_service(&store);
        // Missing providers make the real turn tasks finish without network I/O.
        // Their durable completion must not depend on the service reaping them.
        for (bot, turn) in &turns {
            service.spawn(bot.clone(), *turn, false, false);
        }
        let mut results = Vec::new();
        while let Some(result) = service.jobs.join_next().await {
            results.push(result.unwrap());
        }
        let check = turns.clone();
        store
            .call(move |db| {
                for (bot, turn) in check {
                    assert_eq!(db.turn_status(&bot, turn)?, "failed");
                    assert!(db.inspect(&bot)?.running_turn.is_none());
                }
                Ok(())
            })
            .await
            .unwrap();
        for (bot, turn, task, exit) in results {
            service.complete(bot, turn, task, exit).await.unwrap();
        }
        assert!(service.active.is_empty());
        let mut terminal = 0;
        while let Ok(publication) = publications.try_recv() {
            if let Publication::Event(entry) = publication {
                terminal += usize::from(entry["event"] == "turn_finished");
            }
        }
        assert_eq!(terminal, turns.len(), "one terminal event per turn");
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn submit(bot: &str, request_id: &str) -> Command {
        Command::Submit {
            bot: bot.into(),
            bot_id: None,
            request_id: request_id.into(),
            prompt: "work".into(),
            workspace: None,
            model: None,
            delivery: None,
            expected_turn: None,
        }
    }
    /// Take requests the way the run loop does: an admission with room joins
    /// the queue, anything else first waits for those queued. Returns every
    /// answer, in the order the loop would send them, with each request's
    /// index as its id.
    async fn requests(
        service: &mut Service,
        commands: Vec<Command>,
        output: &Output,
    ) -> Vec<(Value, Result<Value>)> {
        let mut answers = Vec::new();
        for (index, command) in commands.into_iter().enumerate() {
            let bound = service.admission_sends(&command, &json!(index), 0);
            while service.must_settle(Some(&command), 0, output, bound) {
                let (_, _, id, result) = service.settle().await;
                answers.push((id, result));
            }
            let admission = matches!(command, Command::Create { .. } | Command::Submit { .. });
            let result = service
                .dispatch(command, 0, output, json!(index), bound)
                .await;
            if result.as_ref().is_err_and(|e| e.code == "deferred") {
                continue;
            }
            if admission && !service.admissions.is_empty() {
                service.admissions.push_back(Admission {
                    session: 0,
                    output: output.clone(),
                    id: json!(index),
                    bound,
                    pending: Pending::Settled(result),
                });
                continue;
            }
            answers.push((json!(index), result));
        }
        while !service.admissions.is_empty() {
            let (_, _, id, result) = service.settle().await;
            answers.push((id, result));
        }
        answers
    }
    async fn request(service: &mut Service, command: Command, output: &Output) -> Result<Value> {
        requests(service, vec![command], output)
            .await
            .pop()
            .unwrap()
            .1
    }
    /// Bots with one running turn each, bound to a provider the test service
    /// does not have, so their tasks finish at once without network I/O.
    async fn running(store: &Store, names: &[String]) -> Vec<(String, i64)> {
        let names = names.to_vec();
        store
            .call(move |db| {
                names
                    .iter()
                    .map(|bot| {
                        db.create(
                            bot,
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
                                created_by_id: None,
                                compaction_instructions: None,
                                compaction_model: None,
                                fallbacks: false,
                            },
                        )?;
                        let turn = db
                            .begin(
                                bot,
                                "first",
                                "work",
                                true,
                                &TurnOptions::default(),
                                |_, _| Ok(()),
                            )?
                            .turn;
                        Ok((bot.clone(), turn))
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .await
            .unwrap()
    }
    fn bare_service(store: &Store) -> Service {
        Service {
            identity: 0,
            store: store.clone(),
            transport: Transport::new(64, 1).unwrap(),
            providers: Arc::new(HashMap::new()),
            registry: Registry::new("echo").unwrap(),
            hub: Hub::default(),
            handles: Handles::new(mpsc::unbounded_channel().0),
            limits: Limits {
                pending: 0,
                pending_bytes: 0,
                processes: 16,
                detached: 16,
                active: 1024,
                connecting: 64,
                connections: 11,
                context_bytes: 8 << 20,
                context_items: 4096,
                note_turns: 48,
                compact_at: 75,
                compact_keep: 25,
            },
            retain_turns: None,
            sessions: 0,
            background_failures: mpsc::unbounded_channel().0,
            limit_active: 1024,
            active: HashMap::new(),
            next_task: 0,
            jobs: JoinSet::new(),
            replays: JoinSet::new(),
            retention: JoinSet::new(),
            ready_hint: false,
            tokens: Arc::default(),
            paced: std::collections::BinaryHeap::new(),
            draining: false,
            admissions: std::collections::VecDeque::new(),
            unpublished: std::collections::VecDeque::new(),
            reserved: 0,
            storage_backoff: Duration::ZERO,
            ready_retry_at: None,
        }
    }
    /// Refuse Bob's completion the way a full disk refuses a commit: the
    /// finish job fails and leaves nothing durable. Another connection
    /// installs it, as an operator's disk would, between the worker's groups.
    fn refuse_bobs_finish(path: &Path, refuse: bool) {
        rusqlite::Connection::open(path)
            .unwrap()
            .execute_batch(if refuse {
                "CREATE TRIGGER refuse_finish BEFORE UPDATE OF status ON turns
                 WHEN NEW.bot='Bob' AND NEW.status!='running'
                 BEGIN SELECT RAISE(ABORT,'refused'); END;"
            } else {
                "DROP TRIGGER refuse_finish;"
            })
            .unwrap();
    }
    async fn finish_errors(store: &Store, at_least: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while store.stats()["operations"]["finish"]["storage_errors"]
            .as_u64()
            .unwrap_or(0)
            < at_least
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "finish never failed"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn a_finish_that_cannot_commit_retries_while_other_bots_go_on() {
        let dir = std::env::temp_dir().join(format!("agent-finish-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("state.sqlite");
        let (store, mut publications) = Store::open(&path).await.unwrap();
        let turns = running(&store, &["Bob".into(), "Alice".into()]).await;
        refuse_bobs_finish(&path, true);
        let mut service = bare_service(&store);
        for (bot, turn) in &turns {
            service.spawn(bot.clone(), *turn, false, false);
        }
        // Alice finishes while Bob's completion keeps failing; nothing ends
        // the daemon, and Bob stays durably busy.
        let (bot, turn, task, exit) = service.jobs.join_next().await.unwrap().unwrap();
        assert_eq!(bot, "Alice");
        service.complete(bot, turn, task, exit).await.unwrap();
        finish_errors(&store, 3).await;
        assert!(
            service.jobs.try_join_next().is_none(),
            "Bob is still retrying"
        );
        let bob = turns[0].1;
        let status = store
            .call(move |db| db.turn_status("Bob", bob))
            .await
            .unwrap();
        assert_eq!(status, "running");
        // Once the store takes it, the same completion commits.
        refuse_bobs_finish(&path, false);
        let (bot, turn, task, exit) = service.jobs.join_next().await.unwrap().unwrap();
        assert_eq!((bot.as_str(), turn), ("Bob", bob));
        service.complete(bot, turn, task, exit).await.unwrap();
        assert!(service.active.is_empty());
        let status = store
            .call(move |db| db.turn_status("Bob", bob))
            .await
            .unwrap();
        assert_eq!(status, "failed", "the provider was missing, as recorded");
        let mut terminal = Vec::new();
        while let Ok(publication) = publications.try_recv() {
            if let Publication::Event(entry) = publication
                && entry["event"] == "turn_finished"
            {
                terminal.push(entry["bot"].as_str().unwrap().to_owned());
            }
        }
        assert_eq!(
            terminal,
            ["Alice", "Bob"],
            "one terminal event each, in commit order"
        );
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_completion_whose_retention_fails_leaves_nothing_and_retries() {
        let dir = scratch("finish-prune");
        let path = dir.join("state.sqlite");
        let (store, _publications) = Store::open(&path).await.unwrap();
        let first = running(&store, &["Bob".into()]).await[0].1;
        let last = store
            .call(move |db| {
                db.finish(first, None)?;
                let next = |db: &mut agent_runtime::store::Database, request: &str| {
                    db.begin(
                        "Bob",
                        request,
                        "work",
                        true,
                        &TurnOptions::default(),
                        |_, _| Ok(()),
                    )
                    .map(|started| started.turn)
                };
                let second = next(db, "second")?;
                db.finish(second, None)?;
                next(db, "third")
            })
            .await
            .unwrap();
        // The completion commits, then its retention is refused: Bob's
        // older events cannot be deleted.
        let refuse = |on: bool| {
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute_batch(if on {
                    "CREATE TRIGGER refuse_prune BEFORE DELETE ON events WHEN OLD.bot='Bob'
                     BEGIN SELECT RAISE(ABORT,'refused'); END;"
                } else {
                    "DROP TRIGGER refuse_prune;"
                })
                .unwrap()
        };
        refuse(true);
        let mut service = bare_service(&store);
        service.retain_turns = Some(1);
        service.spawn("Bob".into(), last, false, false);
        finish_errors(&store, 2).await;
        let status = || {
            let store = store.clone();
            async move { store.call(move |db| db.turn_status("Bob", last)).await }
        };
        assert_eq!(
            status().await.unwrap(),
            "running",
            "the refused retention took the finish back with it"
        );
        refuse(false);
        let (bot, turn, task, exit) = service.jobs.join_next().await.unwrap().unwrap();
        service.complete(bot, turn, task, exit).await.unwrap();
        assert_eq!(status().await.unwrap(), "failed");
        let kept: i64 = rusqlite::Connection::open(&path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM events WHERE turn=?", [first], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(kept, 0, "retention ran with the completion");
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn an_interrupt_during_a_finish_retry_ends_the_turn_interrupted() {
        let dir = scratch("finish-interrupt");
        let path = dir.join("state.sqlite");
        let (store, _publications) = Store::open(&path).await.unwrap();
        let bob = running(&store, &["Bob".into()]).await[0].1;
        refuse_bobs_finish(&path, true);
        let mut service = bare_service(&store);
        service.spawn("Bob".into(), bob, false, false);
        finish_errors(&store, 2).await;
        assert!(
            service.active["Bob"].cancel(turn::INTERRUPTED),
            "acknowledged"
        );
        // An attempt already under way may not see it; the one after does.
        let errors = store.stats()["operations"]["finish"]["storage_errors"]
            .as_u64()
            .unwrap();
        finish_errors(&store, errors + 2).await;
        refuse_bobs_finish(&path, false);
        let (bot, turn, task, exit) = service.jobs.join_next().await.unwrap().unwrap();
        service.complete(bot, turn, task, exit).await.unwrap();
        let status = store
            .call(move |db| db.turn_status("Bob", bob))
            .await
            .unwrap();
        assert_eq!(status, "interrupted", "not the failure it was retrying");
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Lose every group that changes one of Bob's turns' status, the way a
    /// full disk loses a commit: a dangling deferred reference fails COMMIT.
    fn lose_bobs_status_changes(path: &Path, lose: bool) {
        rusqlite::Connection::open(path)
            .unwrap()
            .execute_batch(if lose {
                "CREATE TABLE IF NOT EXISTS parent(id INTEGER PRIMARY KEY);
                 CREATE TABLE IF NOT EXISTS child(parent INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);
                 CREATE TRIGGER lose_status AFTER UPDATE OF status ON turns WHEN NEW.bot='Bob'
                 BEGIN INSERT INTO child VALUES (1); END;"
            } else {
                "DROP TRIGGER lose_status;"
            })
            .unwrap();
    }

    #[tokio::test]
    async fn a_ready_turn_the_store_cannot_start_stays_ready() {
        let dir = scratch("ready-refused");
        let path = dir.join("state.sqlite");
        let (store, _publications) = Store::open(&path).await.unwrap();
        let bob = store
            .call(|db| {
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
                        created_by_id: None,
                        compaction_instructions: None,
                        compaction_model: None,
                        fallbacks: false,
                    },
                )?;
                // No slot was free when it was accepted, so it queued.
                db.begin(
                    "Bob",
                    "first",
                    "work",
                    false,
                    &TurnOptions {
                        delivery: Delivery::Queue,
                        ..TurnOptions::default()
                    },
                    |_, _| Ok(()),
                )
                .map(|started| started.turn)
            })
            .await
            .unwrap();
        let status = || {
            let store = store.clone();
            async move {
                store
                    .call(move |db| db.turn_status("Bob", bob))
                    .await
                    .unwrap()
            }
        };
        assert_eq!(status().await, "ready");
        lose_bobs_status_changes(&path, true);
        let mut service = admitting(&store, 1024);
        service.ready_hint = true;
        service.dispatch_ready().await.unwrap();
        assert!(service.active.is_empty());
        assert_eq!(status().await, "ready", "neither started nor ended");
        assert!(service.ready_retry_at.is_some(), "tried again later");
        // Once the store takes writes again, the retry starts it.
        lose_bobs_status_changes(&path, false);
        service.ready_retry_at = None;
        service.dispatch_ready().await.unwrap();
        assert!(service.active.contains_key("Bob"));
        assert_eq!(service.storage_backoff, Duration::ZERO);
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_wake_up_the_store_loses_comes_due_again() {
        let dir = scratch("resume-refused");
        let path = dir.join("state.sqlite");
        let (store, _publications) = Store::open(&path).await.unwrap();
        let bob = running(&store, &["Bob".into()]).await[0].1;
        // A creation that loses its group shares it with the wake-up's check.
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE parent(id INTEGER PRIMARY KEY);
                 CREATE TABLE child(parent INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);
                 CREATE TRIGGER lose_zed AFTER INSERT ON bots WHEN NEW.name='Zed'
                 BEGIN INSERT INTO child VALUES (1); END;",
            )
            .unwrap();
        let mut service = bare_service(&store);
        let (held, gate) = hold(&store).await;
        let workspace = dir.clone();
        let lost = store
            .queue("create", move |db| {
                db.create(
                    "Zed",
                    Some(workspace.to_str().unwrap()),
                    Binding {
                        provider: "openai",
                        family: Family::Responses,
                        model: "synthetic",
                        instructions: "",
                        reasoning: None,
                        budget_tokens: None,
                        tools: &[],
                        created_by: None,
                        created_by_id: None,
                        compaction_instructions: None,
                        compaction_model: None,
                        fallbacks: false,
                    },
                )
                .map(|_| ())
            })
            .await
            .unwrap();
        let (resumed, ()) = tokio::join!(service.resume("Bob".into(), bob), async {
            gate.send(()).unwrap();
        });
        resumed.unwrap();
        assert_eq!(held.await.unwrap_err().code, "storage_error");
        assert_eq!(lost.await.unwrap_err().code, "storage_error");
        assert!(service.active.is_empty());
        let std::cmp::Reverse((at, bot, turn)) = service.paced.peek().unwrap();
        assert_eq!((bot.as_str(), *turn), ("Bob", bob), "due again");
        assert!(*at > now_ms() - 1000);
        assert!(service.storage_backoff > Duration::ZERO);
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_refused_interrupt_leaves_the_parked_turn_waiting_on_its_handle() {
        let dir = scratch("parked-interrupt-refused");
        let path = dir.join("state.sqlite");
        let (store, _publications) = Store::open(&path).await.unwrap();
        let turns = running(&store, &["Bob".into(), "Zed".into()]).await;
        let (bob, zed) = (turns[0].1, turns[1].1);
        let handle = handles::Handle::Turn {
            bot: "Zed".into(),
            turn: zed,
        }
        .to_string();
        let parked = handle.clone();
        store
            .call(move |db| {
                let call = agent_runtime::provider::ToolCall {
                    name: "wait".into(),
                    call_id: "wait-1".into(),
                    arguments: json!({"handles":[parked]}).to_string(),
                };
                let item = serde_json::to_vec(&json!({"type":"function_call","name":call.name,
                "call_id":call.call_id,"arguments":call.arguments}))?
                .into();
                db.append(bob, vec![item], std::slice::from_ref(&call), None)?;
                db.tool_start(bob, &call)?;
                db.suspend(bob, &call.call_id, &[parked], None, false, &[], None)
                    .map(|_| ())
            })
            .await
            .unwrap();
        let (resume, mut resumed) = mpsc::unbounded_channel();
        let mut service = bare_service(&store);
        service.handles = Handles::new(resume);
        service
            .handles
            .attach(
                &store,
                Waiter::Turn(bob),
                &[handle],
                None,
                false,
                Completion::Resume {
                    bot: "Bob".into(),
                    turn: bob,
                },
            )
            .await;
        refuse_bobs_finish(&path, true);
        let output = Output::writer(tokio::io::sink());
        let refused = service
            .dispatch(
                Command::Interrupt {
                    bot: "Bob".into(),
                    turn: bob,
                },
                0,
                &output,
                Value::Null,
                Bound::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(refused.code, "storage_error");
        let status = store
            .call(move |db| db.turn_status("Bob", bob))
            .await
            .unwrap();
        assert_eq!(status, "waiting");
        // The awaited turn ends: its wait still wakes the parked turn.
        service
            .handles
            .turn_finished("Zed", zed, json!({"status":"completed"}));
        assert_eq!(resumed.try_recv().ok(), Some(("Bob".into(), bob)));
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_stdout_holds_up_shutdown_once_not_per_queued_reply() {
        let dir = scratch("stalled-drain");
        let (store, _publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let mut service = bare_service(&store);
        // A consumer that never reads: its queue fills and stays full.
        let (stalled, _unread) = tokio::io::duplex(1);
        let output = Output::writer(stalled);
        for _ in 0..3 {
            while output.try_send(json!({})).is_ok() {}
            tokio::task::yield_now().await;
        }
        for id in 0..4 {
            service.admissions.push_back(Admission {
                session: 0,
                output: output.clone(),
                id: json!(id),
                bound: Bound::default(),
                pending: Pending::Settled(Ok(json!({}))),
            });
        }
        let started = tokio::time::Instant::now();
        answer_queued(&mut service, &mut HashMap::new(), true).await;
        assert!(service.admissions.is_empty());
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(10),
            "one reply's timeout, not one per admission: {waited:?}"
        );
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_resume_the_store_refuses_leaves_the_turn_parked() {
        let dir = scratch("resume-write-refused");
        let path = dir.join("state.sqlite");
        let (store, _publications) = Store::open(&path).await.unwrap();
        let bob = running(&store, &["Bob".into()]).await[0].1;
        store
            .call(move |db| {
                db.suspend_paced(bob, 0, 0, 0, 0, 0, false, None)
                    .map(|_| ())
            })
            .await
            .unwrap();
        // The wake-up's check only reads, so it passes; the resume's own
        // write is what the store refuses.
        lose_bobs_status_changes(&path, true);
        let mut service = admitting(&store, 1024);
        service.resume("Bob".into(), bob).await.unwrap();
        assert!(service.active.contains_key("Bob"), "the check passed");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while store.stats()["operations"]["resume"]["storage_errors"]
            .as_u64()
            .unwrap_or(0)
            == 0
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "resume never failed"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        lose_bobs_status_changes(&path, false);
        let (bot, turn, task, exit) = service.jobs.join_next().await.unwrap().unwrap();
        service.complete(bot, turn, task, exit).await.unwrap();
        let status = store
            .call(move |db| db.turn_status("Bob", bob))
            .await
            .unwrap();
        assert_eq!(status, "paced", "still parked, not ended");
        assert!(service.active.is_empty());
        let std::cmp::Reverse((_, bot, turn)) = service.paced.peek().unwrap();
        assert_eq!((bot.as_str(), *turn), ("Bob", bob), "due again");
        assert!(service.storage_backoff > Duration::ZERO);
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn shutdown_stops_a_finish_retry_and_leaves_the_turn_for_recovery() {
        let dir = std::env::temp_dir().join(format!("agent-finish-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("state.sqlite");
        let (store, _publications) = Store::open(&path).await.unwrap();
        let turns = running(&store, &["Bob".into()]).await;
        refuse_bobs_finish(&path, true);
        let mut service = bare_service(&store);
        let bob = turns[0].1;
        service.spawn("Bob".into(), bob, false, false);
        // An interrupt that lands first keeps its cause; shutdown releasing
        // the slot still stops the retries.
        assert!(service.active["Bob"].cancel(turn::INTERRUPTED));
        finish_errors(&store, 2).await;
        for active in std::mem::take(&mut service.active).into_values() {
            active.cancel(turn::SHUTDOWN);
        }
        let (bot, turn, task, exit) = service.jobs.join_next().await.unwrap().unwrap();
        let error = service.complete(bot, turn, task, exit).await.unwrap_err();
        assert_eq!(error.code, "storage_error");
        let status = store
            .call(move |db| db.turn_status("Bob", bob))
            .await
            .unwrap();
        assert_eq!(status, "running", "the next start ends it as interrupted");
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A service whose bots may start turns, bound to `openai`, whose
    /// provider refuses connections: a started turn fails without traffic.
    fn admitting(store: &Store, limit_active: usize) -> Service {
        let mut service = bare_service(store);
        let provider = Provider::new(
            service.transport.clone(),
            Family::Responses,
            "http://127.0.0.1:1/v1",
            None,
        )
        .unwrap();
        service.providers = Arc::new(HashMap::from([("openai".to_owned(), provider)]));
        service.limit_active = limit_active;
        service
    }
    fn create(bot: &str, workspace: &Path) -> Command {
        Command::Create {
            bot: bot.into(),
            workspace: Some(workspace.to_str().unwrap().into()),
            model: Some("openai/synthetic".into()),
            instructions: Some(String::new()),
            reasoning: None,
            budget_tokens: None,
            tools: Some(Vec::new()),
            created_by: None,
            created_by_id: None,
            compaction_instructions: None,
            compaction_model: None,
            fallbacks: false,
        }
    }
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    /// Hold the storage worker inside a job until the gate is fed, so the
    /// jobs queued meanwhile join its group.
    async fn hold(store: &Store) -> (Answer<()>, std::sync::mpsc::Sender<()>) {
        let (gate, wait) = std::sync::mpsc::channel::<()>();
        let held = store
            .queue("hold", move |_| {
                let _ = wait.recv();
                Ok(())
            })
            .await
            .unwrap();
        (held, gate)
    }

    #[tokio::test]
    async fn admissions_queued_together_share_one_commit_and_answer_in_order() {
        let dir = scratch("admit-group");
        let (store, _publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let mut service = admitting(&store, 1024);
        let (output, _writer) = Output::stdout();
        let groups = |store: &Store| store.stats()["groups"]["count"].as_u64().unwrap();
        let (held, gate) = hold(&store).await;
        let before = groups(&store);
        let bots = ["A", "B", "C", "D"];
        let mut commands: Vec<Command> = bots.iter().map(|bot| create(bot, &dir)).collect();
        commands.extend(bots.iter().map(|bot| submit(bot, "r1")));
        // A retry of an admission still queued, and new work for a bot whose
        // turn is still queued, see the writes queued before them.
        commands.push(submit("A", "r1"));
        commands.push(submit("A", "r2"));
        let count = commands.len();
        for (index, command) in commands.into_iter().enumerate() {
            let bytes = service.admission_sends(&command, &json!(index), 0);
            assert!(
                !service.must_settle(Some(&command), 0, &output, bytes),
                "the window has room"
            );
            let deferred = service
                .dispatch(command, 0, &output, json!(index), bytes)
                .await;
            assert_eq!(deferred.unwrap_err().code, "deferred");
        }
        assert_eq!(service.admissions.len(), count);
        assert_eq!(service.reserved, 6, "every submission holds a slot");
        assert!(
            service.must_settle(
                Some(&Command::Interrupt {
                    bot: "A".into(),
                    turn: 1
                }),
                0,
                &output,
                Bound::default()
            ),
            "anything else waits for queued admissions"
        );
        gate.send(()).unwrap();
        held.await.unwrap();
        let mut answers = Vec::new();
        while !service.admissions.is_empty() {
            let (_, _, id, result) = service.settle().await;
            answers.push((id, result));
        }
        assert_eq!(groups(&store) - before, 1, "one commit for all of them");
        assert_eq!(
            answers.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            (0..count).map(|index| json!(index)).collect::<Vec<_>>(),
            "answered in request order"
        );
        for (_, created) in &answers[..4] {
            assert!(created.is_ok());
        }
        let turns: Vec<_> = answers[4..8]
            .iter()
            .map(|(_, started)| {
                let started = started.as_ref().unwrap();
                assert_eq!(started["status"], "running");
                assert_eq!(started["duplicate"], false);
                started["turn"].clone()
            })
            .collect();
        let retry = answers[8].1.as_ref().unwrap();
        assert_eq!(retry["duplicate"], true);
        assert_eq!(retry["turn"], turns[0]);
        assert_eq!(answers[9].1.as_ref().unwrap_err().code, "bot_busy");
        assert_eq!(
            service.active.len(),
            4,
            "one turn started per fresh submission"
        );
        assert_eq!(service.reserved, 0);
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_steer_queued_behind_its_turns_start_arms_the_first_round() {
        let dir = scratch("admit-steer");
        let (store, _publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let mut service = admitting(&store, 1024);
        let (output, _writer) = Output::stdout();
        let (held, gate) = hold(&store).await;
        let mut steer = submit("A", "r2");
        if let Command::Submit { delivery, .. } = &mut steer {
            *delivery = Some("steer".into());
        }
        for (index, command) in [create("A", &dir), submit("A", "r1"), steer]
            .into_iter()
            .enumerate()
        {
            let bytes = service.admission_sends(&command, &json!(index), 0);
            let deferred = service
                .dispatch(command, 0, &output, json!(index), bytes)
                .await;
            assert_eq!(deferred.unwrap_err().code, "deferred");
        }
        gate.send(()).unwrap();
        held.await.unwrap();
        assert!(service.settle().await.3.is_ok(), "created");
        let started = service.settle().await.3.unwrap();
        assert_eq!(started["status"], "running");
        // Nothing has run since the spawn: the turn has not absorbed yet.
        assert!(
            service.active["A"]
                .steers
                .load(std::sync::atomic::Ordering::Relaxed),
            "the steer committed with it is waiting at the first boundary"
        );
        let steered = service.settle().await.3.unwrap();
        assert_eq!(
            (steered["status"].as_str(), steered["duplicate"].as_bool()),
            (Some("queued"), Some(false))
        );
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_session_queues_no_more_admissions_than_its_output_can_take() {
        let dir = scratch("admit-room");
        let (store, _publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let mut service = admitting(&store, 1024);
        // A client that has read nothing yet: its queue holds every reply.
        let (near, _far) = tokio::io::duplex(64);
        let output = Output::writer(near);
        let large = |bot: String| {
            let mut command = create(&bot, &dir);
            if let Command::Create {
                instructions,
                compaction_instructions,
                ..
            } = &mut command
            {
                *instructions = Some("i".repeat(64 * 1024));
                *compaction_instructions = Some("c".repeat(64 * 1024));
            }
            command
        };
        let mut queued = 0;
        while queued < ADMISSION_WINDOW {
            let command = large(format!("B{queued}"));
            let bytes = service.admission_sends(&command, &json!(queued), 1);
            if service.must_settle(Some(&command), 1, &output, bytes) {
                break;
            }
            let deferred = service
                .dispatch(command, 1, &output, json!(queued), bytes)
                .await;
            assert_eq!(deferred.unwrap_err().code, "deferred");
            queued += 1;
        }
        assert!(queued > 1, "replies that fit share the window");
        assert!(queued < ADMISSION_WINDOW, "the session's queue fills first");
        let elsewhere = Output::writer(tokio::io::sink());
        let command = large("Elsewhere".into());
        let bytes = service.admission_sends(&command, &json!(0), 2);
        assert!(
            !service.must_settle(Some(&command), 2, &elsewhere, bytes),
            "another session's admission still joins"
        );
        // Sent back to back, as the run loop sends them, every reply fits.
        while !service.admissions.is_empty() {
            let (_, output, id, result) = service.settle().await;
            output.try_respond(id, Ok(result.unwrap())).unwrap();
        }
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_session_following_a_bot_two_ways_has_room_for_both_events() {
        use tokio::io::AsyncBufReadExt;
        let dir = scratch("admit-follows");
        let (store, publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let mut service = admitting(&store, 1024);
        let publisher = tokio::spawn(publish(
            publications,
            service.hub.clone(),
            service.handles.clone(),
        ));
        let sink = Output::writer(tokio::io::sink());
        let names: Vec<String> = (0..ADMISSION_WINDOW).map(|n| format!("B{n}")).collect();
        let creates = || names.iter().map(|name| create(name, &dir)).collect();
        for (_, created) in requests(&mut service, creates(), &sink).await {
            created.unwrap();
        }
        // The session follows every bot, and each of these by name. Its
        // client reads until a marker, then stops, as a slow client would.
        let (near, far) = tokio::io::duplex(64);
        let output = Output::writer(near);
        let reader = tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(far).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                if line.contains("marker") {
                    break;
                }
            }
            lines
        });
        for name in std::iter::once(hub::ALL).chain(names.iter().map(String::as_str)) {
            let sub = service.hub.subscribe(name, 1, output.clone(), 0);
            replay(store.clone(), service.hub.clone(), name.to_owned(), sub)
                .await
                .unwrap();
        }
        // Deleting a bot keeps its followers for a bot of the same name.
        let deletes = names
            .iter()
            .map(|bot| Command::Delete { bot: bot.clone() })
            .collect();
        for (_, deleted) in requests(&mut service, deletes, &sink).await {
            deleted.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        output.try_send(json!({"marker": true})).unwrap();
        let _stalled = reader.await.unwrap();
        let mut queued = 0;
        while queued < names.len() {
            let command = create(&names[queued], &dir);
            let bound = service.admission_sends(&command, &json!(queued), 1);
            if service.must_settle(Some(&command), 1, &output, bound) {
                break;
            }
            let deferred = service
                .dispatch(command, 1, &output, json!(queued), bound)
                .await;
            assert_eq!(deferred.unwrap_err().code, "deferred");
            queued += 1;
        }
        assert!(queued > 1 && queued < names.len(), "{queued} queued");
        while !service.admissions.is_empty() {
            let (_, output, id, result) = service.settle().await;
            output.try_respond(id, Ok(result.unwrap())).unwrap();
        }
        // Both copies of each creation's event reach the queue.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !*output.subscribe_closed().borrow(),
            "{queued} replies and their events fit"
        );
        publisher.abort();
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn answered_admissions_keep_room_for_events_not_yet_published() {
        use tokio::io::AsyncBufReadExt;
        let dir = scratch("admit-unpublished");
        let (store, publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let mut service = admitting(&store, 1024);
        // The session follows every bot. Its client reads until a marker,
        // then stops, as a slow client would.
        let (near, far) = tokio::io::duplex(64);
        let output = Output::writer(near);
        let reader = tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(far).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                if line.contains("marker") {
                    break;
                }
            }
            lines
        });
        let sub = service.hub.subscribe(hub::ALL, 1, output.clone(), 0);
        replay(store.clone(), service.hub.clone(), hub::ALL.to_owned(), sub)
            .await
            .unwrap();
        output.try_send(json!({"marker": true})).unwrap();
        let _stalled = reader.await.unwrap();
        // Two windows are answered before the publisher runs: the second
        // must leave room for the first one's events.
        let names: Vec<String> = (0..2 * ADMISSION_WINDOW).map(|n| format!("B{n}")).collect();
        let mut queued = 0;
        let mut windows = Vec::new();
        for most in [ADMISSION_WINDOW / 2, ADMISSION_WINDOW] {
            let first = queued;
            while queued < names.len() && queued - first < most {
                let command = create(&names[queued], &dir);
                let bound = service.admission_sends(&command, &json!(queued), 1);
                if service.must_settle(Some(&command), 1, &output, bound) {
                    break;
                }
                let deferred = service
                    .dispatch(command, 1, &output, json!(queued), bound)
                    .await;
                assert_eq!(deferred.unwrap_err().code, "deferred");
                queued += 1;
            }
            windows.push(queued - first);
            while !service.admissions.is_empty() {
                let (_, output, id, result) = service.settle().await;
                output.try_respond(id, Ok(result.unwrap())).unwrap();
            }
        }
        let publisher = tokio::spawn(publish(
            publications,
            service.hub.clone(),
            service.handles.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !*output.subscribe_closed().borrow(),
            "windows of {windows:?} fit with their events"
        );
        assert!(windows[1] > 1, "{windows:?}");
        // Published events no longer hold room.
        assert_eq!(service.hub.published(), queued as i64);
        let (_, output, id, result) = {
            let command = create("Late", &dir);
            let bound = service.admission_sends(&command, &json!("late"), 1);
            let deferred = service
                .dispatch(command, 1, &output, json!("late"), bound)
                .await;
            assert_eq!(deferred.unwrap_err().code, "deferred");
            service.settle().await
        };
        output.try_respond(id, Ok(result.unwrap())).unwrap();
        assert_eq!(service.unpublished.len(), 1, "only the late event waits");
        publisher.abort();
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn an_admission_after_a_window_waits_for_that_windows_events() {
        use tokio::io::AsyncBufReadExt;
        let dir = scratch("admit-after-window");
        let (store, publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let mut service = admitting(&store, 1024);
        let (near, far) = tokio::io::duplex(64);
        let output = Output::writer(near);
        let reader = tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(far).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                if line.contains("marker") {
                    break;
                }
            }
            lines
        });
        let sub = service.hub.subscribe(hub::ALL, 1, output.clone(), 0);
        replay(store.clone(), service.hub.clone(), hub::ALL.to_owned(), sub)
            .await
            .unwrap();
        output.try_send(json!({"marker": true})).unwrap();
        let _stalled = reader.await.unwrap();
        // A window is answered before the publisher runs.
        let window = ADMISSION_WINDOW / 2;
        for n in 0..window {
            let command = create(&format!("B{n}"), &dir);
            let bound = service.admission_sends(&command, &json!(n), 1);
            assert!(!service.must_settle(Some(&command), 1, &output, bound));
            let deferred = service.dispatch(command, 1, &output, json!(n), bound).await;
            assert_eq!(deferred.unwrap_err().code, "deferred");
        }
        while !service.admissions.is_empty() {
            let (_, output, id, result) = service.settle().await;
            output.try_respond(id, Ok(result.unwrap())).unwrap();
        }
        // Other traffic leaves exactly the room those events need.
        let next = create("Next", &dir);
        let bound = service.admission_sends(&next, &json!("next"), 1);
        assert_eq!(
            service.awaits_publication(Some(&next), 1, &output, bound),
            None,
            "the window's events and the next admission fit"
        );
        while output.room().1 > window {
            output.try_send(json!({"filler": true})).unwrap();
        }
        let newest = service.awaits_publication(Some(&next), 1, &output, bound);
        assert_eq!(newest, Some(window as i64), "the next admission waits");
        assert_eq!(
            service.awaits_publication(Some(&Command::Stats), 1, &output, Bound::default()),
            None,
            "only admissions wait"
        );
        let publisher = tokio::spawn(publish(
            publications,
            service.hub.clone(),
            service.handles.clone(),
        ));
        tokio::time::timeout(
            Duration::from_secs(1),
            service.hub.published_through(newest.unwrap()),
        )
        .await
        .unwrap();
        assert!(!*output.subscribe_closed().borrow(), "the events fit");
        assert_eq!(
            service.awaits_publication(Some(&next), 1, &output, bound),
            None
        );
        publisher.abort();
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn admission_bounds_cover_a_reply_and_its_event() {
        let dir = scratch("admission-bound");
        let (store, mut publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let mut service = admitting(&store, 1024);
        let output = Output::writer(tokio::io::sink());
        // Every character here is escaped in JSON, or longer than one byte.
        let awkward = "\"\\\u{1}\n/é".repeat(1000);
        let id = json!("\u{1}".repeat(128));
        let mut create = create("Bot", &dir);
        if let Command::Create {
            model,
            instructions,
            reasoning,
            compaction_instructions,
            compaction_model,
            ..
        } = &mut create
        {
            *model = Some(format!("openai/{}", "\u{1}".repeat(256)));
            *instructions = Some(awkward.clone());
            *reasoning = Some("high".into());
            *compaction_instructions = Some(awkward);
            *compaction_model = Some("openai/synthetic".into());
        }
        for command in [create, submit("Bot", "r1")] {
            let bound = admission_bound(&command, &id, 1);
            assert!(!service.must_settle(Some(&command), 0, &output, bound));
            let deferred = service
                .dispatch(command, 0, &output, id.clone(), bound)
                .await;
            assert_eq!(deferred.unwrap_err().code, "deferred");
            let (_, _, _, result) = service.settle().await;
            let reply = json!({"id": id, "result": result.unwrap()});
            let event = loop {
                if let Publication::Event(entry) = publications.recv().await.unwrap()
                    && matches!(
                        entry["event"].as_str(),
                        Some("created" | "accepted" | "queued")
                    )
                {
                    break entry;
                }
            };
            // Each is a line: its JSON and a newline.
            let replied = output::encoded_len(&reply).unwrap() + 1;
            let published = output::encoded_len(&event).unwrap() + 1;
            assert!(
                replied <= bound.reply.bytes && published <= bound.events.bytes,
                "{replied} and {published} bytes sent, {} and {} promised",
                bound.reply.bytes,
                bound.events.bytes
            );
            assert_eq!((bound.reply.packets, bound.events.packets), (1, 1));
        }
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn admissions_at_the_limit_answer_as_one_at_a_time_would() {
        let dir = scratch("admit-limit");
        let (store, _publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let mut service = admitting(&store, 2);
        let (output, _writer) = Output::stdout();
        let created = requests(
            &mut service,
            ["A", "B", "C"]
                .iter()
                .map(|bot| create(bot, &dir))
                .collect(),
            &output,
        )
        .await;
        assert!(created.iter().all(|(_, result)| result.is_ok()));
        // Two slots: the retry promised one it does not take, so B still
        // starts, and only C finds the daemon full.
        let answers = requests(
            &mut service,
            vec![
                submit("A", "r1"),
                submit("A", "r1"),
                submit("B", "r1"),
                submit("C", "r1"),
            ],
            &output,
        )
        .await;
        let results: Vec<_> = answers
            .iter()
            .map(|(_, result)| match result {
                Ok(started) => format!("{} {}", started["status"], started["duplicate"]),
                Err(error) => error.code.clone(),
            })
            .collect();
        assert_eq!(
            results,
            [
                "\"running\" false",
                "\"running\" true",
                "\"running\" false",
                "active_agent_limit"
            ]
        );
        let mut active: Vec<_> = service.active.keys().cloned().collect();
        active.sort();
        assert_eq!(active, ["A", "B"]);
        assert_eq!(service.reserved, 0);
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_lost_group_commit_fails_its_admissions_and_starts_nothing() {
        let dir = scratch("admit-lost");
        let path = dir.join("state.sqlite");
        let (store, _publications) = Store::open(&path).await.unwrap();
        let mut service = admitting(&store, 1024);
        let (output, _writer) = Output::stdout();
        let created = requests(
            &mut service,
            vec![create("Alice", &dir), create("Bob", &dir)],
            &output,
        )
        .await;
        assert!(created.iter().all(|(_, result)| result.is_ok()));
        // Bob's turn leaves a dangling deferred reference, so the group's
        // COMMIT fails the way a full disk fails it: every job is lost.
        let lose = |install: bool| {
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute_batch(if install {
                    "CREATE TABLE parent(id INTEGER PRIMARY KEY);
                     CREATE TABLE child(parent INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);
                     CREATE TRIGGER lose_commit AFTER INSERT ON turns WHEN NEW.bot='Bob'
                     BEGIN INSERT INTO child VALUES (1); END;"
                } else {
                    "DROP TRIGGER lose_commit;"
                })
                .unwrap()
        };
        lose(true);
        let (held, gate) = hold(&store).await;
        for (index, bot) in ["Alice", "Bob"].into_iter().enumerate() {
            let deferred = service
                .dispatch(
                    submit(bot, "r1"),
                    0,
                    &output,
                    json!(index),
                    Bound::default(),
                )
                .await;
            assert_eq!(deferred.unwrap_err().code, "deferred");
        }
        assert_eq!(service.reserved, 2);
        gate.send(()).unwrap();
        assert_eq!(held.await.unwrap_err().code, "storage_error");
        for _ in 0..2 {
            let (_, _, _, result) = service.settle().await;
            assert_eq!(result.unwrap_err().code, "storage_error");
        }
        assert!(service.active.is_empty(), "no lost admission starts a turn");
        assert_eq!(service.reserved, 0, "their slots are free again");
        // Nothing of the lost group is durable: the retry is new work.
        lose(false);
        let retry = request(&mut service, submit("Alice", "r1"), &output)
            .await
            .unwrap();
        assert_eq!(retry["duplicate"], false);
        assert_eq!(retry["status"], "running");
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn an_interrupt_waits_for_the_admission_before_it() {
        let dir = scratch("admit-interrupt");
        let (store, _publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let mut service = admitting(&store, 1024);
        let (output, _writer) = Output::stdout();
        // The store is fresh, so the submission's turn is the first.
        let answers = requests(
            &mut service,
            vec![
                create("A", &dir),
                submit("A", "r1"),
                Command::Interrupt {
                    bot: "A".into(),
                    turn: 1,
                },
            ],
            &output,
        )
        .await;
        assert_eq!(answers[1].1.as_ref().unwrap()["turn"], 1);
        let interrupted = answers[2].1.as_ref().unwrap();
        assert_eq!(interrupted["interrupt_requested"], true);
        assert!(
            interrupted.get("queued").is_none(),
            "it reached the started task, not a durable row behind its back"
        );
        let (bot, turn, task, exit) = service.jobs.join_next().await.unwrap().unwrap();
        service.complete(bot, turn, task, exit).await.unwrap();
        let status = store.call(|db| db.turn_status("A", 1)).await.unwrap();
        assert_eq!(status, "interrupted");
        drop(service);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
