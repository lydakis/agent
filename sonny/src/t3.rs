//! T3 Code, as `sonny acp` meets it: the tools T3 Code gives the agents it
//! runs, over an MCP endpoint it names in their environment, with which a
//! lead's thread opens a child thread for each task the lead starts; the
//! tasks waiting for their thread, and the bot each thread is.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The MCP protocol version Sonny asks T3 Code's endpoint for, as T3
/// Code's own terminal fallback does.
const MCP_VERSION: &str = "2025-06-18";

/// How long a task waits for its thread to claim it.
const PENDING_SECS: u64 = 10 * 60;

/// T3 Code's tools for the thread this process serves: the endpoint and the
/// credential T3 Code scopes to that thread.
pub struct T3 {
    endpoint: String,
    authorization: String,
}

impl T3 {
    /// T3 Code's endpoint, when T3 Code started this process.
    pub fn from_env() -> Option<T3> {
        let var = |name| std::env::var(name).ok().filter(|v| !v.is_empty());
        Some(T3 {
            endpoint: var("T3_ACP_MCP_ENDPOINT")?,
            authorization: var("T3_ACP_MCP_AUTHORIZATION")?,
        })
    }

    /// The variables `from_env` reads: the credential is this thread's, and
    /// no program Sonny starts (the daemon, the lookout) takes it.
    pub const VARS: [&str; 4] = [
        "T3_ACP_MCP_ENDPOINT",
        "T3_ACP_MCP_AUTHORIZATION",
        "T3_ACP_MCP_NODE",
        "T3_ACP_MCP_ENTRYPOINT",
    ];

    /// Call one of T3 Code's tools: what it returned, or why not.
    pub async fn call(&self, tool: &str, arguments: Value) -> Result<Value, String> {
        let http = reqwest::Client::new();
        let mut session = Mcp {
            http: &http,
            t3: self,
            id: None,
            version: None,
        };
        session
            .request(
                "initialize",
                json!({"protocolVersion": MCP_VERSION, "capabilities": {},
                "clientInfo": {"name": "sonny", "version": env!("CARGO_PKG_VERSION")}}),
            )
            .await?;
        let called = async {
            session
                .post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .await?;
            session
                .request("tools/call", json!({"name": tool, "arguments": arguments}))
                .await
        }
        .await;
        session.end().await;
        result(tool, called?)
    }
}

/// One MCP session on T3 Code's endpoint.
struct Mcp<'a> {
    http: &'a reqwest::Client,
    t3: &'a T3,
    id: Option<String>,
    version: Option<String>,
}

impl Mcp<'_> {
    fn headers(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut request = request
            .header("accept", "application/json, text/event-stream")
            .header("authorization", &self.t3.authorization);
        if let Some(id) = &self.id {
            request = request.header("mcp-session-id", id);
        }
        if let Some(version) = &self.version {
            request = request.header("mcp-protocol-version", version);
        }
        request
    }

    /// Post one message: what came back, as JSON or as server-sent events.
    async fn post(&mut self, message: &Value) -> Result<Vec<Value>, String> {
        let response = self
            .headers(self.http.post(&self.t3.endpoint))
            .header("content-type", "application/json")
            .body(message.to_string())
            .send()
            .await
            .map_err(|e| format!("T3 Code's tools: {e}"))?;
        if let Some(id) = response.headers().get("mcp-session-id") {
            self.id = id.to_str().ok().map(str::to_owned);
        }
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("T3 Code's tools: {e}"))?;
        if !status.is_success() {
            return Err(format!("T3 Code's tools answered HTTP {}", status.as_u16()));
        }
        Ok(payloads(&body))
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = format!("sonny-{method}");
        let replies = self
            .post(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let reply = replies
            .into_iter()
            .find(|reply| reply["id"] == id.as_str())
            .ok_or_else(|| format!("T3 Code's tools did not answer {method}"))?;
        if !reply["error"].is_null() {
            return Err(format!("T3 Code's {method}: {}", reply["error"]));
        }
        if let Some(version) = reply["result"]["protocolVersion"].as_str() {
            self.version = Some(version.to_owned());
        }
        Ok(reply["result"].clone())
    }

    async fn end(&self) {
        if self.id.is_some() {
            let _ = self
                .headers(self.http.delete(&self.t3.endpoint))
                .send()
                .await;
        }
    }
}

