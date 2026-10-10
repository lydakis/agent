//! The `agent` client: a build-like command over the shared daemon. It
//! connects to the daemon's Unix socket, starting one when none is running,
//! then relays the bot's event stream until the requested turn ends. The
//! JSONL stream is the contract for programs; `--pretty` is a human view.
use agent_runtime::{Error, Result, fail, fail_with};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, BufReader, IsTerminal, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEFAULT_TOOLS: &str = "shell,read,write,edit,wait,history";
use agent_client::approver::{AUTO_EXPIRE_MS, UNGATED};
use agent_client::policy::DEFAULT_COMPACTION_INSTRUCTIONS;
/// What a new bot is told when the caller gives no instructions: the
/// harness preamble every client shares. `--agents` layers AGENTS.md files
/// and skills on top; a program that wants that asks for it.
const DEFAULT_INSTRUCTIONS: &str = agent_client::policy::PREAMBLE;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Shutdown drains each client and the event publisher under 5 s bounds,
/// after any grace period the caller gave running turns.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

fn startup_remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| Error::new("daemon_start_timeout"))
}

struct Options {
    /// The command being run; `run` treats `--model` as the turn's model.
    command: String,
    store: PathBuf,
    socket: PathBuf,
    providers: Vec<String>,
    /// Daemon-scoped values the caller stated, checked against a running
    /// daemon at attach. Defaults and environment-implied values never conflict.
    providers_explicit: bool,
    tools_explicit: bool,
    tools: String,
    /// A fork's allowed tools, comma-separated; empty allows none.
    allow: Option<String>,
    model: Option<String>,
    instructions: Option<String>,
    reasoning: Option<String>,
    workspace: Option<PathBuf>,
    bot: Option<String>,
    source: Option<String>,
    checkpoint: Option<i64>,
    request_id: Option<String>,
    bot_id: Option<i64>,
    /// A new bot's compaction instructions: the default text, a caller's
    /// own, or none with --no-compaction.
    compaction_instructions: Option<String>,
    compaction_model: Option<String>,
    fallbacks: bool,
    discover: bool,
    after: i64,
    pretty: bool,
    new: bool,
    agents: bool,
    /// A new bot's role: `.agents/agents/ROLE.md` in the workspace or home.
    profile: Option<String>,
    detach: bool,
    no_spawn: bool,
    timeout_ms: Option<u64>,
    /// `shutdown --grace`: seconds running turns may take to finish.
    grace: u64,
    budget_tokens: Option<u64>,
    turn: Option<i64>,
    keep_turns: Option<usize>,
    /// `follow --all`: every bot on one connection.
    all: bool,
    /// `wait --any`: return on the first resolved handle.
    any: bool,
    /// `run --delivery`: what to do when the bot is busy.
    delivery: Option<String>,
    /// A new bot's approval mode and gated tools.
    approval: Option<String>,
    approve: Option<String>,
    /// `answer`: the call, its request number, the gate, and the reason.
    call: Option<String>,
    request: Option<i64>,
    tag: Option<String>,
    reason: Option<String>,
    /// `approver`: the environment note file, the judge, and Jev's URL.
    note: Option<PathBuf>,
    judge: Option<String>,
    judge_url: Option<String>,
    /// Daemon limits forwarded when this client starts the daemon.
    daemon_flags: Vec<(String, String)>,
    /// A new bot's settings, as `create` takes them, and the flags that set them.
    settings: serde_json::Map<String, Value>,
    settings_flags: Vec<String>,
    positional: Vec<String>,
    /// The `--store` and `--socket` flags, shell-quoted, that reach this
    /// daemon from any shell; empty when both are the defaults.
    target: String,
}

fn parse(args: &[String]) -> Result<Options> {
    let mut options = Options {
        command: String::new(),
        socket: PathBuf::new(),
        store: PathBuf::new(),
        providers: Vec::new(),
        providers_explicit: false,
        tools_explicit: false,
        tools: DEFAULT_TOOLS.into(),
        allow: None,
        model: None,
        instructions: None,
        reasoning: None,
        workspace: None,
        bot: None,
        source: None,
        checkpoint: None,
        request_id: None,
        bot_id: None,
        compaction_instructions: Some(DEFAULT_COMPACTION_INSTRUCTIONS.to_owned()),
        compaction_model: None,
        fallbacks: false,
        discover: false,
        after: 0,
        pretty: false,
        new: false,
        agents: false,
        profile: None,
        detach: false,
        no_spawn: false,
        timeout_ms: None,
        grace: 0,
        budget_tokens: None,
        turn: None,
        keep_turns: None,
        all: false,
        any: false,
        delivery: std::env::var("AGENT_DELIVERY").ok(),
        approval: std::env::var("AGENT_APPROVAL")
            .ok()
            .filter(|mode| !mode.is_empty()),
        approve: None,
        call: None,
        request: None,
        tag: None,
        reason: None,
        note: std::env::var_os("AGENT_APPROVER_NOTE").map(PathBuf::from),
        judge: std::env::var("AGENT_APPROVER_JUDGE")
            .ok()
            .filter(|judge| !judge.is_empty()),
        judge_url: None,
        daemon_flags: Vec::new(),
        settings: serde_json::Map::new(),
        settings_flags: Vec::new(),
        positional: Vec::new(),
        target: String::new(),
    };
    let mut iter = args.iter();
    let mut socket = None;
    let mut store = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--pretty" => options.pretty = true,
            "--no-spawn" => options.no_spawn = true,
            "--new" => options.new = true,
            "--agents" => options.agents = true,
            "--no-compaction" => options.compaction_instructions = None,
            "--fallbacks" => options.fallbacks = true,
            "--discover" => options.discover = true,
            "--detach" => options.detach = true,
            "--all" => options.all = true,
            "--any" => options.any = true,
            "--" => options.positional.extend(iter.by_ref().cloned()),
            flag if flag.starts_with("--") => {
                let value = iter
                    .next()
                    .ok_or(Error::with("usage", format!("{flag} needs a value")))?
                    .clone();
                match flag {
                    "--store" => store = Some(PathBuf::from(value)),
                    "--socket" => socket = Some(PathBuf::from(value)),
                    "--provider" => {
                        options.providers.push(value);
                        options.providers_explicit = true;
                    }
                    "--tools" => {
                        options.tools = value;
                        options.tools_explicit = true;
                    }
                    "--model" => options.model = Some(value),
                    "--profile" => options.profile = Some(value),
                    "--delivery" => options.delivery = Some(value),
                    "--approval" => options.approval = Some(value),
                    "--approve" => options.approve = Some(value),
                    "--allow" => options.allow = Some(value),
                    "--call" => options.call = Some(value),
                    "--request" => {
                        options.request = Some(
                            value
                                .parse()
                                .map_err(|_| Error::with("usage", "--request needs an integer"))?,
                        )
                    }
                    "--tag" => options.tag = Some(value),
                    "--reason" => options.reason = Some(value),
                    "--note" => options.note = Some(PathBuf::from(value)),
                    "--judge-url" => options.judge_url = Some(value),
                    "--judge" => options.judge = Some(value),
                    "--instructions" => options.instructions = Some(value),
                    "--instructions-file" => {
                        options.instructions =
                            Some(std::fs::read_to_string(&value).map_err(|_| {
                                Error::with("usage", format!("cannot read {value}"))
                            })?)
                    }
                    "--compaction-instructions" => {
                        options.compaction_instructions = Some(value).filter(|v| !v.is_empty())
                    }
                    "--compaction-instructions-file" => {
                        options.compaction_instructions =
                            Some(std::fs::read_to_string(&value).map_err(|_| {
                                Error::with("usage", format!("cannot read {value}"))
                            })?)
                    }
                    "--compaction-model" => options.compaction_model = Some(value),
                    "--reasoning" => options.reasoning = Some(value),
                    "--workspace" => options.workspace = Some(value.into()),
                    "--bot" => options.bot = Some(value),
                    "--source" => options.source = Some(value),
                    "--checkpoint" => {
                        options.checkpoint =
                            Some(value.parse().map_err(|_| {
                                Error::with("usage", "--checkpoint needs an integer")
                            })?)
                    }
                    "--request-id" => options.request_id = Some(value),
                    "--bot-id" => {
                        options.bot_id = Some(
                            value
                                .parse()
                                .map_err(|_| Error::with("usage", "--bot-id needs an integer"))?,
                        )
                    }
                    "--keep-turns" => {
                        options.keep_turns = Some(value.parse().ok().filter(|n| *n > 0).ok_or(
                            Error::with("usage", "--keep-turns needs a positive integer"),
                        )?)
                    }
                    "--grace" => {
                        options.grace = value
                            .parse()
                            .map_err(|_| Error::with("usage", "--grace needs seconds"))?
                    }
                    "--timeout-ms" => {
                        options.timeout_ms =
                            Some(value.parse().map_err(|_| {
                                Error::with("usage", "--timeout-ms needs an integer")
                            })?)
                    }
                    "--max-processes"
                    | "--max-detached"
                    | "--max-active"
                    | "--max-connecting"
                    | "--max-pending"
                    | "--max-pending-bytes"
                    | "--stall-timeout"
                    | "--idle-exit" => {
                        value.parse::<usize>().map_err(|_| {
                            Error::with("usage", format!("{flag} needs an integer"))
                        })?;
                        options.daemon_flags.push((flag.to_owned(), value));
                    }
                    "--context-bytes"
                    | "--context-items"
                    | "--note-turns"
                    | "--compact-at"
                    | "--compact-keep"
                    | "--retain-turns"
                    | "--approval-hold-ms"
                    | "--max-output-tokens"
                    | "--keep-warm" => {
                        let number = value.parse::<u64>().map_err(|_| {
                            Error::with("usage", format!("{flag} needs an integer"))
                        })?;
                        // The daemon checks each setting's range.
                        let key = flag.trim_start_matches("--").replace('-', "_");
                        options.settings.insert(key, json!(number));
                        options.settings_flags.push(flag.to_owned());
                    }
                    "--cache-ttl" => {
                        if !matches!(value.as_str(), "5m" | "1h") {
                            return fail_with("usage", "--cache-ttl needs 5m or 1h");
                        }
                        options
                            .settings
                            .insert("cache_ttl".to_owned(), json!(value));
                        options.settings_flags.push(flag.to_owned());
                    }
                    "--budget-tokens" => {
                        options.budget_tokens = Some(value.parse().map_err(|_| {
                            Error::with("usage", "--budget-tokens needs an integer")
                        })?)
                    }
                    "--turn" => {
                        options.turn = Some(
                            value
                                .parse()
                                .map_err(|_| Error::with("usage", "--turn needs an integer"))?,
                        )
                    }
                    "--after" => {
                        options.after = value
                            .parse()
                            .map_err(|_| Error::with("usage", "--after needs an integer"))?
                    }
                    _ => return fail_with("usage", format!("unknown option {flag}")),
                }
            }
            _ => options.positional.push(arg.clone()),
        }
    }
    let explicit_store = store.is_some();
    let home_store = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".agent").join("state.sqlite"));
    options.store = match store.or_else(|| std::env::var_os("AGENT_STORE").map(PathBuf::from)) {
        Some(store) => store,
        None => home_store
            .clone()
            .ok_or(Error::with("usage", "set --store, AGENT_STORE, or HOME"))?,
    };
    options.socket = match socket.or_else(|| {
        if explicit_store {
            None
        } else {
            std::env::var_os("AGENT_SOCKET").map(PathBuf::from)
        }
    }) {
        Some(socket) => socket,
        None => crate::client_path::default_socket(&options.store)?,
    };
    options.target = target(&options.store, &options.socket, home_store.as_deref());
    if options.providers.is_empty() {
        options.providers = environment_providers(&|name| std::env::var(name).ok());
    }
    Ok(options)
}

