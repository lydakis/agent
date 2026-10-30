//! `exec`, the one tool through which agents reach the MCP servers in
//! `[sonny.mcp_servers]`: the model writes JavaScript, the lookout runs it
//! in QuickJS with each server's tools as `tools.mcp__SERVER__TOOL`, and
//! only what the script passes to `text()` or `image()` comes back. The
//! contract is Codex's code mode, word for word where it fits, since the
//! models already write it; Pi's code mode keeps the same names. As in
//! Codex, the script is not approved as a whole: each call of a tool that
//! its server's `approval_mode` says asks first goes to the agent's gate,
//! and the script's time stops while it waits.
use crate::mcp::{Hub, Known, Servers};
use agent_client::Client;
use rquickjs::prelude::Async;
use rquickjs::{AsyncContext, AsyncRuntime, Function, Promise};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const NAME: &str = "exec";

/// Codex's grammar for `exec`: the source, after an optional options line.
const GRAMMAR: &str = r#"start: pragma_source | plain_source
pragma_source: PRAGMA_LINE NEWLINE SOURCE
plain_source: SOURCE

PRAGMA_LINE: /[ \t]*\/\/ @exec:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;

const PRAGMA: &str = "// @exec:";

/// A script's memory, and its time unless its options line or its call's
/// expiry say less.
const MEMORY: usize = 256 << 20;
pub const TIMEOUT: Duration = Duration::from_secs(600);
/// Scripts that run at once in the lookout; the rest wait their turn.
const SCRIPTS: usize = 4;
static RUNNING: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(SCRIPTS);
/// Scripts alive at once, running or waiting for verdicts, so that their
/// heaps at the memory cap hold at most 4 GB however many calls wait.
const LIVE: usize = 16;
static ALIVE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(LIVE);
/// The longest pause between two of a script's interrupt checks that is
/// still one stretch of running JavaScript.
const RUNNING_GAP: Duration = Duration::from_millis(50);
/// The output a script returns, in tokens of about four bytes, unless its
/// options line says otherwise.
const OUTPUT_TOKENS: usize = 10_000;
/// The bytes of tool declarations the description holds, about 3,000
/// tokens as Pi's code mode allows; the rest are found from the script.
const DECLARATIONS: usize = 12_000;

const DESCRIPTION: &str = r#"Run JavaScript code to orchestrate/compose MCP tool calls
- Evaluates the provided JavaScript code in a fresh QuickJS context as the body of an async function, so `await` works at the top level.
- The tools of the MCP servers below are on the global `tools` object, as `await tools.mcp__<server>__<tool>(args)`. Tool names are exposed as normalized JavaScript identifiers.
- A tool takes an object of arguments and resolves to its MCP `CallToolResult`; a call that fails rejects with an `Error` saying why. Run independent calls together with `Promise.all`.
- Runs raw JavaScript -- no Node, no file system, no network access, no console, no timers.
- Accepts raw JavaScript source text, not JSON, quoted strings, or markdown code fences.
- Only what the script passes to `text()` and `image()` is returned to you.
- You may optionally start the tool input with a first-line pragma like `// @exec: {"max_output_tokens": 1000, "timeout_ms": 60000}`.
- `max_output_tokens` sets the token budget for `exec` results. Defaults to 10000 tokens; longer output keeps its start and end, and the whole of it is saved to a file the result names.
- `timeout_ms` ends the script after that many milliseconds. Defaults to 600000, the most it may run.
- When the JS code is fully evaluated, its context ends and unawaited promises are silently discarded.

- Global helpers:
- `exit()`: Immediately ends the current script successfully (like an early return from the top level).
- `text(value: string | number | boolean | undefined | null)`: Appends a text item. Non-string values are stringified with `JSON.stringify(...)` when possible.
- `image(imageUrlOrItem: string | { image_url: string } | ImageContent)`: Saves an image to a file and appends its path. `image_url` should be a base64-encoded `data:` URL. To forward an MCP tool image, pass an individual `ImageContent` block from `result.content`, for example `image(result.content[0])`.
- `store(key: string, value: any)`: stores a serializable value under a string key for later `exec` calls by the same agent.
- `load(key: string)`: returns the stored value for a string key, or `undefined` if it is missing.
- `ALL_TOOLS`: metadata for the enabled nested tools as `{ name, description }` entries.

Some nested tools may be omitted from this description. They are still available on the global `tools` object and listed in `ALL_TOOLS`.

Use `await tools.tool_search({query: "...", limit: 8})` for BM25-ranked discovery of tools (up to `limit` matches; 8 by default). It returns `{tools: [{name, description}]}` with callable names and full TypeScript declarations. Print results with `text()` and inspect unfamiliar declarations before calling `tools[result.name](arguments)` in a later execution."#;

/// The MCP result types the declarations name, after Codex's.
const TYPES: &str = r#"Shared MCP Types:
```ts
type TextContent = { type: "text"; text: string; };
type ImageContent = { type: "image"; data: string; mimeType: string; };
type AudioContent = { type: "audio"; data: string; mimeType: string; };
type ResourceLink = { type: "resource_link"; uri: string; name: string; description?: string; mimeType?: string; };
type EmbeddedResource = { type: "resource"; resource: { uri: string; mimeType?: string; text?: string; blob?: string; }; };
type ContentBlock = TextContent | ImageContent | AudioContent | ResourceLink | EmbeddedResource;
type CallToolResult<TStructured = { [key: string]: unknown }> = {
  content: ContentBlock[];
  isError?: boolean;
  structuredContent?: TStructured;
  [key: string]: unknown;
};
```"#;

/// `exec`, declared for every agent while the person has servers on, or
/// `None`: its description lists them, with the tools they last listed.
pub fn tool() -> Option<Value> {
    let servers = agent_client::config::load().ok()?.sonny.mcp_servers;
    let servers: Servers = servers.into_iter().filter(|(_, s)| s.enabled).collect();
    if servers.is_empty() {
        return None;
    }
    let cache = crate::mcp::cache_dir();
    let known: BTreeMap<String, Known> = servers
        .iter()
        .map(|(name, config)| {
            let known = cache
                .as_deref()
                .and_then(|cache| crate::mcp::kept(cache, name, config));
            (name.clone(), known.unwrap_or_default())
        })
        .collect();
    Some(json!({
        "name": NAME,
        "description": description(&servers, &known),
        "parameters": {
            "type": "object",
            "properties": {"input": {"type": "string",
                "description": "Raw JavaScript, optionally after a `// @exec:` options line."}},
            "required": ["input"],
            "additionalProperties": false
        },
        "grammar": GRAMMAR,
        "answered_by": crate::served::TAG,
        "expire_ms": TIMEOUT.as_millis() as u64,
        // The agent's gate judges the script's calls, not the script.
        "steps": true,
    }))
}

/// The tool's description: Codex's, then each server with its tools'
/// declarations while they fit.
fn description(servers: &Servers, known: &BTreeMap<String, Known>) -> String {
    let mut sections = vec![DESCRIPTION.to_owned(), TYPES.to_owned()];
    let mut left = DECLARATIONS;
    let mut omitted = 0;
    for (name, config) in servers {
        let known = &known[name];
        let mut section = format!("## {name}");
        if let Some(about) = config
            .description
            .as_deref()
            .or(known.instructions.as_deref())
        {
            section.push('\n');
            section.push_str(about.trim());
        }
        if known.tools.is_empty() {
            section.push_str("\nIts tools are listed in `ALL_TOOLS` once it starts.");
        }
        sections.push(section);
        for tool in &known.tools {
            let declared = declaration(name, tool);
            if declared.len() > left {
                omitted += 1;
                continue;
            }
            left -= declared.len();
            sections.push(declared);
        }
    }
    if omitted > 0 {
        sections.push(format!("{omitted} more tools are in `ALL_TOOLS`."));
    }
    sections.join("\n\n")
}

