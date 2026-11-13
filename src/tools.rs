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
    sync::{Semaphore, watch},
};

mod fetch;
mod files;
mod patch;
mod picture;
#[cfg(unix)]
mod terminal;
use files::read_bounded;
pub use patch::Patch;
#[cfg(unix)]
pub use terminal::POLL_YIELD_MS;

/// Rows of the audit trail one history call reads.
const AUDIT_ROWS: usize = 50;
/// Model-facing text per tool result stream.
pub const PREVIEW_BYTES: usize = 64 * 1024;
/// Retained output per stream. Past this a stream keeps its first and last
/// `KEPT_END` bytes and the command runs on.
pub const ARTIFACT_BYTES: usize = 1024 * 1024;
/// Each end kept of a stream past `ARTIFACT_BYTES`, leaving room for the
/// marker that counts the dropped middle.
const KEPT_END: usize = (ARTIFACT_BYTES - 256) / 2;
const FILE_BYTES: usize = 4 * 1024 * 1024;
const WRITE_BYTES: usize = 1024 * 1024;
const DEFAULT_SHELL_TIMEOUT_MS: u64 = 120_000;
const MAX_SHELL_TIMEOUT_MS: u64 = 600_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 86_400_000;
/// A background command holds no turn while it runs, so it may run as long
/// as a `wait` on it may last.
const MAX_BACKGROUND_TIMEOUT_MS: u64 = MAX_WAIT_TIMEOUT_MS;
/// Simultaneously running child processes, foreground or background.
pub const DEFAULT_PROCESS_BUDGET: usize = 64;
/// Processes that outlive their call, terminals and detached commands,
/// running at once daemon-wide unless configured: four per logical CPU,
/// which keeps their descriptors well inside a default 256-descriptor
/// limit on a laptop.
pub fn default_lasting() -> usize {
    4 * std::thread::available_parallelism().map_or(4, |n| n.get())
}
/// How long a terminal's start waits for output by default, and how long
/// it or a write waits at most.
const START_YIELD_MS: u64 = 1_000;
const MAX_YIELD_MS: u64 = 30_000;
/// The most a write to a terminal may type at once.
const INPUT_BYTES: usize = 16 * 1024;

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
    WebSearch,
    WebFetch,
    WriteStdin,
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
            "web_search" => Tool::WebSearch,
            "web_fetch" => Tool::WebFetch,
            "write_stdin" => Tool::WriteStdin,
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
            Tool::WebSearch => "web_search",
            Tool::WebFetch => "web_fetch",
            Tool::WriteStdin => "write_stdin",
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
                "Run a noninteractive bash command (sh where bash is not installed) in the workspace, or in workdir when given (relative to the workspace unless absolute). stdout and stderr are returned separately; each is truncated to a head and tail beyond 64 KiB, with up to 1 MiB of it retained: past that, its first and last 512 KiB, and the command keeps running. Default timeout 120 s, maximum 600 s; with background=true there is no timeout unless timeout_ms sets one, of up to a day. A command that times out is killed and returns its output so far with timed_out. Every result reports wall_time_ms. Commands run with no terminal: TERM=dumb, NO_COLOR=1, and cat as the pager; AI_AGENT=agent tells tools an agent runs them. When the command returns or times out, every process it started is killed, so `cmd &` does not outlive it. With background=true the command is started and a proc handle is returned immediately; collect its result later with wait. If no wait collects it, its end comes as a message once no turn of yours runs: {\"finished\":{\"proc:N\":{how it ended}}}; wait on the handle reads the output. A steer that arrives while a command runs moves it to the background: the result is its proc handle with steered=true, and the command keeps running. A background command runs until it exits, its timeout_ms passes, kill=true on its handle stops it, or the daemon stops. With detach=true the command starts in its own session and a proc handle and its pid are returned at once; it outlives the turn and the daemon and runs until it exits, a pause, or kill=true on its handle. Its output is discarded unless the command redirects it to a file; its end comes as a background command's does. \"$AGENT_BIN\" ps --bot \"$AGENT_BOT\" lists your commands still running. With tty=true it runs in a terminal, for a REPL or a program that prompts: the result is what it wrote within yield_time_ms (default 1000) and, while it runs, a proc handle for write_stdin.",
                json!({"type":"object","properties":{"command":{"type":"string"},
                "workdir":{"type":"string"},
                "timeout_ms":{"type":"integer","minimum":1,"maximum":MAX_BACKGROUND_TIMEOUT_MS},
                "background":{"type":"boolean"},"detach":{"type":"boolean"},"tty":{"type":"boolean"},
                "yield_time_ms":{"type":"integer","minimum":0,"maximum":MAX_YIELD_MS}},
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
                "Read a conversation turn: one of yours the context left out (numbered from 1; see the context note), or with bot, another agent's, as it stands, including one still running. With bot and no turn, lists that agent's newest turns, newest first: each one's number, handle, status and prompt. Or give handle alone, a message's number as run, wait and the listing give it (BOT/N), to read the turn that took that message in. Returns provider items as JSONL, omitting only encrypted_content from reasoning items; summaries remain. Pages contain at most 64 KiB and may shrink to fit context. Optional limit: 4 to 65536 bytes. Start at offset 0; use next_offset until done. Offsets count UTF-8 bytes of the filtered view. Pages may split records; concatenate text before decoding. With query instead of turn, searches your earlier turns (or with bot, that agent's) for that literal text, ignoring ASCII case, newest first: up to 20 matches, each with its turn, item type, and the JSON text around it; read a turn for all of its matches. When done is false, pass next_before as before to search older turns. With query, bot may also be a pattern of * ? and [...], case-sensitive, such as * for every agent or proj.* for some: it searches every agent it matches at once, newest first; each match then names its bot and its handle. With audit instead, reads the audit trail: every agent's tool calls and their steps, or with bot one agent's, newest first, each with its time, turn, call, tool, kind, place, who approved it, access, outcome and the start of its arguments; audit's fields filter it, and next_before passed as before pages older rows.",
                json!({"type":"object","properties":{"bot":{"type":"string","minLength":1,"maxLength":128},
                "turn":{"type":"integer","minimum":1},"handle":{"type":"string","minLength":1,"maxLength":256},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":4,"maximum":65536},
                "query":{"type":"string","minLength":1,"maxLength":256},"before":{"type":"integer","minimum":1},
                "audit":{"type":"object","properties":{"kind":{"enum":crate::store::audit::KINDS},
                "place":{"enum":crate::store::audit::PLACES},"approved":{"enum":crate::store::audit::APPROVERS},
                "access":{"enum":crate::store::audit::ACCESS},"outcome":{"enum":crate::store::audit::OUTCOMES},
                "tool":{"type":"string","minLength":1,"maxLength":512},"since_ms":{"type":"integer","minimum":0}},
                "additionalProperties":false}},
                "additionalProperties":false}),
            ),
            // What a model is shown is its provider's: Codex's `web.run`,
            // or the provider's own hosted tool (see provider::Search).
            Tool::WebSearch => (
                include_str!("tools/web_run.md"),
                serde_json::from_str(include_str!("tools/web_run.json")).expect("web.run schema"),
            ),
            Tool::WebFetch => (
                "Fetch a public http or https URL and return its text: an HTML page as Markdown, with its title and links resolved to absolute URLs; other text as it is. The result names the final URL after redirects and the HTTP status. Text past 64 KiB is cut to its start and end, with the whole kept as an artifact to page with read. Use it to read a page you were given or found; it does not run the page's scripts or sign in.",
                json!({"type":"object","properties":{"url":{"type":"string","minLength":1,"maxLength":8192}},
                "required":["url"],"additionalProperties":false}),
            ),
            Tool::WriteStdin => (
                "Type into a terminal that shell started with tty=true, and return what it wrote since the last read, stdout and stderr together, which echoes what was typed. A terminal runs until it exits or is killed, across turns; wait on its handle reports how it ended. chars is typed as it is: end a line with \\n, and send a control key as its character, such as \\u0003 for Ctrl-C or \\u0004 for Ctrl-D; empty chars only reads. It waits yield_time_ms for output, 250 by default after typing and 5000 when only reading, at most 30000; a command that ends sooner returns at once with its exit_code. kill=true ends the command and every process it started; on a background or detached command's proc handle, or a call a message moved to the background while it waited for approval or an answer, kill=true stops it (a call still waiting never runs).",
                json!({"type":"object","properties":{"handle":{"type":"string"},
                "chars":{"type":"string","maxLength":INPUT_BYTES},
                "yield_time_ms":{"type":"integer","minimum":0,"maximum":MAX_YIELD_MS},"kill":{"type":"boolean"}},
                "required":["handle"],"additionalProperties":false}),
            ),
            Tool::Wait => (
                "Suspend until every handle resolves, without holding any execution capacity. Handles are 'BOT/N' (a message sent to a peer agent, numbered as run gives it) or 'proc:N' (a background or detached shell, or a call a message moved to the background). Each result reports the outcome: for a message that opened a turn, that turn's status and final text, with before_steer, the last text it wrote before it was steered, when it ended on a reply to a steer; for one another turn took in, status taken and that turn's number, to wait on in turn; or a process's output and exit code.
 With timeout_ms, unresolved handles are reported as pending and stay valid for a later wait; 0 polls without waiting. With any: true, the first resolved handle ends the wait and the rest are reported pending. A steer ends the wait at once with steered=true: unresolved handles are reported pending and stay valid, so wait again to keep waiting.",
                json!({"type":"object","properties":{"handles":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":64},
                "timeout_ms":{"type":"integer","minimum":0,"maximum":MAX_WAIT_TIMEOUT_MS},"any":{"type":"boolean"}},
                "required":["handles"],"additionalProperties":false}),
            ),
        };
        ToolSchema {
            name: self.name().into(),
            description: description.into(),
            parameters,
            grammar: (self == Tool::ApplyPatch).then_some(patch::GRAMMAR.into()),
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
    /// A Lark grammar the call's input follows, for a tool whose input is
    /// free-form text: declared as a custom tool where the provider has
    /// them, and as a function of that text as `input` elsewhere, so
    /// `parameters` must take `input` as a string. Either way the call's
    /// arguments are `{"input": TEXT}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grammar: Option<String>,
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
    /// Which bots this one creates take the tool too: every generation
    /// (`true`), none (`false`, the declaring bot's own), or that many.
    #[serde(default, skip_serializing_if = "Inherit::is_all")]
    pub inherit: Inherit,
    /// Whether a gate that names the tool judges the steps its client puts
    /// to it with `approve_step` instead of the call as a whole: a script
    /// whose nested calls are each approved, as in Codex's code mode.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub steps: bool,
    /// What kind of tool it is, one of the audit trail's kinds, which its
    /// calls' rows there name; none when its client says nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
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
/// How far down the bots a bot creates a declared tool goes: to every
/// generation, the default, or to `Generations(n)`, each bot that takes it
/// passing it on one generation fewer, as a packet's hop count does. On the
/// wire it is `true`, `false` (no generation) or the count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Inherit {
    #[default]
    All,
    Generations(u32),
}
impl Inherit {
    fn is_all(&self) -> bool {
        *self == Inherit::All
    }
    /// Whether a bot this one creates takes the tool.
    pub fn passes(self) -> bool {
        self != Inherit::Generations(0)
    }
    /// Whether the tool goes no further down than `bound` lets it.
    pub fn within(self, bound: Inherit) -> bool {
        match (self, bound) {
            (_, Inherit::All) => true,
            (Inherit::All, Inherit::Generations(_)) => false,
            (Inherit::Generations(n), Inherit::Generations(most)) => n <= most,
        }
    }
    /// What a bot this one creates takes with the tool.
    pub fn passed(self) -> Inherit {
        match self {
            Inherit::All => Inherit::All,
            Inherit::Generations(n) => Inherit::Generations(n.saturating_sub(1)),
        }
    }
}
impl Serialize for Inherit {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Inherit::All => serializer.serialize_bool(true),
            Inherit::Generations(0) => serializer.serialize_bool(false),
            Inherit::Generations(n) => serializer.serialize_u32(*n),
        }
    }
}
impl<'de> Deserialize<'de> for Inherit {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Every(bool),
            Count(u32),
        }
        Ok(
            match Wire::deserialize(deserializer).map_err(|_| {
                serde::de::Error::custom("inherit is true, false or a whole number of generations")
            })? {
                Wire::Every(true) => Inherit::All,
                Wire::Every(false) => Inherit::Generations(0),
                Wire::Count(n) => Inherit::Generations(n),
            },
        )
    }
}
/// The most tools a bot may have declared, and the most bytes they take.
pub const MAX_DECLARED: usize = 32;
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
        if let Some(kind) = &tool.kind
            && !crate::store::audit::KINDS.contains(&kind.as_str())
        {
            return fail_with(
                "invalid_declare",
                format!(
                    "{name}: kind is one of {}",
                    crate::store::audit::KINDS.join(", ")
                ),
            );
        }
        if tool.parameters["type"] != "object" {
            return fail_with(
                "invalid_declare",
                format!("{name}: parameters must be an object schema"),
            );
        }
        if let Some(grammar) = &tool.grammar {
            let input = &tool.parameters["properties"]["input"];
            // Its call is the text alone, so nothing else may be required.
            let required = tool.parameters["required"]
                .as_array()
                .is_some_and(|fields| fields.len() == 1 && fields[0] == "input");
            if grammar.is_empty() || input["type"] != "string" || !required {
                return fail_with(
                    "invalid_declare",
                    format!(
                        "{name}: a tool with a grammar takes its text as `input`, a string and its only required field"
                    ),
                );
            }
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
            None if tool.expire_ms.is_some() || tool.steps => {
                return fail_with(
                    "invalid_declare",
                    format!("{name}: expire_ms and steps go with answered_by"),
                );
            }
            None => {}
        }
    }
    Ok(())
}