/// The providers a daemon starts with when no `--provider` is given:
/// `AGENT_PROVIDER`, `--provider` specs separated by whitespace since a spec
/// holds commas; otherwise those whose well-known key variable is set. Like
/// `AGENT_MODEL`, they are defaults: a running daemon is not checked against
/// them, since every shell a bot runs inherits the daemon's environment.
fn environment_providers(var: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let set = |name| var(name).filter(|value| !value.trim().is_empty());
    if let Some(specs) = set("AGENT_PROVIDER") {
        return specs.split_whitespace().map(str::to_owned).collect();
    }
    [
        ("anthropic", "ANTHROPIC_API_KEY"),
        ("openai", "OPENAI_API_KEY"),
        ("openrouter", "OPENROUTER_API_KEY"),
    ]
    .into_iter()
    .filter(|(_, key)| set(key).is_some())
    .map(|(name, _)| name.to_owned())
    .collect()
}

struct Connection {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next: u64,
    pending: VecDeque<Value>,
    pub ready: Value,
}

impl Connection {
    fn connect(socket: &PathBuf) -> Result<Self> {
        Self::connect_until(socket, Instant::now() + STARTUP_TIMEOUT)
    }
    fn connect_until(socket: &PathBuf, deadline: Instant) -> Result<Self> {
        startup_remaining(deadline)?;
        // Bound connect itself as well as readiness, including a full listener
        // backlog. This temporary I/O runtime has no worker threads.
        let stream = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()?
            .block_on(async {
                tokio::time::timeout_at(deadline.into(), tokio::net::UnixStream::connect(socket))
                    .await
                    .map_err(|_| Error::new("daemon_start_timeout"))?
                    .map_err(|_| Error::new("daemon_unavailable"))?
                    .into_std()
                    .map_err(Error::from)
            })?;
        stream.set_nonblocking(false)?;
        let writer = stream.try_clone()?;
        let mut connection = Self {
            reader: BufReader::new(stream),
            writer,
            next: 0,
            pending: VecDeque::new(),
            ready: Value::Null,
        };
        connection.ready = connection.read_ready(deadline)?;
        if connection.ready["event"] != "ready" {
            return fail("daemon_protocol_mismatch");
        }
        if connection.ready["protocol"].as_u64() != Some(agent_client::PROTOCOL) {
            // The greeting goes with the error: `start` shows which daemon
            // holds the socket, and `shutdown` may stop an older one.
            let detail = format!(
                "the daemon speaks protocol {}, this agent {}",
                connection.ready["protocol"],
                agent_client::PROTOCOL
            );
            return Err(Error::with("daemon_protocol_mismatch", detail)
                .facts(json!({"ready": connection.ready})));
        }
        Ok(connection)
    }
    fn read_ready(&mut self, deadline: Instant) -> Result<Value> {
        let mut line = Vec::new();
        loop {
            // Recompute the remaining budget before each read, so partial
            // lines cannot turn this into an indefinitely renewed idle timeout.
            self.reader
                .get_ref()
                .set_read_timeout(Some(startup_remaining(deadline)?))?;
            let bytes = match self.reader.fill_buf() {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return fail("daemon_start_timeout");
                }
                Err(error) => return Err(error.into()),
            };
            if bytes.is_empty() {
                return fail("daemon_disconnected");
            }
            let newline = bytes.iter().position(|&byte| byte == b'\n');
            let consumed = newline.map_or(bytes.len(), |index| index + 1);
            line.extend_from_slice(&bytes[..consumed]);
            self.reader.consume(consumed);
            if newline.is_some() {
                // Keep read-ahead bytes in the same buffer. Normal requests and
                // event streams may wait longer than the startup deadline.
                self.reader.get_ref().set_read_timeout(None)?;
                return Ok(serde_json::from_slice(&line)?);
            }
        }
    }
    fn read_line(&mut self) -> Result<Value> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return fail("daemon_disconnected");
        }
        Ok(serde_json::from_str(&line)?)
    }
    fn request(&mut self, op: &str, mut params: Value) -> Result<Value> {
        self.next += 1;
        params["id"] = json!(self.next);
        params["op"] = json!(op);
        let mut line = serde_json::to_vec(&params)?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        loop {
            let message = self.read_line()?;
            if message.get("id").is_some_and(|id| *id == json!(self.next)) {
                let Some(code) = message.get("error").and_then(Value::as_str) else {
                    return Ok(message["result"].clone());
                };
                let mut error = Error::new(code);
                error.detail = message["detail"].as_str().map(str::to_owned);
                if let Value::Object(mut facts) = message {
                    for key in ["id", "error", "detail"] {
                        facts.remove(key);
                    }
                    error.facts = (!facts.is_empty()).then(|| Box::new(facts));
                }
                return Err(error);
            }
            if message.get("id").is_none() {
                self.pending.push_back(message);
            }
        }
    }
    fn next_event(&mut self) -> Result<Value> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }
        loop {
            let message = self.read_line()?;
            if message.get("id").is_none() {
                return Ok(message);
            }
        }
    }
}

fn ensure_existing_daemon(options: &Options) -> Result<Connection> {
    // A command on existing bots must never initialize a replacement store.
    if !options.store.is_file() {
        return fail_with(
            "store_not_found",
            format!(
                "no store at {}; this command never creates one",
                options.store.display()
            ),
        );
    }
    ensure_daemon(options)
}

/// A running daemon serves whatever configuration started it. Every
/// daemon-scoped value this client stated must match what the daemon
/// announces, or the client fails here naming each difference, before any
/// work is submitted under a configuration nobody asked for.
fn check_daemon(options: &Options, ready: &Value) -> Result<()> {
    let mut differences = Vec::new();
    if options.providers_explicit {
        for spec in &options.providers {
            let requested = crate::server::ProviderSpec::parse(spec)?;
            let running = &ready["providers"][&requested.name];
            if running.is_null() {
                differences.push(format!("--provider {}: not registered", requested.name));
            } else if running["family"] != requested.family.name()
                || running["url"] != requested.url
                || running["transport"] != requested.transport()
                || (running["auth"] == "sigv4") != requested.sigv4
            {
                let signed = |sigv4| if sigv4 { " signed with SigV4" } else { "" };
                differences.push(format!(
                    "--provider {}: requested {},{} over {}{} but daemon has {},{} over {}{}",
                    requested.name,
                    requested.family.name(),
                    requested.url,
                    requested.transport(),
                    signed(requested.sigv4),
                    running["family"].as_str().unwrap_or(""),
                    running["url"].as_str().unwrap_or(""),
                    running["transport"].as_str().unwrap_or(""),
                    signed(running["auth"] == "sigv4")
                ));
            }
        }
    }
    for (flag, value) in &options.daemon_flags {
        let key = match flag.as_str() {
            "--max-processes" => "processes",
            "--max-detached" => "detached",
            "--max-active" => "active",
            "--max-connecting" => "connecting",
            "--max-pending" => "pending",
            "--max-pending-bytes" => "pending_bytes",
            "--stall-timeout" => "stall_timeout_seconds",
            "--idle-exit" => "idle_exit_seconds",
            _ => continue,
        };
        let running = &ready["limits"][key];
        let requested = value
            .parse::<u64>()
            .ok()
            .filter(|value| flag != "--idle-exit" || *value != 0);
        if running.as_u64() != requested {
            differences.push(format!(
                "{flag}: requested {value} but daemon has {}",
                if running.is_null() {
                    "no value".to_owned()
                } else {
                    running.to_string()
                }
            ));
        }
    }
    if differences.is_empty() {
        Ok(())
    } else {
        fail_with("daemon_configuration_mismatch", differences.join("; "))
    }
}

fn ensure_daemon(options: &Options) -> Result<Connection> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let connect = || match Connection::connect_until(&options.socket, deadline) {
        Ok(connection) => {
            check_daemon(options, &connection.ready)?;
            Ok(Some(connection))
        }
        // A daemon that answers with another protocol is running; starting
        // a second one for the same socket would not replace it.
        Err(error)
            if error.code == "daemon_start_timeout" || error.code == "daemon_protocol_mismatch" =>
        {
            Err(error)
        }
        Err(_) => Ok(None),
    };
    if let Some(connection) = connect()? {
        return Ok(connection);
    }
    startup_remaining(deadline)?;
    if options.no_spawn {
        return fail_with("daemon_unavailable", options.socket.display().to_string());
    }
    if options.providers.is_empty() {
        return fail_with(
            "usage",
            "no provider: pass --provider (anthropic, openai, openrouter, chatgpt, bedrock, bedrock-openai, or NAME=FAMILY,URL,KEY_ENV), set AGENT_PROVIDER, or export a provider key",
        );
    }
    if let Some(parent) = options.store.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut log = options.store.clone().into_os_string();
    log.push(".log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("serve")
        .arg("--store")
        .arg(&options.store)
        .arg("--socket")
        .arg(&options.socket);
    for provider in &options.providers {
        command.arg("--provider").arg(provider);
    }
    for (flag, value) in &options.daemon_flags {
        command.arg(flag).arg(value);
    }
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    // The judge's key is the approver's alone, unless a provider the daemon
    // runs names it as its key variable.
    let provider_key = options.providers.iter().any(|spec| {
        crate::server::ProviderSpec::parse(spec)
            .is_ok_and(|spec| spec.key_env.as_deref() == Some(JUDGE_KEY))
    });
    if !provider_key {
        command.env_remove(JUDGE_KEY);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    loop {
        if let Some(connection) = connect()? {
            return Ok(connection);
        }
        startup_remaining(deadline)?;
        if let Some(status) = child.try_wait()?
            && status.code() != Some(crate::DAEMON_OWNERSHIP_CONFLICT)
        {
            // The owner might have become ready while our child exited.
            if let Some(connection) = connect()? {
                return Ok(connection);
            }
            startup_remaining(deadline)?;
            let mut log = options.store.clone().into_os_string();
            log.push(".log");
            let detail = std::fs::read_to_string(log)
                .ok()
                .and_then(|text| text.lines().last().map(str::to_owned))
                .unwrap_or_else(|| format!("daemon exited with {status}"));
            return fail_with("daemon_start_failed", detail);
        }
        // An ownership-conflict exit only says our child lost. The winner
        // can still be recovering its store before binding the socket.
        std::thread::sleep(Duration::from_millis(50).min(startup_remaining(deadline)?));
    }
}

/// Explicit text wins; `--agents` composes the shared policy for the
/// workspace, and `--profile` composes it in that role; otherwise the
/// preamble alone.
fn composed_instructions(
    options: &Options,
    workspace: &str,
    role: Option<&agent_client::policy::Profile>,
) -> Result<String> {
    if (options.agents || role.is_some()) && options.instructions.is_some() {
        return fail_with(
            "usage",
            "--agents and --profile compose instructions; --instructions sets them",
        );
    }
    if let Some(text) = &options.instructions {
        return Ok(text.clone());
    }
    if options.agents || role.is_some() {
        return agent_client::policy::instructions(std::path::Path::new(workspace), role)
            .map(|composed| composed.text)
            .map_err(|error| Error::with(error.code(), error.to_string()));
    }
    Ok(DEFAULT_INSTRUCTIONS.to_owned())
}

/// The role a new bot is started in, which must exist.
fn role(options: &Options, workspace: &str) -> Result<Option<agent_client::policy::Profile>> {
    let Some(name) = &options.profile else {
        return Ok(None);
    };
    agent_client::policy::profile(std::path::Path::new(workspace), name)
        .map_err(|error| Error::with(error.code(), error.to_string()))?
        .map(Some)
        .ok_or_else(|| {
            Error::with(
                "profile_not_found",
                format!("no .agents/agents/{name}.md in the workspace or ~/.agents/agents"),
            )
        })
}

/// Inside a bot's shell tool the daemon names the bot; a client run there
/// declares that bot as the creator of anything it creates or forks.
fn created_by() -> Result<(Option<String>, Option<i64>)> {
    let name = std::env::var("AGENT_BOT").ok().filter(|b| !b.is_empty());
    let id = std::env::var("AGENT_BOT_ID").ok();
    match (name, id) {
        (None, None) => Ok((None, None)),
        (Some(name), Some(id)) => {
            let id = id
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or(Error::new("creator_identity_required"))?;
            Ok((Some(name), Some(id)))
        }
        _ => fail("creator_identity_required"),
    }
}

/// Inside a bot's shell tool the daemon also names the turn; a prompt a
/// client submits from there is that turn's model's words, not a person's.
fn author() -> Result<Value> {
    let bot = std::env::var("AGENT_BOT").ok().filter(|b| !b.is_empty());
    let turn = std::env::var("AGENT_TURN").ok();
    match (bot, turn) {
        (None, None) => Ok(Value::Null),
        (Some(bot), Some(turn)) => {
            let turn = turn
                .parse::<i64>()
                .ok()
                .filter(|turn| *turn > 0)
                .ok_or(Error::new("author_turn_required"))?;
            Ok(json!({"bot":bot,"turn":turn}))
        }
        _ => fail("author_turn_required"),
    }
}

/// Whether a bot record has a gate the `auto` approver answers.
fn answered_by_auto(bot: &Value) -> bool {
    bot["gates"]
        .as_array()
        .is_some_and(|gates| gates.iter().any(|gate| gate["tag"] == "auto"))
}

/// A bot record's model, as `provider/model`.
fn bot_model(bot: &Value) -> Option<String> {
    Some(format!(
        "{}/{}",
        bot["provider"].as_str()?,
        bot["model"].as_str()?
    ))
}

fn judge_key() -> Option<String> {
    std::env::var(JUDGE_KEY).ok().filter(|key| !key.is_empty())
}

/// Who judges for an approver: `--judge` or AGENT_APPROVER_JUDGE, then Jev
/// when its key is set, then `model`.
fn judge(options: &Options, model: Option<String>) -> Result<String> {
    options
        .judge
        .clone()
        .or_else(|| judge_key().map(|_| "typesafe/jev-latest".into()))
        .or(model)
        .ok_or_else(|| {
            Error::with(
                "judge_required",
                format!(
                    "auto needs a judge: pass --judge PROVIDER/MODEL, or set AGENT_APPROVER_JUDGE, {JUDGE_KEY}, or AGENT_MODEL"
                ),
            )
        })
}

/// Whether the approver `pid` logged that it serves, after byte `start`.
fn serving(log: &std::ffi::OsStr, start: u64, pid: u32) -> Result<bool> {
    use std::io::{BufRead, Seek};
    let mut file = std::fs::File::open(log)?;
    file.seek(std::io::SeekFrom::Start(start))?;
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        if line["event"] == "serving" && line["pid"] == pid {
            return Ok(true);
        }
    }
    Ok(false)
}