/// The messages in a reply's body: one JSON value or a list of them, or
/// the `data` of each server-sent event.
fn payloads(body: &str) -> Vec<Value> {
    let parsed = |text: &str| match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(all)) => all,
        Ok(one) => vec![one],
        Err(_) => Vec::new(),
    };
    let whole = parsed(body.trim());
    if !whole.is_empty() {
        return whole;
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .flat_map(|data| parsed(data.trim()))
        .collect()
}

/// A tool's result: its structured content, else its text as JSON, else
/// its text; or the error it reported.
fn result(tool: &str, called: Value) -> Result<Value, String> {
    let text: String = called["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect();
    if called["isError"] == true {
        return Err(format!("T3 Code's {tool}: {text}"));
    }
    if !called["structuredContent"].is_null() {
        return Ok(called["structuredContent"].clone());
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

/// The note T3 Code adds to every prompt as a block of its own, after the
/// person's words: which harness runs the thread, and how it links pull
/// requests. It is T3 Code's, not the person's.
pub fn is_note(block: &str) -> bool {
    block.starts_with("<runtime_info>")
}

/// The message T3 Code sends a thread when a task it delegated ends,
/// which asks it to read the task's result with T3 Code's tools: a lead
/// hears of its tasks from the lookout instead.
pub fn is_completion(prompt: &str) -> bool {
    let prompt = prompt.trim();
    (prompt.starts_with("Delegated task ") && prompt.contains(" reached a terminal state."))
        || (prompt.starts_with("Delegated tasks ") && prompt.contains(" reached terminal states."))
}

/// What the person wrote, out of a prompt T3 Code wrapped in its own
/// instructions: those name tools the bot does not have.
pub fn request(prompt: &str) -> &str {
    const OPEN: &str = "<user_request>\n";
    const CLOSE: &str = "\n</user_request>";
    if !prompt.starts_with("<t3_code") {
        return prompt;
    }
    match (prompt.find(OPEN), prompt.strip_suffix(CLOSE)) {
        (Some(at), Some(inside)) if at + OPEN.len() <= inside.len() => &inside[at + OPEN.len()..],
        _ => prompt,
    }
}

/// Where Sonny keeps what threads are which bots, and the tasks waiting
/// for theirs.
fn dir(of: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".agent/sonny/t3").join(of))
}

fn file_name(bot: &str) -> String {
    bot.replace('/', "%2F")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A folder as T3 Code and the daemon may each spell it.
fn same_folder(a: &str, b: &Path) -> bool {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    real(Path::new(a)) == real(b)
}

/// The tasks waiting for their thread, oldest first; ones waiting too
/// long go.
fn pending(dir: &Path) -> Vec<(PathBuf, Value)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let now = now();
    let mut all: Vec<(PathBuf, Value)> = entries
        .flatten()
        .filter_map(|entry| {
            let record =
                serde_json::from_str::<Value>(&std::fs::read_to_string(entry.path()).ok()?).ok()?;
            Some((entry.path(), record))
        })
        .filter(|(path, record)| {
            let fresh = now.saturating_sub(record["at"].as_u64().unwrap_or(0)) <= PENDING_SECS;
            if !fresh {
                let _ = std::fs::remove_file(path);
            }
            fresh
        })
        .collect();
    all.sort_by_key(|(_, record)| record["at"].as_u64());
    all
}

/// The task `bot` waits for the thread T3 Code opens in `cwd`: the first
/// message to open it with, the brief, told apart by the task's name from
/// another task's waiting with the same brief in the same folder.
pub fn expect(bot: &str, brief: &str, cwd: &Path) -> Result<String, String> {
    expect_in(&dir("pending").ok_or("HOME is not set")?, bot, brief, cwd)
}

fn expect_in(dir: &Path, bot: &str, brief: &str, cwd: &Path) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let taken = pending(dir).into_iter().any(|(_, record)| {
        record["bot"] != bot
            && record["message"].as_str() == Some(brief)
            && record["cwd"]
                .as_str()
                .is_some_and(|at| same_folder(at, cwd))
    });
    let message = match taken {
        true => format!("{brief}\n\n({bot})"),
        false => brief.to_owned(),
    };
    let record = json!({"bot": bot, "message": message, "cwd": cwd, "at": now()});
    std::fs::write(dir.join(file_name(bot)), record.to_string()).map_err(|e| e.to_string())?;
    Ok(message)
}