/// A tool as the script calls it: `mcp__SERVER__TOOL`, made an identifier.
pub fn identifier(server: &str, tool: &str) -> String {
    let raw = format!("mcp__{server}__{tool}");
    let mut id: String = raw
        .chars()
        .enumerate()
        .map(|(n, c)| match c {
            '_' | '$' => c,
            c if c.is_ascii_alphabetic() || (n > 0 && c.is_ascii_digit()) => c,
            _ => '_',
        })
        .collect();
    if id.is_empty() {
        id.push('_');
    }
    id
}

/// One tool's heading, description and TypeScript declaration, as Codex
/// writes them.
fn declaration(server: &str, tool: &Value) -> String {
    let name = tool["name"].as_str().unwrap_or_default();
    let id = identifier(server, name);
    let mut text = format!("### `{id}`");
    if let Some(about) = tool["description"]
        .as_str()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        text.push('\n');
        text.push_str(about);
    }
    let input = typescript(&tool["inputSchema"]);
    let output = match tool.get("outputSchema") {
        Some(schema) if schema.is_object() => format!("CallToolResult<{}>", typescript(schema)),
        _ => "CallToolResult".to_owned(),
    };
    text.push_str(&format!(
        "\n```ts\ndeclare const tools: {{ {id}(args: {input}): Promise<{output}>; }};\n```"
    ));
    text
}

/// A JSON Schema as a TypeScript type, `unknown` for what it cannot say.
pub fn typescript(schema: &Value) -> String {
    Render {
        root: schema,
        refs: 0,
    }
    .render(schema, 0)
}

struct Render<'a> {
    root: &'a Value,
    /// References followed, bounded against cycles.
    refs: usize,
}

impl Render<'_> {
    fn render(&mut self, schema: &Value, depth: usize) -> String {
        let Some(map) = schema.as_object() else {
            return "unknown".into();
        };
        if depth > 8 {
            return "unknown".into();
        }
        if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
            let target = reference
                .strip_prefix('#')
                .and_then(|pointer| self.root.pointer(pointer));
            return match target {
                Some(target) if self.refs < 32 => {
                    self.refs += 1;
                    self.render(target, depth + 1)
                }
                _ => "unknown".into(),
            };
        }
        if let Some(value) = map.get("const") {
            return value.to_string();
        }
        if let Some(values) = map.get("enum").and_then(Value::as_array) {
            return union(values.iter().map(Value::to_string).collect());
        }
        for key in ["anyOf", "oneOf"] {
            if let Some(schemas) = map.get(key).and_then(Value::as_array) {
                return union(schemas.iter().map(|s| self.render(s, depth + 1)).collect());
            }
        }
        if let Some(schemas) = map.get("allOf").and_then(Value::as_array) {
            let parts: Vec<String> = schemas.iter().map(|s| self.render(s, depth + 1)).collect();
            return match parts.len() {
                1 => parts.into_iter().next().unwrap_or_default(),
                _ => parts
                    .iter()
                    .map(|p| format!("({p})"))
                    .collect::<Vec<_>>()
                    .join(" & "),
            };
        }
        match &map.get("type") {
            Some(Value::Array(types)) => union(
                types
                    .iter()
                    .map(|t| {
                        let mut one = map.clone();
                        one.insert("type".into(), t.clone());
                        self.render(&Value::Object(one), depth)
                    })
                    .collect(),
            ),
            Some(Value::String(kind)) => match kind.as_str() {
                "string" => "string".into(),
                "number" | "integer" => "number".into(),
                "boolean" => "boolean".into(),
                "null" => "null".into(),
                "array" => match map.get("items") {
                    Some(items) => format!("Array<{}>", self.render(items, depth + 1)),
                    None => "unknown[]".into(),
                },
                "object" => self.object(map, depth),
                _ => "unknown".into(),
            },
            _ if map.contains_key("properties") => self.object(map, depth),
            _ => "unknown".into(),
        }
    }

    fn object(&mut self, map: &Map<String, Value>, depth: usize) -> String {
        let required: Vec<&str> = (map.get("required").and_then(Value::as_array).into_iter())
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let empty = Map::new();
        let properties = map
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let described = properties.values().any(|p| p["description"].is_string());
        let mut lines = Vec::new();
        for (name, property) in properties {
            if described {
                for line in property["description"].as_str().unwrap_or_default().lines() {
                    let line = line.trim();
                    if !line.is_empty() {
                        lines.push(format!("// {line}"));
                    }
                }
            }
            let optional = if required.contains(&name.as_str()) {
                ""
            } else {
                "?"
            };
            let key = match is_identifier(name) {
                true => name.clone(),
                false => Value::from(name.as_str()).to_string(),
            };
            lines.push(format!(
                "{key}{optional}: {};",
                self.render(property, depth + 1)
            ));
        }
        match map.get("additionalProperties") {
            Some(Value::Bool(false)) => {}
            Some(Value::Bool(true)) | None if properties.is_empty() => {
                lines.push("[key: string]: unknown;".into())
            }
            Some(extra @ Value::Object(_)) => {
                lines.push(format!("[key: string]: {};", self.render(extra, depth + 1)))
            }
            _ => {}
        }
        match (lines.is_empty(), described) {
            (true, _) => "{}".into(),
            (false, false) => format!("{{ {} }}", lines.join(" ")),
            (false, true) => {
                let pad = "  ".repeat(depth + 1);
                let body: Vec<String> = lines.iter().map(|l| format!("{pad}{l}")).collect();
                format!("{{\n{}\n{}}}", body.join("\n"), "  ".repeat(depth))
            }
        }
    }
}

fn union(parts: Vec<String>) -> String {
    let mut seen: Vec<String> = Vec::new();
    for part in parts {
        if !seen.contains(&part) {
            seen.push(part);
        }
    }
    match seen.len() {
        0 => "never".into(),
        _ => seen.join(" | "),
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c == '$' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}

/// A script and what its options line asked for.
#[derive(Debug, PartialEq)]
pub struct Script {
    pub code: String,
    pub max_output_tokens: Option<usize>,
    pub timeout_ms: Option<u64>,
}

/// The source of an `exec` call, with its options line read off, as
/// Codex reads it; `yield_time_ms`, which asks Codex to hand back a script
/// still running, is taken and ignored, since a script here runs to its end.
pub fn parse(input: &str) -> Result<Script, String> {
    if input.trim().is_empty() {
        return Err(
            "exec expects raw JavaScript source text (non-empty). Provide JS only, \
            optionally with first-line `// @exec: {\"max_output_tokens\": 1000}`."
                .into(),
        );
    }
    let (first, rest) = input.split_once('\n').unwrap_or((input, ""));
    let Some(options) = first.trim_start().strip_prefix(PRAGMA) else {
        return Ok(Script {
            code: input.to_owned(),
            max_output_tokens: None,
            timeout_ms: None,
        });
    };
    if rest.trim().is_empty() {
        return Err("exec pragma must be followed by JavaScript source on subsequent lines".into());
    }
    let fields = "supported fields `max_output_tokens` and `timeout_ms`";
    let options: Value = serde_json::from_str(options.trim())
        .map_err(|e| format!("exec pragma must be valid JSON with {fields}: {e}"))?;
    let options = options
        .as_object()
        .ok_or_else(|| format!("exec pragma must be a JSON object with {fields}"))?;
    let mut script = Script {
        code: rest.to_owned(),
        max_output_tokens: None,
        timeout_ms: None,
    };
    for (key, value) in options {
        let number = || {
            value
                .as_u64()
                .ok_or_else(|| format!("exec pragma field `{key}` must be a non-negative integer"))
        };
        match key.as_str() {
            "max_output_tokens" => script.max_output_tokens = Some(number()? as usize),
            "timeout_ms" => script.timeout_ms = Some(number()?),
            "yield_time_ms" => {
                number()?;
            }
            _ => return Err(format!("exec pragma only supports {fields}; got `{key}`")),
        }
    }
    Ok(script)
}

/// Where a script's images, stored values and whole outputs are kept.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent/sonny/exec"))
}

