//! Registered tools share an execution budget; no per-bot worker or shell exists.
//! Tools run with the caller's OS permissions inside the bot workspace path,
//! confined by the bot's sandbox when it has one (see `sandbox`).
use crate::{
    Error, Result,
    codec::{Family, ToolSchema},
    fail, fail_with,
    sandbox::{Daemon, Policy},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::RawValue;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::Semaphore,
};

mod files;
mod patch;
mod picture;
use files::read_bounded;
pub use patch::Patch;

/// Model-facing text per tool result stream.
pub const PREVIEW_BYTES: usize = 64 * 1024;
/// Retained output per stream. Past this a stream keeps its first and last
/// `KEPT_END` bytes and the command runs on.
pub const ARTIFACT_BYTES: usize = 1024 * 1024;
/// Each end kept of a stream past `ARTIFACT_BYTES`, leaving room for the
/// marker that counts the dropped middle.
const KEPT_END: usize = (ARTIFACT_BYTES - 256) / 2;
/// The carry-forward note a bot may keep ahead of its window.
pub const NOTE_BYTES: usize = 8192;
const FILE_BYTES: usize = 4 * 1024 * 1024;
const WRITE_BYTES: usize = 1024 * 1024;
const DEFAULT_SHELL_TIMEOUT_MS: u64 = 120_000;
const MAX_SHELL_TIMEOUT_MS: u64 = 600_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 86_400_000;
/// Simultaneously running child processes, foreground or background.
pub const DEFAULT_PROCESS_BUDGET: usize = 64;
/// Detached commands still running at once, daemon-wide, unless configured.
pub const DEFAULT_DETACHED_BUDGET: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Echo,
    Shell,
    Read,
    Write,
    Edit,
    ApplyPatch,
    Wait,
    History,
    Note,
    WebSearch,
}
impl Tool {
    fn parse(name: &str) -> Option<Tool> {
        Some(match name {
            "echo" => Tool::Echo,
            "shell" => Tool::Shell,
            "read" => Tool::Read,
            "write" => Tool::Write,
            "edit" => Tool::Edit,
            "apply_patch" => Tool::ApplyPatch,
            "wait" => Tool::Wait,
            "history" => Tool::History,
            "note" => Tool::Note,
            "web_search" => Tool::WebSearch,
            _ => return None,
        })
    }
    fn name(self) -> &'static str {
        match self {
            Tool::Echo => "echo",
            Tool::Shell => "shell",
            Tool::Read => "read",
            Tool::Write => "write",
            Tool::Edit => "edit",
            Tool::ApplyPatch => "apply_patch",
            Tool::Wait => "wait",
            Tool::History => "history",
            Tool::Note => "note",
            Tool::WebSearch => "web_search",
        }
    }
    fn schema(self) -> ToolSchema {
        let (description, parameters) = match self {
            Tool::Echo => (
                "Return the supplied text unchanged.",
                json!({"type":"object",
                "properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
            ),
            Tool::Shell => (
                "Run a noninteractive bash command (sh where bash is not installed) in the workspace. stdout and stderr are returned separately; each is truncated to a head and tail beyond 64 KiB, with up to 1 MiB of it retained: past that, its first and last 512 KiB, and the command keeps running. Default timeout 120 s, maximum 600 s; a command that times out returns its output so far with timed_out. Every result reports wall_time_ms. Commands run with no terminal: TERM=dumb, NO_COLOR=1, and cat as the pager. When the command returns or times out, every process it started is killed, so `cmd &` does not outlive it. With background=true the command is started and a proc handle is returned immediately; collect its result later with wait. A steer that arrives while a command runs moves it to the background: the result is its proc handle with steered=true, and the command keeps running. A background command runs until it exits, times out, or the daemon stops. With detach=true the command starts in its own session and its pid is returned at once; nothing kills or tracks it, so it outlives the turn and the daemon. Its output is discarded unless the command redirects it to a file. Use detach for a server or service that must keep running, and stop it yourself with kill.",
                json!({"type":"object","properties":{"command":{"type":"string"},
                "timeout_ms":{"type":"integer","minimum":1,"maximum":MAX_SHELL_TIMEOUT_MS},
                "background":{"type":"boolean"},"detach":{"type":"boolean"}},
                "required":["command"],"additionalProperties":false}),
            ),
            Tool::Read => (
                "Read UTF-8 text with line numbers, or see a picture: a PNG, JPEG, GIF or WebP file comes back as the image itself, scaled down when large (any other binary file is refused, naming its kind). A file by path (relative to the workspace unless absolute), or a retained tool output by artifact reference: 'TURN/CALL_ID/STREAM' as listed in a truncated result's artifacts, or 'result/NODE' as an elided result names it, whose lines longer than 4 KiB are split into numbered pieces. Use offset (1-based line) and limit (lines, default 500) to page.",
                json!({"type":"object","properties":{"path":{"type":"string"},"artifact":{"type":"string"},
                "offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":5000}},
                "additionalProperties":false}),
            ),
            Tool::Write => (
                "Create or overwrite a file with the given content, creating parent directories. Content is limited to 1 MiB.",
                json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},
                "required":["path","content"],"additionalProperties":false}),
            ),
            Tool::Edit => (
                "Replace an exact string in a file. old must occur exactly once unless replace_all is true. Include enough surrounding text to make old unique.",
                json!({"type":"object","properties":{"path":{"type":"string"},"old":{"type":"string"},
                "new":{"type":"string"},"replace_all":{"type":"boolean"}},
                "required":["path","old","new"],"additionalProperties":false}),
            ),
            Tool::ApplyPatch => (
                "Edit files with a patch: add, update, move and delete any number of files in one call. The patch is checked against every file first, so one that does not apply changes nothing. Format: `*** Begin Patch`, then for each file `*** Add File: PATH` followed by its lines each starting with +, or `*** Delete File: PATH`, or `*** Update File: PATH` with an optional `*** Move to: NEW_PATH` and chunks of lines starting with a space (kept), - (removed) or + (added); an `@@ LINE` before a chunk names a line above it, such as a function's, to place it, and `*** End of File` after a chunk anchors it at the end. Show about three kept lines around each change. Finish with `*** End Patch`. Paths are relative to the workspace unless absolute.",
                json!({"type":"object","properties":{"input":{"type":"string","description":"The whole patch, from *** Begin Patch to *** End Patch."}},
                "required":["input"],"additionalProperties":false}),
            ),
            Tool::History => (
                "Read a conversation turn: one of yours the context left out (numbered from 1; see the context note), or with bot, another agent's, as it stands, including one still running. With bot and no turn, lists that agent's newest turns, newest first: each one's number, handle, status and prompt. Returns provider items as JSONL, omitting only encrypted_content from reasoning items; summaries remain. Pages contain at most 64 KiB and may shrink to fit context. Optional limit: 4 to 65536 bytes. Start at offset 0; use next_offset until done. Offsets count UTF-8 bytes of the filtered view. Pages may split records; concatenate text before decoding.",
                json!({"type":"object","properties":{"bot":{"type":"string","minLength":1,"maxLength":128},
                "turn":{"type":"integer","minimum":1},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":4,"maximum":65536}},
                "additionalProperties":false}),
            ),
            Tool::Note => (
                "Write or replace your carry-forward note: text the runtime places ahead of the conversation window in every request, so it stays in view when earlier turns leave the window. Up to 8192 bytes and must fit the current context budget; an oversized write preserves the previous note. Empty text removes it. The note is versioned with the conversation and a fork inherits the version at its checkpoint.",
                json!({"type":"object","properties":{"text":{"type":"string","maxLength":8192}},
                "required":["text"],"additionalProperties":false}),
            ),
            // What a model is shown is its provider's: Codex's `web.run`,
            // or the provider's own hosted tool (see provider::Search).
            Tool::WebSearch => (
                include_str!("tools/web_run.md"),
                serde_json::from_str(include_str!("tools/web_run.json")).expect("web.run schema"),
            ),
            Tool::Wait => (
                "Suspend until every handle resolves, without holding any execution capacity. Handles are 'turn:BOT/N' (a peer agent's turn, printed by run --detach) or 'proc:N' (a background shell). Each result reports the outcome: a peer's status and final text, with before_steer, the last text it wrote before it was steered, when it ended on a reply to a steer; or a process's output and exit code. With timeout_ms, unresolved handles are reported as pending and stay valid for a later wait; 0 polls without waiting. With any: true, the first resolved handle ends the wait and the rest are reported pending. A steer ends the wait at once with steered=true: unresolved handles are reported pending and stay valid, so wait again to keep waiting. To wait for a condition, start a shell with background=true running a loop such as `until CHECK; do sleep 5; done` and wait on its proc handle, rather than checking in repeated calls.",
                json!({"type":"object","properties":{"handles":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":64},
                "timeout_ms":{"type":"integer","minimum":0,"maximum":MAX_WAIT_TIMEOUT_MS},"any":{"type":"boolean"}},
                "required":["handles"],"additionalProperties":false}),
            ),
        };
        ToolSchema {
            name: self.name().into(),
            description: description.into(),
            parameters,
            grammar: (self == Tool::ApplyPatch).then_some(patch::GRAMMAR),
        }
    }
}

