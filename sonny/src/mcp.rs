//! The MCP servers in `[sonny.mcp_servers]`, which agents reach through
//! their scripts: the lookout keeps one connection per server for every
//! agent, started by the first call and stopped after ten idle minutes, as
//! Codex and Pi keep theirs beside the agent loop rather than in its
//! sandbox. Each server's tools are kept on disk once listed, so they are
//! known without starting it.
use agent_client::config::McpServer;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a server no call has used stays up.
pub const IDLE: Duration = Duration::from_secs(600);
/// How long a server has to start and list its tools, and a call to end,
/// unless its config says otherwise.
const START: Duration = Duration::from_secs(30);
const CALL: Duration = Duration::from_secs(60);
/// The last lines a stdio server wrote to stderr, for when it fails.
const STDERR_LINES: usize = 20;

pub type Servers = BTreeMap<String, McpServer>;

/// The servers, each connected while used.
#[derive(Clone)]
pub struct Hub(Arc<Inner>);

struct Inner {
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    cache: Option<PathBuf>,
}

struct Slot {
    name: String,
    config: McpServer,
    live: tokio::sync::Mutex<Option<Live>>,
    used: Mutex<Instant>,
    /// Calls in flight, which keep it up.
    busy: AtomicUsize,
}

struct Live {
    client: RunningService<RoleClient, ClientConfig>,
    tools: Vec<Value>,
    instructions: Option<String>,
    /// A stdio server's process group, stopped with it.
    group: Option<u32>,
}

/// What is known of a server: its tools and instructions, from its last
/// listing, and why it last failed to start.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Known {
    pub tools: Vec<Value>,
    pub instructions: Option<String>,
    pub error: Option<String>,
    pub live: bool,
}

impl Hub {
    /// No servers yet; listings are kept in `cache` when there is one.
    pub fn new(cache: Option<PathBuf>) -> Hub {
        Hub(Arc::new(Inner {
            slots: Mutex::default(),
            cache,
        }))
    }

    /// The person's servers, their listings kept in `cache_dir`.
    pub fn for_person() -> Hub {
        Hub::new(cache_dir())
    }

    /// Serve `servers` from now on: one removed, turned off or changed
    /// stops, and is started again as it now reads by its next call.
    pub async fn configure(&self, servers: &Servers) {
        let gone: Vec<Arc<Slot>> = {
            let mut slots = self.0.slots.lock().unwrap();
            let gone = slots
                .extract_if(|name, slot| {
                    servers
                        .get(name)
                        .is_none_or(|config| *config != slot.config || !config.enabled)
                })
                .map(|(_, slot)| slot)
                .collect();
            for (name, config) in servers.iter().filter(|(_, c)| c.enabled) {
                slots.entry(name.clone()).or_insert_with(|| {
                    Arc::new(Slot {
                        name: name.clone(),
                        config: config.clone(),
                        live: tokio::sync::Mutex::default(),
                        used: Mutex::new(Instant::now()),
                        busy: AtomicUsize::new(0),
                    })
                });
            }
            gone
        };
        for slot in gone {
            slot.stop().await;
        }
    }

    /// The servers served, by name.
    pub fn servers(&self) -> Servers {
        let slots = self.0.slots.lock().unwrap();
        (slots.iter())
            .map(|(name, slot)| (name.clone(), slot.config.clone()))
            .collect()
    }

    fn slot(&self, server: &str) -> Result<Arc<Slot>, String> {
        let slots = self.0.slots.lock().unwrap();
        slots
            .get(server)
            .cloned()
            .ok_or_else(|| format!("no MCP server {server}"))
    }

    /// What is known of `server` without starting it: what it said while
    /// up, else its last listing on disk, if made with the same config.
    pub async fn known(&self, server: &str) -> Result<Known, String> {
        let slot = self.slot(server)?;
        if let Some(live) = slot.live.lock().await.as_ref() {
            return Ok(Known {
                tools: live.tools.clone(),
                instructions: live.instructions.clone(),
                error: None,
                live: true,
            });
        }
        Ok(self.cached(&slot).unwrap_or_default())
    }

