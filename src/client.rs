//! The `agent` client: a build-like command over the shared daemon. It
//! connects to the daemon's Unix socket, starting one when none is running,
//! then relays the bot's event stream until the requested turn ends. The
//! JSONL stream is the contract for programs; `--pretty` is a human view.
use agent_runtime::{Error, Result, fail, fail_with};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, IsTerminal, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEFAULT_TOOLS: &str = "shell,read,write,edit,wait,history";
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
    after: i64,
    pretty: bool,
    new: bool,
    agents: bool,
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
    /// Daemon limits forwarded when this client starts the daemon.
    daemon_flags: Vec<(String, String)>,
    positional: Vec<String>,
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
        after: 0,
        pretty: false,
        new: false,
        agents: false,
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
        daemon_flags: Vec::new(),
        positional: Vec::new(),
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
                    "--delivery" => options.delivery = Some(value),
                    "--approval" => options.approval = Some(value),
                    "--approve" => options.approve = Some(value),
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
                    | "--max-output-tokens"
                    | "--stall-timeout"
                    | "--keep-warm"
                    | "--idle-exit"
                    | "--context-bytes"
                    | "--context-items"
                    | "--note-turns"
                    | "--compact-at"
                    | "--compact-keep"
                    | "--retain-turns"
                    | "--approval-hold-ms" => {
                        value.parse::<usize>().map_err(|_| {
                            Error::with("usage", format!("{flag} needs an integer"))
                        })?;
                        options.daemon_flags.push((flag.to_owned(), value));
                    }
                    "--cache-ttl" => {
                        if !matches!(value.as_str(), "5m" | "1h") {
                            return fail_with("usage", "--cache-ttl needs 5m or 1h");
                        }
                        options.daemon_flags.push((flag.to_owned(), value));
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
    options.store = match store.or_else(|| std::env::var_os("AGENT_STORE").map(PathBuf::from)) {
        Some(store) => store,
        None => std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".agent").join("state.sqlite"))
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
                return match message.get("error").and_then(Value::as_str) {
                    Some(code) => Err(Error {
                        code: code.to_owned(),
                        detail: message["detail"].as_str().map(str::to_owned),
                    }),
                    None => Ok(message["result"].clone()),
                };
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
    // Inspection must never initialize a replacement store for a missing one.
    if !options.store.is_file() {
        return fail("store_not_found");
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
        if flag == "--cache-ttl" {
            let running = ready["limits"]["cache_ttl"].as_str().unwrap_or("no value");
            if running != value {
                differences.push(format!(
                    "{flag}: requested {value} but daemon has {running}"
                ));
            }
            continue;
        }
        let key = match flag.as_str() {
            "--max-processes" => "processes",
            "--max-detached" => "detached",
            "--max-active" => "active",
            "--max-connecting" => "connecting",
            "--max-pending" => "pending",
            "--max-pending-bytes" => "pending_bytes",
            "--max-output-tokens" => "output_tokens",
            "--stall-timeout" => "stall_timeout_seconds",
            "--keep-warm" => "keep_warm_seconds",
            "--idle-exit" => "idle_exit_seconds",
            "--context-bytes" => "context_bytes",
            "--context-items" => "context_items",
            "--note-turns" => "note_turns",
            "--compact-at" => "compact_at",
            "--compact-keep" => "compact_keep",
            "--retain-turns" => "retain_turns",
            "--approval-hold-ms" => "approval_hold_ms",
            _ => continue,
        };
        let running = &ready["limits"][key];
        let requested = value
            .parse::<u64>()
            .ok()
            .map(|value| match flag.as_str() {
                "--context-bytes" if value > 0 => {
                    value.max(crate::server::MIN_CONTEXT_BYTES as u64)
                }
                "--context-items" if value > 0 => {
                    value.max(crate::server::MIN_CONTEXT_ITEMS as u64)
                }
                _ => value,
            })
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
        Err(error) if error.code == "daemon_start_timeout" => Err(error),
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
/// workspace; otherwise the preamble alone.
fn composed_instructions(options: &Options, workspace: &str) -> Result<String> {
    if options.agents && options.instructions.is_some() {
        return fail_with(
            "usage",
            "--agents and --instructions are mutually exclusive",
        );
    }
    if let Some(text) = &options.instructions {
        return Ok(text.clone());
    }
    if options.agents {
        return agent_client::policy::instructions(std::path::Path::new(workspace))
            .map(|composed| composed.text)
            .map_err(|error| Error::with(error.code(), error.to_string()));
    }
    Ok(DEFAULT_INSTRUCTIONS.to_owned())
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

/// Tools the approval modes never gate: they touch only the bot's own
/// store records.
const UNGATED: [&str; 4] = ["history", "wait", "note", "echo"];

/// The gate a new bot asks for, as `create` or `fork` fields. The mode
/// comes from --approval or AGENT_APPROVAL, `full` by default: no gate.
/// `manual` gates --approve, or every tool the bot has but the four that
/// touch only its own records, for a person or a program to answer.
fn requested_gate(options: &Options, tools: &[String]) -> Result<Value> {
    match options.approval.as_deref().unwrap_or("full") {
        "full" if options.approve.is_some() => fail_with(
            "usage",
            "--approve names tools for an approver; use --approval manual",
        ),
        "full" => Ok(json!({})),
        "manual" => {
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
            if approve.is_empty() {
                return Ok(json!({}));
            }
            Ok(json!({"approve":approve,"approver":"manual"}))
        }
        "auto" => fail_with(
            "approval_mode_unsupported",
            "auto needs the automatic approver, which is not built yet; use manual or full",
        ),
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
        "result" => result(&options),
        "rm" => remove(&options),
        "prune" => prune(&options),
        "approvals" => approvals(&options),
        "answer" => answer(&options),
        "stats" => {
            let mut connection = ensure_existing_daemon(&options)?;
            let stats = connection.request("stats", json!({}))?;
            print_json(&stats, options.pretty)?;
            Ok(0)
        }
        "ls" => list(&options),
        "shutdown" => {
            let mut connection = Connection::connect(&options.socket)?;
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
        exists
            && std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
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
    let mut connection = ensure_daemon(options)?;
    let workspace = workspace(options)?;
    // A named bot is continued, never silently replaced: an unknown name is an
    // error unless --new asks for creation. No name means a fresh identity.
    let created = options.new || options.bot.is_none();
    if !created && options.tools_explicit {
        return fail_with(
            "usage",
            "--tools chooses a new bot's tools; an existing bot keeps its own",
        );
    }
    if !created && (options.instructions.is_some() || options.agents) {
        return fail_with(
            "usage",
            "--instructions and --agents set a new bot's instructions; an existing bot keeps its own",
        );
    }
    let bot = options.bot.clone().unwrap_or_else(|| unique("bot"));
    if created {
        // The client chooses; the bot retains. Nothing about a bot comes
        // from the daemon or from whichever client connects later.
        let model = options
            .model
            .clone()
            .or_else(|| std::env::var("AGENT_MODEL").ok())
            .ok_or(Error::with(
                "usage",
                "a new bot needs a model: pass --model PROVIDER/MODEL or set AGENT_MODEL",
            ))?;
        let instructions = composed_instructions(options, &workspace)?;
        let (created_by, created_by_id) = created_by()?;
        let tools: Vec<String> = options
            .tools
            .split(',')
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect();
        let mut create = json!({"bot":bot,"workspace":workspace,"model":model,
            "instructions":instructions,"reasoning":options.reasoning,
            "budget_tokens":options.budget_tokens,"tools":tools,
            "created_by":created_by,"created_by_id":created_by_id,
            "compaction_instructions":options.compaction_instructions,
            "compaction_model":options.compaction_model,"fallbacks":options.fallbacks});
        if let Value::Object(gate) = requested_gate(options, &tools)? {
            create.as_object_mut().expect("object").extend(gate);
        }
        connection.request("create", create)?;
    }
    let request_id = options.request_id.clone().unwrap_or_else(|| unique("run"));
    // Existing bots keep their model unless --model explicitly overrides it.
    // AGENT_MODEL is only a creation default, including inside a peer's shell.
    let submitted = connection.request(
        "submit",
        json!({"bot":bot,"bot_id":options.bot_id,"request_id":request_id,"prompt":prompt,
            "workspace":workspace,
            "model":if created { Value::Null } else { json!(options.model) },
            "delivery":options.delivery,"expected_turn":options.turn}),
    )?;
    if options.detach {
        print_json(&submitted, options.pretty)?;
        return Ok(0);
    }
    let turn = submitted["turn"]
        .as_i64()
        .ok_or(Error::new("daemon_protocol_mismatch"))?;
    let after = submitted["cursor"].as_i64().map(|c| c - 1).unwrap_or(0);
    connection.request("follow", json!({"bot":bot,"after":after}))?;
    let mut renderer = Renderer::new(options.pretty, Some(turn));
    if options.pretty {
        eprintln!(
            "agent: {bot} turn {turn}{} in {workspace}",
            if created { " (new bot)" } else { "" }
        );
    }
    loop {
        let event = connection.next_event()?;
        if let Some(code) = renderer.event(&mut connection, &event)? {
            return Ok(code);
        }
    }
}

fn follow(options: &Options) -> Result<i32> {
    if options.all {
        // Every bot's events from a store-wide cursor, then live, until the
        // connection ends: the fleet controller's view.
        let mut connection = Connection::connect(&options.socket)?;
        connection.request("follow", json!({"bot":"*","after":options.after}))?;
        let mut renderer = Renderer::new(options.pretty, None);
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
    let mut renderer = Renderer::new(options.pretty, turn);
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
            "fork needs --source and --bot; --checkpoint N picks a message, default is the current head",
        );
    };
    let checkpoint = options.checkpoint;
    let mut connection = Connection::connect(&options.socket)?;
    // A fork inherits only the conversation; its turns name their own workspace.
    let (created_by, created_by_id) = created_by()?;
    let mut request = json!({"source":source,"checkpoint":checkpoint,"bot":bot,
        "workspace":options.workspace.as_ref().map(|_| workspace(options)).transpose()?,
        "budget_tokens":options.budget_tokens,
        "created_by":created_by,"created_by_id":created_by_id});
    // A fork keeps its source's tools and gates; its own gate adds to them.
    if options.approval.as_deref().unwrap_or("full") != "full" || options.approve.is_some() {
        let state = connection.request("resume", json!({"bot":source}))?;
        let tools: Vec<String> = serde_json::from_value(state["tools"].clone())
            .map_err(|_| Error::new("daemon_protocol_mismatch"))?;
        if let Value::Object(gate) = requested_gate(options, &tools)? {
            request.as_object_mut().expect("object").extend(gate);
        }
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
        eprintln!("agent: {bot} has no running turn");
        return Ok(1);
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
        &connection.request("delete", json!({"bot":bot}))?,
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
                    "{} turn {} {} {}\n  waits for {} · agent answer --bot {} --turn {} --call {} --request {} allow|deny",
                    call["bot"].as_str().unwrap_or(""),
                    call["turn"],
                    call["name"].as_str().unwrap_or(""),
                    summary(
                        call["name"].as_str().unwrap_or(""),
                        call["arguments"].as_str().unwrap_or("")
                    ),
                    call["gates"]
                        .as_array()
                        .map(|gates| gates
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", "))
                        .unwrap_or_default(),
                    shell_word(call["bot"].as_str().unwrap_or("")),
                    call["turn"],
                    shell_word(call["call_id"].as_str().unwrap_or("")),
                    call["request"],
                );
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

/// A finished turn's outcome, or its live status. Exit 0 only when completed.
fn result(options: &Options) -> Result<i32> {
    let (Some(bot), Some(turn)) = (&options.bot, options.turn) else {
        return fail_with("usage", "result needs --bot and --turn");
    };
    let mut connection = ensure_existing_daemon(options)?;
    let outcome = connection.request("result", json!({"bot":bot,"turn":turn}))?;
    print_json(&outcome, options.pretty)?;
    Ok(if outcome["status"] == "completed" {
        0
    } else {
        1
    })
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
}

impl Renderer {
    fn new(pretty: bool, turn: Option<i64>) -> Self {
        Self {
            pretty,
            color: pretty && std::io::stdout().is_terminal(),
            mode: Mode::Idle,
            turn,
            usage: (0, 0, 0),
        }
    }
    fn dim(&self, text: &str) -> String {
        if self.color {
            format!("\x1b[2m{text}\x1b[0m")
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
        let mut stdout = std::io::stdout();
        let _ = match mode {
            Mode::Thinking => write!(stdout, "{}", self.dim(text)),
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
            connection.request("result", json!({"bot":event["bot"],"turn":turn}))?;
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
        let data = &event["data"];
        match event["event"].as_str().unwrap_or("") {
            "text_delta" => self.print(Mode::Text, event["text"].as_str().unwrap_or("")),
            "thinking_delta" => self.print(Mode::Thinking, event["text"].as_str().unwrap_or("")),
            "tool_started" => {
                self.flush();
                let name = data["name"].as_str().unwrap_or("tool");
                let summary = summary(name, data["arguments"].as_str().unwrap_or(""));
                println!("{}", self.dim(&format!("▸ {name} {summary}")));
            }
            // A person answering from another terminal needs the command.
            "approval_requested" => {
                self.flush();
                let bot = event["bot"].as_str().unwrap_or("");
                for call in data["calls"].as_array().into_iter().flatten() {
                    let gates = call["gates"]
                        .as_array()
                        .map(|gates| {
                            gates
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    println!(
                        "{}",
                        self.dim(&format!(
                            "⏸ {} waits for {gates} · agent answer --bot {} --turn {} --call {} --request {} allow|deny",
                            call["name"].as_str().unwrap_or("tool"),
                            shell_word(bot),
                            event["turn"],
                            shell_word(call["call_id"].as_str().unwrap_or("")),
                            call["request"],
                        ))
                    );
                }
            }
            "tool_completed" => {
                let bot = event["bot"].as_str().unwrap_or("");
                if let Some(node) = data["node"].as_i64()
                    && let Ok(item) = connection.request("item", json!({"bot":bot,"node":node}))
                {
                    let output = item["output"]
                        .as_str()
                        .or_else(|| item["content"][0]["content"].as_str())
                        .unwrap_or("");
                    for line in preview(output).lines() {
                        println!("{}", self.dim(&format!("  {line}")));
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

fn exit_code(data: &Value) -> i32 {
    // A steer delivered into the running turn did what was asked.
    if data["status"] == "completed" || data["status"] == "steered" {
        0
    } else {
        1
    }
}

/// A call's arguments as one short line: the command or path when the
/// tool has one.
fn summary(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let text = match name {
        "shell" => args["command"].as_str().unwrap_or(""),
        "read" | "write" | "edit" => args["path"].as_str().unwrap_or(""),
        _ => arguments,
    };
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

/// A value as one shell word, for a command a person copies: bare when it
/// is plainly safe, single-quoted otherwise, and ANSI-C quoted when it has
/// control characters, which would otherwise reach the terminal raw.
fn shell_word(value: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_-.,:/@%+=".contains(c);
    if !value.is_empty() && value.chars().all(plain) {
        return value.to_owned();
    }
    if !value.chars().any(char::is_control) {
        return format!("'{}'", value.replace('\'', r"'\''"));
    }
    let mut word = String::from("$'");
    for c in value.chars() {
        match c {
            '\\' | '\'' => {
                word.push('\\');
                word.push(c);
            }
            c if c.is_control() => {
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
    fn shell_words_keep_ids_one_argument() {
        assert_eq!(shell_word("call_Ab-9.x"), "call_Ab-9.x");
        assert_eq!(shell_word(""), "''");
        assert_eq!(shell_word("a b"), "'a b'");
        assert_eq!(shell_word("$(touch x)"), "'$(touch x)'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(shell_word("a\nb'\x1b"), r"$'a\x0ab\'\x1b'");
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