/// An `auto` bot needs an approver serving `auto`. If none is, start one:
/// detached, logging its verdicts next to the store, judged by the model
/// `judge` picks, with the bot's own as the last choice.
fn ensure_approver(
    options: &Options,
    connection: &mut Connection,
    model: Option<String>,
) -> Result<()> {
    let served = |connection: &mut Connection| -> Result<bool> {
        let stats = connection.request("stats", json!({}))?;
        Ok(stats["approvers"]
            .as_array()
            .is_some_and(|tags| tags.iter().any(|tag| tag == "auto")))
    };
    if served(connection)? {
        return Ok(());
    }
    let judge = judge(options, model)?;
    let mut log = options.store.clone().into_os_string();
    log.push(".approver.log");
    let path = log;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    // Where this approver's lines start: it is ready once it says so there.
    let start = log.metadata()?.len();
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("approver")
        .arg("--store")
        .arg(&options.store)
        .arg("--socket")
        .arg(&options.socket)
        .arg("--judge")
        .arg(&judge);
    if let Some(note) = &options.note {
        command.arg("--note").arg(note);
    }
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        // The tag is served as soon as the lease is held, before the judge
        // is ready, so wait for the approver's own word.
        if serving(&path, start, child.id())? {
            return Ok(());
        }
        // Another approver won the tag first: that one serves it.
        if let Some(status) = child.try_wait()? {
            if served(connection)? {
                return Ok(());
            }
            return fail_with(
                "approver_start_failed",
                format!("approver exited with {status}"),
            );
        }
        startup_remaining(deadline)?;
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The gate a new bot asks for, as `create` or `fork` fields. The mode
/// comes from --approval or AGENT_APPROVAL, `full` by default: no gate.
/// `manual` gates --approve, or every tool the bot has but the four that
/// touch only its own records, for a person or a program to answer.
fn requested_gate(options: &Options, tools: &[String]) -> Result<Value> {
    // The tools gated: --approve, or every tool but the ungated four.
    let approve = || -> Result<Vec<String>> {
        let approve: Vec<String> = match &options.approve {
            Some(list) => list
                .split(',')
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .collect(),
            None => tools
                .iter()
                .filter(|t| !UNGATED.contains(&t.as_str()))
                .cloned()
                .collect(),
        };
        // Asked to gate nothing: refused rather than run ungated.
        if approve.is_empty() && options.approve.is_some() {
            return fail_with("usage", "--approve names no tools");
        }
        Ok(approve)
    };
    match options.approval.as_deref().unwrap_or("full") {
        "full" if options.approve.is_some() => fail_with(
            "usage",
            "--approve names tools for an approver; use --approval manual or auto",
        ),
        "full" => Ok(json!({})),
        // A bot with only ungated tools has nothing to wait for.
        "manual" => Ok(match approve()? {
            approve if approve.is_empty() => json!({}),
            approve => json!({"approve":approve,"approver":"manual"}),
        }),
        // The same tools, answered by the approver serving `auto`, which
        // has until the expiry before the call is denied and the turn ends.
        "auto" => Ok(match approve()? {
            approve if approve.is_empty() => json!({}),
            approve => json!({"approve":approve,"approver":"auto",
                "approve_expire_ms":AUTO_EXPIRE_MS}),
        }),
        mode => fail_with(
            "usage",
            format!("unknown approval mode {mode}; use full, manual, or auto"),
        ),
    }
}

fn unique(prefix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{prefix}-{millis}-{}", std::process::id())
}

pub fn main(args: Vec<String>) -> Result<i32> {
    let command = args[0].as_str();
    let mut options = parse(&args[1..])?;
    options.command = command.to_owned();
    if std::env::var("AGENT_SHELL_CONTEXT").as_deref() == Ok("1")
        && (command == "follow" || command == "wait" || (command == "run" && !options.detach))
    {
        return fail_with(
            "blocking_tool_client",
            "use run --detach to submit peer work and the wait tool to collect it; a blocking client would hold a process-budget unit while waiting",
        );
    }
    match command {
        "run" => run(&options),
        "follow" => follow(&options),
        "fork" => fork(&options),
        "interrupt" => interrupt(&options),
        "wait" => wait(&options),
        "turns" => turns(&options),
        "rm" => remove(&options),
        "prune" => prune(&options),
        "approvals" => approvals(&options),
        "answer" => answer(&options),
        "approver" => approver(&options),
        "models" => models(&options),
        // The daemon's ready line, from the running one or one started now,
        // with the socket it answered on, so a caller that reached this
        // command over SSH knows what to forward. A daemon of another
        // protocol is printed too, then refused.
        "start" => {
            // Absolute, as a caller elsewhere must name it: a relative
            // AGENT_SOCKET is relative to this command's directory. A path
            // JSON cannot carry is refused before anything starts.
            let socket = std::path::absolute(&options.socket)?;
            let Some(socket) = socket.to_str().map(str::to_owned) else {
                return fail_with(
                    "socket_path_unsupported",
                    format!(
                        "{} is not UTF-8, so the ready line cannot name it",
                        socket.display()
                    ),
                );
            };
            let (mut ready, refused) = match ensure_daemon(&options) {
                Ok(connection) => (connection.ready, None),
                Err(error) => match error.facts.as_ref().and_then(|facts| facts.get("ready")) {
                    Some(ready) => (ready.clone(), Some(error)),
                    None => return Err(error),
                },
            };
            ready["socket"] = json!(socket);
            print_json(&ready, options.pretty)?;
            refused.map_or(Ok(0), Err)
        }
        "stats" => {
            let mut connection = ensure_existing_daemon(&options)?;
            let stats = connection.request("stats", json!({}))?;
            print_json(&stats, options.pretty)?;
            Ok(0)
        }
        "ls" => list(&options),
        "shutdown" => {
            let mut connection = match Connection::connect(&options.socket) {
                Ok(connection) => connection,
                Err(error) => return stop_older(error),
            };
            let pid = connection.ready["pid"]
                .as_u64()
                .and_then(|pid| i32::try_from(pid).ok())
                .ok_or(Error::new("daemon_protocol_mismatch"))?;
            connection.request("shutdown", json!({"grace_ms": options.grace * 1000}))?;
            await_exit(pid, SHUTDOWN_TIMEOUT + Duration::from_secs(options.grace))?;
            Ok(0)
        }
        _ => fail("usage"),
    }
}

/// A daemon older than this agent cannot be asked to shut down in this
/// protocol, but every protocol honours SIGTERM: running turns end as
/// interrupted and the store keeps every chat. This is how an upgrade on a
/// host replaces its daemon. A newer daemon, or a listener that did not greet
/// as a daemon, is left alone and the error stands.
fn stop_older(error: Error) -> Result<i32> {
    let ready = (error.facts.as_ref()).and_then(|facts| facts.get("ready"));
    let older = ready
        .and_then(|ready| ready["protocol"].as_u64())
        .is_some_and(|protocol| protocol < agent_client::PROTOCOL);
    let pid = (ready.and_then(|ready| ready["pid"].as_u64()))
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 1);
    let (true, Some(pid)) = (older, pid) else {
        return Err(error);
    };
    // SAFETY: a signal to the process the daemon named as itself.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        let error = std::io::Error::last_os_error();
        // It exited after it greeted: it is stopped.
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(0);
        }
        return fail_with("daemon_stop_failed", error.to_string());
    }
    await_exit(pid, SHUTDOWN_TIMEOUT)?;
    Ok(0)
}