    /// `server`'s tools, started to list them when no listing is kept.
    pub async fn tools(&self, server: &str) -> Result<Vec<Value>, String> {
        let known = self.known(server).await?;
        if known.live || !known.tools.is_empty() {
            return Ok(known.tools);
        }
        let slot = self.slot(server)?;
        let mut live = slot.live.lock().await;
        let tools = self.start(&slot, &mut live).await?.tools.clone();
        *slot.used.lock().unwrap() = Instant::now();
        Ok(tools)
    }

    /// Call `tool` of `server`, starting it if need be: its whole
    /// `CallToolResult`, or why there is none.
    pub async fn call(&self, server: &str, tool: &str, arguments: Value) -> Result<Value, String> {
        let slot = self.slot(server)?;
        if !allowed(&slot.config, tool) {
            return Err(format!("{server} has no tool {tool}"));
        }
        let arguments = match arguments {
            Value::Object(arguments) => arguments,
            Value::Null => serde_json::Map::new(),
            _ => return Err(format!("{server}.{tool} takes an object of arguments")),
        };
        slot.busy.fetch_add(1, Ordering::SeqCst);
        let _done = Busy(&slot);
        let peer = {
            let mut live = slot.live.lock().await;
            self.start(&slot, &mut live).await?.client.peer().clone()
        };
        let timeout = seconds(slot.config.tool_timeout_sec).unwrap_or(CALL);
        let call =
            peer.call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments));
        let result = tokio::time::timeout(timeout, call).await;
        *slot.used.lock().unwrap() = Instant::now();
        match result {
            Err(_) => Err(format!(
                "{server}.{tool} did not answer in {}s",
                timeout.as_secs()
            )),
            Ok(Err(error)) => {
                // A server whose connection broke starts again next time.
                if peer.is_transport_closed() {
                    slot.stop().await;
                }
                Err(format!("{server}.{tool}: {error}"))
            }
            Ok(Ok(result)) => serde_json::to_value(result).map_err(|e| e.to_string()),
        }
    }

    /// Stop the servers no call has used for `idle`.
    pub async fn reap(&self, idle: Duration) {
        let slots: Vec<Arc<Slot>> = self.0.slots.lock().unwrap().values().cloned().collect();
        for slot in slots {
            let quiet = slot.busy.load(Ordering::SeqCst) == 0
                && slot.used.lock().unwrap().elapsed() >= idle;
            if quiet {
                slot.stop().await;
            }
        }
    }

    /// Stop every server.
    pub async fn stop(&self) {
        let slots: Vec<Arc<Slot>> = self.0.slots.lock().unwrap().values().cloned().collect();
        for slot in slots {
            slot.stop().await;
        }
    }

    async fn start<'a>(&self, slot: &Slot, live: &'a mut Option<Live>) -> Result<&'a Live, String> {
        if live.as_ref().is_some_and(|live| live.client.is_closed())
            && let Some(gone) = live.take()
        {
            gone.stop().await;
        }
        if live.is_none() {
            let timeout = seconds(slot.config.startup_timeout_sec).unwrap_or(START);
            let started = tokio::time::timeout(timeout, connect(&slot.config))
                .await
                .unwrap_or_else(|_| Err(format!("did not start in {}s", timeout.as_secs())));
            match started {
                Ok(started) => {
                    self.keep(slot, &started.tools, &started.instructions, None);
                    *live = Some(started);
                }
                Err(error) => {
                    let error = format!("{} did not start: {error}", slot.name);
                    let kept = self.cached(slot).unwrap_or_default();
                    self.keep(slot, &kept.tools, &kept.instructions, Some(&error));
                    return Err(error);
                }
            }
        }
        Ok(live.as_ref().expect("started"))
    }

    fn file(&self, slot: &Slot) -> Option<PathBuf> {
        Some(self.0.cache.as_ref()?.join(format!("{}.json", slot.name)))
    }

    fn cached(&self, slot: &Slot) -> Option<Known> {
        kept(self.0.cache.as_ref()?, &slot.name, &slot.config)
    }

    fn keep(
        &self,
        slot: &Slot,
        tools: &[Value],
        instructions: &Option<String>,
        error: Option<&str>,
    ) {
        let Some(file) = self.file(slot) else {
            return;
        };
        let kept = json!({"config": fingerprint(&slot.config), "tools": tools,
            "instructions": instructions, "error": error});
        let _ = write(&file, &kept.to_string());
    }
}