/// A tool the script may call.
#[derive(Clone)]
struct Nested {
    server: String,
    tool: String,
    description: String,
    /// Whether a call of it asks the agent's gate first.
    asks: bool,
}

/// The `exec` call a script answers, whose steps its calls are: where a
/// call that asks first is put to the agent's gate.
pub struct Steps {
    pub client: Arc<Client>,
    pub bot: String,
    pub turn: i64,
    pub call_id: String,
    next: std::sync::atomic::AtomicU64,
}

impl Steps {
    pub fn new(client: Arc<Client>, bot: String, turn: i64, call_id: String) -> Self {
        Self {
            client,
            bot,
            turn,
            call_id,
            next: Default::default(),
        }
    }

    /// Put the call of `tool` to the agent's gate: none, an allow, or why
    /// it may not run.
    async fn ask(&self, tool: &str, arguments: Value) -> Result<(), String> {
        let step = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let asked = json!({"bot": self.bot, "turn": self.turn, "call_id": self.call_id,
            "step": step.to_string(), "name": tool, "arguments": arguments});
        match self.client.request("approve_step", asked).await {
            Ok(reply) if reply["decision"] == "allow" => Ok(()),
            Ok(reply) => Err(format!(
                "tools.{tool} was not approved: {}",
                reply["reason"].as_str().unwrap_or("denied")
            )),
            Err(error) => Err(format!("tools.{tool} could not be approved: {error}")),
        }
    }
}

/// Answer `bot`'s `exec` call: run the script against `hub`'s servers
/// within `left`, and return what it printed, headed by how it ended.
/// Dropping the answer, as an interrupted turn does, stops the script.
pub async fn answer(
    hub: &Hub,
    bot: &str,
    input: &str,
    left: Duration,
    steps: Option<Steps>,
) -> Result<String, String> {
    let servers = agent_client::config::load()?.sonny.mcp_servers;
    hub.configure(&servers).await;
    run(hub, home_dir().as_deref(), bot, input, left, steps).await
}

/// Run a script against the servers `hub` serves, keeping what it saves
/// under `dir`.
async fn run(
    hub: &Hub,
    dir: Option<&Path>,
    bot: &str,
    input: &str,
    left: Duration,
    steps: Option<Steps>,
) -> Result<String, String> {
    let script = parse(input)?;
    let waiting = Instant::now();
    let alive = ALIVE.acquire().await.map_err(|e| e.to_string())?;
    let running = RUNNING.acquire().await.map_err(|e| e.to_string())?;
    let left = left.saturating_sub(waiting.elapsed());
    let mut nested: BTreeMap<String, Nested> = BTreeMap::new();
    let mut listed = Vec::new();
    let mut failed = Vec::new();
    for (server, config) in hub.servers() {
        match hub.tools(&server).await {
            Ok(tools) => {
                for tool in &tools {
                    let name = tool["name"].as_str().unwrap_or_default();
                    let id = identifier(&server, name);
                    let description = declaration(&server, tool);
                    listed.push(json!({"name": id, "description": description}));
                    let asks = config.approval_mode(name).asks(&tool["annotations"]);
                    nested.insert(
                        id,
                        Nested {
                            server: server.clone(),
                            tool: name.to_owned(),
                            description,
                            asks,
                        },
                    );
                }
            }
            Err(error) => failed.push(error),
        }
    }
    let timeout = script
        .timeout_ms
        .map_or(TIMEOUT, Duration::from_millis)
        .min(TIMEOUT)
        .min(left);
    let budget = script.max_output_tokens.unwrap_or(OUTPUT_TOKENS);
    let run = Run {
        hub: hub.clone(),
        lookout: tokio::runtime::Handle::current(),
        nested: Arc::new(nested),
        failed: Arc::new(failed),
        listed: Value::Array(listed).to_string(),
        store: dir.map(|dir| dir.join("store").join(format!("{bot}.json"))),
        images: dir.map(|dir| dir.join("images")),
        said: Arc::default(),
        calls: Arc::default(),
        stopped: Arc::default(),
        steps: steps.map(Arc::new),
        clock: Clock::new(Instant::now() + timeout, running),
    };
    let stopped = Arc::clone(&run.stopped);
    let _stop = Stop(Arc::clone(&stopped));
    let started = Instant::now();
    let (said, calls) = (Arc::clone(&run.said), Arc::clone(&run.calls));
    let clock = Arc::clone(&run.clock);
    let ended = tokio::task::spawn_blocking(move || {
        // Its places are held until the script's runtime is gone, so an
        // interrupted turn frees them only once its script has stopped.
        let _running = (clock, alive);
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|e| e.to_string())?
            .block_on(run.script(script.code))
    })
    .await
    .map_err(|e| e.to_string())??;
    let status = match &ended {
        Ended::Completed => "Script completed".to_owned(),
        Ended::Failed(_) => "Script failed".to_owned(),
        Ended::Terminated(why) => format!("Script terminated: {why}"),
    };
    let mut head = format!(
        "{status}\nWall time {:.1} seconds\n",
        started.elapsed().as_secs_f64()
    );
    let calls = calls.lock().unwrap().clone();
    if !calls.is_empty() {
        let each: Vec<String> = calls
            .iter()
            .map(|(server, n)| format!("{server} {n}"))
            .collect();
        head.push_str(&format!("MCP calls: {}\n", each.join(", ")));
    }
    head.push_str("Output:\n");
    let mut body = said.lock().unwrap().join("\n");
    if let Ended::Failed(error) = &ended {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&format!("Script error:\n{error}"));
    }
    Ok(head + &budgeted(body, budget, dir))
}

/// Sets the script's stop flag when the answer is dropped or done.
struct Stop(Arc<AtomicBool>);
impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// `body` within `tokens`, of about four bytes each: its start and end,
/// with the whole saved to a file the cut names.
fn budgeted(body: String, tokens: usize, dir: Option<&Path>) -> String {
    let bytes = tokens.saturating_mul(4);
    if body.len() <= bytes {
        return body;
    }
    let saved = dir.and_then(|dir| {
        let file = fresh(&dir.join("output"))
            .ok()?
            .join(format!("{}.txt", stamp()));
        std::fs::write(&file, &body).ok()?;
        Some(file)
    });
    let half = bytes / 2;
    let mut start = half;
    while !body.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = body.len() - half;
    while !body.is_char_boundary(end) {
        end += 1;
    }
    let cut = (body.len() - start - (body.len() - end)).div_ceil(4);
    let whole = match saved {
        Some(file) => format!("; the whole output is in {}", file.display()),
        None => String::new(),
    };
    format!(
        "{}\n…{cut} tokens truncated{whole}…\n{}",
        &body[..start],
        &body[end..]
    )
}

/// A name no other file of this lookout's takes.
/// How long the outputs and images scripts save are kept.
const KEPT: Duration = Duration::from_secs(7 * 24 * 3600);

/// `dir`, made, with what was saved there over `KEPT` ago removed, so
/// saved outputs and images hold a week's worth rather than ever more.
fn fresh(dir: &Path) -> Result<&Path, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
        let old = (entry.metadata().and_then(|m| m.modified()).ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > KEPT);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(dir)
}

fn stamp() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{now}-{}-{n}", std::process::id())
}

enum Ended {
    Completed,
    Failed(String),
    Terminated(String),
}

/// One script's run: what it reaches and what it said.
struct Run {
    hub: Hub,
    /// The lookout's runtime, where the servers' connections live.
    lookout: tokio::runtime::Handle,
    nested: Arc<BTreeMap<String, Nested>>,
    failed: Arc<Vec<String>>,
    listed: String,
    store: Option<PathBuf>,
    images: Option<PathBuf>,
    said: Arc<Mutex<Vec<String>>>,
    calls: Arc<Mutex<BTreeMap<String, usize>>>,
    stopped: Arc<AtomicBool>,
    steps: Option<Arc<Steps>>,
    clock: Arc<Clock>,
}