/// A tool a client declares on a bot: shown to the model beside the
/// daemon's own, never run. A call that fits its parameters is recorded
/// with `result` at once, or, when a client answers the tool, with the
/// answer that client gives; what it means is the client's, which reads
/// the call from events and history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declared {
    pub name: String,
    pub description: String,
    /// A JSON Schema object for the call's arguments.
    pub parameters: serde_json::Value,
    /// What the model reads as the call's result.
    #[serde(default = "declared_result")]
    pub result: String,
    /// The tag of the client that answers a call, as a gate names its
    /// approver: the call waits for its `tool_result` instead of taking
    /// `result`. The daemon never reads the tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<String>,
    /// How long an answered call waits for its answer, from its start,
    /// before it ends `tool_unserved`; `ANSWER_EXPIRE_MS` when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expire_ms: Option<u64>,
    /// Whether a bot this one creates takes the tool too; `false` keeps it
    /// the declaring bot's own.
    #[serde(default = "inherited", skip_serializing_if = "is_inherited")]
    pub inherit: bool,
}
fn declared_result() -> String {
    "ok".into()
}
impl Declared {
    /// Until when a call that started at `started_ms` waits for its
    /// answer, for a tool a client answers.
    pub fn answer_by(&self, started_ms: u64) -> Option<u64> {
        self.answered_by.as_ref()?;
        Some(started_ms + self.expire_ms.unwrap_or(ANSWER_EXPIRE_MS))
    }
}
fn inherited() -> bool {
    true
}
fn is_inherited(inherit: &bool) -> bool {
    *inherit
}
/// The most tools a bot may have declared, and the most bytes they take.
pub const MAX_DECLARED: usize = 16;
pub const DECLARED_BYTES: usize = 64 * 1024;
/// How long an answered call waits by default, and at most.
pub const ANSWER_EXPIRE_MS: u64 = 60_000;
pub const MAX_ANSWER_EXPIRE_MS: u64 = 3_600_000;
/// The most bytes an answer's output or detail may take.
pub const ANSWER_BYTES: usize = 64 * 1024;

/// A bot's declared tools must be few, small, named apart from each other
/// and from the daemon's own tools, and take an object of arguments.
pub fn validate_declared(declared: &[Declared]) -> Result<()> {
    if declared.len() > MAX_DECLARED {
        return fail_with(
            "invalid_declare",
            format!("at most {MAX_DECLARED} declared tools"),
        );
    }
    if serde_json::to_vec(declared)?.len() > DECLARED_BYTES {
        return fail_with(
            "invalid_declare",
            format!("declared tools may take up to {DECLARED_BYTES} bytes"),
        );
    }
    for (index, tool) in declared.iter().enumerate() {
        let name = tool.name.as_str();
        let named = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !named {
            return fail_with(
                "invalid_declare",
                format!("{name:?}: a name is 1 to 64 letters, digits, _ or -"),
            );
        }
        if Tool::parse(name).is_some() {
            return fail_with(
                "invalid_declare",
                format!("{name} is one of the daemon's own tools"),
            );
        }
        if declared[..index].iter().any(|other| other.name == name) {
            return fail_with("invalid_declare", format!("{name} is declared twice"));
        }
        if tool.parameters["type"] != "object" {
            return fail_with(
                "invalid_declare",
                format!("{name}: parameters must be an object schema"),
            );
        }
        match &tool.answered_by {
            Some(tag) => {
                let named = !tag.is_empty()
                    && tag.len() <= 64
                    && tag
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
                if !named {
                    return fail_with(
                        "invalid_declare",
                        format!("{name}: answered_by is 1 to 64 letters, digits, _, - or ."),
                    );
                }
                if tool.result != declared_result() {
                    return fail_with(
                        "invalid_declare",
                        format!("{name}: an answered tool takes its client's answer, not a result"),
                    );
                }
                if tool
                    .expire_ms
                    .is_some_and(|ms| ms == 0 || ms > MAX_ANSWER_EXPIRE_MS)
                {
                    return fail_with(
                        "invalid_declare",
                        format!("{name}: expire_ms must be 1 to {MAX_ANSWER_EXPIRE_MS}"),
                    );
                }
            }
            None if tool.expire_ms.is_some() => {
                return fail_with(
                    "invalid_declare",
                    format!("{name}: expire_ms goes with answered_by"),
                );
            }
            None => {}
        }
    }
    Ok(())
}