/// No thread comes for `bot`.
pub fn forget(bot: &str) {
    if let Some(dir) = dir("pending") {
        let _ = std::fs::remove_file(dir.join(file_name(bot)));
    }
}

/// The task a thread in `cwd` opens with its first prompt: the one waiting
/// for that message there, which no other thread then claims.
pub fn claim(prompt: &str, cwd: &Path) -> Option<String> {
    claim_in(&dir("pending")?, prompt, cwd)
}

fn claim_in(dir: &Path, prompt: &str, cwd: &Path) -> Option<String> {
    pending(dir).into_iter().find_map(|(path, record)| {
        let ours = record["message"].as_str().map(str::trim) == Some(prompt.trim())
            && record["cwd"]
                .as_str()
                .is_some_and(|at| same_folder(at, cwd));
        // Claimed by whoever removes it.
        (ours && std::fs::remove_file(&path).is_ok())
            .then(|| record["bot"].as_str().map(str::to_owned))
            .flatten()
    })
}

/// The bot a thread is: its name, and once bound, its identity and its
/// store's lineage, so a thread that comes back is that bot or none: not
/// another made since under its name, nor one of another store's.
#[derive(Clone, Debug, PartialEq)]
pub struct Pinned {
    pub bot: String,
    pub bot_id: Option<i64>,
    pub store: Option<String>,
}

impl Pinned {
    /// A bot known by name only, as a task waiting for its thread is.
    pub fn named(bot: String) -> Pinned {
        Pinned {
            bot,
            bot_id: None,
            store: None,
        }
    }
}

/// The bot the thread `session` is, for its next start.
pub fn remember(session: &str, pinned: &Pinned) {
    if let Some(dir) = dir("sessions") {
        remember_in(&dir, session, pinned);
    }
}

fn remember_in(dir: &Path, session: &str, pinned: &Pinned) {
    let record = json!({"bot": pinned.bot, "bot_id": pinned.bot_id, "store": pinned.store});
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join(file_name(session)), record.to_string());
}

pub fn recall(session: &str) -> Option<Pinned> {
    recall_in(&dir("sessions")?, session)
}