/// A script's time, which stops while any of its calls waits for the
/// agent's gate, as the time of its `exec` call does in the daemon, and its
/// place among the scripts running, which it gives up meanwhile: a verdict
/// can take a person hours, and four scripts waiting on theirs would hold
/// back every other agent's. JavaScript that runs meanwhile, beside the
/// calls waiting, still spends the time.
struct Clock {
    times: Mutex<Times>,
    resumed: tokio::sync::Notify,
    place: Mutex<Option<tokio::sync::SemaphorePermit<'static>>>,
    /// Taken while a call takes the place back, so only one waits for it.
    returning: tokio::sync::Mutex<()>,
}

struct Times {
    deadline: Instant,
    /// The calls waiting, since when, and how long JavaScript has run since.
    waiting: usize,
    since: Instant,
    ran: Duration,
    /// The last interrupt check while calls wait.
    checked: Option<Instant>,
}

impl Times {
    fn out(&self, now: Instant) -> bool {
        match self.waiting {
            0 => now >= self.deadline,
            _ => self.ran >= self.deadline.saturating_duration_since(self.since),
        }
    }
}

impl Clock {
    fn new(deadline: Instant, place: tokio::sync::SemaphorePermit<'static>) -> Arc<Self> {
        let now = Instant::now();
        Arc::new(Self {
            times: Mutex::new(Times {
                deadline,
                waiting: 0,
                since: now,
                ran: Duration::ZERO,
                checked: None,
            }),
            resumed: tokio::sync::Notify::new(),
            place: Mutex::new(Some(place)),
            returning: tokio::sync::Mutex::new(()),
        })
    }

    /// Take the script's place back once a call's verdict came, waiting
    /// for one as a new script does, with the time running.
    async fn take_place(&self) -> Result<(), String> {
        let _one = self.returning.lock().await;
        if self.place.lock().unwrap().is_none() {
            let place = RUNNING.acquire().await.map_err(|e| e.to_string())?;
            *self.place.lock().unwrap() = Some(place);
        }
        Ok(())
    }

    fn out(&self) -> bool {
        self.times.lock().unwrap().out(Instant::now())
    }

    /// Whether the time ran out, from the script's interrupt checks, which
    /// come only while JavaScript runs: checks close together while calls
    /// wait are a stretch of it, and spend the time they span.
    fn check(&self) -> bool {
        let now = Instant::now();
        let mut times = self.times.lock().unwrap();
        if times.waiting == 0 {
            return times.out(now);
        }
        if let Some(checked) = times.checked
            && now - checked <= RUNNING_GAP
        {
            times.ran += now - checked;
        }
        times.checked = Some(now);
        let out = times.out(now);
        drop(times);
        if out {
            self.resumed.notify_waiters();
        }
        out
    }

    /// Stop the clock until the guard is dropped, and give up the place.
    fn stop(self: &Arc<Self>) -> Stopped {
        let mut times = self.times.lock().unwrap();
        if times.waiting == 0 {
            times.since = Instant::now();
            times.ran = Duration::ZERO;
            times.checked = None;
            self.place.lock().unwrap().take();
        }
        times.waiting += 1;
        Stopped(Arc::clone(self))
    }

    /// Once the time has run out.
    async fn run_out(&self) {
        loop {
            let resumed = self.resumed.notified();
            tokio::pin!(resumed);
            resumed.as_mut().enable();
            let (deadline, waiting) = {
                let times = self.times.lock().unwrap();
                if times.out(Instant::now()) {
                    return;
                }
                (times.deadline, times.waiting)
            };
            if waiting > 0 {
                resumed.await;
                continue;
            }
            tokio::time::sleep_until(deadline.into()).await;
        }
    }
}

/// A stopped clock, which runs again once no call waits, later by as long
/// as it was stopped less the JavaScript that ran meanwhile.
struct Stopped(Arc<Clock>);

impl Drop for Stopped {
    fn drop(&mut self) {
        let mut times = self.0.times.lock().unwrap();
        times.waiting -= 1;
        if times.waiting == 0 {
            let stopped = times.since.elapsed().saturating_sub(times.ran);
            times.deadline += stopped;
            drop(times);
            self.0.resumed.notify_waiters();
        }
    }
}

/// The globals the script sees, made from the host's `__` functions.
const PRELUDE: &str = r#"(() => {
  const host = { call: __call, search: __search, text: __text, image: __image, store: __store, load: __load };
  for (const name of ["__call", "__search", "__text", "__image", "__store", "__load", "__tools"]) delete globalThis[name];
  const EXIT = globalThis.__exit;
  delete globalThis.__exit;
  const said = (raw) => { const said = JSON.parse(raw); if ("error" in said) throw new Error(said.error); return said.ok; };
  const show = (value) => typeof value === "string" ? value : (JSON.stringify(value) ?? String(value));
  const named = {
    tool_search: async (args) => said(await host.search(JSON.stringify(args ?? {}))),
  };
  globalThis.tools = new Proxy(named, {
    get(target, key) {
      if (key in target) return target[key];
      if (typeof key !== "string" || key === "then") return undefined;
      return async (args) => said(await host.call(key, JSON.stringify(args ?? {})));
    },
  });
  globalThis.text = (value) => { host.text(show(value)); };
  globalThis.image = (item) => { host.text(said(host.image(JSON.stringify(item)))); };
  globalThis.store = (key, value) => { said(host.store(String(key), JSON.stringify(value) ?? "null")); };
  globalThis.load = (key) => { const kept = host.load(String(key)); return kept == null ? undefined : JSON.parse(kept); };
  globalThis.exit = () => { throw EXIT; };
  globalThis.__ended = (e) => e === EXIT ? null : (e instanceof Error ? `${e}${e.stack ? "\n" + e.stack.trimEnd() : ""}` : show(e));
})();"#;

impl Run {
    async fn script(self, code: String) -> Result<Ended, String> {
        let runtime = AsyncRuntime::new().map_err(|e| e.to_string())?;
        runtime.set_memory_limit(MEMORY).await;
        let (stopped, clock) = (Arc::clone(&self.stopped), Arc::clone(&self.clock));
        runtime
            .set_interrupt_handler(Some(Box::new(move || {
                stopped.load(Ordering::SeqCst) || clock.check()
            })))
            .await;
        let context = AsyncContext::full(&runtime)
            .await
            .map_err(|e| e.to_string())?;
        let stopped = Arc::clone(&self.stopped);
        let ran = context.async_with(async |ctx| self.evaluate(ctx, &code).await);
        let ended = tokio::select! {
            ended = ran => ended,
            _ = self.clock.run_out() => Ended::Terminated("it ran out of time".into()),
            _ = stopped_flag(&stopped) => Ended::Terminated("its turn ended".into()),
        };
        Ok(match ended {
            // An interrupt ends the evaluation as an uncatchable error.
            Ended::Failed(_) if self.stopped.load(Ordering::SeqCst) => {
                Ended::Terminated("its turn ended".into())
            }
            Ended::Failed(_) if self.clock.out() => Ended::Terminated("it ran out of time".into()),
            ended => ended,
        })
    }