/// Lowers a server's count of calls in flight however the call ends.
struct Busy<'a>(&'a Slot);
impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Slot {
    async fn stop(&self) {
        if let Some(live) = self.live.lock().await.take() {
            live.stop().await;
        }
    }
}

impl Live {
    async fn stop(self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.client.cancel()).await;
        // What a wrapper such as npx started goes with it.
        if let Some(group) = self.group {
            GROUPS.lock().unwrap().retain(|g| *g != group);
            kill(group);
        }
    }
}

/// Where the person's servers' listings are kept: `~/.agent/sonny/mcp`.
pub fn cache_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent/sonny/mcp"))
}

/// What `server`'s last listing in `cache` says, if it was made with
/// `config`.
pub fn kept(cache: &Path, server: &str, config: &McpServer) -> Option<Known> {
    let file = cache.join(format!("{server}.json"));
    let kept: Value = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    (kept["config"] == fingerprint(config)).then(|| Known {
        tools: kept["tools"].as_array().cloned().unwrap_or_default(),
        instructions: kept["instructions"].as_str().map(str::to_owned),
        error: kept["error"].as_str().map(str::to_owned),
        live: false,
    })
}

/// The process groups of the stdio servers running, which `kill_all` ends.
static GROUPS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Stop every stdio server at once, as a lookout that exits does.
pub fn kill_all() {
    for group in std::mem::take(&mut *GROUPS.lock().unwrap()) {
        kill(group);
    }
}

fn kill(group: u32) {
    // SAFETY: a signal to the group a server's process leads.
    unsafe {
        libc::kill(-(group as libc::pid_t), libc::SIGKILL);
    }
}

/// Whether `tool` is one the config lets agents call.
pub fn allowed(config: &McpServer, tool: &str) -> bool {
    config
        .enabled_tools
        .as_ref()
        .is_none_or(|enabled| enabled.iter().any(|t| t == tool))
        && !config.disabled_tools.iter().any(|t| t == tool)
}

fn seconds(value: Option<f64>) -> Option<Duration> {
    value.map(Duration::from_secs_f64)
}

/// What a listing was made with, so a changed config lists again.
fn fingerprint(config: &McpServer) -> String {
    format!("{config:?}")
}

fn write(file: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let staged = file.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&staged, text)?;
    std::fs::rename(&staged, file)
}

fn client_info() -> ClientConfig {
    let mut info = ClientConfig::default();
    info.client_info.name = "sonny".into();
    info.client_info.version = env!("CARGO_PKG_VERSION").into();
    info
}

async fn connect(config: &McpServer) -> Result<Live, String> {
    let (client, group, stderr) = match (&config.command, &config.url) {
        (Some(command), _) => {
            let mut process = tokio::process::Command::new(command);
            process
                .args(&config.args)
                .envs(&config.env)
                .process_group(0);
            if let Some(cwd) = config.cwd.as_ref().map(|cwd| home_relative(cwd)) {
                process.current_dir(cwd);
            }
            let (transport, stderr) = TokioChildProcess::builder(process)
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("{command}: {e}"))?;
            let group = transport.id();
            let tail = Arc::new(Mutex::new(VecDeque::new()));
            if let Some(stderr) = stderr {
                tokio::spawn(collect(stderr, Arc::clone(&tail)));
            }
            if let Some(group) = group {
                GROUPS.lock().unwrap().push(group);
            }
            match client_info().serve(transport).await {
                Ok(client) => (client, group, tail),
                Err(error) => {
                    if let Some(group) = group {
                        GROUPS.lock().unwrap().retain(|g| *g != group);
                        kill(group);
                    }
                    // Its last words, if it said why.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let said: Vec<String> = tail.lock().unwrap().iter().cloned().collect();
                    return Err(match said.is_empty() {
                        true => error.to_string(),
                        false => format!("{error}: {}", said.join(" / ")),
                    });
                }
            }
        }
        (None, Some(url)) => {
            let mut transport = StreamableHttpClientTransportConfig::with_uri(url.as_str());
            if let Some(name) = &config.bearer_token_env_var {
                let token = agent_client::env_file::var(name)
                    .ok_or_else(|| format!("{name} is not set"))?;
                transport.auth_header = Some(token);
            }
            for (name, value) in &config.http_headers {
                let name = name
                    .parse()
                    .map_err(|_| format!("bad header name {name}"))?;
                let value = value.parse().map_err(|_| "bad header value".to_owned())?;
                transport.custom_headers.insert(name, value);
            }
            let transport = StreamableHttpClientTransport::from_config(transport);
            let client = client_info()
                .serve(transport)
                .await
                .map_err(|e| e.to_string())?;
            (client, None, Arc::default())
        }
        (None, None) => return Err("no command or url".into()),
    };
    drop(stderr);
    let instructions = client
        .peer_info()
        .and_then(|info| info.instructions.clone());
    let listed = client.list_all_tools().await;
    let tools = match listed {
        Ok(tools) => tools,
        Err(error) => {
            let live = Live {
                client,
                tools: Vec::new(),
                instructions: None,
                group,
            };
            live.stop().await;
            return Err(format!("listing its tools: {error}"));
        }
    };
    let tools = (tools.iter())
        .filter(|tool| allowed(config, &tool.name))
        .filter_map(|tool| serde_json::to_value(tool).ok())
        .collect();
    Ok(Live {
        client,
        tools,
        instructions,
        group,
    })
}