fn recall_in(dir: &Path, session: &str) -> Option<Pinned> {
    let text = std::fs::read_to_string(dir.join(file_name(session))).ok()?;
    let record = match serde_json::from_str::<Value>(&text) {
        Ok(record @ Value::Object(_)) => record,
        // A thread kept before identities were: its name, which binding it
        // once pins.
        _ => {
            let bot = text.trim();
            return (!bot.is_empty()).then(|| Pinned::named(bot.to_owned()));
        }
    };
    Some(Pinned {
        bot: record["bot"]
            .as_str()
            .filter(|bot| !bot.is_empty())?
            .to_owned(),
        bot_id: record["bot_id"].as_i64(),
        store: record["store"].as_str().map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_persons_words_come_out_of_t3_codes_wrapping() {
        let wrapped = "<t3_code_instructions>\n## T3 Code interaction mode: Default\n\nUse delegate_task.\n</t3_code_instructions>\n\n<user_request>\nFix the cart.\nThen test it.\n</user_request>";
        assert_eq!(request(wrapped), "Fix the cart.\nThen test it.");
        let orchestration = "<t3_code_orchestration_instructions>x</t3_code_orchestration_instructions>\n\n<user_request>\nHi\n</user_request>";
        assert_eq!(request(orchestration), "Hi");
        // Words of the person's own that look like it stay.
        assert_eq!(
            request("<user_request>\nHi\n</user_request>"),
            "<user_request>\nHi\n</user_request>"
        );
        assert_eq!(request("Fix it."), "Fix it.");
    }

    #[test]
    fn a_delegated_tasks_end_is_t3_codes_message() {
        assert!(is_completion(
            "Delegated task node-1 reached a terminal state. Use task_status with taskId node-1 to read the result."
        ));
        assert!(is_completion(
            "Delegated tasks node-1, node-2 reached terminal states. Use task_status with each taskId to read the results."
        ));
        assert!(!is_completion("Delegated task review: is it done?"));
    }

    #[test]
    fn a_thread_claims_the_task_waiting_for_its_message_in_its_folder() {
        let home = std::env::temp_dir().join(format!("sonny-t3-{}", std::process::id()));
        let (dir, one, two) = (home.join("pending"), home.join("one"), home.join("two"));
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();
        assert_eq!(
            expect_in(&dir, "a.fix", "Fix it.", &one).unwrap(),
            "Fix it."
        );
        assert_eq!(
            expect_in(&dir, "b.fix", "Fix it.", &two).unwrap(),
            "Fix it."
        );
        // The same brief in the same folder is told apart by its name.
        let other = expect_in(&dir, "a.other", "Fix it.", &one).unwrap();
        assert_eq!(other, "Fix it.\n\n(a.other)");
        assert_eq!(claim_in(&dir, "Fix it.", &two).as_deref(), Some("b.fix"));
        assert_eq!(claim_in(&dir, "Fix it.", &two), None);
        assert_eq!(claim_in(&dir, &other, &one).as_deref(), Some("a.other"));
        assert_eq!(claim_in(&dir, " Fix it.\n", &one).as_deref(), Some("a.fix"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_reply_is_json_or_server_sent_events() {
        assert_eq!(payloads(r#"{"id":1}"#), vec![json!({"id": 1})]);
        assert_eq!(
            payloads("event: message\ndata: {\"id\":1}\n\ndata: {\"id\":2}\n\n"),
            vec![json!({"id": 1}), json!({"id": 2})]
        );
        assert!(payloads("").is_empty());
    }

    #[test]
    fn a_result_is_structured_or_text() {
        let structured = json!({"content": [{"type": "text", "text": "x"}], "structuredContent": {"threadId": "t"}});
        assert_eq!(result("t", structured), Ok(json!({"threadId": "t"})));
        let text = json!({"content": [{"type": "text", "text": "{\"threadId\":\"t\"}"}]});
        assert_eq!(result("t", text), Ok(json!({"threadId": "t"})));
        let failed = json!({"content": [{"type": "text", "text": "no project"}], "isError": true});
        assert_eq!(
            result("launch", failed),
            Err("T3 Code's launch: no project".into())
        );
    }

    #[test]
    fn a_thread_remembers_its_bot_s_identity_and_store() {
        let dir = std::env::temp_dir().join(format!("sonny-t3-pinned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pinned = Pinned {
            bot: "cart/fix".into(),
            bot_id: Some(7),
            store: Some("00ab".into()),
        };
        remember_in(&dir, "s1", &pinned);
        assert_eq!(recall_in(&dir, "s1"), Some(pinned));
        assert_eq!(recall_in(&dir, "s2"), None);
        // One kept by name only binds by name once, and is pinned then.
        std::fs::write(dir.join("s3"), "errand.lead\n").unwrap();
        assert_eq!(
            recall_in(&dir, "s3"),
            Some(Pinned::named("errand.lead".into()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