    async fn evaluate<'js>(&self, ctx: rquickjs::Ctx<'js>, code: &str) -> Ended {
        let caught = |ctx: &rquickjs::Ctx<'js>, error: rquickjs::Error| -> Ended {
            let thrown = ctx.catch();
            let text = match thrown.as_exception() {
                Some(exception) => {
                    let message = exception.message().unwrap_or_default();
                    let name: String = exception.get("name").unwrap_or_else(|_| "Error".into());
                    match exception.stack() {
                        Some(stack) if !stack.trim().is_empty() => {
                            format!("{name}: {message}\n{}", stack.trim_end())
                        }
                        _ => format!("{name}: {message}"),
                    }
                }
                None => match error {
                    rquickjs::Error::Exception => format!("{thrown:?}"),
                    error => error.to_string(),
                },
            };
            Ended::Failed(text)
        };
        if let Err(error) = self.globals(&ctx) {
            return caught(&ctx, error);
        }
        if let Err(error) = ctx.eval::<(), _>(PRELUDE) {
            return caught(&ctx, error);
        }
        // The script on the wrapper's first line, so its line numbers hold.
        let wrapped = format!("(async () => {{{code}\n}})().then(() => null, __ended)");
        let promise: Promise = match ctx.eval(wrapped) {
            Ok(promise) => promise,
            Err(error) => return caught(&ctx, error),
        };
        match promise.into_future::<Option<String>>().await {
            Ok(None) => Ended::Completed,
            Ok(Some(error)) => Ended::Failed(error),
            Err(error) => caught(&ctx, error),
        }
    }

    fn globals<'js>(&self, ctx: &rquickjs::Ctx<'js>) -> rquickjs::Result<()> {
        let globals = ctx.globals();
        let exit = rquickjs::Object::new(ctx.clone())?;
        globals.set("__exit", exit)?;
        globals.set("ALL_TOOLS", ctx.json_parse(self.listed.clone())?)?;
        let said = Arc::clone(&self.said);
        globals.set(
            "__text",
            Function::new(ctx.clone(), move |text: String| {
                said.lock().unwrap().push(text)
            })?,
        )?;
        let call = Caller {
            hub: self.hub.clone(),
            lookout: self.lookout.clone(),
            nested: Arc::clone(&self.nested),
            failed: Arc::clone(&self.failed),
            calls: Arc::clone(&self.calls),
            steps: self.steps.clone(),
            clock: Arc::clone(&self.clock),
        };
        globals.set(
            "__call",
            Function::new(
                ctx.clone(),
                Async(move |id: String, arguments: String| {
                    let call = call.clone();
                    async move { rquickjs::Result::Ok(call.call(id, arguments).await.to_string()) }
                }),
            )?,
        )?;
        let nested = Arc::clone(&self.nested);
        globals.set(
            "__search",
            Function::new(
                ctx.clone(),
                Async(move |arguments: String| {
                    let nested = Arc::clone(&nested);
                    async move { rquickjs::Result::Ok(search(&nested, &arguments).to_string()) }
                }),
            )?,
        )?;
        let images = self.images.clone();
        globals.set(
            "__image",
            Function::new(ctx.clone(), move |item: String| {
                said_of(
                    images
                        .as_deref()
                        .ok_or("no HOME".into())
                        .and_then(|dir| image(dir, &item)),
                )
            })?,
        )?;
        let store = self.store.clone();
        globals.set(
            "__store",
            Function::new(ctx.clone(), move |key: String, value: String| {
                said_of(
                    store
                        .as_deref()
                        .ok_or("no HOME".into())
                        .and_then(|file| keep(file, &key, &value))
                        .map(|()| "stored".to_owned()),
                )
            })?,
        )?;
        let store = self.store.clone();
        globals.set(
            "__load",
            Function::new(ctx.clone(), move |key: String| -> Option<String> {
                let kept = kept(store.as_deref()?);
                kept.get(&key).map(Value::to_string)
            })?,
        )?;
        Ok(())
    }
}

async fn stopped_flag(stopped: &AtomicBool) {
    while !stopped.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// What a host function says to the script: `{ok}` or `{error}`.
fn said_of(result: Result<String, String>) -> String {
    match result {
        Ok(ok) => json!({"ok": ok}),
        Err(error) => json!({"error": error}),
    }
    .to_string()
}

/// A script's way to its tools.
#[derive(Clone)]
struct Caller {
    hub: Hub,
    lookout: tokio::runtime::Handle,
    nested: Arc<BTreeMap<String, Nested>>,
    failed: Arc<Vec<String>>,
    calls: Arc<Mutex<BTreeMap<String, usize>>>,
    steps: Option<Arc<Steps>>,
    clock: Arc<Clock>,
}

impl Caller {
    async fn call(&self, id: String, arguments: String) -> Value {
        let Some(nested) = self.nested.get(&id).cloned() else {
            let mut error = format!("tools.{id} is not a tool; ALL_TOOLS lists them");
            if !self.failed.is_empty() {
                error.push_str(&format!(" ({})", self.failed.join("; ")));
            }
            return json!({"error": error});
        };
        let arguments: Value = match serde_json::from_str(&arguments) {
            Ok(arguments) => arguments,
            Err(e) => return json!({"error": e.to_string()}),
        };
        // Put to the agent's gate first, with the script's time stopped,
        // on the lookout's runtime, where its connection lives.
        if let Some(steps) = self.steps.clone().filter(|_| nested.asks) {
            let asked = match &arguments {
                Value::Null => json!({}),
                arguments => arguments.clone(),
            };
            let stopped = self.clock.stop();
            let approval = Abort(
                self.lookout
                    .spawn(async move { steps.ask(&id, asked).await }),
            );
            let approved = approval.await;
            drop(stopped);
            // Whatever the verdict, the script runs on from here.
            if let Err(error) = self.clock.take_place().await {
                return json!({"error": error});
            }
            match approved {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return json!({"error": error}),
                Err(e) => return json!({"error": e.to_string()}),
            }
        }
        *self
            .calls
            .lock()
            .unwrap()
            .entry(nested.server.clone())
            .or_default() += 1;
        let hub = self.hub.clone();
        let call = Abort(
            self.lookout
                .spawn(async move { hub.call(&nested.server, &nested.tool, arguments).await }),
        );
        match call.await {
            Ok(Ok(result)) => json!({"ok": result}),
            Ok(Err(error)) => json!({"error": error}),
            Err(e) => json!({"error": e.to_string()}),
        }
    }
}

/// A call on the lookout's runtime, stopped when the script no longer
/// waits for it.
struct Abort<T>(tokio::task::JoinHandle<T>);

impl<T> std::future::Future for Abort<T> {
    type Output = Result<T, tokio::task::JoinError>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx)
    }
}

impl<T> Drop for Abort<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `tools.tool_search`: the tools BM25 ranks first for the query, by
/// their names and descriptions.
fn search(nested: &BTreeMap<String, Nested>, arguments: &str) -> Value {
    let arguments: Value = serde_json::from_str(arguments).unwrap_or_default();
    let Some(query) = arguments["query"].as_str() else {
        return json!({"error": "tool_search takes {query: string, limit?: number}"});
    };
    let limit = arguments["limit"].as_u64().unwrap_or(8) as usize;
    let words = |text: &str| -> Vec<String> {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    let documents: Vec<(&String, Vec<String>)> = nested
        .iter()
        .map(|(id, tool)| {
            (
                id,
                words(&format!(
                    "{} {} {}",
                    tool.server, tool.tool, tool.description
                )),
            )
        })
        .collect();
    let count = documents.len() as f64;
    let average = documents.iter().map(|(_, w)| w.len()).sum::<usize>() as f64 / count.max(1.0);
    let mut holding: HashMap<&str, usize> = HashMap::new();
    for (_, document) in &documents {
        let mut seen: Vec<&str> = document.iter().map(String::as_str).collect();
        seen.sort_unstable();
        seen.dedup();
        for word in seen {
            *holding.entry(word).or_default() += 1;
        }
    }
    let (k1, b) = (1.2, 0.75);
    let query = words(query);
    let mut ranked: Vec<(f64, &String)> = documents
        .iter()
        .map(|(id, document)| {
            let score = query
                .iter()
                .map(|word| {
                    let n = holding.get(word.as_str()).copied().unwrap_or(0) as f64;
                    let idf = ((count - n + 0.5) / (n + 0.5) + 1.0).ln();
                    let tf = document.iter().filter(|w| *w == word).count() as f64;
                    idf * tf * (k1 + 1.0)
                        / (tf + k1 * (1.0 - b + b * document.len() as f64 / average.max(1.0)))
                })
                .sum();
            (score, *id)
        })
        .filter(|(score, _)| *score > 0.0)
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let tools: Vec<Value> = ranked
        .into_iter()
        .take(limit)
        .map(|(_, id)| json!({"name": id, "description": nested[id].description}))
        .collect();
    json!({"ok": {"tools": tools}})
}

/// `image()`: the image saved under `dir`, as the line naming it.
fn image(dir: &Path, item: &str) -> Result<String, String> {
    use base64::Engine;
    let item: Value = serde_json::from_str(item).map_err(|e| e.to_string())?;
    let (data, mime) = match &item {
        Value::String(url) => data_url(url)?,
        Value::Object(fields) if fields.get("type") == Some(&json!("image")) => (
            str_of(&item["data"]).to_owned(),
            str_of(&item["mimeType"]).to_owned(),
        ),
        Value::Object(fields) if fields.contains_key("image_url") => {
            data_url(str_of(&item["image_url"]))?
        }
        _ => return Err("image() takes a data: URL, {image_url} or an MCP ImageContent".into()),
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|e| format!("image data is not base64: {e}"))?;
    let extension = match mime.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "img",
    };
    let file = fresh(dir)?.join(format!("{}.{extension}", stamp()));
    std::fs::write(&file, bytes).map_err(|e| e.to_string())?;
    Ok(format!("[image saved to {}]", file.display()))
}