/// Return once the daemon process is gone, so a caller may copy or reopen
/// the store: it answers shutdown before it lets running turns finish within
/// the grace period, cancels the rest, commits their records and closes the
/// database. An unreaped zombie counts as gone.
fn await_exit(pid: i32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let running = || {
        // SAFETY: signal 0 only checks that the process exists.
        let exists = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !exists {
            return false;
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: proc_pidinfo fills this fixed-size process record.
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of_val(&info) as i32;
            let read = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    (&mut info as *mut libc::proc_bsdinfo).cast(),
                    size,
                )
            };
            if read == size {
                return info.pbi_status != libc::SZOMB;
            }
            std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        }
        #[cfg(not(target_os = "macos"))]
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            stat.rsplit_once(") ")
                .is_none_or(|(_, state)| !state.starts_with('Z'))
        })
    };
    while running() {
        if Instant::now() >= deadline {
            return fail_with("daemon_shutdown_timeout", pid.to_string());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// The models clients offer, from `~/.agent/models`; reading it needs no
/// daemon. `--discover` first writes one from the listings of the providers
/// the daemon runs, and never replaces a list that exists.
fn models(options: &Options) -> Result<i32> {
    let path = agent_client::models::path().ok_or(Error::with("usage", "set HOME"))?;
    let client_error = |e: agent_client::Error| Error {
        code: e.code,
        detail: e.detail,
        facts: e.facts,
    };
    if options.discover {
        if path.exists() {
            return fail_with(
                "models_file_exists",
                format!(
                    "{}: edit it, or remove it to discover again",
                    path.display()
                ),
            );
        }
        let mut connection = ensure_daemon(options)?;
        let listing = connection.request("provider_models", json!({}))?;
        let text = agent_client::models::render(&listing, &[]).map_err(client_error)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Written whole beside it, then linked into place: a failed write
        // leaves no partial list, and a list made meanwhile is not replaced.
        let staged = path.with_file_name(format!(".models.{}", std::process::id()));
        let installed = (|| {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::hard_link(&staged, &path)
        })();
        let _ = std::fs::remove_file(&staged);
        installed.map_err(|error| match error.kind() {
            std::io::ErrorKind::AlreadyExists => {
                Error::with("models_file_exists", path.display().to_string())
            }
            _ => error.into(),
        })?;
        // The new name is durable only once its directory is.
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        eprintln!("wrote {}", path.display());
    }
    let models = agent_client::models::read(&path).map_err(client_error)?;
    if options.pretty {
        for model in &models {
            match &model.note {
                Some(note) => println!("{}  # {}", visible(&model.id), visible(note)),
                None => println!("{}", visible(&model.id)),
            }
        }
        if models.is_empty() {
            eprintln!(
                "no models listed in {}; agent models --discover writes a first list",
                path.display()
            );
        }
    } else {
        print_json(
            &Value::Array(models.iter().map(|model| model.json()).collect()),
            false,
        )?;
    }
    Ok(0)
}

fn print_json(value: &Value, pretty: bool) -> Result<()> {
    if pretty {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{value}");
    }
    Ok(())
}

fn workspace(options: &Options) -> Result<String> {
    let path = match &options.workspace {
        Some(path) => path.clone(),
        None => std::env::current_dir()?,
    };
    Ok(std::fs::canonicalize(path)?
        .to_str()
        .ok_or(Error::new("workspace_not_utf8"))?
        .to_owned())
}

fn run(options: &Options) -> Result<i32> {
    let mut prompt = options.positional.join(" ");
    if prompt == "-" || (prompt.is_empty() && !std::io::stdin().is_terminal()) {
        prompt.clear();
        std::io::stdin().read_to_string(&mut prompt)?;
    }
    if prompt.trim().is_empty() {
        return fail_with("usage", "run needs a prompt");
    }
    let from = author()?;
    let mut connection = ensure_daemon(options)?;
    // A named bot is continued, never silently replaced: an unknown name is an
    // error unless --new asks for creation. No name means a fresh identity.
    let created = options.new || options.bot.is_none();
    // A new bot starts here or in --workspace; a bot keeps its folder
    // unless --workspace moves it, so only then is a folder resolved.
    let workspace = if created || options.workspace.is_some() {
        Some(workspace(options)?)
    } else {
        None
    };
    if !created && options.tools_explicit {
        return fail_with(
            "usage",
            "--tools chooses a new bot's tools; an existing bot keeps its own",
        );
    }
    if !created && (options.instructions.is_some() || options.agents || options.profile.is_some()) {
        return fail_with(
            "usage",
            "--instructions, --agents and --profile set a new bot's instructions; an existing bot keeps its own",
        );
    }
    if !created && !options.settings_flags.is_empty() {
        return fail_with(
            "usage",
            format!(
                "{} set a new bot's settings; an existing bot keeps its own",
                options.settings_flags.join(", ")
            ),
        );
    }
    let bot = options.bot.clone().unwrap_or_else(|| unique("bot"));
    // One key for the creation and the submission, so resending the same
    // command gets the bot and the turn it made the first time.
    let request_id = options.request_id.clone().unwrap_or_else(|| unique("run"));
    if created {
        // The client chooses; the bot retains. Nothing about a bot comes
        // from the daemon or from whichever client connects later.
        let workspace = workspace.as_deref().expect("a new bot resolves its folder");
        let role = role(options, workspace)?;
        let chosen = options
            .model
            .clone()
            .or_else(|| role.as_ref().and_then(|r| r.model.clone()));
        // A peer on its creator's model takes its creator's effort level too;
        // a model chosen for it takes its own --reasoning or none.
        let reasoning = options.reasoning.clone().or_else(|| {
            chosen
                .is_none()
                .then(|| std::env::var("AGENT_REASONING").ok())
                .flatten()
                .filter(|level| !level.is_empty())
        });
        let model = chosen
            .or_else(|| std::env::var("AGENT_MODEL").ok())
            .ok_or(Error::with(
                "usage",
                "a new bot needs a model: pass --model PROVIDER/MODEL or set AGENT_MODEL",
            ))?;
        let instructions = composed_instructions(options, workspace, role.as_ref())?;
        let (created_by, created_by_id) = created_by()?;
        // --tools, else the role's, else the default set.
        let tools: Vec<String> = match role.as_ref().and_then(|r| r.tools.clone()) {
            Some(tools) if !options.tools_explicit => tools,
            _ => options
                .tools
                .split(',')
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .collect(),
        };
        let mut create = json!({"bot":bot,"workspace":workspace,"model":model,
            "instructions":instructions,"reasoning":reasoning,
            "budget_tokens":options.budget_tokens,"tools":tools,
            "created_by":created_by,"created_by_id":created_by_id,
            "compaction_instructions":options.compaction_instructions,
            "compaction_model":options.compaction_model,"fallbacks":options.fallbacks,
            "settings":options.settings,"request_id":request_id});
        if let Value::Object(gate) = requested_gate(options, &tools)? {
            // Its approver first, so a missing judge leaves no bot behind.
            if gate.get("approver").is_some_and(|tag| tag == "auto") {
                ensure_approver(options, &mut connection, Some(model.clone()))?;
            }
            create.as_object_mut().expect("object").extend(gate);
        }
        connection.request("create", create)?;
    } else {
        // Gates are the bot's own, set when it was made: a bot gated for
        // `auto` gets its approver back, as after a daemon restart.
        let record = connection.request("resume", json!({"bot":bot}))?;
        if answered_by_auto(&record) {
            ensure_approver(options, &mut connection, bot_model(&record))?;
        }
    }
    // Existing bots keep their model and effort unless --model or
    // --reasoning overrides them for this turn. AGENT_MODEL and
    // AGENT_REASONING are only creation defaults, including in a peer's shell.
    let submitted = connection
        .request(
            "submit",
            json!({"bot":bot,"bot_id":options.bot_id,"request_id":request_id,"prompt":prompt,
                "workspace":options.workspace.as_ref().and(workspace.as_ref()),
                "model":if created { Value::Null } else { json!(options.model) },
                "reasoning":if created { Value::Null } else { json!(options.reasoning) },
                "delivery":options.delivery,"expected_turn":options.turn,"from":from}),
        )
        .map_err(|error| ways_past_busy(&bot, error))?;
    if options.detach {
        print_json(&submitted, options.pretty)?;
        return Ok(0);
    }
    let turn = submitted["turn"]
        .as_i64()
        .ok_or(Error::new("daemon_protocol_mismatch"))?;
    let after = submitted["cursor"].as_i64().map(|c| c - 1).unwrap_or(0);
    connection.request("follow", json!({"bot":bot,"after":after}))?;
    let mut renderer = Renderer::new(options.pretty, Some(turn), &options.target);
    if options.pretty {
        eprintln!(
            "agent: {bot} turn {turn}{}{}",
            if created { " (new bot)" } else { "" },
            workspace
                .as_deref()
                .map(|w| format!(" in {w}"))
                .unwrap_or_default()
        );
    }
    loop {
        let event = connection.next_event()?;
        if let Some(code) = renderer.event(&mut connection, &event)? {
            return Ok(code);
        }
    }
}

/// A busy bot's refusal, with the ways past it as flags to copy: callers,
/// models included, do not act on a description of them. The daemon names
/// the same ways as request fields.
fn ways_past_busy(bot: &str, error: Error) -> Error {
    if error.code == "active_agent_limit" {
        let mut error = Error {
            detail: Some("the daemon runs as many turns as it may".into()),
            ..error
        };
        error.facts.get_or_insert_default().insert(
            "hint".into(),
            json!("resend with --delivery queue to run it when there is room"),
        );
        return error;
    }
    if error.code != "bot_busy" {
        return error;
    }
    // The refusal's own fact, as of the refusing transaction.
    let running = error
        .facts
        .as_deref()
        .and_then(|facts| facts.get("running_turn"))
        .and_then(Value::as_i64);
    let join = match running {
        Some(turn) => format!(
            "resend with --delivery steer --turn {turn} to add this to it, \
             or --delivery queue to run it afterwards"
        ),
        None => "resend with --delivery queue to run this after it".to_owned(),
    };
    let fork =
        format!("; to ask without interrupting, fork --source {bot} --bot NEW and send it to NEW");
    // The daemon's own detail names request fields; at the CLI the detail
    // states the refusal and the hint gives the ways past it in flags.
    let stated = match running {
        Some(turn) => format!("turn {turn} is running"),
        None => "earlier work is waiting".to_owned(),
    };
    let mut error = Error {
        detail: Some(stated),
        ..error
    };
    error
        .facts
        .get_or_insert_default()
        .insert("hint".into(), json!(format!("{join}{fork}")));
    error
}

fn follow(options: &Options) -> Result<i32> {
    if options.all {
        // Every bot's events from a store-wide cursor, then live, until the
        // connection ends: the fleet controller's view.
        let mut connection = Connection::connect(&options.socket)?;
        connection.request("follow", json!({"bot":"*","after":options.after}))?;
        let mut renderer = Renderer::new(options.pretty, None, &options.target);
        loop {
            let event = connection.next_event()?;
            renderer.event(&mut connection, &event)?;
        }
    }
    let bot = options
        .bot
        .clone()
        .ok_or(Error::with("usage", "follow needs --bot or --all"))?;
    let mut connection = Connection::connect(&options.socket)?;
    // Choose the turn before subscribing. If it finishes during attachment,
    // replay still delivers its terminal event; a later idle snapshot cannot
    // make us abandon events queued while waiting for an RPC response.
    let state = connection.request("resume", json!({"bot":bot}))?;
    let turn = state["running_turn"].as_i64();
    connection.request("follow", json!({"bot":bot,"after":options.after}))?;
    let mut renderer = Renderer::new(options.pretty, turn, &options.target);
    loop {
        let event = connection.next_event()?;
        if let Some(code) = renderer.event(&mut connection, &event)? {
            return Ok(code);
        }
        // An initially idle bot only needs replay. Otherwise the selected
        // turn's terminal event decides the exit, whether replayed or live.
        if event["event"] == "follow_live" && turn.is_none() {
            renderer.flush();
            return Ok(0);
        }
    }
}

fn fork(options: &Options) -> Result<i32> {
    let (Some(source), Some(bot)) = (&options.source, &options.bot) else {
        return fail_with(
            "usage",
            "fork needs --source and --bot; --checkpoint N picks a message, default is an idle source's head or a running turn's newest finished round",
        );
    };
    let checkpoint = options.checkpoint;
    let mut connection = Connection::connect(&options.socket)?;
    // A fork inherits only the conversation; its turns name their own workspace.
    let (created_by, created_by_id) = created_by()?;
    let mut request = json!({"source":source,"checkpoint":checkpoint,"bot":bot,
        "workspace":options.workspace.as_ref().map(|_| workspace(options)).transpose()?,
        "budget_tokens":options.budget_tokens,"request_id":options.request_id,
        "created_by":created_by,"created_by_id":created_by_id});
    if let Some(allow) = &options.allow {
        let allow: Vec<&str> = allow.split(',').filter(|t| !t.is_empty()).collect();
        request["allow"] = json!(allow);
    }
    // A fork keeps its source's tools and gates; its own gate adds to them.
    // Its approver starts first, so a missing judge leaves no fork behind.
    // Only a gate of the fork's own is built from the source's tools.
    let gated =
        options.approval.as_deref().unwrap_or("full") != "full" || options.approve.is_some();
    let state = match connection.request("resume", json!({"bot":source})) {
        // A keyed fork outlives its source: its resend is answered from the
        // fork, so it needs nothing from the source.
        Err(error) if error.code == "bot_not_found" && options.request_id.is_some() && !gated => {
            let result = connection.request("fork", request)?;
            print_json(&result, options.pretty)?;
            return Ok(0);
        }
        state => state?,
    };
    let mut auto = answered_by_auto(&state);
    if gated {
        let tools: Vec<String> = serde_json::from_value(state["tools"].clone())
            .map_err(|_| Error::new("daemon_protocol_mismatch"))?;
        if let Value::Object(gate) = requested_gate(options, &tools)? {
            auto |= gate.get("approver").is_some_and(|tag| tag == "auto");
            request.as_object_mut().expect("object").extend(gate);
        }
    }
    if auto {
        ensure_approver(options, &mut connection, bot_model(&state))?;
    }
    let result = connection.request("fork", request)?;
    print_json(&result, options.pretty)?;
    Ok(0)
}

fn interrupt(options: &Options) -> Result<i32> {
    let bot = options
        .bot
        .clone()
        .ok_or(Error::with("usage", "interrupt needs --bot"))?;
    let mut connection = Connection::connect(&options.socket)?;
    let state = connection.request("resume", json!({"bot":bot}))?;
    let Some(turn) = state["running_turn"].as_i64() else {
        return fail_with("no_active_turn", format!("{bot} has no running turn"));
    };
    connection.request("interrupt", json!({"bot":bot,"turn":turn}))?;
    Ok(0)
}

/// Delete an idle bot; prints what was freed.
fn remove(options: &Options) -> Result<i32> {
    let bot = options
        .bot
        .clone()
        .ok_or(Error::with("usage", "rm needs --bot"))?;
    let mut connection = ensure_existing_daemon(options)?;
    print_json(
        &connection.request("delete", json!({"bot":bot,"bot_id":options.bot_id}))?,
        options.pretty,
    )?;
    Ok(0)
}

/// Keep the newest --keep-turns turns' records of a bot and drop the rest.
fn prune(options: &Options) -> Result<i32> {
    let bot = options
        .bot
        .clone()
        .ok_or(Error::with("usage", "prune needs --bot"))?;
    let keep = options
        .keep_turns
        .ok_or(Error::with("usage", "prune needs --keep-turns N"))?;
    let mut connection = ensure_existing_daemon(options)?;
    print_json(
        &connection.request("prune", json!({"bot":bot,"keep_turns":keep}))?,
        options.pretty,
    )?;
    Ok(0)
}

/// Wait for all handles, or the first with --any. Pending peers are expected
/// in any mode; timeout without a result and resolved errors still exit 1.
fn wait(options: &Options) -> Result<i32> {
    if options.positional.is_empty() {
        return fail_with("usage", "wait needs at least one handle");
    }
    let mut connection = Connection::connect(&options.socket)?;
    let result = connection.request(
        "wait",
        json!({"handles":options.positional,"timeout_ms":options.timeout_ms,"any":options.any}),
    )?;
    print_json(&result, options.pretty)?;
    let clean = result["results"].as_object().is_some_and(|results| {
        let resolved = |v: &&Value| v.get("pending") != Some(&Value::Bool(true));
        let mut completed = results.values().filter(resolved).peekable();
        let enough = if options.any {
            completed.peek().is_some()
        } else {
            result["pending"].as_array().is_some_and(Vec::is_empty)
        };
        enough && completed.all(|v| v.get("error").is_none_or(Value::is_null))
    });
    Ok(if clean { 0 } else { 1 })
}

/// Calls waiting for a verdict, paged through completely; JSON array or
/// one line per call with the command that answers it.
fn approvals(options: &Options) -> Result<i32> {
    let mut connection = ensure_existing_daemon(options)?;
    let mut after = json!(0);
    let mut first = true;
    if !options.pretty {
        print!("[");
    }
    loop {
        let page = connection.request(
            "approvals",
            json!({"bot":options.bot,"tag":options.tag,"after":after,"limit":256}),
        )?;
        let calls = page["approvals"]
            .as_array()
            .ok_or(Error::new("daemon_protocol_mismatch"))?;
        for call in calls {
            if options.pretty {
                println!(
                    "{} turn {} {}",
                    call["bot"].as_str().unwrap_or(""),
                    call["turn"],
                    call_line(call)
                );
                for line in answer_lines(call, &options.target) {
                    println!("{line}");
                }
            } else {
                if !first {
                    print!(",");
                }
                print!("{call}");
                first = false;
            }
        }
        after = page["next_after"].clone();
        if after.is_null() {
            break;
        }
    }
    if !options.pretty {
        println!("]");
    }
    Ok(0)
}

/// Allow or deny one gated call. The request number is required, so a
/// decision made on what a person saw cannot land on a call announced
/// again since.
/// The judge's key, which only the approver holds.
const JUDGE_KEY: &str = agent_runtime::tools::JUDGE_KEY;

/// The approver's note: a regular file no larger than a judge's whole
/// state, opened without blocking so a FIFO cannot hold startup.
fn note(path: &Path) -> Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let unreadable = || Error::with("usage", format!("cannot read {}", path.display()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| unreadable())?;
    if !file.metadata().is_ok_and(|meta| meta.is_file()) {
        return Err(unreadable());
    }
    let limit = agent_client::approver::STATE_TOKENS * 3;
    let mut note = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut note)
        .map_err(|_| unreadable())?;
    if note.len() > limit {
        return fail_with(
            "usage",
            format!(
                "{} is over {limit} bytes, more than a judge sees",
                path.display()
            ),
        );
    }
    String::from_utf8(note).map_err(|_| unreadable())
}

/// Serve a gate tag, `auto` by default, with a judge model deciding every
/// call; runs until the daemon goes away or another session takes the tag.
fn approver(options: &Options) -> Result<i32> {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let judge = judge(options, env("AGENT_MODEL"))?;
    let note = options.note.as_deref().map(note).transpose()?;
    // Jev is `typesafe/jev-*`; any other name, a daemon provider called
    // `typesafe` included, runs through the daemon.
    let judge = match judge
        .strip_prefix("typesafe/")
        .filter(|model| model.starts_with("jev-"))
    {
        Some(model) => crate::approver::JudgeSpec::Jev {
            url: options
                .judge_url
                .clone()
                .or_else(|| env("TYPESAFE_BASE_URL"))
                .unwrap_or_else(|| "https://api.typesafe.ai".into()),
            model: model.to_owned(),
            key: judge_key().ok_or_else(|| {
                Error::with("judge_key_required", format!("set {JUDGE_KEY} for Jev"))
            })?,
        },
        None => crate::approver::JudgeSpec::Model {
            model: judge,
            reasoning: options.reasoning.clone(),
        },
    };
    crate::approver::main(crate::approver::Settings {
        socket: options.socket.clone(),
        tag: options.tag.clone().unwrap_or_else(|| "auto".into()),
        judge,
        note,
    })
}

fn answer(options: &Options) -> Result<i32> {
    // The same kind of guard as the creator identity: it stops a bot's own
    // shell from answering by accident, not a determined command.
    if std::env::var("AGENT_SHELL_CONTEXT").as_deref() == Ok("1")
        || std::env::var("AGENT_BOT").is_ok_and(|bot| !bot.is_empty())
    {
        return fail_with(
            "answer_in_tool_shell",
            "agent answer does not run inside a bot's tool shell",
        );
    }
    let (Some(bot), Some(turn), Some(call), Some(request)) =
        (&options.bot, options.turn, &options.call, options.request)
    else {
        return fail_with(
            "usage",
            "answer needs --bot, --turn, --call, and --request, as agent approvals lists them",
        );
    };
    let decision = options.positional[0].as_str();
    if !matches!(decision, "allow" | "deny") {
        return fail_with("usage", "answer needs a decision: allow or deny");
    }
    let mut connection = ensure_existing_daemon(options)?;
    let answered = connection.request(
        "answer",
        json!({"bot":bot,"turn":turn,"call_id":call,"request":request,"tag":options.tag,
            "decision":decision,"reason":options.reason,"by":"cli"}),
    )?;
    print_json(&answered, false)?;
    Ok(0)
}

/// A bot's turns, paged through completely; JSON array or a table.
fn turns(options: &Options) -> Result<i32> {
    let bot = options
        .bot
        .clone()
        .ok_or(Error::with("usage", "turns needs --bot"))?;
    let mut connection = ensure_existing_daemon(options)?;
    let mut after = json!(options.after);
    let mut first = true;
    if !options.pretty {
        print!("[");
    }
    loop {
        let page = connection.request("turns", json!({"bot":bot,"after":after,"limit":64}))?;
        let turns = page["turns"]
            .as_array()
            .ok_or(Error::new("daemon_protocol_mismatch"))?;
        for turn in turns {
            if options.pretty {
                println!(
                    "{:>6} {:<11} in {:>7} out {:>6} rounds {:>3}  {}  {}",
                    turn["turn"],
                    turn["status"].as_str().unwrap_or(""),
                    turn["input_tokens"],
                    turn["output_tokens"],
                    turn["model_rounds"],
                    turn["workspace"].as_str().unwrap_or("-"),
                    turn["prompt_preview"]
                        .as_str()
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(60)
                        .collect::<String>()
                );
            } else {
                if !first {
                    print!(",");
                }
                print!("{turn}");
                first = false;
            }
        }
        after = page["next_after"].clone();
        if after.is_null() {
            break;
        }
    }
    if !options.pretty {
        println!("]");
    }
    Ok(0)
}

fn list(options: &Options) -> Result<i32> {
    let mut connection = Connection::connect(&options.socket)?;
    let mut after = Value::Null;
    let mut first = true;
    if !options.pretty {
        print!("[");
    }
    loop {
        let page = connection.request("bots", json!({"after":after,"limit":64}))?;
        let bots = page["bots"]
            .as_array()
            .ok_or(Error::new("daemon_protocol_mismatch"))?;
        for bot in bots {
            if options.pretty {
                println!(
                    "{:<24} {:<11} {}/{}  {}",
                    bot["name"].as_str().unwrap_or(""),
                    bot["status"].as_str().unwrap_or(""),
                    bot["provider"].as_str().unwrap_or(""),
                    bot["model"].as_str().unwrap_or(""),
                    bot["workspace"].as_str().unwrap_or("")
                );
            } else {
                if !first {
                    print!(",");
                }
                print!("{bot}");
                first = false;
            }
        }
        after = page["next_after"].clone();
        if after.is_null() {
            break;
        }
    }
    if !options.pretty {
        println!("]");
    }
    Ok(0)
}

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Idle,
    Text,
    Thinking,
}