/// `~/x` as the home folder's `x`.
fn home_relative(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_owned(),
    }
}

async fn collect(stderr: tokio::process::ChildStderr, tail: Arc<Mutex<VecDeque<String>>>) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut tail = tail.lock().unwrap();
        if tail.len() == STDERR_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
}

/// A server as `/mcp` shows it: what its last listing says, which is
/// what agents opened now are told of it.
struct Shown {
    name: String,
    transport: &'static str,
    state: String,
    description: Option<String>,
    error: Option<String>,
    tools: Vec<String>,
}

fn shown() -> Result<Vec<Shown>, String> {
    let servers = agent_client::config::load()?.sonny.mcp_servers;
    Ok(servers
        .iter()
        .map(|(name, config)| {
            let known = (cache_dir().and_then(|cache| kept(&cache, name, config)))
                .filter(|_| config.enabled)
                .unwrap_or_default();
            let tools: Vec<String> = (known.tools.iter())
                .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                .collect();
            let state = match (config.enabled, &known.error, tools.len()) {
                (false, _, _) => "off".to_owned(),
                (true, Some(_), 0) => "did not start".to_owned(),
                (true, None, 0) => "not started yet".to_owned(),
                (true, _, 1) => "1 tool".to_owned(),
                (true, _, n) => format!("{n} tools"),
            };
            Shown {
                name: name.clone(),
                transport: if config.url.is_some() {
                    "http"
                } else {
                    "stdio"
                },
                state,
                description: config.description.clone(),
                error: known.error,
                tools,
            }
        })
        .collect())
}

/// `/mcp`: the person's servers, their state and tools.
pub fn card() -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::Stylize;
    use ratatui::text::{Line, Span};
    let servers = match shown() {
        Ok(servers) => servers,
        Err(error) => return vec![Line::from(format!(" {error}"))],
    };
    let mut lines = Vec::new();
    if servers.is_empty() {
        lines.push(Line::from(" No MCP servers.").dim());
    }
    for server in servers {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::raw(server.name).bold(),
            Span::raw(format!("  {}  {}", server.transport, server.state)).dim(),
        ]));
        if let Some(about) = server.description {
            lines.push(Line::from(format!("   {about}")));
        }
        if let Some(error) = server.error {
            lines.push(Line::from(format!("   {error}")).red());
        }
        if !server.tools.is_empty() {
            lines.push(Line::from(format!("   {}", server.tools.join(", "))).dim());
        }
        lines.push(Line::default());
    }
    lines.push(Line::from(" Agents call these through exec. Add or remove one with").dim());
    lines.push(Line::from(" sonny mcp add or sonny mcp remove; sonny mcp list starts each.").dim());
    lines
}

const USAGE: &str = "sonny mcp list [--json]
sonny mcp add NAME [--env KEY=VALUE]... [--description TEXT] -- COMMAND [ARG]...
sonny mcp add NAME --url URL [--bearer-token-env-var VAR] [--description TEXT]
sonny mcp remove NAME";