/// The most JSON text an answered call's arguments may take: its client
/// reads the call whole from a `tool_calls` page, one line of at most
/// 1 MiB beside its id (`MAX_CALL_ID`) and the rest. A model's reply has no
/// bound but its own, so a longer call is refused at once, the model told.
pub const ANSWERED_ARGUMENTS: usize = 768 * 1024;

/// A declared tool's call checked against its parameters, as far as the
/// daemon looks: an object, with every required field, no field the
/// schema closes out, and each listed field of its listed basic type.
pub fn check_declared(tool: &Declared, arguments: &str) -> Result<()> {
    let invalid = |detail: String| Error::with("invalid_arguments", detail);
    if tool.answered_by.is_some() && arguments.len() > ANSWERED_ARGUMENTS {
        return Err(invalid(format!(
            "{}: arguments take {} bytes, past the {ANSWERED_ARGUMENTS} its client is sent",
            tool.name,
            arguments.len()
        )));
    }
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
    /// Foreground commands holding a slot now; background ones are the
    /// store's to count, as their results are recorded there.
    foreground: Arc<std::sync::atomic::AtomicUsize>,
    /// Processes that outlive their call, as a systemd slice holds a
    /// service's: a permit stays with each terminal until its session is
    /// gone and with each detached child until its wait completes.
    lasting_slots: Arc<Semaphore>,
    lasting_budget: usize,
    /// Whether the machine's load lets a new command start, as make's
    /// `-l`; see `admits`. None is no gate.
    load: Option<watch::Receiver<bool>>,
    /// Set when a turn parks on the load gate, so the daemon looks for
    /// parked turns to wake only while some may be there.
    load_parked: Arc<std::sync::atomic::AtomicBool>,
    /// The line of background commands waiting at the load gate, in the
    /// order they came: a fair lock its first holds while it waits, so a
    /// later one never takes a slot ahead of it.
    load_line: Arc<tokio::sync::Mutex<()>>,
    /// Running terminals, by proc handle, each its bot's own.
    #[cfg(unix)]
    terminals: terminal::Terminals,
    /// Set once as the daemon stops: background commands still running
    /// end then rather than outlive it.
    stopping: Arc<watch::Sender<bool>>,
    /// Background commands running, by proc handle, each with what stops
    /// it when its agent kills it.
    stops: Arc<std::sync::Mutex<HashMap<i64, tokio::sync::oneshot::Sender<()>>>>,
    /// Counts pulls of the cord: background commands running or waiting
    /// for a slot at one end then.
    pulls: Arc<watch::Sender<u64>>,
    credentials: Credentials,
    environment: Arc<Vec<(String, String)>>,
    /// What the daemon keeps that a sandbox leaves alone or lets through.
    daemon: Arc<Daemon>,
    /// Request encodings per (family, selection); see `encoded`.
    encodings: Encodings,
    /// Bytes of tool output being redacted off the runtime at once; see
    /// `off_runtime`.
    heavy: Arc<Semaphore>,
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
/// A foreground command's place in the running count: taken when it
/// starts and given up once, when it ends or when [`lift`](Self::lift)
/// moves it to the background, whichever comes first. A place lifted
/// before its command starts is never taken.
#[derive(Clone)]
pub struct Place {
    /// `WAITING`, then `COUNTED` while it holds its place, then `LEFT`.
    state: Arc<std::sync::atomic::AtomicU8>,
    count: Arc<std::sync::atomic::AtomicUsize>,
}
const WAITING: u8 = 0;
const COUNTED: u8 = 1;
const LEFT: u8 = 2;
impl Place {
    /// The command now runs as a background process, which the store counts.
    pub fn lift(&self) {
        if self.state.swap(LEFT, std::sync::atomic::Ordering::AcqRel) == COUNTED {
            self.count
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    fn take(&self) -> Taken {
        use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed};
        if (self
            .state
            .compare_exchange(WAITING, COUNTED, AcqRel, Acquire))
        .is_ok()
        {
            self.count.fetch_add(1, Relaxed);
        }
        Taken(self.clone())
    }
}
/// A taken [`Place`], given up when dropped unless lifted first.
struct Taken(Place);
impl Drop for Taken {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed};
        let place = &self.0;
        if (place.state.compare_exchange(COUNTED, LEFT, AcqRel, Acquire)).is_ok() {
            place.count.fetch_sub(1, Relaxed);
        }
    }
}
impl Drop for Queued {
    fn drop(&mut self) {
        if let Some(pending) = &self.pending {
            pending.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
#[derive(Clone)]
struct Credential {
    name: String,
    value: String,
}

/// Values that never appear in tool output and never reach a shell's
/// environment. Shared, so a login re-read while the daemon runs is
/// redacted from then on; earlier values stay listed. Readers take the
/// current set and let the lock go, so a scan of a large output on a
/// blocking thread never holds up a refresh on the runtime's; a change
/// copies the set if a scan still reads it.
#[derive(Clone, Default)]
pub struct Credentials(Arc<std::sync::RwLock<Arc<CredentialSet>>>);

#[derive(Clone, Default)]
struct CredentialSet {
    /// Longer values first, so a shorter old token cannot mask only a prefix
    /// of a new token. Refreshes pay for insertion; tool output stays sorted.
    values: Vec<Credential>,
    names: Vec<String>,
    /// Field names, lowercased without `_` or `-`, whose values a client
    /// had redacted wherever they are printed (`--redact-field`).
    fields: Vec<String>,
    /// What the header of a PEM block a client had redacted names
    /// (`--redact-block`), such as `PRIVATE KEY`.
    blocks: Vec<String>,
}

/// Whether a variable's `name` matches `pattern`: `*` any run of
/// characters and `?` any one, in any ASCII case, as Codex matches its
/// shell environment excludes.
pub fn name_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().map(|c| c.to_ascii_lowercase()).collect();
    let name: Vec<char> = name.chars().map(|c| c.to_ascii_lowercase()).collect();
    let (mut p, mut n) = (0, 0);
    // The last `*` seen, and where in the name its run ends so far.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((at, end)) => {
                    star = Some((at, end + 1));
                    p = at + 1;
                    n = end + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

impl Credentials {
    pub fn set(&self, name: &str, value: &str) {
        if value.is_empty() {
            return;
        }
        let mut guard = self.0.write().unwrap_or_else(|e| e.into_inner());
        let set = Arc::make_mut(&mut guard);
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
        let mut guard = self.0.write().unwrap_or_else(|e| e.into_inner());
        let set = Arc::make_mut(&mut guard);
        if !set.names.iter().any(|existing| existing == name) {
            set.names.push(name.to_owned());
        }
    }
    pub fn names(&self) -> Vec<String> {
        self.current().names.clone()
    }
    /// The set as it is now, the lock held only to take it.
    fn current(&self) -> Arc<CredentialSet> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// Also redact the values after these field names, matched as a name's
    /// end, and the bodies of PEM blocks whose header names one of `blocks`.
    pub fn redact_also(&self, fields: &[String], blocks: &[String]) {
        let mut guard = self.0.write().unwrap_or_else(|e| e.into_inner());
        let set = Arc::make_mut(&mut guard);
        set.fields = fields.iter().map(|field| field_key(field)).collect();
        set.blocks = blocks.to_vec();
    }
    pub fn redact(&self, mut text: String) -> String {
        let set = self.current();
        for credential in &set.values {
            if text.contains(&credential.value) {
                text = text.replace(&credential.value, REDACTED);
            }
        }
        if !set.blocks.is_empty() {
            text = redact_blocks(text, &set.blocks);
        }
        if !set.fields.is_empty() {
            text = redact_fields(text, &set.fields);
        }
        text
    }
    /// How many of `text`'s last bytes could begin a credential that more
    /// output would finish, or a secret field whose value more output
    /// would carry: what a stream holds back until it can redact. What it
    /// shows never ends inside a whole credential.
    pub fn unfinished(&self, text: &[u8]) -> usize {
        let set = self.current();
        let values = || {
            set.values
                .iter()
                .map(|credential| credential.value.as_bytes())
        };
        let started = values()
            .filter_map(|value| {
                (1..value.len().min(text.len() + 1))
                    .rev()
                    .find(|&n| text.ends_with(&value[..n]))
            })
            .max()
            .unwrap_or(0)
            .max(open_field(text, &set.fields).value);
        // A whole credential across the cut, as one that ends with its own
        // start, is shown whole.
        let mut cut = text.len() - started;
        while let Some(end) = values()
            .filter_map(|value| {
                let from = (cut + 1).saturating_sub(value.len());
                let to = (cut + value.len() - 1).min(text.len());
                let window = text.get(from..to)?;
                let start = window.windows(value.len()).position(|w| w == value)?;
                Some(from + start + value.len())
            })
            .max()
        {
            cut = end;
        }
        text.len() - cut
    }
    /// How many of `text`'s last bytes open a secret field whose value has
    /// not started: a name, or a name and its separator. They are shown,
    /// and kept so the next text is redacted as read after them.
    pub fn opening(&self, text: &[u8]) -> usize {
        let set = self.current();
        let open = open_field(text, &set.fields);
        open.name - open.value
    }
    /// `text` redacted as it reads after `before`, already shown: a value
    /// after a field's name shown earlier is still redacted.
    pub fn redact_after(&self, before: &str, text: String) -> String {
        if before.is_empty() {
            return self.redact(text);
        }
        let mut whole = self.redact(format!("{before}{text}"));
        if whole.starts_with(before) {
            whole.replace_range(..before.len(), "");
            whole
        } else {
            self.redact(text)
        }
    }
}

const REDACTED: &str = "[REDACTED]";
/// Shorter values are flags, placeholders and counts, not secrets; so are
/// values without both letters and digits.
const SECRET_MIN: usize = 8;

/// A field name as it is matched: lowercased, without `_` or `-`.
fn field_key(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_' && *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Replace the value after each of `fields` in `text`, printed as
/// `NAME=VALUE`, `NAME: VALUE` or `"NAME": "VALUE"`. A name matches when it
/// ends with a field, so `aws_secret_access_key` matches `secretaccesskey`.
fn redact_fields(text: String, fields: &[String]) -> String {
    let shortest = fields.iter().map(String::len).min().unwrap_or(0);
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
        // A word shorter than every field cannot end with one.
        if i - start < shortest {
            continue;
        }
        let name = field_key(&text[start..i]);
        if !fields.iter().any(|field| name.ends_with(field.as_str())) {
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

/// A secret field still open at a text's end, in bytes from that end.
#[derive(Clone, Copy, Default)]
struct Open {
    /// Where its name starts: a name that may be one's start, a name and
    /// separator with no value yet, or a name and a value not ended.
    name: usize,
    /// Where its value starts, when one has and not ended: held back so it
    /// is redacted whole, at most `FIELD_HOLD` bytes of it. The name before
    /// it is not secret, so it is shown: a prompt such as `Password: `
    /// reaches whoever is to answer it.
    value: usize,
}

fn open_field(text: &[u8], fields: &[String]) -> Open {
    const FIELD_HOLD: usize = 4096;
    // Four bytes of a field's start, so short words go out as typed.
    const NAME_START: usize = 4;
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    let tail = &text[text.len().saturating_sub(FIELD_HOLD)..];
    let key = |name: &[u8]| field_key(&String::from_utf8_lossy(name));
    let mut i = 0;
    while i < tail.len() {
        if !word(tail[i]) || (i > 0 && word(tail[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < tail.len() && word(tail[i]) {
            i += 1;
        }
        let name = key(&tail[start..i]);
        if i == tail.len() {
            // The last word: it may be the start of a field's name.
            // Bytes, not characters: a field's name may be any text.
            let begins = fields.iter().any(|field| {
                (NAME_START.min(field.len())..=field.len())
                    .any(|n| name.as_bytes().ends_with(&field.as_bytes()[..n]))
            });
            let name = if begins { tail.len() - start } else { 0 };
            return Open { name, value: 0 };
        }
        if !fields.iter().any(|field| name.ends_with(field.as_str())) {
            continue;
        }
        // As `redact_fields` reads one: a quote, a separator, a quote.
        let mut j = i;
        if j < tail.len() && matches!(tail[j], b'"' | b'\'') {
            j += 1;
        }
        while j < tail.len() && matches!(tail[j], b' ' | b'\t') {
            j += 1;
        }
        let name = tail.len() - start;
        if j == tail.len() {
            return Open { name, value: 0 };
        }
        if !matches!(tail[j], b'=' | b':') {
            continue;
        }
        j += 1;
        while j < tail.len() && matches!(tail[j], b' ' | b'\t') {
            j += 1;
        }
        let quote = tail.get(j).copied().filter(|b| matches!(b, b'"' | b'\''));
        if quote.is_some() {
            j += 1;
        }
        let ended = tail[j..].iter().any(|&b| match quote {
            Some(q) => b == q || b == b'\n',
            None => {
                b.is_ascii_whitespace() || matches!(b, b',' | b';' | b'"' | b'\'' | b'}' | b']')
            }
        });
        if !ended {
            return Open {
                name,
                value: tail.len() - j,
            };
        }
    }
    Open::default()
}

/// Replace the body of each PEM block in `text` whose header names one of
/// `blocks`.
fn redact_blocks(mut text: String, blocks: &[String]) -> String {
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
        if !blocks
            .iter()
            .any(|block| text[begin..header].contains(block.as_str()))
        {
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
    /// Each command runs in `workdir` when its call names one, read
    /// against the turn's folder ([`Prepared::folder`]), as Codex's do.
    Shell {
        command: String,
        /// `None` runs it until it exits: a background command given no
        /// `timeout_ms`, as Codex's unified exec runs one.
        timeout_ms: Option<u64>,
        background: bool,
        workdir: Option<String>,
    },
    /// A command in its own session that outlives the turn and the
    /// daemon, on a proc handle. Resolved by the runtime; the registry
    /// starts one untracked.
    Detach {
        command: String,
        workdir: Option<String>,
    },
    /// A command in a terminal its bot types into, on a proc handle.
    /// Resolved by the runtime, never by the registry.
    Terminal {
        command: String,
        yield_ms: u64,
        workdir: Option<String>,
    },
    /// Input for a bot's terminal, or its end. Resolved by the runtime,
    /// never by the registry.
    WriteStdin {
        handle: i64,
        chars: String,
        yield_ms: Option<u64>,
        kill: bool,
    },
    /// Resolved by the runtime, never by the registry.
    History {
        /// Another agent's, or the caller's own when `None`.
        bot: Option<String>,
        /// `None` lists the agent's newest turns.
        turn: Option<i64>,
        /// A message handle, `BOT/N`, in place of `bot` and `turn`: the
        /// turn that took that message in.
        handle: Option<String>,
        offset: u64,
        limit: usize,
    },
    /// The audit trail, every agent's or `query.bot`'s. Resolved by the
    /// runtime, never by the registry.
    HistoryAudit(crate::store::audit::Query),
    /// Resolved by the runtime, never by the registry.
    HistorySearch {
        /// Another agent's, or the caller's own when `None`.
        bot: Option<String>,
        query: String,
        /// Search the turns before this one.
        before: Option<i64>,
    },
    /// Resolved by the runtime, never by the registry.
    Wait {
        handles: Vec<String>,
        timeout_ms: Option<u64>,
        any: bool,
    },
    /// A page the daemon fetches.
    WebFetch(reqwest::Url),
    /// A `web.run` call's commands, which the bot's provider answers.
    /// Resolved by the runtime, never by the registry.
    WebSearch(String),
    /// A client's declared tool, whose call that fits gets this result.
    /// Resolved by the runtime, never by the registry.
    Declared(String),
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

impl Prepared {
    /// The folder a command runs in: `workdir` read against the turn's
    /// `folder`, or that folder.
    pub fn folder(folder: &Path, workdir: Option<&str>) -> PathBuf {
        workdir.map_or_else(|| folder.to_path_buf(), |workdir| folder.join(workdir))
    }
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
            && matches!(stream, "stdout" | "stderr" | "text")
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

/// Files in a bot's outputs folder that hold an outcome's cut streams
/// whole: each stream's name and the file's path. Until the store records
/// them they are the outcome's own and go with it, as a temporary file goes
/// unless it is kept: an outcome dropped before its record, by a stopped
/// turn or a write that failed, removes them, wherever it waited. Copies
/// share them; the last one dropped unrecorded removes them. None, the
/// usual case, allocates nothing.
#[derive(Debug, Default, Clone)]
pub struct Files(Option<std::sync::Arc<Owned>>);
#[derive(Debug)]
struct Owned {
    files: Vec<(&'static str, String)>,
    recorded: std::sync::atomic::AtomicBool,
}
impl Files {
    pub fn new(files: Vec<(&'static str, String)>) -> Self {
        Self((!files.is_empty()).then(|| {
            std::sync::Arc::new(Owned {
                files,
                recorded: Default::default(),
            })
        }))
    }
    /// The store's record names them now: they last as it does.
    pub fn recorded(&self) {
        if let Some(owned) = &self.0 {
            owned
                .recorded
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
impl std::ops::Deref for Files {
    type Target = [(&'static str, String)];
    fn deref(&self) -> &Self::Target {
        self.0.as_deref().map_or(&[], |owned| &owned.files)
    }
}
impl Drop for Owned {
    // Dropped wherever its outcome goes, the runtime thread too, so the
    // removal runs on a blocking worker when there is one.
    fn drop(&mut self) {
        if *self.recorded.get_mut() {
            return;
        }
        let files: Vec<String> = self.files.drain(..).map(|(_, file)| file).collect();
        let remove = move || {
            for file in &files {
                let _ = std::fs::remove_file(file);
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(remove)),
            Err(_) => remove(),
        }
    }
}

/// Model-facing result text plus any full outputs that were truncated.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub output: String,
    pub artifacts: Vec<(&'static str, Vec<u8>)>,
    /// Retained streams written whole to a file in the bot's outputs
    /// folder, in place of a copy in the store: each one's path.
    pub files: Files,
    /// What follows the output, in order: an image file read's picture, or
    /// the pictures, and the words between them, a client answered with.
    pub parts: Vec<crate::codec::Part>,
    /// The call failed: an error, or a command that did not succeed. Set by
    /// whoever ran it, never read back from the output's text.
    pub failed: bool,
}
impl Outcome {
    /// The pictures among its parts.
    pub fn images(&self) -> impl Iterator<Item = &crate::codec::Image> {
        self.parts.iter().filter_map(crate::codec::Part::image)
    }
    /// All its words: the output, then each text part on its own line.
    pub fn words(&self) -> std::borrow::Cow<'_, str> {
        let mut words = std::borrow::Cow::Borrowed(self.output.as_str());
        for part in &self.parts {
            if let crate::codec::Part::Text { text } = part {
                let joined = words.to_mut();
                if !joined.is_empty() {
                    joined.push('\n');
                }
                joined.push_str(text);
            }
        }
        words
    }
    pub fn text(output: String) -> Self {
        Self {
            output,
            artifacts: Vec::new(),
            files: Files::default(),
            parts: Vec::new(),
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
            "echo,shell,read,write,edit,apply_patch,wait,history,web_search,web_fetch,write_stdin"
        } else {
            "echo,read,write,edit,apply_patch,wait,history,web_search,web_fetch"
        })
    }
    pub fn new(names: &str) -> Result<Self> {
        let mut tools = Vec::new();
        for name in names.split(',') {
            let tool = Tool::parse(name).ok_or(Error::new("unsupported_tool_set"))?;
            if tools.contains(&tool) {
                return fail("unsupported_tool_set");
            }
            if matches!(tool, Tool::Shell | Tool::WriteStdin) && !cfg!(unix) {
                return fail("shell_platform_unsupported");
            }
            tools.push(tool);
        }
        Ok(Self {
            tools,
            slots: Arc::new(Semaphore::new(DEFAULT_PROCESS_BUDGET)),
            budget: DEFAULT_PROCESS_BUDGET,
            load: None,
            load_parked: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            load_line: Arc::default(),
            pending: Arc::default(),
            foreground: Arc::default(),
            lasting_slots: Arc::new(Semaphore::new(default_lasting())),
            lasting_budget: default_lasting(),
            #[cfg(unix)]
            terminals: Arc::default(),
            stopping: Arc::new(watch::Sender::new(false)),
            stops: Arc::default(),
            pulls: Arc::new(watch::Sender::new(0)),
            credentials: Credentials::default(),
            environment: Arc::new(Vec::new()),
            daemon: Arc::default(),
            encodings: Arc::default(),
            heavy: Arc::new(Semaphore::new(heavy_bytes())),
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
        grammars: bool,
        names: &[String],
        declared: &[Declared],
        sandboxed: bool,
    ) -> Result<Arc<RawValue>> {
        let mut key = (family, names.join(","));
        if grammars {
            key.1.push_str("\0grammars");
        }
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
                grammar: tool.grammar.clone().map(Into::into),
            }))
            .collect();
        let encoded: Arc<RawValue> = Arc::from(RawValue::from_string(serde_json::to_string(
            &family.tools(&schemas, grammars),
        )?)?);
        let mut encodings = self.encodings.lock().unwrap();
        // Fleets share a few selections; a flood of distinct ones is served
        // without being remembered.
        if encodings.len() < 256 {
            encodings.insert(key, encoded.clone());
        }
        Ok(encoded)
    }
    /// Bound on terminals and detached commands still running, together,
    /// daemon-wide; ended ones stop counting. Zero removes the bound.
    pub fn with_lasting_budget(mut self, budget: usize) -> Self {
        self.lasting_budget = budget.min(Semaphore::MAX_PERMITS);
        // Unbounded, every one still holds a permit, so `stats` counts them.
        self.lasting_slots = Arc::new(Semaphore::new(match self.lasting_budget {
            0 => Semaphore::MAX_PERMITS,
            budget => budget,
        }));
        self
    }
    /// A lasting process's permit, or `lasting_limit` naming what holds
    /// them all. Nothing waits for one: a turn is never left in a line.
    fn lasting(&self) -> Result<Option<tokio::sync::OwnedSemaphorePermit>> {
        self.lasting_slots.clone().try_acquire_owned().map(Some).map_err(|_| {
            let (held, terminals) = (self.lasting_running(), self.terminals_running());
            Error::with(
                "lasting_limit",
                format!(
                    "{held} lasting processes are running, the daemon's bound (--max-lasting {}): {terminals} terminals and {} detached commands; end one first",
                    self.lasting_budget,
                    held.saturating_sub(terminals)
                ),
            )
        })
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
    /// Hold new commands back while the 1-minute load average is at
    /// `limit` or over it and one of the daemon's commands is running, as
    /// make's `-l` holds jobs: never while none is, so a busy machine
    /// slows the fleet and never stops it, and the process budget stays
    /// the bound behind it. One thread reads the load each second for
    /// every waiter, which holds nothing as it waits. Zero is no gate.
    pub fn with_load_gate(mut self, limit: f64) -> Self {
        if !(limit > 0.0 && limit.is_finite()) {
            self.load = None;
            return self;
        }
        let (gate, open) = watch::channel(true);
        let started = std::thread::Builder::new()
            .name("load-gate".into())
            .spawn(move || {
                // Every waiter is woken each second the gate is shut, to
                // look again at whether a command is still running.
                while !gate.is_closed() {
                    std::thread::sleep(Duration::from_secs(1));
                    let below = load_average().is_none_or(|load| load < limit);
                    gate.send_modify(|open| *open = below);
                }
            });
        self.load = started.is_ok().then_some(open);
        self
    }
    /// The load gate, read from `readings` instead of its own thread: the
    /// same gate, for a caller that reads the load itself.
    pub fn with_load_readings(mut self, readings: watch::Receiver<bool>) -> Self {
        self.load = Some(readings);
        self
    }
    /// Terminals and detached commands running now, together: the lasting
    /// slots in use, whatever `--max-lasting` bounds them at.
    pub fn lasting_running(&self) -> usize {
        let bound = match self.lasting_budget {
            0 => Semaphore::MAX_PERMITS,
            budget => budget,
        };
        bound.saturating_sub(self.lasting_slots.available_permits())
    }
    /// Terminals running now, of the lasting slots in use.
    pub fn terminals_running(&self) -> usize {
        #[cfg(unix)]
        return self
            .terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        #[cfg(not(unix))]
        0
    }
    /// Whether the load gate is shut, the load at its limit or over it, as
    /// last read; none without a gate. Shut, it holds new commands back
    /// only while one of the daemon's runs.
    pub fn load_shut(&self) -> Option<bool> {
        self.load.as_ref().map(|open| !*open.borrow())
    }
    /// Commands of the daemon's running now.
    pub fn running(&self) -> usize {
        let bound = match self.budget {
            0 => Semaphore::MAX_PERMITS,
            budget => budget,
        };
        bound.saturating_sub(self.slots.available_permits())
    }
    /// Whether the load gate lets a command start now, beside `holding`
    /// of the daemon's commands that are this one's own slots.
    pub fn admits(&self) -> bool {
        self.admits_beside(0)
    }
    fn admits_beside(&self, holding: usize) -> bool {
        self.load
            .as_ref()
            .is_none_or(|open| *open.borrow() || self.running() <= holding)
    }
    /// The load gate's readings, one a second while it is set: `true`
    /// while the load is under its limit. None is no gate.
    pub fn load_gate(&self) -> Option<watch::Receiver<bool>> {
        self.load.clone()
    }
    /// A turn parked on the load gate: the daemon looks for it at the
    /// gate's next reading that lets one start.
    pub fn note_load_park(&self) {
        self.load_parked
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    /// Whether a turn may have parked on the load gate since the last look,
    /// clearing the mark; a daemon starts with it set, for parks from
    /// before a restart.
    pub fn take_load_parked(&self) -> bool {
        self.load_parked
            .swap(false, std::sync::atomic::Ordering::Relaxed)
    }
    /// Wait until the load gate lets a command start, then take a process
    /// slot, and look again with the slot held: one that waited for its
    /// slot while the gate shut gives it back and waits again. Waiters go
    /// in the order they came, one at a time at the gate. A background
    /// command waits here holding no turn; a turn's foreground command
    /// parks before it gets here instead.
    async fn admitted_slot(
        &self,
    ) -> std::result::Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::AcquireError> {
        let Some(open) = &self.load else {
            return self.slots.clone().acquire_owned().await;
        };
        let mut open = open.clone();
        let _first = self.load_line.lock().await;
        loop {
            while !*open.borrow_and_update() && self.running() > 0 {
                // A gate that stopped reading lets everything through.
                if open.changed().await.is_err() {
                    return self.slots.clone().acquire_owned().await;
                }
            }
            let slot = self.slots.clone().acquire_owned().await?;
            if self.admits_beside(1) {
                return Ok(slot);
            }
        }
    }
    /// How many of one bot's calls may run together: `cap` at most, and
    /// no more than half the process budget, so one bot's wide round
    /// leaves room for the others.
    pub fn wide(&self, cap: usize) -> usize {
        match self.budget {
            0 => cap,
            budget => cap.min(budget / 2).max(1),
        }
    }
    /// Take a place in line for a background command, or refuse when as many
    /// are already waiting as may run: a counter and a comparison, so a fleet
    /// cannot accumulate an invisible backlog behind the process bound.
    pub fn queue(&self) -> Result<Queued> {
        use std::sync::atomic::Ordering::Relaxed;
        // A free slot is taken here, not in the task, so the line counts
        // only commands that truly wait; the semaphore is fair, and none
        // waits at the load gate, so this never overtakes a waiter.
        if let Ok(_line) = self.load_line.try_lock()
            && self.admits()
            && let Ok(permit) = self.slots.clone().try_acquire_owned()
        {
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
    /// Foreground commands running now, each holding a process slot; one
    /// moved to the background has left this count for the store's.
    pub fn foreground_running(&self) -> usize {
        self.foreground.load(std::sync::atomic::Ordering::Relaxed)
    }
    /// A foreground command's place in
    /// [`foreground_running`](Self::foreground_running), handed to
    /// [`foreground`](Self::foreground).
    pub fn place(&self) -> Place {
        Place {
            state: Arc::default(),
            count: self.foreground.clone(),
        }
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
            workdir: Option<String>,
            timeout_ms: Option<u64>,
            #[serde(default)]
            background: bool,
            #[serde(default)]
            detach: bool,
            #[serde(default)]
            tty: bool,
            yield_time_ms: Option<u64>,
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
            handle: Option<String>,
            #[serde(default)]
            offset: u64,
            limit: Option<usize>,
            query: Option<String>,
            before: Option<i64>,
            audit: Option<AuditArgs>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct AuditArgs {
            kind: Option<String>,
            place: Option<String>,
            approved: Option<String>,
            access: Option<String>,
            outcome: Option<String>,
            tool: Option<String>,
            since_ms: Option<i64>,
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
                } else if (args.workdir.as_deref())
                    .is_some_and(|workdir| workdir.len() > 4096 || workdir.contains('\0'))
                {
                    Some("workdir is a path of up to 4096 bytes, with no NUL byte".to_owned())
                } else if let Some(timeout_ms) = args.timeout_ms.filter(|t| {
                    let most = match args.background {
                        true => MAX_BACKGROUND_TIMEOUT_MS,
                        false => MAX_SHELL_TIMEOUT_MS,
                    };
                    !(1..=most).contains(t)
                }) {
                    Some(match args.background {
                        true => format!(
                            "timeout_ms is {timeout_ms}; with background it runs from 1 to {MAX_BACKGROUND_TIMEOUT_MS}"
                        ),
                        false => format!(
                            "timeout_ms is {timeout_ms}; it runs from 1 to {MAX_SHELL_TIMEOUT_MS}, or {MAX_BACKGROUND_TIMEOUT_MS} with background"
                        ),
                    })
                } else if args.detach && args.background {
                    Some("detach and background cannot both be set".to_owned())
                } else if args.tty && (args.detach || args.background || args.timeout_ms.is_some())
                {
                    Some(
                        "a terminal runs until it exits or is killed: tty takes no detach, background or timeout_ms"
                            .to_owned(),
                    )
                } else if args.tty
                    && args.sandbox_permissions.as_deref() == Some("require_escalated")
                {
                    // Input typed into an escaped terminal would run unjudged.
                    Some(
                        "a terminal runs in the sandbox: tty takes no require_escalated".to_owned(),
                    )
                } else if !args.tty && args.yield_time_ms.is_some() {
                    Some("yield_time_ms is for a terminal, with tty".to_owned())
                } else if args.yield_time_ms.is_some_and(|y| y > MAX_YIELD_MS) {
                    Some(format!("yield_time_ms runs from 0 to {MAX_YIELD_MS}"))
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
                // An empty one is the turn's folder, as Codex takes it.
                let workdir = args.workdir.filter(|workdir| !workdir.is_empty());
                if args.detach {
                    return Ok(Prepared::Detach {
                        command: args.command,
                        workdir,
                    });
                }
                if args.tty {
                    return Ok(Prepared::Terminal {
                        command: args.command,
                        yield_ms: args.yield_time_ms.unwrap_or(START_YIELD_MS),
                        workdir,
                    });
                }
                Prepared::Shell {
                    command: args.command,
                    // A background command holds no turn, so only a timeout
                    // its call names ends it, as Codex's unified exec does.
                    timeout_ms: match args.background {
                        true => args.timeout_ms,
                        false => Some(args.timeout_ms.unwrap_or(DEFAULT_SHELL_TIMEOUT_MS)),
                    },
                    background: args.background,
                    workdir,
                }
            }
            Tool::History => {
                let args: HistoryArgs = serde_json::from_str(args).map_err(invalid)?;
                if args
                    .bot
                    .as_ref()
                    .is_some_and(|bot| bot.is_empty() || bot.len() > 128)
                {
                    return fail("invalid_tool_arguments");
                }
                if let Some(audit) = args.audit {
                    if args.turn.is_some()
                        || args.handle.is_some()
                        || args.offset != 0
                        || args.limit.is_some()
                        || args.query.is_some()
                        || args.before.is_some_and(|before| before < 1)
                    {
                        return fail("invalid_tool_arguments");
                    }
                    let query = crate::store::audit::Query {
                        before: args.before,
                        since_ms: audit.since_ms,
                        bot: args.bot,
                        kind: audit.kind,
                        place: audit.place,
                        approved: audit.approved,
                        access: audit.access,
                        outcome: audit.outcome,
                        tool: audit.tool,
                        limit: Some(AUDIT_ROWS),
                        ..Default::default()
                    };
                    query.check()?;
                    return Ok(Prepared::HistoryAudit(query));
                }
                if let Some(query) = args.query {
                    // A search names no turn to page; the store checks the
                    // query's length.
                    if args.turn.is_some()
                        || args.handle.is_some()
                        || args.offset != 0
                        || args.limit.is_some()
                        || args.before.is_some_and(|before| before < 1)
                    {
                        return fail("invalid_tool_arguments");
                    }
                    return Ok(Prepared::HistorySearch {
                        bot: args.bot,
                        query,
                        before: args.before,
                    });
                }
                let limit = args.limit.unwrap_or(64 * 1024);
                // Only another agent's turns are listed: a bot's own are in
                // its context, and the context note numbers those left out.
                if args.turn.is_some_and(|turn| turn < 1)
                    || (args.turn.is_none() && args.bot.is_none() && args.handle.is_none())
                    || (args.handle.is_some() && (args.turn.is_some() || args.bot.is_some()))
                    || args
                        .handle
                        .as_ref()
                        .is_some_and(|handle| handle.len() > 256)
                    || args.before.is_some()
                    || !(4..=64 * 1024).contains(&limit)
                {
                    return fail("invalid_tool_arguments");
                }
                Prepared::History {
                    bot: args.bot,
                    turn: args.turn,
                    handle: args.handle,
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
            Tool::WebFetch => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct WebFetch {
                    url: String,
                }
                let args: WebFetch = serde_json::from_str(args).map_err(invalid)?;
                Prepared::WebFetch(fetch::parse(&args.url)?)
            }
            Tool::WriteStdin => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct WriteStdin {
                    handle: String,
                    #[serde(default)]
                    chars: String,
                    yield_time_ms: Option<u64>,
                    #[serde(default)]
                    kill: bool,
                }
                let args: WriteStdin = serde_json::from_str(args).map_err(invalid)?;
                let handle = args
                    .handle
                    .strip_prefix("proc:")
                    .and_then(|id| id.parse::<i64>().ok())
                    .filter(|id| *id > 0 && args.handle == format!("proc:{id}"));
                let invalid = match handle {
                    None => Some(format!("{:?} is no proc handle", args.handle)),
                    Some(_) if args.chars.len() > INPUT_BYTES => {
                        Some(format!("chars is over {INPUT_BYTES} bytes"))
                    }
                    Some(_) if args.kill && !args.chars.is_empty() => {
                        Some("kill takes no chars".to_owned())
                    }
                    Some(_) if args.yield_time_ms.is_some_and(|y| y > MAX_YIELD_MS) => {
                        Some(format!("yield_time_ms runs from 0 to {MAX_YIELD_MS}"))
                    }
                    Some(_) => None,
                };
                match (invalid, handle) {
                    (None, Some(handle)) => Prepared::WriteStdin {
                        handle,
                        chars: args.chars,
                        yield_ms: args.yield_time_ms,
                        kill: args.kill,
                    },
                    (detail, _) => {
                        return fail_with("invalid_tool_arguments", detail.unwrap_or_default());
                    }
                }
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
            // Untracked: what a runtime starts has a proc handle, from
            // `detached`.
            Prepared::Detach { command, workdir } => {
                let folder = Prepared::folder(workspace, workdir.as_deref());
                let Detached { pid, .. } =
                    detach(&command, &folder, self, environment, sandbox, None).await?;
                Ok(Outcome::text(
                    json!({"detached":true,"pid":pid}).to_string(),
                ))
            }
            Prepared::Wait { .. } => fail("wait_requires_runtime"),
            Prepared::Terminal { .. } | Prepared::WriteStdin { .. } => {
                fail("terminal_requires_runtime")
            }
            Prepared::History { .. }
            | Prepared::HistorySearch { .. }
            | Prepared::HistoryAudit(_) => fail("history_requires_runtime"),
            Prepared::Declared(_) => fail("declared_requires_runtime"),
            Prepared::WebSearch(_) => fail("web_search_requires_runtime"),
            Prepared::WebFetch(url) => {
                let page = fetch::fetch(url).await?;
                self.off_runtime(page.text.len(), move |registry| registry.fetched(page))
                    .await
            }
            Prepared::Shell {
                command,
                timeout_ms,
                background: false,
                workdir,
            } => {
                self.foreground(
                    command,
                    Prepared::folder(workspace, workdir.as_deref()),
                    timeout_ms,
                    environment.to_vec(),
                    sandbox,
                    self.place(),
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
                        parts: vec![crate::codec::Part::Image(image)],
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
    /// A fetched page's result: its text up to the preview, the whole of
    /// it kept as the `text` artifact past that.
    fn fetched(&self, page: fetch::Page) -> Outcome {
        let mut artifacts = Vec::new();
        let mut text = self.redact(page.text);
        // Kept as a shell stream is: at most its first and last `KEPT_END`.
        let mut dropped = 0;
        if text.len() > ARTIFACT_BYTES {
            let head = boundary(&text, KEPT_END);
            let tail = boundary(&text, text.len() - KEPT_END);
            dropped = (tail - head) as u64;
            text = format!(
                "{}\n[... {dropped} bytes dropped: past 1 MiB only the first and last {KEPT_END} bytes of this page are kept ...]\n{}",
                &text[..head],
                &text[tail..]
            );
        }
        let shown = if text.len() <= PREVIEW_BYTES {
            text
        } else {
            let shown = truncate(&text, dropped);
            artifacts.push(("text", text.into_bytes()));
            shown
        };
        let success = (200..300).contains(&page.status);
        let mut output = json!({"url":page.url,"status":page.status,
            "content_type":page.content_type,"text":shown});
        if let Some(title) = page.title {
            output["title"] = json!(self.redact(title));
        }
        if page.cut {
            output["cut"] = json!("the page ends past 4 MiB; only its start was read");
        }
        Outcome {
            output: output.to_string(),
            artifacts,
            files: Files::default(),
            parts: Vec::new(),
            failed: !success,
        }
    }

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
            files: Files::default(),
            parts: Vec::new(),
            failed: !success,
        }
    }

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

    /// A foreground command, owning everything it uses so a running one
    /// can be moved to a task of its own. Dropping it kills the command.
    /// `started` fires once it holds a process slot: only then is it
    /// running, and only then may it move.
    #[allow(clippy::too_many_arguments)]
    pub fn foreground(
        &self,
        command: String,
        workspace: PathBuf,
        timeout_ms: Option<u64>,
        environment: Vec<(String, String)>,
        sandbox: Option<Arc<Policy>>,
        place: Place,
        started: tokio::sync::oneshot::Sender<()>,
    ) -> impl Future<Output = Result<Outcome>> + Send + 'static {
        let registry = self.clone();
        async move {
            let (stdout, stderr, exit, elapsed) = {
                // Held until the command and the sandbox around it are
                // gone, and taken before the slot, so no slot waits on it.
                // Both go before the output is redacted, so a result
                // waiting for that holds no process budget.
                let _held = crate::sandbox::hold_for(sandbox.as_deref()).await;
                let _slot = registry
                    .slots
                    .acquire()
                    .await
                    .map_err(|_| Error::new("tool_scheduler_closed"))?;
                let _place = place.take();
                let _ = started.send(());
                shell(
                    &command,
                    &workspace,
                    timeout_ms.map(Duration::from_millis),
                    &registry,
                    &environment,
                    sandbox.as_deref(),
                )
                .await?
            };
            let bytes = stdout.len() + stderr.len();
            registry
                .off_runtime(bytes, move |registry| {
                    registry.shell_outcome(stdout, stderr, exit, elapsed)
                })
                .await
        }
    }

    /// Start a command whose result is delivered later. The process budget is
    /// acquired inside the task, so starting never blocks the caller; the
    /// result carries the same bounded preview and artifacts as a foreground
    /// command, or the error a foreground command would have returned.
    /// `timeout_ms` bounds the command's running time, and `None` leaves it
    /// running until it exits, is killed or the daemon stops; the place in
    /// line is bounded by `queue`, not by the clock.
    #[allow(clippy::too_many_arguments)]
    pub fn background(
        &self,
        stopper: Stopper,
        command: String,
        workspace: PathBuf,
        timeout_ms: Option<u64>,
        environment: Vec<(String, String)>,
        sandbox: Option<Arc<Policy>>,
        queued: Queued,
        done: tokio::sync::oneshot::Sender<Result<Outcome>>,
    ) {
        let registry = self.clone();
        let mut queued = queued;
        let mut stopping = self.stopping.subscribe();
        let mut pulls = self.pulls.subscribe();
        let Stopper { stop, listed } = stopper;
        tokio::spawn(async move {
            let _listed = listed;
            let result = async {
                let slot = match queued.permit.take() {
                    Some(permit) => Ok(permit),
                    None => registry.admitted_slot().await,
                };
                drop(queued);
                // The slot and the sandbox's hold go once the command is
                // done, before its output is redacted.
                let (stdout, stderr, exit, elapsed) = {
                    let _slot = slot.map_err(|_| Error::new("tool_scheduler_closed"))?;
                    let _held = crate::sandbox::hold_for(sandbox.as_deref()).await;
                    shell(
                        &command,
                        &workspace,
                        timeout_ms.map(Duration::from_millis),
                        &registry,
                        &environment,
                        sandbox.as_deref(),
                    )
                    .await?
                };
                let bytes = stdout.len() + stderr.len();
                registry
                    .off_runtime(bytes, move |registry| {
                        registry.shell_outcome(stdout, stderr, exit, elapsed)
                    })
                    .await
            };
            // Its agent killed it: dropped, which kills its group. A kill
            // that came before it began wins, so it never starts.
            tokio::select! {
                biased;
                Ok(()) = stop => {
                    let _ = done.send(Err(stopped_by_agent()));
                }
                // The cord pulled: dropped the same way, its handle saying
                // why.
                Ok(()) = pulls.changed() => {
                    let _ = done.send(Err(stopped_by_pause()));
                }
                // A command still running as the daemon stops is dropped
                // the same way; its handle then reports the process lost,
                // as the next start would.
                _ = stopping.wait_for(|stopping| *stopping) => {}
                result = result => {
                    let _ = done.send(result);
                }
            }
        });
    }

    /// What stops background command `id` when its agent kills it, from
    /// now until it is dropped, however its run ends: taken before the
    /// command is readied, so a kill while it is finds it.
    pub fn stopper(&self, id: i64) -> Stopper {
        let (sender, stop) = tokio::sync::oneshot::channel();
        self.stops.lock().unwrap().insert(id, sender);
        let listed = Stoppable {
            stops: self.stops.clone(),
            id,
        };
        Stopper { stop, listed }
    }

    /// `run`, a command a steer moved to the background, ended when its
    /// agent kills it by `stopper`'s handle, as one started there is.
    pub fn until_stopped<F>(
        &self,
        stopper: Stopper,
        run: F,
    ) -> impl Future<Output = Result<Outcome>> + use<F>
    where
        F: Future<Output = Result<Outcome>>,
    {
        let Stopper { stop, listed } = stopper;
        async move {
            let _listed = listed;
            // A kill that came before it began wins, so it never starts.
            tokio::select! {
                biased;
                Ok(()) = stop => Err(stopped_by_agent()),
                result = run => result,
            }
        }
    }

    /// Kill background command `id`, dropping it, which kills its group:
    /// false when none of that handle runs. Whose it is, the caller checks.
    pub fn stop(&self, id: i64) -> bool {
        let stop = self.stops.lock().unwrap().remove(&id);
        stop.is_some_and(|stop| stop.send(()).is_ok())
    }

    /// Start detached command `command` in `workspace` as background
    /// command `id`, whose `stopper` stops it: it outlives turns and the
    /// daemon, and its end goes to `done` once nothing it left in its
    /// group runs, if this daemon still does. One that does not start
    /// sends its error there too.
    pub async fn detached(
        &self,
        stopper: Stopper,
        command: &str,
        workspace: &Path,
        environment: &[(String, String)],
        sandbox: Option<Arc<Policy>>,
        done: tokio::sync::oneshot::Sender<Result<Outcome>>,
    ) -> Result<Detached> {
        let tracked = Tracked { stopper, done };
        detach(
            command,
            workspace,
            self,
            environment,
            sandbox,
            Some(tracked),
        )
        .await
    }

    /// Watch again detached command `detached`, which an earlier daemon
    /// started, by its `stopper`, as `detached` watches one it starts: its
    /// lasting slot is taken again when one is free, as it runs either way.
    /// False, sending nothing, when it no longer runs.
    #[cfg(unix)]
    pub async fn adopt(
        &self,
        stopper: Stopper,
        detached: Detached,
        sandboxed: bool,
        elapsed: Duration,
        done: tokio::sync::oneshot::Sender<Result<Outcome>>,
    ) -> bool {
        let runs = tokio::task::spawn_blocking(move || still_runs(detached))
            .await
            .unwrap_or(false);
        if !runs {
            return false;
        }
        let permit = self.lasting().ok().flatten();
        let started = std::time::Instant::now()
            .checked_sub(elapsed)
            .unwrap_or_else(std::time::Instant::now);
        tokio::spawn(watch_detached(
            self.pulls.subscribe(),
            None,
            detached.pid,
            sandboxed,
            started,
            Some(Tracked { stopper, done }),
            Box::new(permit),
        ));
        true
    }
    #[cfg(not(unix))]
    pub async fn adopt(
        &self,
        _: Stopper,
        _: Detached,
        _: bool,
        _: Duration,
        _: tokio::sync::oneshot::Sender<Result<Outcome>>,
    ) -> bool {
        false
    }

    /// `run`, a command a steer moved to the background, ended as the pull
    /// cord is pulled, as one started there is.
    pub fn until_pulled<F>(&self, run: F) -> impl Future<Output = Result<Outcome>> + use<F>
    where
        F: Future<Output = Result<Outcome>>,
    {
        let mut pulls = self.pulls.subscribe();
        async move {
            tokio::select! {
                biased;
                Ok(()) = pulls.changed() => Err(stopped_by_pause()),
                result = run => result,
            }
        }
    }

    /// The pull cord as it is now, for a start decided now that runs
    /// later: one the cord was pulled before does not start.
    pub fn cord(&self) -> Cord {
        Cord(self.pulls.subscribe(), self.stopping.subscribe())
    }

    /// End every background command, terminal and detached command this
    /// daemon started now, as the pull cord stops the line.
    pub fn end_running(&self) {
        self.pulls.send_modify(|pulls| *pulls += 1);
        self.end_terminals();
    }

    /// End every background command still running, as the daemon stops.
    pub fn end_background(&self) {
        self.stopping.send_replace(true);
    }
}

/// How a background command its agent killed answers.
fn stopped_by_agent() -> Error {
    Error::with(
        "stopped",
        "its agent killed this command before it finished, so it may have had effects",
    )
}

/// How a command its agent killed before it started answers.
fn stopped_before_start() -> Error {
    Error::with(
        "stopped",
        "its agent killed this command before it started, so it did not run",
    )
}

/// What lets its agent kill a background command by its handle.
pub struct Stopper {
    stop: tokio::sync::oneshot::Receiver<()>,
    listed: Stoppable,
}

impl Stopper {
    /// Refuse a start its agent killed before.
    pub fn check(&mut self) -> Result<()> {
        match self.stop.try_recv() {
            Ok(()) => Err(stopped_before_start()),
            Err(_) => Ok(()),
        }
    }

    /// Give up the place once the command can be killed another way:
    /// whether a kill came first, which the caller then carries out. A
    /// kill after finds the other way, or no place and says so.
    pub fn hand_over(self) -> bool {
        let Stopper { mut stop, listed } = self;
        drop(listed);
        stop.close();
        stop.try_recv().is_ok()
    }
}

/// A background command's place among those its agent can kill, given
/// up when dropped: a run a pause or the daemon's stop drops leaves none
/// behind.
struct Stoppable {
    stops: Arc<std::sync::Mutex<HashMap<i64, tokio::sync::oneshot::Sender<()>>>>,
    id: i64,
}

impl Drop for Stoppable {
    fn drop(&mut self) {
        self.stops.lock().unwrap().remove(&self.id);
    }
}

/// The pull cord as it was when a start was decided, and whether the
/// daemon is stopping.
#[derive(Clone)]
pub struct Cord(watch::Receiver<u64>, watch::Receiver<bool>);

impl Cord {
    /// Whether the cord was pulled since.
    pub fn pulled(&self) -> bool {
        self.0.has_changed().unwrap_or(true)
    }

    /// Refuse a start the cord was pulled before, or the daemon's stop
    /// came before.
    pub fn check(&self) -> Result<()> {
        if self.pulled() {
            return Err(Error::with(
                "daemon_paused",
                "the pause came before this command started, so it did not run; nothing has run it since. Work has since resumed",
            ));
        }
        match *self.1.borrow() {
            true => Err(not_started_by_shutdown()),
            false => Ok(()),
        }
    }

    /// `run`, not begun when the cord was pulled or the daemon's stop came
    /// before, and ended as either comes, as a background command is.
    pub async fn until_pulled<F>(mut self, run: F) -> Result<Outcome>
    where
        F: Future<Output = Result<Outcome>>,
    {
        self.check()?;
        tokio::select! {
            biased;
            Ok(()) = self.0.changed() => Err(stopped_by_pause()),
            Ok(_) = self.1.wait_for(|stopping| *stopping) => Err(stopped_by_shutdown()),
            result = run => result,
        }
    }
}

/// How a command the daemon's stop came before answers.
pub(crate) fn not_started_by_shutdown() -> Error {
    Error::with(
        "daemon_shutdown",
        "the daemon was stopping before this command started, so it did not run",
    )
}

/// How a command the daemon's stop ended answers.
fn stopped_by_shutdown() -> Error {
    Error::with(
        "daemon_shutdown",
        "the daemon stopped this command before it finished, so it may have had effects; nothing has run it again",
    )
}

/// How a command the pull cord ended answers. Nothing reruns it; the agent
/// reads this only once work has resumed, so it says so.
fn stopped_by_pause() -> Error {
    Error::with(
        "daemon_paused",
        "the pause stopped this command before it finished, so it may have had effects; nothing has run it again. Work has since resumed",
    )
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

/// The machine's 1-minute load average, as uptime reports it.
fn load_average() -> Option<f64> {
    let mut load = [0.0f64; 1];
    // SAFETY: getloadavg writes at most the one sample asked for.
    let read = unsafe { libc::getloadavg(load.as_mut_ptr(), 1) };
    (read == 1).then_some(load[0])
}

/// Output up to this many bytes is redacted on the runtime's thread: about
/// half a millisecond with the default fields and blocks and a few
/// credentials. Past it the scans take milliseconds (8 ms a MiB), so they
/// run on a blocking thread.
const INLINE_BYTES: usize = 64 * 1024;

/// Bytes of output redacted on blocking threads at once: a few of the
/// largest results for each logical CPU, so a burst of large outputs waits
/// its turn rather than taking every blocking thread.
fn heavy_bytes() -> usize {
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    (cpus * 2 * (ARTIFACT_BYTES + PREVIEW_BYTES)).min(u32::MAX as usize)
}

impl Registry {
    /// Run `work` over `bytes` of output: here when they are few, or else on
    /// a blocking thread once the bytes being worked on there leave room,
    /// as Tokio's `spawn_blocking` is meant for CPU-bound work.
    async fn off_runtime<T: Send + 'static>(
        &self,
        bytes: usize,
        work: impl FnOnce(&Registry) -> T + Send + 'static,
    ) -> Result<T> {
        if bytes <= INLINE_BYTES {
            return Ok(work(self));
        }
        let wanted = bytes.min(heavy_bytes()) as u32;
        let permit = self
            .heavy
            .clone()
            .acquire_many_owned(wanted)
            .await
            .map_err(|_| Error::new("tool_scheduler_closed"))?;
        let registry = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work(&registry)
        })
        .await
        .map_err(|_| Error::new("tool_redaction_failed"))
    }
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
    fn len(&self) -> usize {
        self.head.len() + self.tail.len()
    }
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

/// bash, as other harnesses run commands, where it is installed; else sh.
#[cfg(unix)]
fn shell_path() -> &'static str {
    static PATH: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    PATH.get_or_init(|| match Path::new("/bin/bash").exists() {
        true => "/bin/bash",
        false => "/bin/sh",
    })
}

/// What a bot's command's environment changes from the daemon's own: the
/// names taken out (the credentials the daemon holds and withholds), then
/// what is set, the later winning: `environment`, the caller's and the
/// bot's, last.
pub fn shell_environment<'a>(
    registry: &'a Registry,
    environment: &'a [(String, String)],
) -> (Vec<String>, impl Iterator<Item = (&'a str, &'a str)>) {
    let mut removed = registry.credentials.names();
    // A set AGENT_PARENT names this bot's creator and nothing else; a root
    // bot must not pass on one the daemon itself was started with. Likewise
    // a set AGENT_EFFORT is this bot's own level.
    removed.extend(["AGENT_PARENT", "AGENT_PARENT_ID", "AGENT_EFFORT"].map(String::from));
    // No command has a terminal: plain output and no pager, as other
    // harnesses run commands. AI_AGENT tells tools an agent runs them, as
    // fx sets it. What the caller or bot sets still wins.
    let set = [
        ("AI_AGENT", "agent"),
        ("TERM", "dumb"),
        ("NO_COLOR", "1"),
        ("PAGER", "cat"),
        ("GIT_PAGER", "cat"),
        ("GH_PAGER", "cat"),
    ]
    .into_iter()
    .chain(
        (registry.environment.iter().chain(environment))
            .map(|(name, value)| (name.as_str(), value.as_str())),
    );
    (removed, set)
}

/// A bot's command's whole environment: the daemon's own as
/// [`shell_environment`] changes it, for a command run as the bot outside
/// a turn (`lend`). A variable that is not UTF-8 is left out.
pub fn shell_environment_whole(
    registry: &Registry,
    environment: &[(String, String)],
) -> std::collections::BTreeMap<String, String> {
    let (removed, set) = shell_environment(registry, environment);
    let mut whole: std::collections::BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| !removed.contains(name))
        .collect();
    whole.extend(set.map(|(name, value)| (name.to_owned(), value.to_owned())));
    whole
}

/// What a command that could not start in `folder` says: that the folder
/// is not there, when it is not, as a missing `workdir` leaves it, or the
/// system's own words. Looked at only once a start failed.
#[cfg(unix)]
pub(crate) fn not_started(error: std::io::Error, folder: &Path) -> Error {
    if error.kind() == std::io::ErrorKind::NotFound && !folder.is_dir() {
        return Error::with(
            "path_not_found",
            format!("{} is not a folder to run the command in", folder.display()),
        );
    }
    error.into()
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
    let (removed, set) = shell_environment(registry, environment);
    for name in removed {
        process.env_remove(name);
    }
    for (name, value) in set {
        process.env(name, value);
    }
    // A sandboxed command reaches the daemon through its bot's own socket.
    if let Some(own) = sandbox.and_then(Policy::reach) {
        process.env("AGENT_SOCKET", own);
    }
    process.current_dir(workspace);
    Ok(process)
}

/// A detached command once started: its pid, which leads its session and
/// process group, and what tells that leader from a later process given
/// the same pid, recorded with its handle so a daemon started later can
/// still find it and stop it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Detached {
    pub pid: u32,
    /// When the leader started, as the kernel counts it; none where it
    /// could not be read.
    pub since: Option<u64>,
}

/// What a tracked detached command reports its end to, and what stops it
/// on its handle.
pub struct Tracked {
    pub stopper: Stopper,
    pub done: tokio::sync::oneshot::Sender<Result<Outcome>>,
}

/// Start a command in a new session for a service that must outlive the
/// turn and the daemon. Its output is discarded: a log the daemon wrote would
/// bypass credential redaction, so a command that wants one redirects itself.
/// No process slot applies to it; a lasting one does. Tracked, it is a proc
/// handle its agent or a person stops, and its end goes to `done` once
/// nothing it left in its group runs; the pull cord kills it either way. An
/// async waiter reaps it on exit; its lasting slot is released once nothing
/// it left in its group runs either.
#[cfg(unix)]
async fn detach(
    command: &str,
    workspace: &Path,
    registry: &Registry,
    environment: &[(String, String)],
    sandbox: Option<Arc<Policy>>,
    tracked: Option<Tracked>,
) -> Result<Detached> {
    let spawned = spawn_detached(command, workspace, registry, environment, sandbox).await;
    let (child, detached, sandboxed, started, held) = match spawned {
        Ok(spawned) => spawned,
        // A command that never started ends here, as its own call says.
        Err(error) => {
            if let Some(tracked) = tracked {
                let _ = tracked.done.send(Err(error.clone()));
            }
            return Err(error);
        }
    };
    tokio::spawn(watch_detached(
        registry.pulls.subscribe(),
        Some(child),
        detached.pid,
        sandboxed,
        started,
        tracked,
        held,
    ));
    Ok(detached)
}

/// A detached command started: its child, what finds it, whether a
/// sandbox holds it, when it started, and what it holds while it runs.
#[cfg(unix)]
type Spawned = (
    tokio::process::Child,
    Detached,
    bool,
    std::time::Instant,
    Box<dyn Send>,
);

#[cfg(unix)]
async fn spawn_detached(
    command: &str,
    workspace: &Path,
    registry: &Registry,
    environment: &[(String, String)],
    sandbox: Option<Arc<Policy>>,
) -> Result<Spawned> {
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
    let permit = registry.lasting()?;
    // What the sandbox holds in place stays held while the command runs.
    let sandboxed = sandbox.is_some();
    let held = crate::sandbox::hold_for(sandbox.as_deref()).await;
    // The bot's socket listens while anything the command left in its
    // group runs, not only while the command itself does.
    let listening = sandbox.as_ref().and_then(|policy| policy.listening.clone());
    let started = std::time::Instant::now();
    let child = process.spawn().map_err(|e| not_started(e, workspace))?;
    let pid = child.id().ok_or(Error::new("shell_spawn_failed"))?;
    // Read off the runtime thread, as every file is.
    let since = tokio::task::spawn_blocking(move || leader_since(pid))
        .await
        .ok()
        .flatten();
    let held: Box<dyn Send> = Box::new((permit, held, listening));
    Ok((child, Detached { pid, since }, sandboxed, started, held))
}

/// Watch detached command `pid` until nothing it left in its group runs:
/// its own `child` when this daemon started it, or one an earlier daemon
/// did, found again. A pull of the cord or a stop on its handle kills the
/// group, and with `sandboxed` the pid namespace it runs in; `held` (its
/// lasting slot, what its sandbox keeps in place) goes with its end, which
/// goes to `tracked`'s `done`.
#[cfg(unix)]
async fn watch_detached(
    mut pulls: watch::Receiver<u64>,
    child: Option<tokio::process::Child>,
    pid: u32,
    sandboxed: bool,
    started: std::time::Instant,
    tracked: Option<Tracked>,
    held: Box<dyn Send>,
) {
    let group = pid as i32;
    let (mut stop, listed, done) = match tracked {
        Some(Tracked { stopper, done }) => {
            let Stopper { stop, listed } = stopper;
            (Some(stop), Some(listed), Some(done))
        }
        None => (None, None, None),
    };
    let end = async || {
        if sandboxed {
            end_namespace(pid).await;
        }
        // SAFETY: kill with a negative pid signals the process group setsid
        // made this command lead; it touches no memory.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    };
    // How it ended, once it did: the leader's own status, when this daemon
    // is its parent, and what ended it otherwise.
    let mut ended: Option<Result<Option<std::process::ExitStatus>>> = None;
    if let Some(mut child) = child {
        ended = tokio::select! {
            // Its end is the group's, below; the leader's status is what
            // it says of how it went.
            status = child.wait() => Some(Ok(status.ok())),
            Ok(()) = pulls.changed() => {
                end().await;
                let _ = child.wait().await;
                Some(Err(stopped_by_pause()))
            }
            () = stopped(&mut stop) => {
                end().await;
                let _ = child.wait().await;
                Some(Err(stopped_by_agent()))
            }
        };
    }
    // What it left running in its group, as `cmd &` leaves, is still the
    // pull's or its handle's to end, and still holds the command's place:
    // looked for each second until the group is empty, so a launcher that
    // starts a service and exits does not hand its slot to the next while
    // the service runs. The kernel keeps a group's id from new processes
    // while any member lives, so only that last second could see it reused.
    let members = move || async move {
        tokio::task::spawn_blocking(move || group_members(group))
            .await
            .unwrap_or(Members::Gone)
    };
    // Where only exited members answer, a walk of /proc is no one moment
    // and can miss a member forked while it read: three walks a second
    // apart must agree that nothing runs.
    let mut exited = 0;
    while !matches!(ended, Some(Err(_))) && exited < 3 {
        match members().await {
            Members::Gone => break,
            Members::Running => exited = 0,
            Members::Exited => exited += 1,
        }
        if exited == 3 {
            break;
        }
        tokio::select! {
            Ok(()) = pulls.changed() => {
                end().await;
                ended = Some(Err(stopped_by_pause()));
            }
            () = stopped(&mut stop) => {
                end().await;
                ended = Some(Err(stopped_by_agent()));
            }
            () = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }
    }
    drop((held, listed));
    let Some(done) = done else { return };
    let wall_time_ms = started.elapsed().as_millis() as u64;
    let _ = done.send(match ended {
        Some(Err(error)) => Err(error),
        Some(Ok(Some(status))) => Ok(Outcome {
            failed: !status.success(),
            ..Outcome::text(
                json!({"exit_code":status.code(),"success":status.success(),
                    "wall_time_ms":wall_time_ms})
                .to_string(),
            )
        }),
        // An earlier daemon was its parent: how its leader exited went
        // with it.
        _ => Ok(Outcome::text(
            json!({"exit_code":null,"wall_time_ms":wall_time_ms}).to_string(),
        )),
    });
}

/// Until a stop comes on a detached command's handle; never without one,
/// or once what would send it is gone.
async fn stopped(stop: &mut Option<tokio::sync::oneshot::Receiver<()>>) {
    if let Some(receiver) = stop {
        let got = receiver.await;
        // A finished receiver is never polled again.
        *stop = None;
        if got.is_ok() {
            return;
        }
    }
    std::future::pending().await
}

/// When process `pid` started, as the kernel counts it: what tells a
/// detached command's leader from a later process given its pid.
#[cfg(target_os = "linux")]
fn leader_since(pid: u32) -> Option<u64> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    let after = &stat[stat.iter().rposition(|&b| b == b')')? + 1..];
    // `starttime` is the stat file's 22nd field, the 20th after comm.
    let field = after
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
        .nth(19)?;
    std::str::from_utf8(field).ok()?.parse().ok()
}
#[cfg(target_os = "macos")]
fn leader_since(pid: u32) -> Option<u64> {
    // SAFETY: a zeroed proc_bsdinfo is valid; proc_pidinfo fills it.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a proc_bsdinfo of `size` bytes this function owns.
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (read == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn leader_since(_: u32) -> Option<u64> {
    None
}

/// Whether detached command `detached`, which an earlier daemon started,
/// still runs: its group has a member, and its leader, when there, is the
/// process that started then and not a later one given its pid. A leader
/// gone with its group still running is the group it led: the kernel
/// gives no new process a group id while a member holds it.
#[cfg(unix)]
fn still_runs(detached: Detached) -> bool {
    let group = detached.pid as i32;
    if matches!(group_members(group), Members::Gone) {
        return false;
    }
    // SAFETY: kill with signal 0 only checks the process.
    let leader = unsafe { libc::kill(group, 0) } == 0;
    !leader
        || detached.since.is_none()
        || leader_since(detached.pid).is_none_or(|since| Some(since) == detached.since)
}

/// What is left of a detached command's process group.
#[cfg(unix)]
enum Members {
    /// Nothing answers a signal.
    Gone,
    Running,
    /// Only members that have exited were seen, though something answers.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Exited,
}

/// What is left of a process group. A member that has exited still answers
/// a signal until it is reaped, and an init that does not reap orphans, as
/// some containers run, leaves it so for good: on Linux only a member that
/// is not a zombie runs, and a process whose main thread has exited is a
/// zombie only once its other threads have too. Elsewhere init reaps orphans.
#[cfg(target_os = "linux")]
fn group_members(group: i32) -> Members {
    // SAFETY: kill with a negative pid and signal 0 only checks the group.
    if unsafe { libc::kill(-group, 0) } != 0 {
        return Members::Gone;
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Members::Running;
    };
    let running = entries.flatten().any(|entry| {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        else {
            return false;
        };
        let Some((state, pgrp)) = stat(&entry.path().join("stat")) else {
            return false;
        };
        pgrp == Some(group) && (!exited(state) || threads_run(pid))
    });
    if running {
        Members::Running
    } else {
        Members::Exited
    }
}
/// The state and process group in a `/proc` stat file, `pid (comm) state
/// ppid pgrp ...`. comm is the process's name, any bytes up to 15 of them:
/// spaces, parens, and UTF-8 the kernel cut short included.
#[cfg(target_os = "linux")]
fn stat(path: &Path) -> Option<(u8, Option<i32>)> {
    let stat = std::fs::read(path).ok()?;
    let after = &stat[stat.iter().rposition(|&b| b == b')')? + 1..];
    let mut fields = after
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty());
    let state = *fields.next()?.first()?;
    let pgrp = (fields.nth(1)).and_then(|pgrp| std::str::from_utf8(pgrp).ok()?.parse().ok());
    Some((state, pgrp))
}
#[cfg(target_os = "linux")]
fn exited(state: u8) -> bool {
    matches!(state, b'Z' | b'X')
}
/// Whether any thread of a process whose main thread is a zombie still runs.
#[cfg(target_os = "linux")]
fn threads_run(pid: &str) -> bool {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return false;
    };
    tasks
        .flatten()
        .any(|task| stat(&task.path().join("stat")).is_some_and(|(state, _)| !exited(state)))
}
#[cfg(all(unix, not(target_os = "linux")))]
fn group_members(group: i32) -> Members {
    // SAFETY: kill with a negative pid and signal 0 only checks the group.
    if unsafe { libc::kill(-group, 0) == 0 } {
        Members::Running
    } else {
        Members::Gone
    }
}

/// End the pid namespace a sandboxed detached command runs in. bubblewrap,
/// the process `pid`, starts the namespace's init as its child in a session
/// of its own and, for a detached command, does not take it down when it
/// dies; killing that init ends every process in the namespace.
#[cfg(target_os = "linux")]
async fn end_namespace(pid: u32) {
    // Read off the runtime thread, as every file is.
    let children = tokio::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).await;
    for init in
        (children.unwrap_or_default().split_whitespace()).filter_map(|c| c.parse::<i32>().ok())
    {
        // SAFETY: kill only signals the process; it touches no memory.
        unsafe { libc::kill(init, libc::SIGKILL) };
    }
}
/// sandbox-exec runs the command in its own place: its group is the command's.
#[cfg(all(unix, not(target_os = "linux")))]
async fn end_namespace(_: u32) {}

#[cfg(not(unix))]
async fn detach(
    _: &str,
    _: &Path,
    _: &Registry,
    _: &[(String, String)],
    _: Option<Arc<Policy>>,
    _: Option<Tracked>,
) -> Result<Detached> {
    fail("shell_platform_unsupported")
}

/// How long a command's pipes are read on for their end once its group is
/// killed. Everything the group wrote is read first; only a process that
/// left the group can still hold a pipe open, and it is not waited for.
#[cfg(unix)]
const PIPES_END: Duration = Duration::from_millis(100);

#[cfg(unix)]
async fn shell(
    command: &str,
    workspace: &Path,
    timeout: Option<Duration>,
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
        .spawn()
        .map_err(|e| not_started(e, workspace))?;
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
    let status = {
        let reads = async {
            tokio::try_join!(
                capture(&mut stdout, &mut out),
                capture(&mut stderr, &mut err)
            )
        };
        tokio::pin!(reads);
        // No timeout arms no timer: the command's own end is all it waits on.
        let deadline = async {
            match timeout {
                Some(timeout) => tokio::time::sleep(timeout).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(deadline);
        // The command's own return ends it, not its pipes' end: a job it
        // left running holds them open for as long as it runs, and goes
        // with the group below.
        let mut read = false;
        let status = loop {
            tokio::select! {
                done = &mut reads, if !read => {
                    done?;
                    read = true;
                }
                status = owned.child.wait() => break Some(status?),
                () = &mut deadline => break None,
            }
        };
        // Finish cleanup before returning. Drop also covers cancellation,
        // with Tokio responsible for reaping its killed child.
        // SAFETY: kill takes no pointers; the group is this command's own.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
        let _ = owned.child.wait().await;
        owned.group = 0;
        // What the group wrote before it ended is still in the pipes, and
        // is read whatever the wait: the bound is on their end alone, which
        // a process that left the group may hold off for as long as it runs.
        if !read && let Ok(done) = tokio::time::timeout(PIPES_END, &mut reads).await {
            done?;
        }
        status
    };
    let exit = status.map_or(Exit::TimedOut, Exit::Status);
    Ok((out, err, exit, started.elapsed()))
}

#[cfg(not(unix))]
async fn shell(
    _: &str,
    _: &Path,
    _: Option<Duration>,
    _: &Registry,
    _: &[(String, String)],
    _: Option<&Policy>,
) -> Result<(Captured, Captured, Exit, Duration)> {
    fail("shell_platform_unsupported")
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn a_start_decided_before_a_pull_never_begins_and_one_running_ends() {
        let registry = Registry::new("shell").unwrap();
        let before = registry.cord();
        assert!(before.check().is_ok());
        registry.end_running();
        assert_eq!(before.check().unwrap_err().code, "daemon_paused");
        let begun = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = begun.clone();
        let refused = before
            .until_pulled(async move {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(Outcome::text("ran".into()))
            })
            .await;
        assert_eq!(refused.unwrap_err().code, "daemon_paused");
        assert!(!begun.load(std::sync::atomic::Ordering::SeqCst));
        // One decided after the pull runs, and a later pull ends it.
        let after = registry.cord();
        let running = tokio::spawn(after.until_pulled(std::future::pending()));
        tokio::task::yield_now().await;
        registry.end_running();
        let ended = running.await.unwrap().unwrap_err();
        assert_eq!(ended.code, "daemon_paused");
    }
    #[tokio::test]
    async fn the_daemons_stop_refuses_a_start_and_ends_a_run() {
        let registry = Registry::new("shell").unwrap();
        let running = tokio::spawn(registry.cord().until_pulled(std::future::pending()));
        tokio::task::yield_now().await;
        registry.end_background();
        let ended = running.await.unwrap().unwrap_err();
        assert_eq!(ended.code, "daemon_shutdown");
        assert!(
            ended
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("may have had effects")
        );
        let refused = registry.cord().check().unwrap_err();
        assert_eq!(refused.code, "daemon_shutdown");
        assert!(
            refused
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("did not run")
        );
    }
    #[tokio::test]
    async fn a_run_killed_before_it_is_first_polled_never_begins() {
        let registry = Registry::new("shell").unwrap();
        let stopper = registry.stopper(11);
        assert!(registry.stop(11));
        let began = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let run = {
            let began = began.clone();
            async move {
                began.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(Outcome::text("ran".into()))
            }
        };
        let ended = registry.until_stopped(stopper, run).await.unwrap_err();
        assert_eq!(ended.code, "stopped");
        assert!(!began.load(std::sync::atomic::Ordering::SeqCst));
    }
    #[tokio::test]
    async fn a_killable_run_a_pull_drops_leaves_nothing_to_kill() {
        let registry = Registry::new("shell").unwrap();
        let running = tokio::spawn(
            registry
                .cord()
                .until_pulled(registry.until_stopped(registry.stopper(7), std::future::pending())),
        );
        tokio::task::yield_now().await;
        assert!(registry.stops.lock().unwrap().contains_key(&7));
        registry.end_running();
        let ended = running.await.unwrap().unwrap_err();
        assert_eq!(ended.code, "daemon_paused");
        assert!(registry.stops.lock().unwrap().is_empty());
        assert!(!registry.stop(7));
    }
    #[test]
    fn an_answered_call_past_what_its_client_is_sent_is_refused() {
        let tool: Declared = serde_json::from_value(serde_json::json!({
            "name": "ask", "description": "", "answered_by": "window",
            "parameters": {"type": "object", "properties": {"text": {"type": "string"}}}
        }))
        .unwrap();
        let call = |n: usize| format!(r#"{{"text":"{}"}}"#, "x".repeat(n));
        assert!(check_declared(&tool, &call(ANSWERED_ARGUMENTS - 11)).is_ok());
        let refused = check_declared(&tool, &call(ANSWERED_ARGUMENTS - 10)).unwrap_err();
        assert_eq!(refused.code, "invalid_arguments");
        // A tool no client answers takes it: nothing sends it whole.
        let unanswered = Declared {
            answered_by: None,
            ..tool
        };
        assert!(check_declared(&unanswered, &call(ANSWERED_ARGUMENTS)).is_ok());
    }

    use super::*;
    #[test]
    fn a_name_matches_a_pattern_s_wildcards_in_any_case() {
        assert!(name_matches("*KEY*", "OPENAI_API_KEY"));
        assert!(name_matches("*KEY*", "ssh_key_path"));
        assert!(name_matches("*TOKEN*", "GITHUB_TOKEN"));
        assert!(name_matches("AWS_*", "AWS_PROFILE"));
        assert!(name_matches("A?S_*", "AWS_"));
        assert!(name_matches("*_*_KEY", "OPENAI_API_KEY"));
        assert!(name_matches("*", ""));
        assert!(!name_matches("*KEY*", "PATH"));
        assert!(!name_matches("AWS_*", "MY_AWS_PROFILE"));
        assert!(!name_matches("A?S", "AS"));
        assert!(!name_matches("*_KEY", "OPENAI_API_KEYS"));
    }
    #[tokio::test]
    async fn a_shut_load_gate_holds_new_commands_only_while_one_runs() {
        let (gate, open) = watch::channel(false);
        let mut registry = Registry::new("shell").unwrap().with_process_budget(2);
        registry.load = Some(open);
        // With none running, a command starts however loaded the machine.
        assert!(registry.admits());
        let first = registry.admitted_slot().await.unwrap();
        assert!(!registry.admits());
        // Nor is a background command's slot taken for it at once.
        assert!(registry.queue().unwrap().permit.is_none());
        let waiting = tokio::spawn({
            let registry = registry.clone();
            async move { registry.admitted_slot().await.unwrap() }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        // The running one ends. A command that comes before the next
        // look waits behind the one at the gate, as a line does: then the
        // first goes.
        drop(first);
        assert!(registry.admits());
        assert!(registry.queue().unwrap().permit.is_none());
        gate.send_modify(|_| {});
        let second = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();
        // One that took its slot as the gate shut, with another running,
        // gives it back and waits: the budget of two is full, so this one
        // waits for the slot while the gate is open, and the gate shuts
        // before the slot frees.
        gate.send_replace(true);
        let third = registry.admitted_slot().await.unwrap();
        let late = tokio::spawn({
            let registry = registry.clone();
            async move { registry.admitted_slot().await.unwrap() }
        });
        tokio::task::yield_now().await;
        gate.send_replace(false);
        drop(third);
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(!late.is_finished());
        assert_eq!(registry.running(), 1);
        // The load falls: it goes, alongside what runs.
        gate.send_replace(true);
        let _late = tokio::time::timeout(Duration::from_secs(5), late)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(registry.running(), 2);
        drop(second);
    }
    #[tokio::test]
    async fn a_foreground_command_leaves_the_running_count_once() {
        let dir = std::env::temp_dir().join(format!("agent-place-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Registry::new("shell").unwrap().with_process_budget(1);
        let start = |command: &str, place: Place| {
            let (started, running) = tokio::sync::oneshot::channel();
            let run = tokio::spawn(registry.foreground(
                command.into(),
                dir.clone(),
                Some(60_000),
                Vec::new(),
                None,
                place,
                started,
            ));
            (run, running)
        };
        // Output past what is redacted inline keeps the result in hand
        // after the process ends.
        let lifted = registry.place();
        let (first, running) = start(
            "sleep 0.5; head -c 200000 /dev/zero | tr '\\0' a",
            lifted.clone(),
        );
        running.await.unwrap();
        assert_eq!(registry.foreground_running(), 1);
        lifted.lift();
        assert_eq!(registry.foreground_running(), 0);
        // One waiting for the only slot, lifted before it starts, is never
        // counted, even once it runs.
        let early = registry.place();
        let (second, waiting) = start("sleep 0.5", early.clone());
        early.lift();
        assert!(first.await.unwrap().is_ok());
        waiting.await.unwrap();
        assert_eq!(registry.foreground_running(), 0);
        assert!(second.await.unwrap().is_ok());
        // One that ends where it began gives its place back once, and
        // a lift after its end changes nothing.
        let plain = registry.place();
        let (third, running) = start("sleep 0.2", plain.clone());
        running.await.unwrap();
        assert_eq!(registry.foreground_running(), 1);
        assert!(third.await.unwrap().is_ok());
        assert_eq!(registry.foreground_running(), 0);
        plain.lift();
        assert_eq!(registry.foreground_running(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn a_load_gate_of_zero_is_none() {
        let registry = Registry::new("shell").unwrap().with_load_gate(0.0);
        assert!(registry.load.is_none());
        assert!(load_average().is_some_and(|load| load >= 0.0));
    }
    #[test]
    fn one_bot_runs_at_most_half_the_process_budget_at_once() {
        let wide = |budget| {
            Registry::new("shell")
                .unwrap()
                .with_process_budget(budget)
                .wide(4)
        };
        assert_eq!(
            [wide(0), wide(1), wide(2), wide(4), wide(64)],
            [4, 1, 1, 2, 4]
        );
    }
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
        let [crate::codec::Part::Image(image)] = &outcome.parts[..] else {
            panic!("one picture: {:?}", outcome.parts);
        };
        assert_eq!(image.media_type, "image/png");
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
    fn the_fields_and_blocks_a_client_names_are_redacted() {
        let credentials = Credentials::default();
        let unnamed = "password: fake8fake8fake8\n-----BEGIN RSA PRIVATE KEY-----\nMIIE";
        assert_eq!(credentials.redact(unnamed.to_owned()), unnamed);
        let fields = ["secret_access_key", "SessionToken", "api-key", "password"];
        credentials.redact_also(&fields.map(String::from), &["PRIVATE KEY".to_owned()]);
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
    /// A background command given no timeout_ms runs until it exits, as
    /// Codex's unified exec runs one: no deadline is armed for it, so a
    /// paused clock, which skips straight to the next deadline whenever the
    /// runtime waits, still sees it finish. One whose call names a
    /// timeout_ms, even a day, is killed at it and says timed_out, and a
    /// foreground command keeps its 120 s default.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_background_command_without_a_timeout_runs_until_it_exits() {
        let registry = Registry::new("shell").unwrap();
        let run = |arguments: serde_json::Value, id: i64| {
            let Ok(Prepared::Shell {
                command,
                timeout_ms,
                background: true,
                ..
            }) = registry.prepare("shell", &arguments.to_string())
            else {
                panic!("{arguments} is not a background command");
            };
            let (sender, receiver) = tokio::sync::oneshot::channel();
            registry.background(
                registry.stopper(id),
                command,
                std::env::temp_dir(),
                timeout_ms,
                Vec::new(),
                None,
                registry.queue().unwrap(),
                sender,
            );
            async move {
                let outcome = receiver.await.unwrap().unwrap();
                serde_json::from_str::<serde_json::Value>(&outcome.output).unwrap()
            }
        };
        let command = "sleep 0.2; echo done";
        let ran = run(json!({"command":command,"background":true}), 1).await;
        assert_eq!(ran["stdout"], "done\n", "{ran}");
        assert_eq!(ran["exit_code"], 0);
        assert!(ran.get("timed_out").is_none(), "{ran}");
        let bounded =
            json!({"command":command,"background":true,"timeout_ms":MAX_BACKGROUND_TIMEOUT_MS});
        let cut = run(bounded, 2).await;
        assert_eq!(cut["timed_out"], true, "{cut}");
        assert_eq!(cut["success"], false);
        assert!(cut["exit_code"].is_null());
        assert!(matches!(
            registry.prepare("shell", r#"{"command":"true"}"#),
            Ok(Prepared::Shell {
                timeout_ms: Some(DEFAULT_SHELL_TIMEOUT_MS),
                background: false,
                ..
            })
        ));
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
            "timeout_ms is 900000; it runs from 1 to 600000, or 86400000 with background"
        );
        // A background command may run as long as a wait on it may last.
        assert!(
            shell
                .prepare(
                    "shell",
                    r#"{"command":"true","timeout_ms":3600000,"background":true}"#
                )
                .is_ok()
        );
        assert_eq!(
            detail(r#"{"command":"true","timeout_ms":86400001,"background":true}"#),
            "timeout_ms is 86400001; with background it runs from 1 to 86400000"
        );
        assert_eq!(detail(r#"{"command":""}"#), "command is empty");
        assert_eq!(
            detail(r#"{"command":"true","detach":true,"background":true}"#),
            "detach and background cannot both be set"
        );
        assert_eq!(
            detail(r#"{"command":"true","workdir":"a\u0000b"}"#),
            "workdir is a path of up to 4096 bytes, with no NUL byte"
        );
    }
    /// A command runs in its workdir, read against the turn's folder unless
    /// absolute, as Codex's; an empty one is the folder itself, and one
    /// that is not there is named.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_runs_in_its_workdir() {
        let dir = std::env::temp_dir().join(format!("agent-workdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let registry = Registry::new("shell").unwrap();
        let run = |workdir: &str| {
            let arguments = json!({"command": "realpath .", "workdir": workdir}).to_string();
            let prepared = registry.prepare("shell", &arguments).unwrap();
            registry.execute(prepared, &dir, &[], None)
        };
        let here = |outcome: Outcome| outcome.output.lines().next().unwrap_or_default().to_owned();
        let sub = dir.join("sub").display().to_string();
        assert!(here(run("sub").await.unwrap()).contains(&sub));
        assert!(here(run(&sub).await.unwrap()).contains(&sub));
        let whole = here(run("").await.unwrap());
        assert!(whole.contains(&dir.display().to_string()) && !whole.contains(&sub));
        let error = run("gone").await.unwrap_err();
        assert_eq!(error.code, "path_not_found");
        assert!(error.detail.unwrap().contains("gone"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