/// Passes events through as JSONL, or renders a human view with `--pretty`.
struct Renderer {
    pretty: bool,
    color: bool,
    mode: Mode,
    turn: Option<i64>,
    usage: (u64, u64, u64),
    /// The daemon's flags for the answer commands it prints.
    target: String,
    /// Program status for the terminal the human view is written to.
    status: Option<Status>,
}

impl Renderer {
    fn new(pretty: bool, turn: Option<i64>, target: &str) -> Self {
        let terminal = pretty && std::io::stdout().is_terminal();
        Self {
            pretty,
            color: terminal,
            mode: Mode::Idle,
            turn,
            usage: (0, 0, 0),
            target: target.to_owned(),
            // Without a selected turn (`follow --all`), one record per bot.
            status: terminal.then(|| Status::new(turn.is_none())),
        }
    }
    /// Dim text for a terminal. It resets every attribute first, so no
    /// state left before it (concealed or invisible text) carries into what
    /// it shows.
    fn dim(&self, text: &str) -> String {
        if self.color {
            format!("\x1b[0;2m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
    fn flush(&mut self) {
        if self.mode != Mode::Idle {
            println!();
            self.mode = Mode::Idle;
        }
        let _ = std::io::stdout().flush();
    }
    fn print(&mut self, mode: Mode, text: &str) {
        if self.mode != mode && self.mode != Mode::Idle {
            println!();
        }
        self.mode = mode;
        let text = streamed(text);
        let mut stdout = std::io::stdout();
        let _ = match mode {
            Mode::Thinking => write!(stdout, "{}", self.dim(&text)),
            _ => write!(stdout, "{text}"),
        };
        let _ = stdout.flush();
    }

    /// Returns an exit code when the followed turn is finished.
    fn event(&mut self, connection: &mut Connection, event: &Value) -> Result<Option<i32>> {
        if event["event"] == "pruned"
            && let Some(turn) = self.turn
        {
            // A retried turn may have lost its terminal event to retention.
            // Reconcile the selected turn, not merely the bot's newest state.
            // Retained and running turns still finish through normal events.
            let handle = format!("turn:{}/{turn}", event["bot"].as_str().unwrap_or(""));
            let found = connection.request("wait", json!({"handles":[&handle],"timeout_ms":0}))?;
            let result = &found["results"][&handle];
            if let Some(code) = result["error"].as_str() {
                return Err(Error {
                    detail: result["detail"].as_str().map(Into::into),
                    ..Error::new(code)
                });
            }
        }
        let finished =
            event["event"] == "turn_finished" && self.turn.is_some_and(|t| event["turn"] == t);
        if !self.pretty {
            println!("{event}");
            let _ = std::io::stdout().flush();
            return Ok(finished.then(|| exit_code(&event["data"])));
        }
        if let Some(turn) = self.turn
            && event
                .get("turn")
                .is_some_and(|t| !t.is_null() && *t != turn)
        {
            return Ok(None);
        }
        if let Some(status) = &mut self.status {
            let reports = status.event(event);
            if !reports.is_empty() {
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(reports.as_bytes());
                let _ = stdout.flush();
            }
        }
        let data = &event["data"];
        match event["event"].as_str().unwrap_or("") {
            "text_delta" => self.print(Mode::Text, event["text"].as_str().unwrap_or("")),
            "thinking_delta" => self.print(Mode::Thinking, event["text"].as_str().unwrap_or("")),
            "tool_started" => {
                self.flush();
                let line = tool_line(
                    data["name"].as_str().unwrap_or("tool"),
                    data["arguments"].as_str().unwrap_or(""),
                );
                println!("{}", self.dim(&line));
            }
            // A person answering from another terminal needs to see what
            // each call would do, then the command. The event names the
            // calls; the listing has their arguments. A call it no longer
            // lists was decided already.
            "approval_requested" => {
                self.flush();
                let bot = event["bot"].as_str().unwrap_or("");
                let wanted: Vec<&str> = data["calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|call| call["call_id"].as_str())
                    .collect();
                for call in pending(connection, bot, event["turn"].as_i64(), &wanted)? {
                    println!("{}", self.dim(&format!("⏸ {}", call_line(&call))));
                    for line in answer_lines(&call, &self.target) {
                        println!("{}", self.dim(&line));
                    }
                }
            }
            "tool_completed" => {
                let bot = event["bot"].as_str().unwrap_or("");
                if let Some(node) = data["node"].as_i64()
                    && let Ok(read) =
                        connection.request("history_items", json!({"bot":bot,"nodes":[node]}))
                {
                    let item = &read["items"][0]["item"];
                    let output = item["output"]
                        .as_str()
                        .or_else(|| item["content"][0]["content"].as_str())
                        .unwrap_or("");
                    for line in preview(output).lines() {
                        println!("{}", self.dim(&format!("  {}", streamed(line))));
                    }
                }
            }
            "usage" => {
                self.usage.0 += data["input_tokens"].as_u64().unwrap_or(0);
                self.usage.1 += data["output_tokens"].as_u64().unwrap_or(0);
                self.usage.2 += data["cached_input_tokens"].as_u64().unwrap_or(0);
            }
            "turn_finished" if finished => {
                self.flush();
                let status = data["status"].as_str().unwrap_or("unknown");
                let (input, output, cached) = self.usage;
                let tokens = if input + output > 0 {
                    format!(" · tokens in {input} (cached {cached}) out {output}")
                } else {
                    String::new()
                };
                match status {
                    "completed" => eprintln!("agent: ✔ completed{tokens}"),
                    "steered" => eprintln!("agent: ✔ steered into turn {}", data["into"]),
                    _ => eprintln!(
                        "agent: ✘ {status}{}{}{tokens}",
                        data["error"]
                            .as_str()
                            .map(|e| format!(": {e}"))
                            .unwrap_or_default(),
                        data["detail"]
                            .as_str()
                            .map(|d| format!(" ({d})"))
                            .unwrap_or_default()
                    ),
                }
                return Ok(Some(exit_code(data)));
            }
            "follow_lagged" => eprintln!("agent: event stream lagged; re-follow from your cursor"),
            _ => {}
        }
        Ok(None)
    }
}

/// Program status (OSC 7501) for a terminal: what the followed turn, or
/// each bot under `follow --all`, is doing, so a terminal can show a bot
/// waiting on an approval or done while its tab is in the background. A
/// state is written only when it changes. A selected turn's every state is
/// news, replayed or live: `run` may see its turn end before the stream is
/// live. Under `follow --all`, history replayed before `follow_live` writes
/// only what is still going on; a turn that ended before is not news.
struct Status {
    per_bot: bool,
    live: bool,
    /// Each bot with a turn in flight; an ended turn leaves nothing behind.
    bots: HashMap<String, Record>,
}

struct Record {
    turn: Option<i64>,
    state: &'static str,
    /// Whether the terminal has this state.
    written: bool,
    /// The turn's announced calls a person answers (the `manual` gate);
    /// other gates are a program's to answer, so their wait is work.
    manual: Vec<String>,
}

impl Status {
    fn new(per_bot: bool) -> Self {
        Self {
            per_bot,
            live: false,
            bots: HashMap::new(),
        }
    }

    /// The reports this event calls for, as escape sequences.
    fn event(&mut self, event: &Value) -> String {
        if event["event"] == "follow_live" {
            self.live = true;
            let mut out = String::new();
            for (bot, record) in &mut self.bots {
                if !record.written {
                    out.push_str(&report(self.per_bot, bot, record.state));
                    record.written = true;
                }
            }
            return out;
        }
        let Some(bot) = event["bot"].as_str() else {
            return String::new();
        };
        let kind = event["event"].as_str().unwrap_or("");
        let turn = event["turn"].as_i64();
        let data = &event["data"];
        // Only the running turn speaks for its bot: one queued behind it,
        // or one that ends before it starts, changes nothing.
        if matches!(kind, "queued" | "turn_finished")
            && self.bots.get(bot).is_some_and(|known| known.turn != turn)
        {
            return String::new();
        }
        let ended = match kind {
            "turn_finished" => Some(match data["status"].as_str() {
                Some("completed") => "done",
                Some("interrupted") => "idle",
                // The steer's message joined a turn that goes on.
                Some("steered") => return String::new(),
                _ => "error",
            }),
            // Only a bot's own record goes; a clear without an id would
            // remove every record on the terminal.
            "deleted" if self.per_bot => Some("clear"),
            "accepted" | "queued" | "steered" | "tool_started" | "tool_completed"
            | "approval_requested" | "turn_waiting" | "turn_paced" | "turn_resumed"
            | "text_delta" | "thinking_delta" => None,
            _ => return String::new(),
        };
        if let Some(state) = ended {
            // A turn that ended before the stream is live is not news.
            self.bots.remove(bot);
            return if self.live || !self.per_bot {
                report(self.per_bot, bot, state)
            } else {
                String::new()
            };
        }
        if !self.bots.contains_key(bot) {
            self.bots.insert(
                bot.to_owned(),
                Record {
                    turn,
                    state: "",
                    written: false,
                    manual: Vec::new(),
                },
            );
        }
        let Some(record) = self.bots.get_mut(bot) else {
            return String::new();
        };
        if record.turn != turn {
            record.turn = turn;
            record.manual.clear();
        }
        let state = match kind {
            "approval_requested" => {
                for call in data["calls"].as_array().into_iter().flatten() {
                    let manual = call["gates"]
                        .as_array()
                        .is_some_and(|gates| gates.iter().any(|tag| tag == "manual"));
                    if let Some(id) = call["call_id"].as_str().filter(|_| manual)
                        && !record.manual.iter().any(|known| known == id)
                    {
                        record.manual.push(id.to_owned());
                    }
                }
                if record.manual.is_empty() {
                    "working"
                } else {
                    "blocked"
                }
            }
            // A turn waits on a gate's verdict or on other turns and processes.
            "turn_waiting"
                if data["approval"] == true
                    && record
                        .manual
                        .iter()
                        .any(|id| data["call_id"] == id.as_str()) =>
            {
                "blocked"
            }
            "tool_started" | "tool_completed" => {
                record.manual.retain(|id| data["call_id"] != id.as_str());
                "working"
            }
            _ => "working",
        };
        if record.state == state {
            return String::new();
        }
        record.state = state;
        record.written = self.live || !self.per_bot;
        if record.written {
            report(self.per_bot, bot, state)
        } else {
            String::new()
        }
    }
}

/// One OSC 7501 report. Under `follow --all` the record's id is the bot's
/// name, which bot names' characters always satisfy; a name past the 32
/// bytes an id segment allows keeps its start and a 64-bit hash of the
/// whole, so two such names share an id only by a negligible chance.
fn report(per_bot: bool, bot: &str, state: &str) -> String {
    use base64::Engine;
    let mut body = format!("state={state}");
    // Only a person's gate blocks: a program answers the others.
    if state == "blocked" {
        body.push_str(":kind=permission");
    }
    if per_bot {
        let id = if bot.len() <= 32 {
            bot.to_owned()
        } else {
            let hash = bot.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
            });
            format!("{}.{hash:016x}", &bot[..15])
        };
        body.push_str(&format!(":id={id}"));
    }
    if state != "clear" {
        let title = base64::engine::general_purpose::STANDARD.encode(bot);
        body.push_str(&format!(":app=agent:title={title}"));
    }
    format!("\x1b]7501;{body}\x1b\\")
}

fn exit_code(data: &Value) -> i32 {
    // A steer delivered into the running turn did what was asked.
    if data["status"] == "completed" || data["status"] == "steered" {
        0
    } else {
        1
    }
}

/// The arguments that say what a call of this tool does, if it has any:
/// the first is shown bare, a later one after its name. A `read` names a
/// file or another call's artifact.
fn summary_keys(name: &str) -> &'static [&'static str] {
    match name {
        "shell" => &["command"],
        "read" => &["path", "artifact"],
        "write" | "edit" => &["path"],
        _ => &[],
    }
}

/// What a file change writes, shown after its path: whoever allows a
/// `write` or an `edit` must see what it puts in the file, not only where.
fn change_keys(name: &str) -> &'static [&'static str] {
    match name {
        "write" => &["content"],
        "edit" => &["old", "new"],
        _ => &[],
    }
}