fn str_of(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

/// A base64 `data:` URL's data and type.
fn data_url(url: &str) -> Result<(String, String), String> {
    let refused = || "image() takes a base64-encoded data: URL".to_owned();
    let rest = url.strip_prefix("data:").ok_or_else(refused)?;
    let (head, data) = rest.split_once(',').ok_or_else(refused)?;
    let mime = head.strip_suffix(";base64").ok_or_else(refused)?;
    Ok((data.to_owned(), mime.to_owned()))
}

/// The values an agent stored.
fn kept(file: &Path) -> Map<String, Value> {
    std::fs::read(file)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Store `value`, JSON, under `key` in the agent's `file`.
fn keep(file: &Path, key: &str, value: &str) -> Result<(), String> {
    let value: Value = serde_json::from_str(value).map_err(|e| e.to_string())?;
    let mut values = kept(file);
    values.insert(key.to_owned(), value);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let staged = file.with_extension(format!("json.{}", stamp()));
    std::fs::write(&staged, Value::Object(values).to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&staged, file).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client::config::McpServer;

    #[test]
    fn the_options_line_is_codex_s_and_read_off_the_source() {
        let plain = parse("text('hi')").unwrap();
        assert_eq!(
            (plain.code.as_str(), plain.max_output_tokens),
            ("text('hi')", None)
        );
        let options = parse(
            "// @exec: {\"max_output_tokens\": 5, \"yield_time_ms\": 10, \"timeout_ms\": 9}\ntext(1)",
        );
        assert_eq!(
            options.unwrap(),
            Script {
                code: "text(1)".into(),
                max_output_tokens: Some(5),
                timeout_ms: Some(9)
            }
        );
        assert!(parse("  ").is_err());
        assert!(parse("// @exec: {}\n").is_err());
        assert!(
            parse("// @exec: {\"other\": 1}\ntext(1)")
                .unwrap_err()
                .contains("`other`")
        );
        assert!(parse("// @exec: [1]\ntext(1)").is_err());
    }

    #[test]
    fn schemas_are_declared_as_typescript() {
        let schema = json!({"type": "object", "properties": {
            "query": {"type": "string"}, "limit": {"type": "integer"},
            "state": {"enum": ["open", "closed"]}, "labels": {"type": "array", "items": {"type": "string"}},
            "my-key": {"type": ["string", "null"]}},
            "required": ["query"], "additionalProperties": false});
        assert_eq!(
            typescript(&schema),
            "{ labels?: Array<string>; limit?: number; \"my-key\"?: string | null; query: string; \
             state?: \"open\" | \"closed\"; }"
        );
        let described = json!({"type": "object", "properties": {"q": {"type": "string",
            "description": "What to find."}}});
        assert_eq!(
            typescript(&described),
            "{\n  // What to find.\n  q?: string;\n}"
        );
        let referred = json!({"$defs": {"Id": {"type": "number"}}, "type": "object",
            "properties": {"id": {"$ref": "#/$defs/Id"}}, "required": ["id"]});
        assert_eq!(typescript(&referred), "{ id: number; }");
        // A cycle ends in unknown.
        let cycle = json!({"$defs": {"N": {"type": "object", "properties": {"next": {"$ref": "#/$defs/N"}}}},
            "$ref": "#/$defs/N"});
        assert!(typescript(&cycle).contains("unknown"));
        assert_eq!(
            typescript(&json!({"type": "object"})),
            "{ [key: string]: unknown; }"
        );
        assert_eq!(
            identifier("my-server", "get.item"),
            "mcp__my_server__get_item"
        );
    }

    #[test]
    fn the_description_declares_tools_while_they_fit() {
        let servers: Servers = [(
            "test".to_owned(),
            McpServer {
                description: Some("Test tools".into()),
                ..McpServer::default()
            },
        )]
        .into();
        let tool = |n: usize| {
            json!({"name": format!("tool{n}"), "description": "x".repeat(500),
            "inputSchema": {"type": "object"}})
        };
        let known: BTreeMap<String, Known> = [(
            "test".to_owned(),
            Known {
                tools: (0..40).map(tool).collect(),
                ..Known::default()
            },
        )]
        .into();
        let text = description(&servers, &known);
        assert!(text.contains("## test\nTest tools"));
        assert!(text.contains("declare const tools: { mcp__test__tool0(args: { [key: string]: unknown; }): Promise<CallToolResult>; };"));
        let declared = text.matches("### `mcp__test__").count();
        assert!(declared > 10 && declared < 40, "{declared}");
        assert!(text.ends_with(&format!("{} more tools are in `ALL_TOOLS`.", 40 - declared)));
        // A server not listed yet is still named.
        let unlisted: BTreeMap<String, Known> = [("test".to_owned(), Known::default())].into();
        assert!(description(&servers, &unlisted).contains("once it starts"));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sonny-exec-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn hub(dir: &Path) -> Hub {
        let server = McpServer {
            command: Some("python3".into()),
            args: vec![concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/mcp_server.py").into()],
            env: [(
                "MCP_STARTS".to_owned(),
                dir.join("starts").display().to_string(),
            )]
            .into(),
            enabled: true,
            ..McpServer::default()
        };
        let hub = Hub::new(Some(dir.join("mcp")));
        hub.configure(&[("test".to_owned(), server)].into()).await;
        hub
    }

    async fn ran(hub: &Hub, dir: &Path, input: &str) -> String {
        run(
            hub,
            Some(dir),
            "shop.lead",
            input,
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
    }

    /// A daemon that only greets and answers `approve_step`: `add` is
    /// denied, and `slow` allowed after `wait`, with each step it was
    /// asked sent on.
    /// A daemon that denies `add` and allows other steps after `wait`, or
    /// once the test lets one through `go`.
    async fn gate(
        dir: &Path,
        wait: Duration,
        go: Option<Arc<tokio::sync::Semaphore>>,
    ) -> (Arc<Client>, tokio::sync::mpsc::UnboundedReceiver<Value>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let socket = dir.join("gate.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (asked, steps) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, write) = stream.into_split();
            let write = Arc::new(tokio::sync::Mutex::new(write));
            let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL});
            write
                .lock()
                .await
                .write_all(format!("{ready}\n").as_bytes())
                .await
                .unwrap();
            let mut lines = tokio::io::BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: Value = serde_json::from_str(&line).unwrap();
                asked.send(request.clone()).unwrap();
                let write = Arc::clone(&write);
                let go = go.clone();
                tokio::spawn(async move {
                    let result = match request["name"].as_str() {
                        Some("mcp__test__add") => {
                            json!({"decision": "deny", "reason": "no adding"})
                        }
                        _ => {
                            match go {
                                Some(go) => go.acquire().await.unwrap().forget(),
                                None => tokio::time::sleep(wait).await,
                            }
                            json!({"decision": "allow"})
                        }
                    };
                    let reply = json!({"id": request["id"], "result": result});
                    let mut write = write.lock().await;
                    write
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                });
            }
        });
        let (client, _events) = Client::connect(&socket).await.unwrap();
        (client, steps)
    }

    #[tokio::test]
    async fn each_call_that_asks_first_waits_for_the_gate_with_the_script_s_time_stopped() {
        let dir = scratch("steps");
        let hub = hub(&dir).await;
        // Under `writes`, only `echo`, marked read-only, asks nothing.
        let mut servers = hub.servers();
        let test = servers.get_mut("test").unwrap();
        test.default_tools_approval_mode = agent_client::config::ApprovalMode::Writes;
        test.tools
            .insert("fail".into(), agent_client::config::ApprovalMode::Approve);
        hub.configure(&servers).await;
        let (client, mut steps) = gate(&dir, Duration::from_millis(600), None).await;
        let asker = Steps::new(client, "shop.lead".into(), 7, "c1".into());
        // A script with 300 ms whose `slow` waits 600 ms for its verdict.
        let script = r#"// @exec: {"timeout_ms": 300}
text(await tools.mcp__test__echo({text: "hi"}).then(r => r.content[0].text));
try { await tools.mcp__test__add({a: 1, b: 2}); } catch (e) { text(e.message); }
text((await tools.mcp__test__fail({})).isError);
await tools.mcp__test__slow({ms: 10});
text("done");"#;
        let said = run(
            &hub,
            Some(&dir),
            "shop.lead",
            script,
            Duration::from_secs(30),
            Some(asker),
        )
        .await
        .unwrap();
        assert!(said.starts_with("Script completed"), "{said}");
        assert!(
            said.ends_with(
                "Output:\nhi\ntools.mcp__test__add was not approved: no adding\ntrue\ndone"
            ),
            "{said}"
        );
        let mut asked = Vec::new();
        while let Ok(step) = steps.try_recv() {
            asked.push(step);
        }
        let seen: Vec<(&str, &str, &str, i64, &str)> = asked
            .iter()
            .map(|s| {
                (
                    s["op"].as_str().unwrap(),
                    s["step"].as_str().unwrap(),
                    s["name"].as_str().unwrap(),
                    s["turn"].as_i64().unwrap(),
                    s["call_id"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("approve_step", "1", "mcp__test__add", 7, "c1"),
                ("approve_step", "2", "mcp__test__slow", 7, "c1")
            ]
        );
        assert_eq!(asked[1]["arguments"], json!({"ms": 10}));
        // With no gate to ask, every call runs.
        let said = run(
            &hub,
            Some(&dir),
            "shop.lead",
            script,
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap();
        assert!(said.ends_with("Output:\nhi\ntrue\ndone"), "{said}");
    }

    #[tokio::test]
    async fn scripts_waiting_for_verdicts_leave_their_places_to_others() {
        let dir = scratch("steps-places");
        let hub = hub(&dir).await;
        let mut servers = hub.servers();
        servers.get_mut("test").unwrap().default_tools_approval_mode =
            agent_client::config::ApprovalMode::Prompt;
        hub.configure(&servers).await;
        let (client, _steps) = gate(&dir, Duration::from_secs(3), None).await;
        let waiting: Vec<_> = (0..SCRIPTS)
            .map(|n| {
                let (hub, dir) = (hub.clone(), dir.clone());
                let asker = Steps::new(client.clone(), "shop.lead".into(), 7, format!("c{n}"));
                tokio::spawn(async move {
                    let script = "await tools.mcp__test__slow({ms: 1}); text('approved');";
                    run(
                        &hub,
                        Some(&dir),
                        "shop.lead",
                        script,
                        Duration::from_secs(30),
                        Some(asker),
                    )
                    .await
                })
            })
            .collect();
        // Every place is free while their verdicts are out: another runs.
        let said = ran(&hub, &dir, "text('meanwhile');").await;
        assert!(said.ends_with("Output:\nmeanwhile"), "{said}");
        assert!(waiting.iter().all(|script| !script.is_finished()));
        for script in waiting {
            let said = script.await.unwrap().unwrap();
            assert!(said.ends_with("Output:\napproved"), "{said}");
        }
    }

    #[tokio::test]
    async fn scripts_waiting_for_verdicts_still_count_toward_those_alive() {
        let dir = scratch("steps-alive");
        let hub = hub(&dir).await;
        let mut servers = hub.servers();
        servers.get_mut("test").unwrap().default_tools_approval_mode =
            agent_client::config::ApprovalMode::Prompt;
        hub.configure(&servers).await;
        let go = Arc::new(tokio::sync::Semaphore::new(0));
        let (client, mut steps) = gate(&dir, Duration::ZERO, Some(Arc::clone(&go))).await;
        assert_eq!(LIVE, 16);
        let waiting: Vec<_> = (0..LIVE)
            .map(|n| {
                let (hub, dir) = (hub.clone(), dir.clone());
                let asker = Steps::new(client.clone(), "shop.lead".into(), 7, format!("c{n}"));
                tokio::spawn(async move {
                    let script = "await tools.mcp__test__slow({ms: 1}); text('approved');";
                    run(
                        &hub,
                        Some(&dir),
                        "shop.lead",
                        script,
                        Duration::from_secs(30),
                        Some(asker),
                    )
                    .await
                })
            })
            .collect();
        for _ in 0..LIVE {
            steps.recv().await.unwrap();
        }
        // Every one waits with its heap kept: none is left for another.
        assert_eq!(ALIVE.available_permits(), 0);
        go.add_permits(LIVE);
        for script in waiting {
            let said = script.await.unwrap().unwrap();
            assert!(said.ends_with("Output:\napproved"), "{said}");
        }
    }

    #[tokio::test]
    async fn javascript_running_beside_a_waiting_call_spends_the_script_s_time() {
        let dir = scratch("steps-busy");
        let hub = hub(&dir).await;
        let mut servers = hub.servers();
        let test = servers.get_mut("test").unwrap();
        test.default_tools_approval_mode = agent_client::config::ApprovalMode::Prompt;
        test.tools
            .insert("echo".into(), agent_client::config::ApprovalMode::Approve);
        hub.configure(&servers).await;
        // No verdict ever comes: only the time the spinning spends ends it.
        let go = Arc::new(tokio::sync::Semaphore::new(0));
        let (client, _steps) = gate(&dir, Duration::ZERO, Some(go)).await;
        let asker = Steps::new(client, "shop.lead".into(), 7, "c1".into());
        // `slow` waits for its verdict while the other branch spins.
        let script = r#"// @exec: {"timeout_ms": 300}
await Promise.all([
  tools.mcp__test__slow({ms: 1}),
  tools.mcp__test__echo({text: "x"}).then(() => { for (;;) {} }),
]);"#;
        let said = run(
            &hub,
            Some(&dir),
            "shop.lead",
            script,
            Duration::from_secs(30),
            Some(asker),
        )
        .await
        .unwrap();
        assert!(
            said.starts_with("Script terminated: it ran out of time"),
            "{said}"
        );
        hub.stop().await;
    }

    #[tokio::test]
    async fn a_script_calls_tools_together_and_returns_only_what_it_prints() {
        let dir = scratch("calls");
        let hub = hub(&dir).await;
        let said = ran(
            &hub,
            &dir,
            r#"const [a, b] = await Promise.all([
  tools.mcp__test__echo({text: "one"}), tools.mcp__test__add({a: 2, b: 3})]);
text(a.content[0].text);
text(b.structuredContent);
text(ALL_TOOLS.map(t => t.name).join(","));"#,
        )
        .await;
        assert!(said.starts_with("Script completed\nWall time "), "{said}");
        assert!(
            said.contains("MCP calls: test 2\nOutput:\none\n{\"sum\":5}\n"),
            "{said}"
        );
        assert!(said.ends_with("mcp__test__echo,mcp__test__add,mcp__test__fail,mcp__test__slow"));
        // Fan-out runs at once: four calls of 300 ms in well under 1.2 s,
        // by the script's own clock, which starts once it may run.
        let fanned = ran(
            &hub,
            &dir,
            "await Promise.all([1,2,3,4].map(() => tools.mcp__test__slow({ms: 300})));",
        )
        .await;
        let wall: f64 = (fanned.split("Wall time ").nth(1))
            .and_then(|rest| rest.split(' ').next())
            .and_then(|seconds| seconds.parse().ok())
            .unwrap();
        assert!(wall < 1.0, "{fanned}");
        // One server process served every call, kept up between scripts.
        let starts = std::fs::read_to_string(dir.join("starts")).unwrap();
        assert_eq!(starts.lines().count(), 1, "{starts}");
        hub.stop().await;
    }

    #[tokio::test]
    async fn failures_say_why_and_keep_what_was_printed() {
        let dir = scratch("failures");
        let hub = hub(&dir).await;
        let failed = ran(
            &hub,
            &dir,
            "text('before');\nawait tools.mcp__test__nope({});",
        )
        .await;
        assert!(failed.starts_with("Script failed\n"), "{failed}");
        assert!(
            failed.contains(
                "Output:\nbefore\nScript error:\nError: tools.mcp__test__nope is not a tool"
            ),
            "{failed}"
        );
        // A tool's own error comes back as its result, isError set.
        let tool = ran(
            &hub,
            &dir,
            "const r = await tools.mcp__test__fail({}); text(r.isError);",
        )
        .await;
        assert!(tool.ends_with("Output:\ntrue"), "{tool}");
        let syntax = ran(&hub, &dir, "text(").await;
        assert!(syntax.contains("Script error:\nSyntaxError"), "{syntax}");
        let thrown = ran(&hub, &dir, "throw {code: 7};").await;
        assert!(thrown.ends_with("Script error:\n{\"code\":7}"), "{thrown}");
        let exited = ran(&hub, &dir, "text(1); exit(); text(2);").await;
        assert!(
            exited.starts_with("Script completed") && exited.ends_with("Output:\n1"),
            "{exited}"
        );
        hub.stop().await;
    }

    #[tokio::test]
    async fn a_script_ends_on_its_time_its_memory_or_its_turn() {
        let dir = scratch("limits");
        let hub = hub(&dir).await;
        let spun = ran(
            &hub,
            &dir,
            "// @exec: {\"timeout_ms\": 200}\ntext('x'); while (true) {}",
        )
        .await;
        assert!(
            spun.starts_with("Script terminated: it ran out of time"),
            "{spun}"
        );
        assert!(spun.ends_with("Output:\nx"), "{spun}");
        let waited = ran(
            &hub,
            &dir,
            "// @exec: {\"timeout_ms\": 200}\nawait tools.mcp__test__slow({ms: 5000});",
        )
        .await;
        assert!(
            waited.starts_with("Script terminated: it ran out of time"),
            "{waited}"
        );
        let grew = ran(
            &hub,
            &dir,
            "const a = []; while (true) a.push('x'.repeat(1 << 20));",
        )
        .await;
        assert!(
            grew.starts_with("Script failed") && grew.contains("out of memory"),
            "{grew}"
        );
        // A dropped answer, as an interrupted turn's, stops its script.
        let started = Instant::now();
        let spinning = run(
            &hub,
            Some(&dir),
            "shop.lead",
            "while (true) {}",
            Duration::from_secs(30),
            None,
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), spinning)
                .await
                .is_err()
        );
        let after = ran(&hub, &dir, "text('next');").await;
        assert!(after.ends_with("next") && started.elapsed() < Duration::from_secs(5));
        hub.stop().await;
    }

    #[tokio::test]
    async fn scripts_past_the_running_limit_wait_their_turn() {
        let dir = scratch("running");
        let hub = hub(&dir).await;
        let slept = "await tools.mcp__test__slow({ms: 400}); text('done');";
        let started = Instant::now();
        assert_eq!(SCRIPTS, 4);
        let (a, b, c, d, e) = tokio::join!(
            ran(&hub, &dir, slept),
            ran(&hub, &dir, slept),
            ran(&hub, &dir, slept),
            ran(&hub, &dir, slept),
            ran(&hub, &dir, slept)
        );
        let all = [a, b, c, d, e];
        assert!(
            all.iter().all(|said| said.ends_with("Output:\ndone")),
            "{all:?}"
        );
        // One more than may run at once ran after the first finished.
        assert!(
            started.elapsed() >= Duration::from_millis(800),
            "{:?}",
            started.elapsed()
        );
        hub.stop().await;
    }

    #[tokio::test]
    async fn helpers_keep_values_find_tools_save_images_and_cut_long_output() {
        let dir = scratch("helpers");
        let hub = hub(&dir).await;
        ran(&hub, &dir, "store('seen', {n: 1});").await;
        let loaded = ran(
            &hub,
            &dir,
            "text(load('seen').n); text(String(load('missing')));",
        )
        .await;
        assert!(loaded.ends_with("Output:\n1\nundefined"), "{loaded}");
        let found = ran(&hub, &dir, "const r = await tools.tool_search({query: 'add numbers', limit: 1}); text(r.tools[0].name); text(r.tools[0].description.includes('declare const tools'));").await;
        assert!(found.ends_with("Output:\nmcp__test__add\ntrue"), "{found}");
        let pixel = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAwS2OUAAAAABJRU5ErkJggg==";
        let shown = ran(
            &hub,
            &dir,
            &format!("image({{type: 'image', data: '{pixel}', mimeType: 'image/png'}});"),
        )
        .await;
        let path = shown
            .split("[image saved to ")
            .nth(1)
            .unwrap()
            .trim_end_matches(']');
        assert!(
            path.ends_with(".png") && Path::new(path).is_file(),
            "{shown}"
        );
        let long = ran(
            &hub,
            &dir,
            "// @exec: {\"max_output_tokens\": 100}\ntext('a'.repeat(1000)); text('end');",
        )
        .await;
        assert!(
            long.contains("tokens truncated; the whole output is in ") && long.ends_with("end"),
            "{long}"
        );
        assert!(long.len() < 700, "{}", long.len());
        hub.stop().await;
    }

    #[test]
    fn saved_outputs_and_images_are_kept_a_week() {
        let dir = scratch("kept").join("output");
        std::fs::create_dir_all(&dir).unwrap();
        let (old, new) = (dir.join("old.txt"), dir.join("new.txt"));
        for file in [&old, &new] {
            std::fs::write(file, "x").unwrap();
        }
        let week_ago = std::time::SystemTime::now() - KEPT - Duration::from_secs(60);
        (std::fs::File::options().write(true).open(&old).unwrap())
            .set_modified(week_ago)
            .unwrap();
        budgeted("y".repeat(100), 1, dir.parent());
        let left: Vec<String> = (std::fs::read_dir(&dir).unwrap().flatten())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!left.contains(&"old.txt".to_owned()), "{left:?}");
        assert!(
            left.contains(&"new.txt".to_owned()) && left.len() == 2,
            "{left:?}"
        );
    }

    #[tokio::test]
    async fn an_idle_server_stops_and_the_next_call_starts_it_again() {
        let dir = scratch("idle");
        let hub = hub(&dir).await;
        ran(&hub, &dir, "await tools.mcp__test__echo({text: 'a'});").await;
        let first = std::fs::read_to_string(dir.join("starts")).unwrap();
        let alive = |pid: &str| {
            (std::process::Command::new("kill").args(["-0", pid.trim()]))
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(alive(&first));
        // Used within the idle time, it stays.
        hub.reap(Duration::from_secs(60)).await;
        assert!(alive(&first));
        hub.reap(Duration::ZERO).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(&first) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!alive(&first), "server {first} outlived its idle time");
        ran(&hub, &dir, "await tools.mcp__test__echo({text: 'b'});").await;
        let starts = std::fs::read_to_string(dir.join("starts")).unwrap();
        assert_eq!(starts.lines().count(), 2, "{starts}");
        hub.stop().await;
    }
}