/// `sonny mcp`: the person's servers, as `codex mcp` keeps Codex's, in
/// `[sonny.mcp_servers]` of ~/.agent/config.toml.
pub fn cli(args: &[String]) -> Result<(), String> {
    let path = agent_client::config::path().ok_or("no HOME")?;
    match args.split_first() {
        Some((verb, rest)) if verb == "list" => {
            let json = match rest {
                [] => false,
                [flag] if flag == "--json" => true,
                _ => return Err(USAGE.into()),
            };
            let servers = agent_client::config::read(&path)?.sonny.mcp_servers;
            let listed = listed(&servers)?;
            match json {
                true => println!("{}", Value::Array(listed)),
                false => {
                    for server in &listed {
                        println!("{}", line(server));
                    }
                    if listed.is_empty() {
                        println!("no MCP servers; add one with `sonny mcp add`");
                    }
                }
            }
            Ok(())
        }
        Some((verb, [name, rest @ ..])) if verb == "add" => {
            let keys = added(rest)?;
            agent_client::config::set_table(
                &path,
                &format!("sonny.mcp_servers.{name}"),
                Some(&keys),
            )?;
            // Listed now, so agents opened from here on are told its tools.
            let mut servers = agent_client::config::read(&path)?.sonny.mcp_servers;
            servers.retain(|server, _| server == name);
            for server in listed(&servers)? {
                println!("added {}", line(&server));
            }
            Ok(())
        }
        Some((verb, [name])) if verb == "remove" => {
            let servers = agent_client::config::read(&path)?.sonny.mcp_servers;
            if !servers.contains_key(name) {
                return Err(format!("no MCP server {name}"));
            }
            agent_client::config::set_table(&path, &format!("sonny.mcp_servers.{name}"), None)?;
            println!("removed {name}");
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

/// The keys `sonny mcp add`'s flags name.
fn added(args: &[String]) -> Result<toml::Table, String> {
    let mut keys = toml::Table::new();
    let mut env = toml::Table::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut value = || {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{arg} takes a value"))
        };
        match arg.as_str() {
            "--url" => {
                keys.insert("url".into(), value()?.into());
            }
            "--bearer-token-env-var" => {
                keys.insert("bearer_token_env_var".into(), value()?.into());
            }
            "--description" => {
                keys.insert("description".into(), value()?.into());
            }
            "--env" => {
                let pair = value()?;
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("--env takes KEY=VALUE, not {pair}"))?;
                env.insert(key.into(), value.into());
            }
            "--" => {
                let mut command = rest.by_ref().cloned();
                let program = command.next().ok_or("-- takes the server's command")?;
                keys.insert("command".into(), program.into());
                let args: Vec<toml::Value> = command.map(toml::Value::from).collect();
                if !args.is_empty() {
                    keys.insert("args".into(), toml::Value::Array(args));
                }
            }
            _ => return Err(USAGE.into()),
        }
    }
    if !env.is_empty() {
        keys.insert("env".into(), toml::Value::Table(env));
    }
    Ok(keys)
}

fn listed(servers: &Servers) -> Result<Vec<Value>, String> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(list(servers)))
}

/// Each server started and listed, or why it would not start.
async fn list(servers: &Servers) -> Vec<Value> {
    let hub = Hub::for_person();
    hub.configure(servers).await;
    let mut listed = Vec::new();
    for (name, config) in servers {
        let mut row = json!({"name": name, "enabled": config.enabled,
            "transport": if config.url.is_some() { "http" } else { "stdio" }});
        if config.enabled {
            match hub.tools(name).await {
                Ok(tools) => {
                    let names: Vec<&Value> = tools.iter().map(|tool| &tool["name"]).collect();
                    row["tools"] = json!(names);
                }
                Err(error) => row["error"] = json!(error),
            }
        }
        listed.push(row);
    }
    hub.stop().await;
    listed
}

fn line(server: &Value) -> String {
    let name = server["name"].as_str().unwrap_or_default();
    let transport = server["transport"].as_str().unwrap_or_default();
    if server["enabled"] == false {
        return format!("{name}  {transport}  off");
    }
    match server["tools"].as_array() {
        Some(tools) => {
            let names: Vec<&str> = tools.iter().filter_map(Value::as_str).collect();
            format!(
                "{name}  {transport}  {} tools: {}",
                names.len(),
                names.join(", ")
            )
        }
        None => format!(
            "{name}  {transport}  {}",
            server["error"].as_str().unwrap_or("failed")
        ),
    }
}