/// The first of `keys` that `field` finds, labeled when it is not the
/// first, or what is missing.
fn summary_field(
    keys: &[&str],
    mut field: impl FnMut(&str) -> Option<String>,
) -> std::result::Result<String, String> {
    keys.iter()
        .enumerate()
        .find_map(|(index, key)| {
            field(key).map(|text| match index {
                0 => text,
                _ => format!("{key} {text}"),
            })
        })
        .ok_or_else(|| keys.join(" or "))
}

/// A started call's arguments as one short line: the command or path when
/// the tool has one. Arguments may be a preview cut short, so the field is
/// read from as much of the text as there is.
/// A started call as `run --pretty` shows it. The name is the model's too:
/// a call to a tool that does not exist still starts, and fails after.
fn tool_line(name: &str, arguments: &str) -> String {
    format!("▸ {} {}", visible(name), summary(name, arguments))
}

fn summary(name: &str, arguments: &str) -> String {
    let keys = summary_keys(name);
    let text = if keys.is_empty() {
        arguments.to_owned()
    } else {
        let parsed = serde_json::from_str::<Value>(arguments).ok();
        summary_field(keys, |key| match &parsed {
            Some(args) => args[key].as_str().map(str::to_owned),
            None => preview_field(arguments, key),
        })
        .unwrap_or_else(|missing| format!("[no {missing} in the arguments shown]"))
    };
    one_line(&text, false)
}

/// The first line of `text`, marked when more lines follow or it was cut,
/// with anything a terminal would act on shown escaped instead.
fn one_line(text: &str, cut: bool) -> String {
    let mut lines = text.lines();
    let first: String = lines.next().unwrap_or("").chars().take(200).collect();
    let rest = lines.count();
    let cut = cut || (rest == 0 && first.len() < text.trim_end().len());
    let first = visible(&first);
    if rest > 0 {
        format!(
            "{first} … (+{rest} more lines{})",
            if cut { ", then cut" } else { "" }
        )
    } else if cut {
        format!("{first} …")
    } else {
        first
    }
}

/// Every line of a pending call's field, each escaped: a later line of a
/// command runs too, so whoever allows it must see it. The field is the
/// listing's preview, at most 2,048 characters, which bounds the display;
/// a field cut short ends marked.
fn every_line(text: &str, cut: bool) -> String {
    let mut shown = text.lines().map(visible).collect::<Vec<_>>().join("\n  │ ");
    if cut {
        shown.push_str(" …");
    }
    shown
}

/// Model-written text as a terminal should show it: control characters
/// and bidirectional overrides escaped, so a call cannot clear the screen
/// or reorder what the person reads before they allow it.
fn visible(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for c in text.chars() {
        if acted_on(c) {
            shown.extend(c.escape_default());
        } else {
            shown.push(c);
        }
    }
    shown
}

/// Streamed model text or tool output as a terminal should show it: line
/// breaks and tabs kept, anything else a terminal acts on escaped, so it
/// cannot hide or restyle what follows, such as a call awaiting approval.
fn streamed(text: &str) -> std::borrow::Cow<'_, str> {
    let kept = |c: char| c == '\n' || c == '\t' || !acted_on(c);
    if text.chars().all(kept) {
        return text.into();
    }
    let mut shown = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if kept(c) {
            shown.push(c);
        } else {
            shown.extend(c.escape_default());
        }
    }
    shown.into()
}