/// A declared tool's call checked against its parameters, as far as the
/// daemon looks: an object, with every required field, no field the
/// schema closes out, and each listed field of its listed basic type.
pub fn check_declared(tool: &Declared, arguments: &str) -> Result<()> {
    let invalid = |detail: String| Error::with("invalid_arguments", detail);
    let arguments: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|_| invalid(format!("{}: arguments are not JSON", tool.name)))?;
    let fields = arguments
        .as_object()
        .ok_or_else(|| invalid(format!("{}: arguments are not an object", tool.name)))?;
    let properties = tool.parameters["properties"].as_object();
    for required in tool.parameters["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str())
    {
        if !fields.contains_key(required) {
            return Err(invalid(format!("{}: {required} is required", tool.name)));
        }
    }
    for (field, value) in fields {
        let Some(schema) = properties.and_then(|p| p.get(field)) else {
            if tool.parameters["additionalProperties"] == false {
                return Err(invalid(format!("{}: no field {field}", tool.name)));
            }
            continue;
        };
        let fits = match schema["type"].as_str() {
            Some("string") => value.is_string(),
            Some("integer") => value.is_i64() || value.is_u64(),
            Some("number") => value.is_number(),
            Some("boolean") => value.is_boolean(),
            Some("array") => value.is_array(),
            Some("object") => value.is_object(),
            _ => true,
        };
        if !fits {
            return Err(invalid(format!(
                "{}: {field} must be {}",
                tool.name, schema["type"]
            )));
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct Registry {
    tools: Vec<Tool>,
    slots: Arc<Semaphore>,
    /// The running bound, also the bound on background commands accepted but
    /// not yet started; zero means neither is bounded.
    budget: usize,
    /// Background commands accepted and waiting for a slot. The operating
    /// system never sees these, so only this count can bound them.
    pending: Arc<std::sync::atomic::AtomicUsize>,
    /// A permit stays with each detached child until its wait completes.
    detached_slots: Arc<Semaphore>,
    detached_budget: usize,
    credentials: Credentials,
    environment: Arc<Vec<(String, String)>>,
    /// What the daemon keeps that a sandbox leaves alone or lets through.
    daemon: Arc<Daemon>,
    /// Request encodings per (family, selection); see `encoded`.
    encodings: Encodings,
}
type Encodings = Arc<std::sync::Mutex<HashMap<(Family, String), Arc<RawValue>>>>;
/// What a file that is not text holds, named by its first bytes, as `file`
/// would: a NUL in its first 8 KiB marks one as binary, as git decides.
fn binary(bytes: &[u8]) -> Option<&'static str> {
    const KINDS: [(&[u8], &str); 6] = [
        (b"\x89PNG", "a PNG image"),
        (b"\xff\xd8\xff", "a JPEG image"),
        (b"GIF8", "a GIF image"),
        (b"%PDF", "a PDF"),
        (b"PK\x03\x04", "a zip archive"),
        (b"\x7fELF", "an executable"),
    ];
    if let Some((_, kind)) = KINDS.iter().find(|(magic, _)| bytes.starts_with(magic)) {
        return Some(kind);
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some("a WebP image");
    }
    bytes[..bytes.len().min(8192)]
        .contains(&0)
        .then_some("a binary file")
}

/// A background command's admission: a slot taken at once when one was
/// free, otherwise a counted place in line released when it gets a slot
/// or is dropped before starting.
pub struct Queued {
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    pending: Option<Arc<std::sync::atomic::AtomicUsize>>,
}
impl Drop for Queued {
    fn drop(&mut self) {
        if let Some(pending) = &self.pending {
            pending.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
struct Credential {
    name: String,
    value: String,
}

/// Values that never appear in tool output and never reach a shell's
/// environment. Shared, so a login re-read while the daemon runs is
/// redacted from then on; earlier values stay listed.
#[derive(Clone, Default)]
pub struct Credentials(Arc<std::sync::RwLock<CredentialSet>>);

#[derive(Default)]
struct CredentialSet {
    /// Longer values first, so a shorter old token cannot mask only a prefix
    /// of a new token. Refreshes pay for insertion; tool output stays sorted.
    values: Vec<Credential>,
    names: Vec<String>,
}

impl Credentials {
    pub fn set(&self, name: &str, value: &str) {
        if value.is_empty() {
            return;
        }
        let mut set = self.0.write().unwrap_or_else(|e| e.into_inner());
        if set
            .values
            .iter()
            .any(|c| c.name == name && c.value == value)
        {
            return;
        }
        if !set.names.iter().any(|existing| existing == name) {
            set.names.push(name.to_owned());
        }
        let index = set.values.partition_point(|c| c.value.len() > value.len());
        set.values.insert(
            index,
            Credential {
                name: name.to_owned(),
                value: value.to_owned(),
            },
        );
    }
    /// Keep a variable out of shell environments without a value to
    /// redact: configuration the daemon read for itself.
    pub fn withhold(&self, name: &str) {
        let mut set = self.0.write().unwrap_or_else(|e| e.into_inner());
        if !set.names.iter().any(|existing| existing == name) {
            set.names.push(name.to_owned());
        }
    }
    pub fn names(&self) -> Vec<String> {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .names
            .clone()
    }
    pub fn redact(&self, mut text: String) -> String {
        {
            let set = self.0.read().unwrap_or_else(|e| e.into_inner());
            for credential in &set.values {
                if text.contains(&credential.value) {
                    text = text.replace(&credential.value, REDACTED);
                }
            }
        }
        redact_secret_fields(redact_private_keys(text))
    }
}

const REDACTED: &str = "[REDACTED]";
/// Field names, lowercased without `_` or `-`, whose values are secrets
/// wherever they are printed: `NAME=VALUE`, `NAME: VALUE`, `"NAME": "VALUE"`.
/// A name matches when it ends with one of these, so `aws_secret_access_key`
/// and `SecretAccessKey` match and `AccessKeyId` or `NextToken` do not.
const SECRET_FIELDS: [&str; 12] = [
    "secret",
    "secretkey",
    "secretaccesskey",
    "sessiontoken",
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "authtoken",
    "apikey",
    "password",
    "passwd",
    "privatekey",
];
/// Shorter values are flags, placeholders and counts, not secrets; so are
/// values without both letters and digits.
const SECRET_MIN: usize = 8;

/// Replace the value after each secret field name in `text`.
fn redact_secret_fields(text: String) -> String {
    let bytes = text.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !word(bytes[i]) || (i > 0 && word(bytes[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && word(bytes[i]) {
            i += 1;
        }
        // The shortest field is six letters; most words are shorter.
        if i - start < 6 {
            continue;
        }
        let name: String = text[start..i]
            .chars()
            .filter(|c| *c != '_' && *c != '-')
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if !SECRET_FIELDS.iter().any(|field| name.ends_with(field)) {
            continue;
        }
        // The name's closing quote, the separator, and the value's quote.
        let mut j = i;
        if j < bytes.len() && matches!(bytes[j], b'"' | b'\'') {
            j += 1;
        }
        while j < bytes.len() && matches!(bytes[j], b' ' | b'\t') {
            j += 1;
        }
        if j >= bytes.len() || !matches!(bytes[j], b'=' | b':') {
            continue;
        }
        j += 1;
        while j < bytes.len() && matches!(bytes[j], b' ' | b'\t') {
            j += 1;
        }
        let quote = bytes.get(j).copied().filter(|b| matches!(b, b'"' | b'\''));
        if quote.is_some() {
            j += 1;
        }
        let from = j;
        while j < bytes.len() {
            let b = bytes[j];
            let ends = match quote {
                Some(q) => b == q || b == b'\n',
                None => {
                    b.is_ascii_whitespace() || matches!(b, b',' | b';' | b'"' | b'\'' | b'}' | b']')
                }
            };
            if ends {
                break;
            }
            // An escaped quote is part of a quoted value.
            j = match (quote, b) {
                (Some(_), b'\\') => (j + 2).min(bytes.len()),
                _ => j + 1,
            };
        }
        let value = &text[from..j];
        // Code names a type or a variable there; a key mixes in digits.
        let keylike = value.bytes().any(|b| b.is_ascii_digit())
            && value.bytes().any(|b| b.is_ascii_alphabetic());
        if value.len() >= SECRET_MIN && keylike && !value.starts_with(['{', '[', '$']) {
            spans.push(from..j);
        }
        i = j;
    }
    if spans.is_empty() {
        return text;
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for span in spans {
        out.push_str(&text[at..span.start]);
        out.push_str(REDACTED);
        at = span.end;
    }
    out.push_str(&text[at..]);
    out
}

/// Replace the body of each PEM private key block in `text`.
fn redact_private_keys(mut text: String) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const END: &str = "-----END ";
    let mut from = 0;
    while let Some(found) = text[from..].find(BEGIN) {
        let begin = from + found;
        // The header line ends in five dashes, then LF or CRLF.
        let Some(dashes) = text[begin + BEGIN.len()..]
            .find("-----")
            .map(|n| begin + BEGIN.len() + n + 5)
        else {
            break;
        };
        let header = match &text.as_bytes()[dashes..] {
            [b'\r', b'\n', ..] => dashes + 2,
            [b'\n', ..] => dashes + 1,
            _ => dashes,
        };
        if !text[begin..header].contains("PRIVATE KEY") {
            from = header;
            continue;
        }
        let Some(end) = text[header..].find(END).map(|n| header + n) else {
            // A key cut off by truncation is still a key.
            text.replace_range(header.., REDACTED);
            break;
        };
        text.replace_range(header..end, &format!("{REDACTED}\n"));
        from = header + REDACTED.len() + 1 + END.len();
    }
    text
}

pub enum Prepared {
    Echo(String),
    Shell {
        command: String,
        timeout_ms: u64,
        background: bool,
    },
    /// A command in its own session, neither tracked nor killed.
    Detach(String),
    /// Resolved by the runtime, never by the registry.
    History {
        /// Another agent's, or the caller's own when `None`.
        bot: Option<String>,
        /// `None` lists the agent's newest turns.
        turn: Option<i64>,
        offset: u64,
        limit: usize,
    },
    /// Resolved by the runtime, never by the registry.
    Wait {
        handles: Vec<String>,
        timeout_ms: Option<u64>,
        any: bool,
    },
    /// A `web.run` call's commands, which the bot's provider answers.
    /// Resolved by the runtime, never by the registry.
    WebSearch(String),
    /// A client's declared tool, whose call that fits gets this result.
    /// Resolved by the runtime, never by the registry.
    Declared(String),
    /// Recorded by the store with the call's result, so the note's version
    /// is the node of that result and forks bind to it by position.
    Note {
        text: String,
    },
    Read {
        source: ReadSource,
        offset: usize,
        limit: usize,
    },
    Write {
        path: String,
        content: String,
    },
    ApplyPatch(Patch),
    Edit {
        path: String,
        old: String,
        new: String,
        replace_all: bool,
    },
}

/// What a `read` call reads: a workspace file, or a retained tool output
/// that only the runtime can fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadSource {
    Path(String),
    Artifact {
        turn: i64,
        call_id: String,
        stream: String,
    },
    /// A recorded tool result, whole: what an elided result's stub names.
    Result {
        node: i64,
    },
}
impl ReadSource {
    /// `TURN/CALL_ID/STREAM`, the reference a truncated result lists, or
    /// `result/NODE`, the one an elided result gives.
    pub fn parse_artifact(text: &str) -> Result<ReadSource> {
        if let Some(node) = text.strip_prefix("result/")
            && let Ok(node) = node.parse::<i64>()
            && node > 0
        {
            return Ok(ReadSource::Result { node });
        }
        let mut parts = text.splitn(3, '/');
        if let (Some(turn), Some(call_id), Some(stream)) =
            (parts.next(), parts.next(), parts.next())
            && let Ok(turn) = turn.parse::<i64>()
            && turn > 0
            && !call_id.is_empty()
            && matches!(stream, "stdout" | "stderr")
        {
            return Ok(ReadSource::Artifact {
                turn,
                call_id: call_id.into(),
                stream: stream.into(),
            });
        }
        crate::fail_with("invalid_artifact_reference", text)
    }
}

/// Model-facing result text plus any full outputs that were truncated.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub output: String,
    pub artifacts: Vec<(&'static str, Vec<u8>)>,
    /// Pictures that go to the model after the text: an image file read.
    pub images: Vec<crate::codec::Image>,
    /// A carry-forward note to record with this result; empty text clears.
    pub note: Option<String>,
    /// The call failed: an error, or a command that did not succeed. Set by
    /// whoever ran it, never read back from the output's text.
    pub failed: bool,
}
impl Outcome {
    pub fn text(output: String) -> Self {
        Self {
            output,
            artifacts: Vec::new(),
            images: Vec::new(),
            note: None,
            failed: false,
        }
    }
}

/// The shell's fields for leaving the sandbox, as Codex describes them,
/// shown only to a bot that has one.
fn escalation(parameters: &mut serde_json::Value) {
    let properties = &mut parameters["properties"];
    properties["sandbox_permissions"] = json!({"type":"string",
        "enum":["use_default","require_escalated"],
        "description":"Sandbox permissions for the command. Set to \"require_escalated\" to request running without sandbox restrictions; defaults to \"use_default\"."});
    properties["justification"] = json!({"type":"string",
        "description":"Only set if sandbox_permissions is \"require_escalated\". Request approval from the user to run this command outside the sandbox. Phrased as a simple question that summarizes the purpose of the command as it relates to the task at hand - e.g. 'Do you want to fetch and pull the latest version of this git branch?'"});
}

impl Registry {
    /// Every tool this build knows: what a daemon registers at start. Which
    /// of them a bot may call is the bot's own, chosen at creation.
    pub fn all() -> Result<Self> {
        Self::new(if cfg!(unix) {
            "echo,shell,read,write,edit,apply_patch,wait,history,note,web_search"
        } else {
            "echo,read,write,edit,apply_patch,wait,history,note,web_search"
        })
    }
    pub fn new(names: &str) -> Result<Self> {
        let mut tools = Vec::new();
        for name in names.split(',') {
            let tool = Tool::parse(name).ok_or(Error::new("unsupported_tool_set"))?;
            if tools.contains(&tool) {
                return fail("unsupported_tool_set");
            }
            if tool == Tool::Shell && !cfg!(unix) {
                return fail("shell_platform_unsupported");
            }
            tools.push(tool);
        }
        Ok(Self {
            tools,
            slots: Arc::new(Semaphore::new(DEFAULT_PROCESS_BUDGET)),
            budget: DEFAULT_PROCESS_BUDGET,
            pending: Arc::default(),
            detached_slots: Arc::new(Semaphore::new(DEFAULT_DETACHED_BUDGET)),
            detached_budget: DEFAULT_DETACHED_BUDGET,
            credentials: Credentials::default(),
            environment: Arc::new(Vec::new()),
            daemon: Arc::default(),
            encodings: Arc::default(),
        })
    }
    /// A bot's selection must name registered tools, each once.
    pub fn validate(&self, names: &[String]) -> Result<()> {
        for (index, name) in names.iter().enumerate() {
            let tool =
                Tool::parse(name).ok_or_else(|| Error::with("unsupported_tool_set", name))?;
            if !self.tools.contains(&tool) || names[..index].contains(name) {
                return fail_with("unsupported_tool_set", name);
            }
        }
        Ok(())
    }
    /// The request encoding of a bot's tools for one family, its declared
    /// tools after the daemon's, shared by every bot with the same
    /// selection: definitions live once in the registry and encodings once
    /// per distinct selection, never per bot or turn.
    pub fn encoded(
        &self,
        family: Family,
        names: &[String],
        declared: &[Declared],
        sandboxed: bool,
    ) -> Result<Arc<RawValue>> {
        let mut key = (family, names.join(","));
        if sandboxed {
            key.1.push_str("\0sandboxed");
        }
        if !declared.is_empty() {
            key.1.push('\0');
            key.1.push_str(&serde_json::to_string(declared)?);
        }
        if let Some(found) = self.encodings.lock().unwrap().get(&key) {
            return Ok(found.clone());
        }
        let schemas: Vec<ToolSchema> = names
            .iter()
            .filter_map(|name| Tool::parse(name))
            // Search is declared by the provider, as it offers it.
            .filter(|tool| self.tools.contains(tool) && *tool != Tool::WebSearch)
            .map(Tool::schema)
            .map(|mut schema| {
                if sandboxed && schema.name == "shell" {
                    escalation(&mut schema.parameters);
                }
                schema
            })
            .chain(declared.iter().map(|tool| ToolSchema {
                name: tool.name.clone().into(),
                description: tool.description.clone().into(),
                parameters: tool.parameters.clone(),
                grammar: None,
            }))
            .collect();
        let encoded: Arc<RawValue> = Arc::from(RawValue::from_string(serde_json::to_string(
            &family.tools(&schemas),
        )?)?);
        let mut encodings = self.encodings.lock().unwrap();
        // Fleets share a few selections; a flood of distinct ones is served
        // without being remembered.
        if encodings.len() < 256 {
            encodings.insert(key, encoded.clone());
        }
        Ok(encoded)
    }
    /// Bound on detached commands still running, daemon-wide; exited ones
    /// are reaped and stop counting. Zero removes the bound.
    pub fn with_detached_budget(mut self, budget: usize) -> Self {
        self.detached_budget = budget.min(Semaphore::MAX_PERMITS);
        self.detached_slots = Arc::new(Semaphore::new(self.detached_budget));
        self
    }
    /// Bound on simultaneously running child processes. Waiting never counts.
    /// Zero removes the bound; the operating system is then the only limit.
    pub fn with_process_budget(mut self, budget: usize) -> Self {
        self.budget = budget.min(Semaphore::MAX_PERMITS);
        self.slots = Arc::new(Semaphore::new(if budget == 0 {
            Semaphore::MAX_PERMITS
        } else {
            self.budget
        }));
        self
    }
    /// Take a place in line for a background command, or refuse when as many
    /// are already waiting as may run: a counter and a comparison, so a fleet
    /// cannot accumulate an invisible backlog behind the process bound.
    pub fn queue(&self) -> Result<Queued> {
        use std::sync::atomic::Ordering::Relaxed;
        // A free slot is taken here, not in the task, so the line counts
        // only commands that truly wait; the semaphore is fair, so this
        // never overtakes a waiter.
        if let Ok(permit) = self.slots.clone().try_acquire_owned() {
            return Ok(Queued {
                permit: Some(permit),
                pending: None,
            });
        }
        if self.budget != 0 && self.pending.load(Relaxed) >= self.budget {
            return fail("capacity_exhausted");
        }
        self.pending.fetch_add(1, Relaxed);
        Ok(Queued {
            permit: None,
            pending: Some(self.pending.clone()),
        })
    }
    /// Background commands accepted but not yet running, for `stats`.
    pub fn pending(&self) -> usize {
        self.pending.load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Variables added to every shell child, such as the store and binary
    /// paths a bot needs to delegate through the same daemon.
    pub fn with_environment(mut self, environment: Vec<(String, String)>) -> Self {
        self.environment = Arc::new(environment);
        self
    }
    /// Share a credential set that others may add to later, such as a login
    /// that is re-read while the daemon runs.
    /// Exact occurrences of these values are redacted from tool results and the
    /// named variables are removed from shell environments.
    pub fn with_credentials(mut self, credentials: Credentials) -> Self {
        self.credentials = credentials;
        self
    }
    /// The daemon's store and socket, which its bots' sandboxes keep
    /// read-only and reachable.
    pub fn with_daemon(mut self, daemon: Daemon) -> Self {
        self.daemon = Arc::new(daemon);
        self
    }
    pub fn daemon(&self) -> &Daemon {
        &self.daemon
    }
    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.name()).collect()
    }
    /// How many tools this registry holds: the most any selection can name.
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.iter().map(|t| t.schema()).collect()
    }
    fn redact(&self, text: String) -> String {
        self.credentials.redact(text)
    }

    // Full access to this configured registry, within caller host permissions.
    // Validate arguments before recording intent so invalid calls never dispatch.
    pub fn prepare(&self, name: &str, args: &str) -> Result<Prepared> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Echo {
            text: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Shell {
            command: String,
            #[serde(default = "default_timeout")]
            timeout_ms: u64,
            #[serde(default)]
            background: bool,
            #[serde(default)]
            detach: bool,
            /// Whether a sandboxed bot's command asks to leave its sandbox;
            /// decided before the call runs, through its gate.
            sandbox_permissions: Option<String>,
            justification: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct HistoryArgs {
            bot: Option<String>,
            turn: Option<i64>,
            #[serde(default)]
            offset: u64,
            limit: Option<usize>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wait {
            handles: Vec<String>,
            timeout_ms: Option<u64>,
            #[serde(default)]
            any: bool,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Read {
            path: Option<String>,
            artifact: Option<String>,
            #[serde(default = "one")]
            offset: usize,
            #[serde(default = "default_limit")]
            limit: usize,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Write {
            path: String,
            content: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ApplyPatch {
            input: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Edit {
            path: String,
            old: String,
            new: String,
            #[serde(default)]
            replace_all: bool,
        }
        fn default_timeout() -> u64 {
            DEFAULT_SHELL_TIMEOUT_MS
        }
        fn one() -> usize {
            1
        }
        fn default_limit() -> usize {
            500
        }
        fn path(value: &str) -> Result<()> {
            if value.is_empty() || value.len() > 4096 || value.contains('\0') {
                return fail("invalid_path");
            }
            Ok(())
        }
        let tool = Tool::parse(name)
            .filter(|tool| self.tools.contains(tool))
            .ok_or(Error::new("unsupported_tool"))?;
        let invalid = |_| Error::new("invalid_tool_arguments");
        Ok(match tool {
            Tool::Echo => {
                let args: Echo = serde_json::from_str(args).map_err(invalid)?;
                if args.text.len() > PREVIEW_BYTES {
                    return fail("tool_output_limit");
                }
                Prepared::Echo(args.text)
            }
            Tool::Shell => {
                let args: Shell = serde_json::from_str(args).map_err(invalid)?;
                let invalid = if args.command.is_empty() {
                    Some("command is empty".to_owned())
                } else if args.command.len() > 16 * 1024 {
                    Some("command is over 16 KiB".to_owned())
                } else if args.command.contains('\0') {
                    Some("command contains a NUL byte".to_owned())
                } else if !(1..=MAX_SHELL_TIMEOUT_MS).contains(&args.timeout_ms) {
                    Some(format!(
                        "timeout_ms is {}; it runs from 1 to {MAX_SHELL_TIMEOUT_MS}",
                        args.timeout_ms
                    ))
                } else if args.detach && args.background {
                    Some("detach and background cannot both be set".to_owned())
                } else if args
                    .sandbox_permissions
                    .as_deref()
                    .is_some_and(|value| !matches!(value, "use_default" | "require_escalated"))
                {
                    Some("sandbox_permissions is use_default or require_escalated".to_owned())
                } else if args.justification.is_some()
                    && args.sandbox_permissions.as_deref() != Some("require_escalated")
                {
                    Some(
                        "justification is only for sandbox_permissions require_escalated"
                            .to_owned(),
                    )
                } else {
                    None
                };
                if let Some(detail) = invalid {
                    return fail_with("invalid_shell_arguments", detail);
                }
                if args.detach {
                    return Ok(Prepared::Detach(args.command));
                }
                Prepared::Shell {
                    command: args.command,
                    timeout_ms: args.timeout_ms,
                    background: args.background,
                }
            }
            Tool::History => {
                let args: HistoryArgs = serde_json::from_str(args).map_err(invalid)?;
                let limit = args.limit.unwrap_or(64 * 1024);
                // Only another agent's turns are listed: a bot's own are in
                // its context, and the context note numbers those left out.
                if args.turn.is_some_and(|turn| turn < 1)
                    || (args.turn.is_none() && args.bot.is_none())
                    || args
                        .bot
                        .as_ref()
                        .is_some_and(|bot| bot.is_empty() || bot.len() > 128)
                    || !(4..=64 * 1024).contains(&limit)
                {
                    return fail("invalid_tool_arguments");
                }
                Prepared::History {
                    bot: args.bot,
                    turn: args.turn,
                    offset: args.offset,
                    limit,
                }
            }
            Tool::WebSearch => {
                let commands: serde_json::Value = serde_json::from_str(args).map_err(invalid)?;
                if !commands.is_object() {
                    return fail("invalid_tool_arguments");
                }
                Prepared::WebSearch(args.to_owned())
            }
            Tool::Note => {
                #[derive(serde::Deserialize)]
                struct Note {
                    text: String,
                }
                let args: Note = serde_json::from_str(args).map_err(invalid)?;
                if args.text.len() > NOTE_BYTES {
                    return fail("invalid_tool_arguments");
                }
                Prepared::Note { text: args.text }
            }
            Tool::Wait => {
                let args: Wait = serde_json::from_str(args).map_err(invalid)?;
                if args.handles.is_empty()
                    || args.handles.len() > 64
                    || args.handles.iter().any(|h| h.len() > 256)
                    || args.timeout_ms.is_some_and(|t| t > MAX_WAIT_TIMEOUT_MS)
                {
                    return fail("invalid_tool_arguments");
                }
                Prepared::Wait {
                    handles: args.handles,
                    timeout_ms: args.timeout_ms,
                    any: args.any,
                }
            }
            Tool::Read => {
                let args: Read = serde_json::from_str(args).map_err(invalid)?;
                if args.offset == 0 || !(1..=5000).contains(&args.limit) {
                    return fail("invalid_tool_arguments");
                }
                let source = match (args.path, args.artifact) {
                    (Some(file), None) => {
                        path(&file)?;
                        ReadSource::Path(file)
                    }
                    (None, Some(reference)) => ReadSource::parse_artifact(&reference)?,
                    _ => return fail("invalid_tool_arguments"),
                };
                Prepared::Read {
                    source,
                    offset: args.offset,
                    limit: args.limit,
                }
            }
            Tool::Write => {
                let args: Write = serde_json::from_str(args).map_err(invalid)?;
                path(&args.path)?;
                if args.content.len() > WRITE_BYTES {
                    return fail("tool_input_limit");
                }
                Prepared::Write {
                    path: args.path,
                    content: args.content,
                }
            }
            Tool::ApplyPatch => {
                let args: ApplyPatch = serde_json::from_str(args).map_err(invalid)?;
                if args.input.len() > WRITE_BYTES {
                    return fail("tool_input_limit");
                }
                let patch = Patch::parse(&args.input)?;
                for target in patch.paths() {
                    path(target)?;
                }
                Prepared::ApplyPatch(patch)
            }
            Tool::Edit => {
                let args: Edit = serde_json::from_str(args).map_err(invalid)?;
                path(&args.path)?;
                if args.old.is_empty() || args.new.len() > WRITE_BYTES {
                    return fail("invalid_tool_arguments");
                }
                Prepared::Edit {
                    path: args.path,
                    old: args.old,
                    new: args.new,
                    replace_all: args.replace_all,
                }
            }
        })
    }

    /// `environment` is what the running turn adds for its children, such
    /// as its own model for peers it delegates to. `sandbox` confines a
    /// command, a write or an edit; `None` runs it unconfined.
    pub async fn execute(
        &self,
        tool: Prepared,
        workspace: &Path,
        environment: &[(String, String)],
        sandbox: Option<Arc<Policy>>,
    ) -> Result<Outcome> {
        match tool {
            Prepared::Echo(text) => Ok(Outcome::text(self.redact(text))),
            Prepared::Shell {
                background: true, ..
            } => fail("background_requires_runtime"),
            Prepared::Detach(command) => {
                detach(&command, workspace, self, environment, sandbox).await
            }
            Prepared::Wait { .. } => fail("wait_requires_runtime"),
            Prepared::History { .. } => fail("history_requires_runtime"),
            Prepared::Note { .. } => fail("note_requires_runtime"),
            Prepared::Declared(_) => fail("declared_requires_runtime"),
            Prepared::WebSearch(_) => fail("web_search_requires_runtime"),
            Prepared::Shell {
                command,
                timeout_ms,
                background: false,
            } => {
                self.foreground(
                    command,
                    workspace.to_path_buf(),
                    timeout_ms,
                    environment.to_vec(),
                    sandbox,
                    tokio::sync::oneshot::channel().0,
                )
                .await
            }
            Prepared::Read {
                source: ReadSource::Artifact { .. } | ReadSource::Result { .. },
                ..
            } => fail("artifact_requires_runtime"),
            Prepared::Read {
                source: ReadSource::Path(path),
                offset,
                limit,
            } => {
                let file = resolve(workspace, &path);
                // A picture goes to the model as one, after a line on it.
                // Its header says so; the file is read only in its turn.
                if picture::kind(&files::read_head(&file, picture::HEAD).await?).is_some()
                    && let Some((image, line)) = picture::read_apart(read_bounded(&file)).await?
                {
                    return Ok(Outcome {
                        images: vec![image],
                        ..Outcome::text(format!("{path}: {line}"))
                    });
                }
                let bytes = read_bounded(&file).await?;
                if let Some(kind) = binary(&bytes) {
                    return fail_with(
                        "file_not_text",
                        format!("{kind} of {} bytes; read returns text", bytes.len()),
                    );
                }
                let text = String::from_utf8_lossy(&bytes);
                Ok(Outcome::text(
                    self.redact(page_lines(&text, offset, limit)?),
                ))
            }
            Prepared::Write { path, content } => {
                let target = resolve(workspace, &path);
                writable(sandbox.clone(), &target, &path).await?;
                let _held = crate::sandbox::hold_for(sandbox.as_deref()).await;
                let _slot = self.helper_slot(&sandbox).await?;
                files::write(&target, content.as_bytes(), sandbox).await?;
                Ok(Outcome::text(format!(
                    "wrote {} bytes to {}",
                    content.len(),
                    path
                )))
            }
            Prepared::ApplyPatch(patch) => {
                for path in patch.paths() {
                    writable(sandbox.clone(), &resolve(workspace, path), path).await?;
                }
                let staged = patch.stage(workspace).await?;
                // A process slot only while the sandbox's helper writes.
                let _held = crate::sandbox::hold_for(sandbox.as_deref()).await;
                let _slot = match staged.writes() {
                    true => self.helper_slot(&sandbox).await?,
                    false => None,
                };
                Ok(Outcome::text(staged.commit(sandbox).await?))
            }
            Prepared::Edit {
                path,
                old,
                new,
                replace_all,
            } => {
                let target = resolve(workspace, &path);
                writable(sandbox.clone(), &target, &path).await?;
                let bytes = read_bounded(&target).await?;
                let text = String::from_utf8(bytes).map_err(|_| Error::new("file_not_utf8"))?;
                let count = text.matches(&old).count();
                if count == 0 {
                    return fail("edit_target_not_found");
                }
                if count > 1 && !replace_all {
                    return Err(Error::with(
                        "edit_target_ambiguous",
                        format!("{count} occurrences; add context or set replace_all"),
                    ));
                }
                let replacements = if replace_all { count } else { 1 };
                let expanded = new
                    .len()
                    .checked_mul(replacements)
                    .and_then(|added| (text.len() - old.len() * replacements).checked_add(added))
                    .filter(|size| *size <= FILE_BYTES)
                    .ok_or(Error::new("tool_output_limit"))?;
                let updated = if replace_all {
                    text.replace(&old, &new)
                } else {
                    text.replacen(&old, &new, 1)
                };
                debug_assert_eq!(updated.len(), expanded);
                let _held = crate::sandbox::hold_for(sandbox.as_deref()).await;
                let _slot = self.helper_slot(&sandbox).await?;
                files::write(&target, updated.as_bytes(), sandbox).await?;
                Ok(Outcome::text(format!(
                    "replaced {count} occurrence{} in {path}",
                    if count == 1 { "" } else { "s" }
                )))
            }
        }
    }
}

impl Registry {
    fn shell_outcome(
        &self,
        stdout: Captured,
        stderr: Captured,
        exit: Exit,
        elapsed: Duration,
    ) -> Outcome {
        let mut artifacts = Vec::new();
        let mut preview = |name: &'static str, captured: Captured| {
            let (bytes, dropped) = captured.finish();
            // Reuse the pipe buffer for ordinary UTF-8 output. Invalid
            // bytes still get the same lossy decoding as before.
            let text = String::from_utf8(bytes)
                .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned());
            let mut text = self.redact(text);
            if text.len() <= PREVIEW_BYTES {
                return text;
            }
            let shown = truncate(&text, dropped);
            // read_to_end may have reserved beyond the output length.
            // Do not retain that spare capacity while storage catches up.
            text.shrink_to_fit();
            artifacts.push((name, text.into_bytes()));
            shown
        };
        let (stdout, stderr) = (preview("stdout", stdout), preview("stderr", stderr));
        let elapsed_ms = elapsed.as_millis() as u64;
        let (output, success) = match exit {
            Exit::Status(status) => (
                json!({"stdout":stdout,"stderr":stderr,
                    "exit_code":status.code(),"success":status.success(),"wall_time_ms":elapsed_ms}),
                status.success(),
            ),
            // The output up to the kill is what a long command has to show.
            Exit::TimedOut => (
                json!({"stdout":stdout,"stderr":stderr,
                    "exit_code":null,"success":false,"timed_out":true,"wall_time_ms":elapsed_ms}),
                false,
            ),
        };
        Outcome {
            output: output.to_string(),
            artifacts,
            images: Vec::new(),
            note: None,
            failed: !success,
        }
    }

    /// A foreground command, owning everything it uses so a running one
    /// can be moved to a task of its own. Dropping it kills the command.
    /// `started` fires once it holds a process slot: only then is it
    /// running, and only then may it move.
    /// A process slot for a confined write, which runs as a process of
    /// its own and so waits its turn as a command does.
    async fn helper_slot(
        &self,
        sandbox: &Option<Arc<Policy>>,
    ) -> Result<Option<tokio::sync::SemaphorePermit<'_>>> {
        match sandbox {
            Some(_) => Ok(Some(
                self.slots
                    .acquire()
                    .await
                    .map_err(|_| Error::new("tool_scheduler_closed"))?,
            )),
            None => Ok(None),
        }
    }

    pub fn foreground(
        &self,
        command: String,
        workspace: PathBuf,
        timeout_ms: u64,
        environment: Vec<(String, String)>,
        sandbox: Option<Arc<Policy>>,
        started: tokio::sync::oneshot::Sender<()>,
    ) -> impl Future<Output = Result<Outcome>> + Send + 'static {
        let registry = self.clone();
        async move {
            // Held until the command and the sandbox around it are gone,
            // and taken before the slot, so no slot waits on it.
            let _held = crate::sandbox::hold_for(sandbox.as_deref()).await;
            let _slot = registry
                .slots
                .acquire()
                .await
                .map_err(|_| Error::new("tool_scheduler_closed"))?;
            let _ = started.send(());
            let (stdout, stderr, exit, elapsed) = shell(
                &command,
                &workspace,
                Duration::from_millis(timeout_ms),
                &registry,
                &environment,
                sandbox.as_deref(),
            )
            .await?;
            Ok(registry.shell_outcome(stdout, stderr, exit, elapsed))
        }
    }

    /// Start a command whose result is delivered later. The process budget is
    /// acquired inside the task, so starting never blocks the caller; the
    /// result carries the same bounded preview and artifacts as a foreground
    /// command, or the error a foreground command would have returned.
    /// `timeout_ms` bounds the command's running time; the place in line is
    /// bounded by `queue`, not by the clock.
    #[allow(clippy::too_many_arguments)]
    pub fn background(
        &self,
        command: String,
        workspace: PathBuf,
        timeout_ms: u64,
        environment: Vec<(String, String)>,
        sandbox: Option<Arc<Policy>>,
        queued: Queued,
        done: tokio::sync::oneshot::Sender<Result<Outcome>>,
    ) {
        let registry = self.clone();
        let mut queued = queued;
        tokio::spawn(async move {
            let result = async {
                let slot = match queued.permit.take() {
                    Some(permit) => Ok(permit),
                    None => registry.slots.clone().acquire_owned().await,
                };
                drop(queued);
                let _slot = slot.map_err(|_| Error::new("tool_scheduler_closed"))?;
                let _held = crate::sandbox::hold_for(sandbox.as_deref()).await;
                let (stdout, stderr, exit, elapsed) = shell(
                    &command,
                    &workspace,
                    Duration::from_millis(timeout_ms),
                    &registry,
                    &environment,
                    sandbox.as_deref(),
                )
                .await?;
                Ok(registry.shell_outcome(stdout, stderr, exit, elapsed))
            }
            .await;
            let _ = done.send(result);
        });
    }
}

/// Number lines from a 1-based offset within the page budget. A single line
/// beyond the budget is an explicit error rather than a silent cut.
pub fn page_lines(text: &str, offset: usize, limit: usize) -> Result<String> {
    page(text.lines(), offset, limit, PREVIEW_BYTES, false)
}

/// The widest piece `page_pieces` numbers as a line.
pub const PIECE_BYTES: usize = 4096;

/// `page_lines` with every line longer than `PIECE_BYTES` split at
/// character boundaries into numbered pieces, so a text with no byte-level
/// reader, such as one stored tool result on a single line, pages whole.
/// A page stays within `max_bytes` as well as the preview bound; a piece
/// wider than that shows in part, keeping its number, since numbers sized
/// to the room would name other text on the next read.
pub fn page_pieces(text: &str, offset: usize, limit: usize, max_bytes: usize) -> Result<String> {
    page(
        text.lines().flat_map(pieces),
        offset,
        limit,
        max_bytes,
        true,
    )
}

fn pieces(line: &str) -> impl Iterator<Item = &str> + Clone {
    let mut rest = Some(line);
    std::iter::from_fn(move || {
        let line = rest?;
        if line.len() <= PIECE_BYTES {
            rest = None;
            return Some(line);
        }
        let (piece, tail) = line.split_at(boundary(line, PIECE_BYTES));
        rest = Some(tail);
        Some(piece)
    })
}

fn page<'a>(
    lines: impl Iterator<Item = &'a str> + Clone,
    offset: usize,
    limit: usize,
    max_bytes: usize,
    partial: bool,
) -> Result<String> {
    let total = lines.clone().count();
    let budget = max_bytes.min(PREVIEW_BYTES).saturating_sub(128);
    let mut output = String::new();
    let mut shown = 0;
    for (index, line) in lines.enumerate().skip(offset - 1).take(limit) {
        // Reserve space for continuation/line-count notices. Check
        // lengths before copying a potentially multi-megabyte line.
        let prefix = format!("{:>6}\t", index + 1);
        let entry_bytes = prefix.len() + line.len() + 1;
        if output.len() + entry_bytes > budget {
            let part = boundary(line, budget.saturating_sub(prefix.len() + 1));
            if shown == 0 && partial && part > 0 {
                output.push_str(&prefix);
                output.push_str(&line[..part]);
                output.push('\n');
                output.push_str(&format!(
                    "[line {} of {total} shown in part ({part} of {} bytes); read offset={} again when the turn has more room]\n",
                    index + 1,
                    line.len(),
                    index + 1
                ));
                return Ok(output);
            }
            if shown == 0 {
                return crate::fail_with(
                    "read_line_too_long",
                    format!(
                        "line {} exceeds the read page budget; use a byte-oriented tool to inspect it",
                        index + 1
                    ),
                );
            }
            output.push_str(&format!(
                "[truncated at line {}; continue with offset={}]\n",
                index + 1,
                index + 1
            ));
            break;
        }
        output.push_str(&prefix);
        output.push_str(line);
        output.push('\n');
        shown += 1;
    }
    if offset > total && total > 0 {
        output = format!("[offset {offset} is past the last line {total}]\n");
    } else if shown < total - (offset - 1).min(total) {
        output.push_str(&format!(
            "[showing lines {offset}-{} of {total}]\n",
            offset + shown - 1
        ));
    }
    Ok(output)
}

/// Redaction for runtime-produced text, such as artifact pages.
impl Registry {
    pub fn redact_text(&self, text: String) -> String {
        self.redact(text)
    }
}

fn resolve(workspace: &Path, path: &str) -> PathBuf {
    workspace.join(path)
}

/// Refuse a confined write or edit of what its sandbox keeps, as a
/// command in it would be refused. Resolving the target reads the
/// filesystem, so it runs off the runtime thread.
async fn writable(sandbox: Option<Arc<Policy>>, target: &Path, path: &str) -> Result<()> {
    let Some(policy) = sandbox else {
        return Ok(());
    };
    let target = target.to_owned();
    let allowed = tokio::task::spawn_blocking(move || policy.can_write(&target))
        .await
        .unwrap_or(false);
    match allowed {
        false => fail_with(
            "write_denied",
            format!(
                "{path} is outside the folders this bot may write, or kept read-only in one; \
                 ask to escalate to write it"
            ),
        ),
        true => Ok(()),
    }
}

fn boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Head and tail of an oversized output, with the omission stated.
/// `dropped` counts the bytes the retained text itself left out.
fn truncate(text: &str, dropped: u64) -> String {
    let half = PREVIEW_BYTES / 2;
    let head = &text[..boundary(text, half)];
    let tail_start = boundary(text, text.len() - half);
    let omitted = text.len() - head.len() - (text.len() - tail_start);
    let retained = if dropped == 0 {
        "the full output is retained".to_owned()
    } else {
        format!(
            "{dropped} more past 1 MiB were not kept, and its first and last {KEPT_END} bytes are retained"
        )
    };
    format!(
        "{head}\n[... {omitted} bytes omitted; {retained}: see this result's artifacts and read them with the read tool ...]\n{}",
        &text[tail_start..]
    )
}

/// What a stream wrote, bounded: all of it up to `ARTIFACT_BYTES`, past
/// that its first and last `KEPT_END` bytes and a count of the rest.
#[derive(Default)]
struct Captured {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    dropped: u64,
}
impl Captured {
    /// Output past the head. Once the whole would pass the bound, the tail
    /// keeps only its last `KEPT_END` bytes.
    fn keep(&mut self, bytes: &[u8]) {
        self.tail.extend(bytes);
        if self.head.len() + self.tail.len() > ARTIFACT_BYTES {
            let cut = self.tail.len() - KEPT_END;
            self.tail.drain(..cut);
            self.dropped += cut as u64;
        }
    }
    /// The kept bytes, with a marker where the middle was dropped, and how
    /// many bytes that was.
    fn finish(self) -> (Vec<u8>, u64) {
        let Captured {
            mut head,
            mut tail,
            mut dropped,
        } = self;
        // Output after the last trim can leave a dropped tail long, short of
        // the room the marker needs.
        if dropped > 0 && tail.len() > KEPT_END {
            let cut = tail.len() - KEPT_END;
            tail.drain(..cut);
            dropped += cut as u64;
        }
        if dropped > 0 {
            head.extend_from_slice(
                format!(
                    "\n[... {dropped} bytes dropped: past 1 MiB only the first and last {KEPT_END} bytes of this stream are kept ...]\n"
                )
                .as_bytes(),
            );
        }
        head.extend(tail);
        (head, dropped)
    }
}

/// Append a pipe's output to `captured` until it closes. What arrived stays
/// there if the read is cancelled, as it is at a shell timeout.
async fn capture(pipe: &mut (impl AsyncRead + Unpin), captured: &mut Captured) -> Result<()> {
    let room = KEPT_END.saturating_sub(captured.head.len());
    if room > 0 {
        (&mut *pipe)
            .take(room as u64)
            .read_to_end(&mut captured.head)
            .await?;
        if captured.head.len() < KEPT_END {
            return Ok(());
        }
    }
    let mut chunk = vec![0; 64 * 1024];
    loop {
        let read = pipe.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        captured.keep(&chunk[..read]);
        // Past the bound nothing more is shown, so a runaway writer is
        // drained slowly: the daemon's one thread serves every bot, and the
        // writer waits on the full pipe instead of spending it.
        if captured.dropped > 0 {
            tokio::time::sleep(DRAIN_PAUSE).await;
        }
    }
}

/// The pause after each read once a stream has passed `ARTIFACT_BYTES`,
/// which bounds its drain to about 64 MiB/s.
const DRAIN_PAUSE: Duration = Duration::from_millis(1);

/// How a shell command ended: on its own, or killed at its timeout.
pub enum Exit {
    Status(std::process::ExitStatus),
    TimedOut,
}

/// The key the approver's Decisions judge reads: OpenAI's own variable, so
/// it is withheld from tools even when no daemon provider holds it.
pub const DECISIONS_KEY: &str = "OPENAI_API_KEY";

/// bash, as other harnesses run commands, where it is installed; else sh.
#[cfg(unix)]
fn shell_path() -> &'static str {
    static PATH: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    PATH.get_or_init(|| match Path::new("/bin/bash").exists() {
        true => "/bin/bash",
        false => "/bin/sh",
    })
}

/// `shell_path() -c command` in the workspace, with the registry's
/// environment and none of the credentials the daemon holds.
#[cfg(unix)]
fn sh(
    command: &str,
    workspace: &Path,
    registry: &Registry,
    environment: &[(String, String)],
    sandbox: Option<&Policy>,
    detached: bool,
) -> Result<tokio::process::Command> {
    let mut process = match sandbox {
        Some(policy) => crate::sandbox::command(policy, shell_path(), &["-c", command], detached)?,
        None => {
            let mut process = tokio::process::Command::new(shell_path());
            process.arg("-c").arg(command);
            process
        }
    };
    for name in registry.credentials.names() {
        process.env_remove(name);
    }
    // A set AGENT_PARENT names this bot's creator and nothing else; a root
    // bot must not pass on one the daemon itself was started with.
    process.env_remove("AGENT_PARENT");
    process.env_remove("AGENT_PARENT_ID");
    // Likewise a set AGENT_EFFORT is this bot's own level.
    process.env_remove("AGENT_EFFORT");
    // The automatic approver's key is for the approver alone, however the
    // daemon was started.
    process.env_remove(DECISIONS_KEY);
    // A terminal multiplexer's place names the terminal the daemon was
    // started in, not the bot's: a command that used it would land there.
    for name in [
        "REX_SESSION",
        "REX_BLOCK",
        "TMUX",
        "TMUX_PANE",
        "HERDR_ENV",
        "HERDR_WORKSPACE_ID",
        "HERDR_TAB_ID",
        "HERDR_PANE_ID",
    ] {
        process.env_remove(name);
    }
    // No command has a terminal: plain output and no pager, as other
    // harnesses run commands. What the caller or bot sets still wins.
    for (name, value) in [
        ("TERM", "dumb"),
        ("NO_COLOR", "1"),
        ("PAGER", "cat"),
        ("GIT_PAGER", "cat"),
        ("GH_PAGER", "cat"),
    ] {
        process.env(name, value);
    }
    for (name, value) in registry.environment.iter().chain(environment) {
        process.env(name, value);
    }
    // A sandboxed command reaches the daemon through its bot's own socket.
    if let Some(own) = sandbox.and_then(Policy::reach) {
        process.env("AGENT_SOCKET", own);
    }
    process.current_dir(workspace);
    Ok(process)
}

/// Start a command in a new session for a service that must outlive the
/// turn and the daemon. Its output is discarded: a log the daemon wrote would
/// bypass credential redaction, so a command that wants one redirects itself.
/// No process slot, group kill, or handle applies to it. An async waiter
/// reaps it on exit and releases its detached slot.
#[cfg(unix)]
async fn detach(
    command: &str,
    workspace: &Path,
    registry: &Registry,
    environment: &[(String, String)],
    sandbox: Option<Arc<Policy>>,
) -> Result<Outcome> {
    use std::process::Stdio;
    let mut process = sh(
        command,
        workspace,
        registry,
        environment,
        sandbox.as_deref(),
        true,
    )?;
    process
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches no parent state.
    unsafe {
        process.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // Admission is atomic across concurrent turns. The child and its permit
    // move into one waiter, so no reusable PID is polled after Tokio reaps it.
    let permit = if registry.detached_budget == 0 {
        None
    } else {
        Some(registry.detached_slots.clone().try_acquire_owned().map_err(|_| {
            Error::with(
                "detached_limit",
                format!(
                    "{} detached commands are still running, the daemon's bound (--max-detached {}); stop one first",
                    registry.detached_budget - registry.detached_slots.available_permits(),
                    registry.detached_budget
                ),
            )
        })?)
    };
    // What the sandbox holds in place stays held while the command runs.
    let held = crate::sandbox::hold_for(sandbox.as_deref()).await;
    let mut child = process.spawn()?;
    let pid = child.id().ok_or(Error::new("shell_spawn_failed"))?;
    tokio::spawn(async move {
        let _permit = permit;
        let _ = child.wait().await;
        drop(held);
    });
    Ok(Outcome::text(
        json!({"detached":true,"pid":pid}).to_string(),
    ))
}

#[cfg(not(unix))]
async fn detach(
    _: &str,
    _: &Path,
    _: &Registry,
    _: &[(String, String)],
    _: Option<Arc<Policy>>,
) -> Result<Outcome> {
    fail("shell_platform_unsupported")
}

#[cfg(unix)]
async fn shell(
    command: &str,
    workspace: &Path,
    timeout: Duration,
    registry: &Registry,
    environment: &[(String, String)],
    sandbox: Option<&Policy>,
) -> Result<(Captured, Captured, Exit, Duration)> {
    use std::process::Stdio;
    use tokio::process::Child;
    struct ProcessGroup {
        child: Child,
        group: i32,
    }
    impl Drop for ProcessGroup {
        fn drop(&mut self) {
            // Only the group created for this command. Also runs if the turn's
            // future is cancelled. Escaped sessions require external isolation.
            if self.group > 1 {
                // SAFETY: kill takes no pointers; the group is this command's own.
                unsafe {
                    libc::kill(-self.group, libc::SIGKILL);
                }
            }
        }
    }
    let child = sh(command, workspace, registry, environment, sandbox, false)?
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let started = tokio::time::Instant::now();
    let group = child.id().ok_or(Error::new("shell_spawn_failed"))? as i32;
    let mut owned = ProcessGroup { child, group };
    let mut stdout = owned
        .child
        .stdout
        .take()
        .ok_or(Error::new("shell_pipe_failed"))?;
    let mut stderr = owned
        .child
        .stderr
        .take()
        .ok_or(Error::new("shell_pipe_failed"))?;
    let (mut out, mut err) = (Captured::default(), Captured::default());
    let result = tokio::time::timeout(timeout, async {
        tokio::try_join!(
            capture(&mut stdout, &mut out),
            capture(&mut stderr, &mut err),
            async { owned.child.wait().await.map_err(Error::from) }
        )
    })
    .await;
    // Finish cleanup before returning a timeout. Drop also covers
    // cancellation, with Tokio responsible for reaping its killed child.
    // SAFETY: kill takes no pointers; the group is this command's own.
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
    let _ = owned.child.wait().await;
    owned.group = 0;
    let elapsed = started.elapsed();
    match result {
        Ok(result) => result.map(|(_, _, status)| (out, err, Exit::Status(status), elapsed)),
        Err(_) => {
            // What the group wrote before it was killed is still in the pipes.
            // A process that left the group may hold them open, so the drain
            // is bounded too.
            if let Ok(Err(error)) = tokio::time::timeout(Duration::from_millis(100), async {
                tokio::try_join!(
                    capture(&mut stdout, &mut out),
                    capture(&mut stderr, &mut err)
                )
            })
            .await
            {
                return Err(error);
            }
            Ok((out, err, Exit::TimedOut, elapsed))
        }
    }
}

#[cfg(not(unix))]
async fn shell(
    _: &str,
    _: &Path,
    _: Duration,
    _: &Registry,
    _: &[(String, String)],
    _: Option<&Policy>,
) -> Result<(Captured, Captured, Exit, Duration)> {
    fail("shell_platform_unsupported")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_files_are_named_and_text_passes() {
        assert_eq!(binary(b"\x89PNG\r\n\x1a\n\0\0"), Some("a PNG image"));
        assert_eq!(binary(b"RIFF\0\0\0\0WEBPVP8 "), Some("a WebP image"));
        assert_eq!(binary(b"data\0more"), Some("a binary file"));
        assert_eq!(binary("caf\u{e9}\n".as_bytes()), None);
        assert_eq!(binary(b""), None);
    }
    #[tokio::test]
    async fn read_shows_a_picture_and_refuses_other_binaries_by_kind() {
        let dir = std::env::temp_dir().join(format!("agent-read-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(4, 3)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(dir.join("shot.png"), &png).unwrap();
        std::fs::write(dir.join("doc.pdf"), b"%PDF-1.7 and more").unwrap();
        let registry = Registry::new("read").unwrap();
        let read = |path: &str| {
            let prepared = registry
                .prepare("read", &json!({"path": path}).to_string())
                .unwrap();
            registry.execute(prepared, &dir, &[], None)
        };
        let outcome = read("shot.png").await.unwrap();
        assert_eq!(
            outcome.output,
            format!(
                "shot.png: a PNG image, 4×3, {} bytes; it follows",
                png.len()
            )
        );
        assert_eq!(outcome.images.len(), 1);
        assert_eq!(outcome.images[0].media_type, "image/png");
        let error = read("doc.pdf").await.unwrap_err();
        assert_eq!(error.code, "file_not_text");
        assert_eq!(
            error.detail.as_deref(),
            Some("a PDF of 17 bytes; read returns text")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn oversized_output_keeps_head_and_tail_on_char_boundaries() {
        let text = format!(
            "{}é{}",
            "a".repeat(PREVIEW_BYTES),
            "z".repeat(PREVIEW_BYTES)
        );
        let shown = truncate(&text, 0);
        assert!(shown.starts_with("aaaa"));
        assert!(shown.ends_with("zzzz"));
        assert!(shown.contains("bytes omitted; the full output is retained"));
        assert!(shown.len() < text.len());
        assert!(truncate(&text, 9).contains("9 more past 1 MiB were not kept"));
    }
    #[test]
    fn captured_output_keeps_both_ends_past_the_bound() {
        let mut whole = Captured::default();
        whole.head.extend(vec![b'h'; KEPT_END]);
        whole.keep(&vec![b't'; ARTIFACT_BYTES - KEPT_END]);
        let (bytes, dropped) = whole.finish();
        assert_eq!((bytes.len(), dropped), (ARTIFACT_BYTES, 0));

        let mut over = Captured::default();
        over.head.extend(vec![b'h'; KEPT_END]);
        for _ in 0..40 {
            over.keep(&vec![b'm'; 64 * 1024]);
        }
        over.keep(b"end");
        let (bytes, dropped) = over.finish();
        assert_eq!(dropped, (40 * 64 * 1024 + 3 - KEPT_END) as u64);
        assert!(bytes.len() <= ARTIFACT_BYTES);
        assert!(bytes.starts_with(b"hhhh"));
        assert!(bytes.ends_with(b"mmmend"));
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains(&format!("[... {dropped} bytes dropped")));
    }
    #[test]
    fn registries_reject_unknown_and_duplicate_tools() {
        assert!(Registry::new("echo,read,write,edit").is_ok());
        assert!(Registry::new("echo,echo").is_err());
        assert!(Registry::new("mcp").is_err());
        assert_eq!(Registry::new("read").unwrap().names(), ["read"]);
    }
    #[test]
    fn secrets_are_redacted_by_field_name_and_key_block() {
        let credentials = Credentials::default();
        let redact = |text: &str| credentials.redact(text.to_owned());
        assert_eq!(
            redact(
                r#"{"AccessKeyId": "example1example1", "SecretAccessKey": "fake2fake2fake2", "SessionToken": "fake3fake3fake3", "Expiration": "2026-01-01T00:00:00Z"}"#
            ),
            r#"{"AccessKeyId": "example1example1", "SecretAccessKey": "[REDACTED]", "SessionToken": "[REDACTED]", "Expiration": "2026-01-01T00:00:00Z"}"#
        );
        assert_eq!(
            redact("aws_secret_access_key = fake4fake4fake4\nregion = us-east-1\n"),
            "aws_secret_access_key = [REDACTED]\nregion = us-east-1\n"
        );
        assert_eq!(
            redact("export OPENAI_API_KEY=fake-5fake-5fake; echo ok"),
            "export OPENAI_API_KEY=[REDACTED]; echo ok"
        );
        // An escaped quote is inside the value, not its end.
        assert_eq!(
            redact(r#"{"password":"a\"fake7fake7fake7","user":"me"}"#),
            r#"{"password":"[REDACTED]","user":"me"}"#
        );
        assert_eq!(redact(r#"password: "x\"#), r#"password: "x\"#);
        // Names that only resemble one, short values, and code keep theirs.
        for kept in [
            "NextToken: page6page6page6",
            "max_tokens: 4096",
            "password: hunter2",
            "    api_key: Option<String>,",
            "token: ${{ secrets.TOKEN }}",
        ] {
            assert_eq!(redact(kept), kept);
        }
        assert_eq!(
            redact(
                "a\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\nAAAA\n-----END OPENSSH PRIVATE KEY-----\nz"
            ),
            "a\n-----BEGIN OPENSSH PRIVATE KEY-----\n[REDACTED]\n-----END OPENSSH PRIVATE KEY-----\nz"
        );
        let certificate = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
        assert_eq!(redact(certificate), certificate);
        assert_eq!(
            redact("-----BEGIN RSA PRIVATE KEY-----\nMIIE"),
            "-----BEGIN RSA PRIVATE KEY-----\n[REDACTED]"
        );
        assert_eq!(
            redact("-----BEGIN PRIVATE KEY-----\r\nMIIE\r\n-----END PRIVATE KEY-----\r\n"),
            "-----BEGIN PRIVATE KEY-----\r\n[REDACTED]\n-----END PRIVATE KEY-----\r\n"
        );
    }
    #[cfg(unix)]
    #[test]
    fn invalid_shell_arguments_say_which() {
        let shell = Registry::new("shell").unwrap();
        let detail = |args: &str| match shell.prepare("shell", args) {
            Err(error) if error.code == "invalid_shell_arguments" => error.detail.unwrap(),
            _ => panic!("{args} was accepted"),
        };
        assert_eq!(
            detail(r#"{"command":"true","timeout_ms":900000}"#),
            "timeout_ms is 900000; it runs from 1 to 600000"
        );
        assert_eq!(detail(r#"{"command":""}"#), "command is empty");
        assert_eq!(
            detail(r#"{"command":"true","detach":true,"background":true}"#),
            "detach and background cannot both be set"
        );
    }
}