/// A character a terminal acts on rather than shows as it stands: a
/// control character, or one of Unicode's bidirectional controls (marks,
/// embeddings, overrides, and isolates).
fn acted_on(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// A top-level string field of JSON text that may be cut short, decoded as
/// far as the text goes.
fn preview_field(text: &str, key: &str) -> Option<String> {
    let mut rest = text.trim_start().strip_prefix('{')?;
    loop {
        rest = rest.trim_start();
        if !rest.starts_with('"') {
            return None;
        }
        let end = value_end(rest)?;
        let (name, _) = agent_runtime::codec::json_string_prefix(&rest[..end], usize::MAX);
        rest = rest[end..].trim_start().strip_prefix(':')?.trim_start();
        if name == key {
            return rest
                .starts_with('"')
                .then(|| agent_runtime::codec::json_string_prefix(rest, usize::MAX).0);
        }
        rest = rest[value_end(rest)?..].trim_start().strip_prefix(',')?;
    }
}

/// Where the JSON value at the start of `text` ends, or `None` when the
/// text is cut before it does.
fn value_end(text: &str) -> Option<usize> {
    let (mut depth, mut quoted, mut escaped) = (0usize, false, false);
    for (i, c) in text.char_indices() {
        if quoted {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => {
                    quoted = false;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            '{' | '[' => depth += 1,
            '}' | ']' | ',' if depth == 0 => return Some(i),
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Calls of one turn still waiting on a gate, as `approvals` lists them,
/// in the order given.
fn pending(
    connection: &mut Connection,
    bot: &str,
    turn: Option<i64>,
    call_ids: &[&str],
) -> Result<Vec<Value>> {
    let mut found: Vec<Value> = Vec::new();
    let mut after = json!(0);
    while !after.is_null() && found.len() < call_ids.len() {
        let page = connection.request("approvals", json!({"bot":bot,"after":after,"limit":256}))?;
        found.extend(
            page["approvals"]
                .as_array()
                .ok_or(Error::new("daemon_protocol_mismatch"))?
                .iter()
                .filter(|call| {
                    call["turn"].as_i64() == turn
                        && call["call_id"]
                            .as_str()
                            .is_some_and(|id| call_ids.contains(&id))
                })
                .cloned(),
        );
        after = page["next_after"].clone();
    }
    found.sort_by_key(|call| {
        call_ids
            .iter()
            .position(|id| call["call_id"].as_str() == Some(*id))
    });
    Ok(found)
}

/// A pending call's tool and what it would do, from the fields `approvals`
/// lists, each cut on its own.
fn call_line(call: &Value) -> String {
    let name = call["name"].as_str().unwrap_or("tool");
    let arguments = &call["arguments"];
    let cut = |key: &str| {
        call["arguments_cut"]
            .as_array()
            .is_some_and(|cut| cut.iter().any(|field| field == key))
    };
    let shown = match summary_keys(name) {
        _ if !arguments.is_object() => "[arguments are not a JSON object]".to_owned(),
        [] => one_line(
            &arguments.to_string(),
            call["arguments_cut"]
                .as_array()
                .is_some_and(|cut| !cut.is_empty())
                || call["arguments_omitted"].as_u64().unwrap_or(0) > 0,
        ),
        keys => {
            let mut shown = summary_field(keys, |key| {
                arguments[key]
                    .as_str()
                    .map(|text| every_line(text, cut(key)))
            })
            .unwrap_or_else(|missing| format!("[no {missing} in the arguments]"));
            for key in change_keys(name) {
                if let Some(text) = arguments[key].as_str() {
                    shown.push_str(&format!("\n  │ {key}:"));
                    if !text.is_empty() || cut(key) {
                        shown.push_str(&format!("\n  │ {}", every_line(text, cut(key))));
                    }
                }
            }
            if arguments["replace_all"] == true {
                shown.push_str("\n  │ replace_all: true");
            }
            shown
        }
    };
    format!("{} {shown}", visible(name))
}

/// One copyable command per gate a pending call still waits on, each
/// naming its gate, since a call with several gates needs `--tag`, and the
/// daemon, since ids mean nothing in another store.
fn answer_lines(call: &Value, target: &str) -> Vec<String> {
    call["gates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .flat_map(|tag| {
            // One whole command per verdict: a placeholder such as
            // `allow|deny` would run as a pipeline that allows. The id goes
            // after `=`, so one that starts with `--` is still its value.
            let command = format!(
                "agent answer{target} --bot {} --turn {} --call={} --request {} --tag {}",
                shell_word(call["bot"].as_str().unwrap_or("")),
                call["turn"],
                shell_word(call["call_id"].as_str().unwrap_or("")),
                call["request"],
                shell_word(tag),
            );
            [
                format!("  waits for {tag}"),
                format!("    {command} allow"),
                format!("    {command} deny"),
            ]
        })
        .collect()
}

/// The flags that reach this daemon from any shell, for commands printed
/// for a person to run: none when the store and socket are the defaults.
fn target(store: &Path, socket: &Path, home_store: Option<&Path>) -> String {
    let mut flags = String::new();
    if home_store != Some(store) {
        flags.push_str(" --store ");
        flags.push_str(&shell_path(store));
    }
    if crate::client_path::default_socket(store).ok().as_deref() != Some(socket) {
        flags.push_str(" --socket ");
        flags.push_str(&shell_path(socket));
    }
    flags
}

/// A path as one shell word, made absolute so it holds from any directory;
/// bytes that are not UTF-8 are written as escapes.
fn shell_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(text) = path.to_str() {
        return shell_word(text);
    }
    let mut word = String::from("$'");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/._-".contains(&byte) {
            word.push(byte as char);
        } else {
            word.push_str(&format!("\\x{byte:02x}"));
        }
    }
    word.push('\'');
    word
}

/// A value as one shell word, for a command a person copies: bare when it
/// is plainly safe, single-quoted otherwise, and ANSI-C quoted when it has
/// characters a terminal would act on, which would otherwise reach it raw.
fn shell_word(value: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_-.,:/@%+=".contains(c);
    if !value.is_empty() && value.chars().all(plain) {
        return value.to_owned();
    }
    if !value.chars().any(acted_on) {
        return format!("'{}'", value.replace('\'', r"'\''"));
    }
    let mut word = String::from("$'");
    for c in value.chars() {
        match c {
            '\\' | '\'' => {
                word.push('\\');
                word.push(c);
            }
            c if acted_on(c) => {
                for byte in c.encode_utf8(&mut [0; 4]).bytes() {
                    word.push_str(&format!("\\x{byte:02x}"));
                }
            }
            c => word.push(c),
        }
    }
    word.push('\'');
    word
}

/// A bounded, readable slice of a tool result for the terminal.
fn preview(output: &str) -> String {
    let text = match serde_json::from_str::<Value>(output) {
        Ok(value) if value.get("stdout").is_some() => {
            let mut text = value["stdout"].as_str().unwrap_or("").to_owned();
            if let Some(stderr) = value["stderr"].as_str().filter(|s| !s.is_empty()) {
                text.push_str("\n[stderr] ");
                text.push_str(stderr);
            }
            if value["success"] == false {
                text.push_str(&format!("\n[exit {}]", value["exit_code"]));
            }
            text
        }
        Ok(value) if value.get("error").is_some() => format!(
            "[error] {}{}",
            value["error"].as_str().unwrap_or(""),
            value["detail"]
                .as_str()
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        ),
        _ => output.to_owned(),
    };
    let mut shown: Vec<&str> = text.lines().take(20).collect();
    let total = text.lines().count();
    if total > 20 {
        shown.push("…");
    }
    shown
        .iter()
        .map(|line| line.chars().take(200).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_past_capacity_give_the_flags_that_get_past_them() {
        let full = ways_past_busy(
            "Bob",
            Error::with("active_agent_limit", "delivery queue waits for room"),
        );
        assert_eq!(
            full.facts.unwrap()["hint"],
            "resend with --delivery queue to run it when there is room"
        );
        let busy = ways_past_busy(
            "Bob",
            Error::new("bot_busy").facts(json!({"running_turn":4})),
        );
        assert_eq!(busy.detail.as_deref(), Some("turn 4 is running"));
        assert!(
            busy.facts.unwrap()["hint"]
                .as_str()
                .unwrap()
                .starts_with("resend with --delivery steer --turn 4")
        );
    }

    #[test]
    fn summaries_read_the_field_from_a_cut_preview_and_mark_what_is_hidden() {
        let long = format!("{{\"content\":\"{}", "x".repeat(64));
        assert_eq!(
            summary("write", r#"{"path":"a \"b\".txt","content":"xx"#),
            r#"a "b".txt"#
        );
        assert_eq!(summary("write", &long), "[no path in the arguments shown]");
        assert_eq!(
            summary(
                "write",
                r#"{"content":"{\"path\":\"fake\"}","path":"real"}"#
            ),
            "real"
        );
        assert_eq!(
            summary(
                "shell",
                r#"{"timeout_ms":5,"nested":{"a":[1,"]"]},"command":"echo hi\nrm -r x"#
            ),
            "echo hi … (+1 more lines)"
        );
        assert_eq!(summary("shell", r#"{"command":"ls"}"#), "ls");
        assert_eq!(summary("shell", r#"{"command":"ec"#), "ec");
        // A read names a file or another call's artifact.
        assert_eq!(
            summary("read", r#"{"artifact":"3/call_1/stdout","offset":1}"#),
            "artifact 3/call_1/stdout"
        );
        assert_eq!(summary("read", r#"{"path":"a.txt"}"#), "a.txt");
        assert_eq!(
            summary("read", r#"{"offset":1}"#),
            "[no path or artifact in the arguments shown]"
        );
    }

    #[test]
    fn a_tool_name_is_shown_escaped() {
        // An unfinished OSC would swallow what prints next.
        let name = "\u{1b}]0;x";
        assert_eq!(tool_line(name, "{}"), r"▸ \u{1b}]0;x {}");
        let call = json!({"name": name, "arguments": {"a": 1}});
        assert_eq!(call_line(&call), r#"\u{1b}]0;x {"a":1}"#);
    }

    #[test]
    fn program_status_reports_each_change_once_and_skips_ended_history() {
        let at = |bot: &str, turn: i64, event: &str, data: Value| json!({"event":event,"bot":bot,"turn":turn,"data":data});
        let ev = |bot: &str, event: &str, status: Option<&str>| {
            at(bot, 1, event, json!({"status":status}))
        };
        let ask = |gates: Value| json!({"calls":[{"call_id":"c1","request":1,"gates":gates}]});
        let gate = json!({"call_id":"c1","approval":true});
        let live = json!({"event":"follow_live","bot":"*"});
        // `run`: its own turn as the root record, replayed or live.
        let mut one = Status::new(false);
        assert_eq!(
            one.event(&ev("demo", "accepted", None)),
            "\x1b]7501;state=working:app=agent:title=ZGVtbw==\x1b\\"
        );
        assert_eq!(one.event(&live), "");
        assert_eq!(one.event(&ev("demo", "text_delta", None)), "");
        assert!(
            one.event(&at("demo", 1, "approval_requested", ask(json!(["manual"]))))
                .contains("state=blocked:kind=permission:")
        );
        // The turn then waits on the person: still blocked, nothing new to say.
        assert_eq!(one.event(&at("demo", 1, "turn_waiting", gate.clone())), "");
        assert!(
            one.event(&at("demo", 1, "tool_started", json!({"call_id":"c1"})))
                .contains("state=working:")
        );
        assert!(
            one.event(&ev("demo", "turn_finished", Some("completed")))
                .contains("state=done:")
        );
        // A turn that ends before the stream is live is still `run`'s news.
        let mut fast = Status::new(false);
        fast.event(&ev("demo", "accepted", None));
        assert!(
            fast.event(&ev("demo", "turn_finished", Some("failed")))
                .contains("state=error:")
        );
        // A program's gate is work, not something to ask the person about.
        let mut auto = Status::new(false);
        auto.event(&ev("demo", "accepted", None));
        assert_eq!(
            auto.event(&at("demo", 1, "approval_requested", ask(json!(["auto"])))),
            ""
        );
        assert_eq!(auto.event(&at("demo", 1, "turn_waiting", gate.clone())), "");

        // `follow --all`: one id per bot; only what still runs comes from history.
        let mut all = Status::new(true);
        all.event(&ev("old", "accepted", None));
        all.event(&ev("old", "turn_finished", Some("completed")));
        all.event(&ev("busy", "accepted", None));
        assert_eq!(
            all.event(&live),
            "\x1b]7501;state=working:id=busy:app=agent:title=YnVzeQ==\x1b\\"
        );
        // A turn queued behind a blocked one, then cancelled, leaves it blocked.
        assert!(
            all.event(&at("busy", 1, "approval_requested", ask(json!(["manual"]))))
                .contains("state=blocked:kind=permission:id=busy:")
        );
        assert_eq!(all.event(&at("busy", 2, "queued", json!({}))), "");
        assert_eq!(
            all.event(&at(
                "busy",
                2,
                "turn_finished",
                json!({"status":"interrupted"})
            )),
            ""
        );
        assert!(
            all.event(&ev("busy", "turn_finished", Some("interrupted")))
                .contains("state=idle:id=busy:")
        );
        // An ended turn leaves nothing to remember.
        assert!(all.bots.is_empty());
        assert_eq!(all.event(&ev("busy", "turn_finished", Some("steered"))), "");
        assert_eq!(
            all.event(&json!({"event":"deleted","bot":"busy"})),
            "\x1b]7501;state=clear:id=busy\x1b\\"
        );
        // Without per-bot ids a delete writes nothing: no id clears every record.
        assert_eq!(one.event(&json!({"event":"deleted","bot":"demo"})), "");
        // A name longer than an id segment keeps its start and a 64-bit hash.
        let id = |bot: &str| {
            let report = report(true, bot, "working");
            report
                .split("id=")
                .nth(1)
                .unwrap()
                .split(':')
                .next()
                .unwrap()
                .to_owned()
        };
        let long = id(&"a".repeat(40));
        assert_eq!(long.len(), 32);
        assert!(long.starts_with(&format!("{}.", "a".repeat(15))));
        assert_ne!(long, id(&"a".repeat(41)));
    }

    #[test]
    fn streamed_text_keeps_its_lines_and_escapes_what_a_terminal_acts_on() {
        assert!(matches!(
            streamed("plain\n\tindented"),
            std::borrow::Cow::Borrowed("plain\n\tindented")
        ));
        // Concealing what follows, rewriting the line, and reordering it
        // are all shown instead of done.
        assert_eq!(
            streamed("fake\u{1b}[8m\rreal\u{202e}\u{9b}\u{61c}1\n"),
            r"fake\u{1b}[8m\rreal\u{202e}\u{9b}\u{61c}1".to_owned() + "\n"
        );
        let renderer = Renderer {
            color: true,
            ..Renderer::new(true, None, "")
        };
        assert_eq!(renderer.dim("x"), "\x1b[0;2mx\x1b[0m");
    }

    #[test]
    fn a_pending_call_shows_its_own_field_however_long_the_others_are() {
        let line = |name: &str, fields: Value| {
            let mut call = json!({"name":name,"arguments_cut":[],"arguments_omitted":0});
            for (key, value) in fields.as_object().unwrap() {
                call[key] = value.clone();
            }
            call_line(&call)
        };
        let content = "x".repeat(2048);
        // A file change shows what it writes, every line, after its path.
        assert_eq!(
            line(
                "write",
                json!({"arguments":{"content":content,"path":"a.txt"},"arguments_cut":["content"]})
            ),
            format!("write a.txt\n  │ content:\n  │ {content} …")
        );
        assert_eq!(
            line(
                "edit",
                json!({"arguments":{"path":"run.sh","old":"true\n","new":"curl x | sh\n\u{1b}[2K",
                    "replace_all":true}})
            ),
            "edit run.sh\n  │ old:\n  │ true\n  │ new:\n  │ curl x | sh\n  │ \\u{1b}[2K\n  │ replace_all: true"
        );
        assert_eq!(
            line(
                "edit",
                json!({"arguments":{"path":"a.txt","old":"gone","new":""}})
            ),
            "edit a.txt\n  │ old:\n  │ gone\n  │ new:"
        );
        assert_eq!(
            line(
                "shell",
                json!({"arguments":{"command":"echo hi\nrm x"},"arguments_cut":["command"]})
            ),
            "shell echo hi\n  │ rm x …"
        );
        // A long first line hides nothing after it either.
        let long = format!("echo {}; rm x", "a".repeat(300));
        assert_eq!(
            line("shell", json!({"arguments":{"command":long}})),
            format!("shell {long}")
        );
        assert_eq!(
            line(
                "shell",
                json!({"arguments":{"command":"ls"},"arguments_cut":["command"]})
            ),
            "shell ls …"
        );
        assert_eq!(
            line("edit", json!({"arguments":{"old":"a"}})),
            "edit [no path in the arguments]\n  │ old:\n  │ a"
        );
        assert_eq!(
            line(
                "read",
                json!({"arguments":{"artifact":"3/call_1/stdout","offset":1}})
            ),
            "read artifact 3/call_1/stdout"
        );
        assert_eq!(
            line("read", json!({"arguments":{"offset":1}})),
            "read [no path or artifact in the arguments]"
        );
        assert_eq!(
            line("shell", json!({"arguments":null})),
            "shell [arguments are not a JSON object]"
        );
        // What a terminal would act on is shown, not sent to it.
        assert_eq!(
            line(
                "shell",
                json!({"arguments":{"command":"\u{1b}[2Jrm x\r\u{202e}txt.exe"}})
            ),
            r"shell \u{1b}[2Jrm x\r\u{202e}txt.exe"
        );
        assert_eq!(
            line(
                "echo",
                json!({"arguments":{"text":"hi"},"arguments_omitted":1})
            ),
            r#"echo {"text":"hi"} …"#
        );
    }

    #[test]
    fn every_open_gate_gets_its_own_answer_command() {
        let call = json!({"bot":"Bob","turn":7,"call_id":"c 1","request":2,
            "gates":["manual","second"],"name":"shell","arguments":{"command":"ls"},
            "arguments_cut":[],"arguments_omitted":0});
        assert_eq!(call_line(&call), "shell ls");
        assert_eq!(
            answer_lines(&call, ""),
            [
                "  waits for manual",
                "    agent answer --bot Bob --turn 7 --call='c 1' --request 2 --tag manual allow",
                "    agent answer --bot Bob --turn 7 --call='c 1' --request 2 --tag manual deny",
                "  waits for second",
                "    agent answer --bot Bob --turn 7 --call='c 1' --request 2 --tag second allow",
                "    agent answer --bot Bob --turn 7 --call='c 1' --request 2 --tag second deny",
            ]
        );
        // An id that reads as a flag is still the value of `--call`.
        let flag = json!({"bot":"Bob","turn":7,"call_id":"--help","request":1,"gates":["manual"]});
        assert_eq!(
            answer_lines(&flag, "")[1],
            "    agent answer --bot Bob --turn 7 --call=--help --request 1 --tag manual allow"
        );
    }

    #[test]
    fn printed_commands_name_a_daemon_that_is_not_the_default() {
        let home = Path::new("/home/a/.agent/state.sqlite");
        let other = Path::new("/tmp/x y/state.sqlite");
        let socket = |store| crate::client_path::default_socket(store).unwrap();
        assert_eq!(target(home, &socket(home), Some(home)), "");
        assert_eq!(
            target(other, &socket(other), Some(home)),
            " --store '/tmp/x y/state.sqlite'"
        );
        assert_eq!(
            target(home, Path::new("/run/a.sock"), Some(home)),
            " --socket /run/a.sock"
        );
        // Without HOME there is no default store to leave out.
        assert!(target(home, &socket(home), None).starts_with(" --store /home/a/"));
        let call = json!({"bot":"Bob","turn":7,"call_id":"c1","request":1,"gates":["manual"]});
        assert_eq!(
            answer_lines(&call, " --store /s")[2],
            "    agent answer --store /s --bot Bob --turn 7 --call=c1 --request 1 --tag manual deny"
        );
    }

    #[test]
    fn shell_words_keep_ids_one_argument() {
        assert_eq!(shell_word("call_Ab-9.x"), "call_Ab-9.x");
        assert_eq!(shell_word(""), "''");
        assert_eq!(shell_word("a b"), "'a b'");
        assert_eq!(shell_word("$(touch x)"), "'$(touch x)'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(shell_word("a\nb'\x1b"), r"$'a\x0ab\'\x1b'");
        // A bidi override would reorder the flags printed after it.
        assert_eq!(shell_word("a\u{202e}b"), r"$'a\xe2\x80\xaeb'");
        // So would the Arabic letter mark, which is no control character.
        assert_eq!(shell_word("a\u{61c}1"), r"$'a\xd8\x9c1'");
    }

    #[test]
    fn agent_provider_names_the_providers_in_place_of_key_variables() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let keys = &[("ANTHROPIC_API_KEY", "k"), ("OPENAI_API_KEY", "k")];
        assert_eq!(environment_providers(&env(keys)), ["anthropic", "openai"]);
        assert_eq!(
            environment_providers(&env(&[
                (
                    "AGENT_PROVIDER",
                    " bedrock  b=anthropic,https://bedrock-runtime.us-east-1.amazonaws.com/anthropic/v1\n"
                ),
                ("ANTHROPIC_API_KEY", "k"),
            ])),
            [
                "bedrock",
                "b=anthropic,https://bedrock-runtime.us-east-1.amazonaws.com/anthropic/v1"
            ]
        );
        assert_eq!(
            environment_providers(&env(&[("AGENT_PROVIDER", " "), ("OPENAI_API_KEY", "k")])),
            ["openai"]
        );
        assert!(environment_providers(&env(&[("OPENAI_API_KEY", "")])).is_empty());
    }

    /// A client asking for SigV4 must not attach to a daemon that sends a
    /// Bedrock API key to the same URL, nor the other way round.
    #[test]
    fn attach_compares_how_a_bedrock_binding_authenticates() {
        let url = "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1";
        let args = |spec: &str| {
            ["stats", "--store", "s", "--provider", spec]
                .map(str::to_owned)
                .to_vec()
        };
        let signed = parse(&args(&format!("b=anthropic,{url}"))).unwrap();
        let keyed = parse(&args(&format!("b=anthropic,{url},BEDROCK_KEY"))).unwrap();
        let ready = |auth: Option<&str>| {
            let mut binding = json!({"family":"anthropic","url":url,"transport":"http"});
            if let Some(auth) = auth {
                binding["auth"] = json!(auth);
            }
            json!({"providers":{"b":binding}})
        };
        assert!(check_daemon(&signed, &ready(Some("sigv4"))).is_ok());
        assert!(check_daemon(&keyed, &ready(None)).is_ok());
        let refused = check_daemon(&signed, &ready(None)).unwrap_err();
        assert_eq!(refused.code, "daemon_configuration_mismatch");
        assert!(
            refused
                .detail
                .unwrap()
                .contains("over http signed with SigV4 but daemon has")
        );
        assert!(check_daemon(&keyed, &ready(Some("sigv4"))).is_err());
    }

    #[test]
    fn readiness_preserves_buffered_events_and_allows_later_events() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut connection = Connection {
            writer: stream.try_clone().unwrap(),
            reader: BufReader::new(stream),
            next: 0,
            pending: VecDeque::new(),
            ready: Value::Null,
        };
        let deadline = Instant::now() + Duration::from_millis(500);
        let writer = std::thread::spawn(move || {
            peer.write_all(b"{\"event\":\"ready\"}\n{\"event\":\"buffered\"}\n")
                .unwrap();
            std::thread::sleep(Duration::from_millis(750));
            let _ = peer.write_all(b"{\"event\":\"later\"}\n");
        });
        assert_eq!(connection.read_ready(deadline).unwrap()["event"], "ready");
        assert_eq!(connection.next_event().unwrap()["event"], "buffered");
        assert_eq!(connection.next_event().unwrap()["event"], "later");
        assert!(Instant::now() > deadline);
        writer.join().unwrap();
    }
}
