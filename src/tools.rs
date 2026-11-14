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
/// The most of one stream a file in a bot's outputs folder holds, written
/// as the command runs: a sanity bound on one that writes without end.
pub const SPILL_BYTES: u64 = 1 << 30;
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
                "Run a noninteractive bash command (sh where bash is not installed) in the workspace, or in workdir when given (relative to the workspace unless absolute). stdout and stderr are returned separately; each is truncated to a head and tail beyond 64 KiB, and retained whole in a file the result names under files, up to 1 GiB, or, where there is no such file, up to 1 MiB: past that, its first and last 512 KiB. The command keeps running. Default timeout 120 s, maximum 600 s; with background=true there is no timeout unless timeout_ms sets one, of up to a day. A command that times out is killed and returns its output so far with timed_out. Every result reports wall_time_ms. Commands run with no terminal: TERM=dumb, NO_COLOR=1, and cat as the pager; AI_AGENT=agent tells tools an agent runs them. When the command returns or times out, every process it started is killed, so `cmd &` does not outlive it. With background=true the command is started and a proc handle is returned immediately; collect its result later with wait. If no wait collects it, its end comes as a message once no turn of yours runs: {\"finished\":{\"proc:N\":{how it ended}}}; wait on the handle reads the output. A steer that arrives while a command runs moves it to the background: the result is its proc handle with steered=true, and the command keeps running. A background command runs until it exits, its timeout_ms passes, kill=true on its handle stops it, or the daemon stops; each agent keeps at most as many as the daemon's process bound, and starting one past them kills the one it started or waited on least recently. With detach=true it starts in its own session and returns a proc handle and pid at once; it outlives the turn and the daemon until it exits, a pause or kill=true, its output discarded unless redirected, its end told as a background command's. With tty=true it runs in a terminal, for a REPL or a program that prompts: the result is what it wrote within yield_time_ms (default 1000) and, while it runs, a proc handle for write_stdin.",
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
    /// Whether a steer lifts a call still waiting for its answer to the
    /// background once it has waited the grace a foreground command gets,
    /// as it lifts that command: for a tool whose calls may run long, a
    /// script say, or wait on a person. Left out, the call is answered
    /// first and the steer is taken at the next boundary, as Codex takes a
    /// message once the running tool returns. A call one of whose steps
    /// waits on a gate is lifted either way, as a gated call is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lift: bool,
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
            None if tool.expire_ms.is_some() || tool.steps || tool.lift => {
                return fail_with(
                    "invalid_declare",
                    format!("{name}: expire_ms, steps and lift go with answered_by"),
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
    /// Detached commands found again past a bound lowered since they
    /// started, each in line for a lasting slot ahead of any new one.
    lasting_owed: Arc<std::sync::atomic::AtomicUsize>,
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
    /// it when its agent kills it, and whose it is once it counts toward
    /// its bot's share.
    stops: Arc<std::sync::Mutex<Stops>>,
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

/// The most held back for whole credentials across a stream's cut.
const CREDENTIAL_HOLD: usize = 64 * 1024;

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
    /// output would finish: what a stream holds back until it can redact.
    /// A whole credential across the cut is held with them.
    fn unfinished(set: &CredentialSet, text: &[u8]) -> usize {
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
            .unwrap_or(0);
        // A whole credential across the cut is held with what follows it:
        // the start held may finish a longer one, which whole-text
        // redaction replaces first. Past `CREDENTIAL_HOLD` of such, as in a
        // run of one that ends with its own start, it is shown whole.
        let start = text.len() - started;
        let mut cut = start;
        while let Some(first) = values()
            .filter_map(|value| {
                let from = (cut + 1).saturating_sub(value.len());
                let to = (cut + value.len() - 1).min(text.len());
                let window = text.get(from..to)?;
                let at = window.windows(value.len()).position(|w| w == value)?;
                Some(from + at)
            })
            .min()
        {
            cut = first;
            if start - cut > CREDENTIAL_HOLD {
                break;
            }
        }
        if start - cut <= CREDENTIAL_HOLD {
            return text.len() - cut;
        }
        let mut cut = start;
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
}

/// A stream redacted piece by piece exactly as [`Credentials::redact`]
/// would redact it whole. Each of its passes runs as a stream in the same
/// order: credentials, held back only while the end of what was read may
/// begin one; then secret PEM blocks; then secret fields' values, each
/// read byte by byte as `redact_blocks` and `redact_fields` read a whole
/// text, so how the stream is cut never changes what they find. Only what
/// may still be redacted is held, and a block or value too long to hold
/// is dropped as it comes.
pub struct Redacting {
    credentials: Credentials,
    /// Read and not yet past the credentials' pass.
    held: Vec<u8>,
    blocks: BlockStream,
    fields: FieldStream,
}

impl Redacting {
    pub fn new(credentials: Credentials) -> Self {
        Redacting {
            credentials,
            held: Vec::new(),
            blocks: BlockStream::default(),
            fields: FieldStream::default(),
        }
    }

    /// `bytes`, the stream's next, and what before them was held: as much
    /// as can be told now, redacted.
    pub fn feed(&mut self, bytes: &[u8]) -> String {
        self.held.extend_from_slice(bytes);
        let set = self.credentials.current();
        // A character cut at the piece's end waits for the rest of it,
        // whatever came before.
        let whole = self.held.len() - unfinished_char(&self.held);
        let kept = Credentials::unfinished(&set, &self.held[..whole]).min(whole);
        let rest = self.held.split_off(whole - kept);
        let read = std::mem::replace(&mut self.held, rest);
        self.pass(&set, read, false)
    }

    /// Begin again after a gap in the stream: what was held, and what was
    /// read before, no longer bear on what comes next.
    pub fn restart(&mut self) {
        *self = Redacting::new(self.credentials.clone());
    }

    /// What is still held, at the stream's end, redacted.
    pub fn finish(mut self) -> String {
        let set = self.credentials.current();
        let read = std::mem::take(&mut self.held);
        self.pass(&set, read, true)
    }

    fn pass(&mut self, set: &CredentialSet, read: Vec<u8>, last: bool) -> String {
        let mut text = String::from_utf8(read)
            .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned());
        for credential in &set.values {
            if text.contains(&credential.value) {
                text = text.replace(&credential.value, REDACTED);
            }
        }
        if !set.blocks.is_empty() || self.blocks.busy() {
            text = self.blocks.feed(&text, &set.blocks, last);
        }
        if !set.fields.is_empty() || self.fields.busy() {
            text = self.fields.feed(&text, &set.fields, last);
        }
        text
    }
}

/// How many of `bytes`' last bytes begin a UTF-8 character that more
/// bytes would finish.
fn unfinished_char(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let lead = bytes[bytes.len() - back];
        if lead & 0xC0 == 0x80 {
            continue;
        }
        let needs = match lead {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => return 0,
        };
        let begun = std::str::from_utf8(&bytes[bytes.len() - back..])
            .err()
            .is_some_and(|error| error.error_len().is_none());
        return if needs > back && begun { back } else { 0 };
    }
    0
}

/// `redact_blocks` as a stream: what it may still replace is held, and a
/// secret block's body is dropped as it comes, line by line.
#[derive(Default)]
struct BlockStream {
    /// Read and not yet told: from a block's start, or the end of the
    /// text that may begin one.
    held: String,
    /// Inside a secret block's body.
    body: Option<PemBody>,
}

/// The most of a block's header held back for its end: far past any
/// key's, so a header still open past it names nothing.
const HEADER_HOLD: usize = 64 * 1024;

impl BlockStream {
    fn busy(&self) -> bool {
        !self.held.is_empty() || self.body.is_some()
    }

    fn feed(&mut self, text: &str, blocks: &[String], last: bool) -> String {
        const BEGIN: &str = "-----BEGIN ";
        let mut out = String::with_capacity(text.len());
        let mut text = std::mem::take(&mut self.held) + text;
        // Read on from `at`: nothing before it is moved again.
        let mut at = 0;
        loop {
            if let Some(body) = &mut self.body {
                // A line held from the last text begins this one.
                let line = std::mem::take(&mut body.line);
                if !line.is_empty() {
                    text = line + &text[at..];
                    at = 0;
                }
                if body.searching {
                    let rest = &text[at..];
                    match pem_end(rest, body.searched, last) {
                        None => {
                            body.searched = rest.len();
                            self.held = rest.to_owned();
                            return out;
                        }
                        Some(Some(end)) => {
                            out.push_str(if end > 0 { "[REDACTED]\n" } else { "" });
                            self.body = None;
                            at += end;
                            continue;
                        }
                        Some(None) => body.searching = false,
                    }
                }
                match body.take(&text[at..], last) {
                    Some((len, told)) => {
                        out.push_str(told);
                        self.body = None;
                        at += len;
                        continue;
                    }
                    None => return out,
                }
            }
            let Some(begin) = text[at..].find(BEGIN).map(|n| at + n) else {
                // The end may begin a block's marker.
                let keep = match last {
                    true => 0,
                    false => (1..BEGIN.len())
                        .rev()
                        .find(|&n| text.ends_with(&BEGIN[..n]))
                        .unwrap_or(0),
                };
                let cut = (text.len() - keep).max(at);
                out.push_str(&text[at..cut]);
                self.held = text[cut..].to_owned();
                return out;
            };
            out.push_str(&text[at..begin]);
            at = begin;
            let from = at + BEGIN.len();
            // The closing dashes first, as `pem_header` reads them.
            let dash = find_dashes(&text[from..]).map(|n| from + n);
            let closed = dash.is_some_and(|dash| !text[from..dash].contains('\n'));
            // No dashes after: no later block starts, so this is read once.
            let line_ends = dash.is_some() || text[from..].contains('\n');
            if !closed && (line_ends || last) {
                // No header line here: read on after its start.
                out.push_str(BEGIN);
                at = from;
                continue;
            }
            if !closed {
                // No header ends after this block's start, nor so after any
                // later one: past `HEADER_HOLD` they are no key's and are
                // told as they are; the last such start is held.
                let from = ceil_char(&text, text.len().saturating_sub(HEADER_HOLD)).max(at);
                let held = match text[from..].find(BEGIN) {
                    Some(n) => from + n,
                    None => text.len(),
                };
                out.push_str(&text[at..held]);
                self.held = text[held..].to_owned();
                return out;
            }
            let Some(header) = pem_header(&text, at, last) else {
                let dashes = dash.unwrap_or(from) + 5;
                let after = &text.as_bytes()[dashes..];
                if !last
                    && after.len() <= PEM_SPACES + 1
                    && after.iter().all(|b| b" \t\r".contains(b))
                {
                    // The line's end may follow.
                    self.held = text[at..].to_owned();
                    return out;
                }
                match pem_escaped(&text, at, last) {
                    Escaped::More => {
                        self.held = text[at..].to_owned();
                        return out;
                    }
                    Escaped::Key {
                        escape,
                        header,
                        end,
                    } => {
                        let named = blocks
                            .iter()
                            .any(|block| text[at..header].contains(block.as_str()));
                        out.push_str(&text[at..header]);
                        if named {
                            out.push_str(REDACTED);
                            out.push_str(&text[escape..header]);
                            at = end;
                        } else {
                            at = header;
                        }
                        continue;
                    }
                    Escaped::No => {}
                }
                out.push_str(BEGIN);
                at = from;
                continue;
            };
            let named = blocks
                .iter()
                .any(|block| text[at..header].contains(block.as_str()));
            out.push_str(&text[at..header]);
            at = header;
            if named {
                self.body = Some(PemBody {
                    searching: true,
                    ..PemBody::default()
                });
            }
        }
    }
}

/// Where the first run of five dashes starts in `text`: a byte search for
/// each dash, with no searcher built per call, as a key's markers are
/// looked for at every header.
fn find_dashes(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut at = 0;
    loop {
        let dash = at + text[at..].find('-')?;
        let run = bytes[dash..]
            .iter()
            .take(5)
            .take_while(|&&b| b == b'-')
            .count();
        if run >= 5 {
            return Some(dash);
        }
        at = dash + run;
    }
}

/// Where `marker`, five dashes and a word, first starts in `text`.
fn find_marker(text: &str, marker: &str) -> Option<usize> {
    let mut at = 0;
    loop {
        let dashes = at + find_dashes(&text[at..])?;
        if text[dashes..].starts_with(marker) {
            return Some(dashes);
        }
        at = dashes + 1;
    }
}

/// The first char boundary of `text` at or after `at`.
fn ceil_char(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// `redact_fields` as a stream, read byte by byte as it reads a whole
/// text: all but a value that may be secret is told as it comes.
#[derive(Default)]
struct FieldStream {
    state: FieldState,
    /// Whether the last byte read was part of a word.
    after_word: bool,
}

#[derive(Default)]
enum FieldState {
    /// Between words, or in a word that cannot be a field's name.
    #[default]
    Scan,
    /// In a word: its length, the end of its key, as `field_key` makes
    /// it, from the texts before this one, up to the longest field's, and
    /// where it starts in this text.
    Word {
        length: usize,
        key: Vec<u8>,
        from: usize,
    },
    /// After a field's name: past its closing quote, if it had one, and
    /// then past its separator.
    Name { quoted: bool, separated: bool },
    /// In a value: its quote, whether the last byte escaped the next, and
    /// what of it is held until it is known to be secret or not.
    Value {
        quote: Option<u8>,
        escaped: bool,
        value: Value,
    },
}

/// A value read so far.
enum Value {
    /// Not yet known to be secret.
    Held(Held),
    /// Told as redacted: the rest of it is dropped.
    Secret,
    /// Known not to be: told as it comes.
    Shown,
}

/// A value's bytes held until it is known to be secret or not, and
/// whether they hold a digit and a letter, kept as they come so each byte
/// costs the same however long the value runs.
#[derive(Default)]
struct Held {
    bytes: Vec<u8>,
    digit: bool,
    letter: bool,
}

impl Held {
    fn push(&mut self, b: u8) {
        self.bytes.push(b);
        self.digit |= b.is_ascii_digit();
        self.letter |= b.is_ascii_alphabetic();
    }

    /// Whether it is secret, as `redact_fields` judges a whole value: long
    /// enough, mixing letters and digits, and not code.
    fn secret(&self) -> bool {
        self.bytes.len() >= SECRET_MIN
            && self.digit
            && self.letter
            && !matches!(self.bytes[0], b'{' | b'[' | b'$')
    }

    /// Told whole at its end: redacted when it is secret.
    fn tell(&mut self, out: &mut Vec<u8>) {
        match self.secret() {
            true => out.extend_from_slice(REDACTED.as_bytes()),
            false => out.append(&mut self.bytes),
        }
    }
}

/// The most of a value held before it is known to be secret or not: one
/// longer is told as redacted.
const VALUE_HOLD: usize = 1 << 20;

impl FieldStream {
    fn busy(&self) -> bool {
        !matches!(self.state, FieldState::Scan)
    }

    fn feed(&mut self, text: &str, fields: &[String], last: bool) -> String {
        let shortest = fields.iter().map(String::len).min().unwrap_or(0);
        let longest = fields.iter().map(String::len).max().unwrap_or(0);
        let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(text.len());
        // What is told as read runs from `mark`, copied out whole when a
        // value is held or dropped, and at the end.
        let mut mark = 0;
        let mut at = 0;
        while at < bytes.len() {
            let b = bytes[at];
            match &mut self.state {
                FieldState::Scan => {
                    if word(b) && !self.after_word {
                        self.state = FieldState::Word {
                            length: 0,
                            key: Vec::new(),
                            from: at,
                        };
                        continue;
                    }
                    // Runs of bytes that start no word pass at once.
                    let mut end = at + 1;
                    while end < bytes.len() && !word(bytes[end]) {
                        end += 1;
                    }
                    if end > at + 1 || !word(b) {
                        self.after_word = false;
                        at = end;
                        continue;
                    }
                    // A word byte after a word: the rest of that word.
                }
                FieldState::Word { length, key, from } => {
                    let mut end = at;
                    while end < bytes.len() && word(bytes[end]) {
                        end += 1;
                    }
                    *length += end - at;
                    if end == bytes.len() {
                        // The word goes on into the next text: keep its
                        // key's end.
                        key.extend(
                            (bytes[*from..end].iter())
                                .filter(|&&b| b != b'_' && b != b'-')
                                .map(u8::to_ascii_lowercase),
                        );
                        let cut = key.len().saturating_sub(longest);
                        key.drain(..cut);
                        *from = 0;
                        self.after_word = true;
                        at = end;
                        continue;
                    }
                    let part = &bytes[*from..end];
                    let named = *length >= shortest
                        && fields.iter().any(|field| {
                            let mut name = (part.iter().rev())
                                .filter(|&&b| b != b'_' && b != b'-')
                                .map(u8::to_ascii_lowercase)
                                .chain(key.iter().rev().copied());
                            field.bytes().rev().all(|b| name.next() == Some(b))
                        });
                    self.state = match named {
                        true => FieldState::Name {
                            quoted: false,
                            separated: false,
                        },
                        false => FieldState::Scan,
                    };
                    self.after_word = true;
                    at = end;
                    continue;
                }
                FieldState::Name { quoted, separated } => {
                    if !*separated {
                        if !*quoted && matches!(b, b'"' | b'\'') || matches!(b, b' ' | b'\t') {
                            *quoted = true;
                        } else if matches!(b, b'=' | b':') {
                            *separated = true;
                        } else {
                            // No separator: no field. The byte is read
                            // again, after what came before it.
                            self.state = FieldState::Scan;
                            continue;
                        }
                    } else if !matches!(b, b' ' | b'\t') {
                        let quote = matches!(b, b'"' | b'\'').then_some(b);
                        self.state = FieldState::Value {
                            quote,
                            escaped: false,
                            value: Value::Held(Held::default()),
                        };
                        if quote.is_none() {
                            continue;
                        }
                    }
                }
                FieldState::Value {
                    quote,
                    escaped,
                    value,
                } => {
                    let ends = !*escaped
                        && match quote {
                            Some(q) => b == *q || b == b'\n',
                            None => {
                                b.is_ascii_whitespace()
                                    || matches!(b, b',' | b';' | b'"' | b'\'' | b'}' | b']')
                            }
                        };
                    if ends {
                        if let Value::Held(held) = value {
                            held.tell(&mut out);
                        }
                        // The byte that ended it is read again.
                        self.state = FieldState::Scan;
                        continue;
                    }
                    *escaped = !*escaped && quote.is_some() && b == b'\\';
                    match value {
                        Value::Shown => {}
                        Value::Secret => {
                            mark = at + 1;
                        }
                        Value::Held(held) => {
                            if held.bytes.is_empty() {
                                out.extend_from_slice(&bytes[mark..at]);
                            }
                            held.push(b);
                            mark = at + 1;
                            if held.secret() {
                                out.extend_from_slice(REDACTED.as_bytes());
                                *value = Value::Secret;
                            } else if matches!(held.bytes[0], b'{' | b'[' | b'$') {
                                out.append(&mut held.bytes);
                                *value = Value::Shown;
                            } else if held.bytes.len() > VALUE_HOLD {
                                out.extend_from_slice(REDACTED.as_bytes());
                                *value = Value::Secret;
                            }
                        }
                    }
                }
            }
            self.after_word = word(b);
            at += 1;
        }
        out.extend_from_slice(&bytes[mark..]);
        if last {
            if let FieldState::Value {
                value: Value::Held(held),
                ..
            } = &mut self.state
            {
                held.tell(&mut out);
            }
            self.state = FieldState::Scan;
        }
        String::from_utf8(out)
            .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
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

/// Whether `name`'s key, as `field_key` makes it, ends with `field`,
/// without making it: `name` is a word, all ASCII.
fn key_ends_with(name: &[u8], field: &str) -> bool {
    let mut key = (name.iter().rev())
        .filter(|&&b| b != b'_' && b != b'-')
        .map(u8::to_ascii_lowercase);
    field.bytes().rev().all(|b| key.next() == Some(b))
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
        if !fields
            .iter()
            .any(|field| key_ends_with(&bytes[start..i], field))
        {
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

/// Replace the body of each PEM block in `text` whose header names one of
/// `blocks`.
fn redact_blocks(text: String, blocks: &[String]) -> String {
    const BEGIN: &str = "-----BEGIN ";
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(begin) = rest.find(BEGIN) {
        let Some(header) = pem_header(rest, begin, true) else {
            if let Escaped::Key {
                escape,
                header,
                end,
            } = pem_escaped(rest, begin, true)
            {
                out.push_str(&rest[..header]);
                let named = blocks
                    .iter()
                    .any(|block| rest[begin..header].contains(block.as_str()));
                if named {
                    out.push_str(REDACTED);
                    out.push_str(&rest[escape..header]);
                    rest = &rest[end..];
                } else {
                    rest = &rest[header..];
                }
                continue;
            }
            // No header line here: read on after its start.
            out.push_str(&rest[..begin + BEGIN.len()]);
            rest = &rest[begin + BEGIN.len()..];
            continue;
        };
        out.push_str(&rest[..header]);
        let named = blocks
            .iter()
            .any(|block| rest[begin..header].contains(block.as_str()));
        rest = &rest[header..];
        if named {
            if let Some(Some(end)) = pem_end(rest, 0, true) {
                out.push_str(if end > 0 { "[REDACTED]\n" } else { "" });
                rest = &rest[end..];
                continue;
            }
            let mut body = PemBody::default();
            let (len, told) = body.take(rest, true).expect("the text's end ends the body");
            out.push_str(told);
            rest = &rest[len..];
        }
    }
    out.push_str(rest);
    out
}

/// Where the header line of the PEM block at `begin` ends, past its line's
/// end: its five dashes close it on the same line, and only a line's end
/// follows them, or the text's end when it is `last`. `None` when it is no
/// header, or when more text may still tell.
fn pem_header(text: &str, begin: usize, last: bool) -> Option<usize> {
    const BEGIN: &str = "-----BEGIN ";
    let from = begin + BEGIN.len();
    // The closing dashes first: the next are at the latest the next
    // block's, so no text is read twice however many starts a line holds.
    let dashes = from + find_dashes(&text[from..])?;
    if text[from..dashes].contains('\n') {
        return None;
    }
    // Spaces or tabs may follow, as a key's reader allows, up to a bound
    // a stream holds them to; nothing else.
    let dashes = dashes + 5;
    let spaces = text[dashes..].len() - text[dashes..].trim_start_matches([' ', '\t']).len();
    if spaces > PEM_SPACES {
        return None;
    }
    let dashes = dashes + spaces;
    match &text.as_bytes()[dashes..] {
        [b'\r', b'\n', ..] => Some(dashes + 2),
        [b'\n', ..] => Some(dashes + 1),
        [] if last => Some(dashes),
        _ => None,
    }
}

/// The most spaces or tabs a key's header may end with.
const PEM_SPACES: usize = 1024;

/// The most of a secret block's body looked through for its END marker:
/// far past any key's, armored PGP keys with their subkeys among them.
const BODY_HOLD: usize = 256 * 1024;

/// Where the line of a secret block's END marker starts in `body`, the
/// text after its header, when one starts within [`BODY_HOLD`] and before
/// any other block's BEGIN: the whole block is hidden through it, whatever
/// its lines hold, as a key's reader takes them. `Some(None)` when none
/// does, `None` when more text may still tell. `from` is how much of
/// `body` was looked through before. Each search stops at the next BEGIN,
/// so many headers read their text once.
fn pem_end(body: &str, from: usize, last: bool) -> Option<Option<usize>> {
    const END: &str = "-----END ";
    const BEGIN: &str = "-----BEGIN ";
    let window = body.floor_char_boundary(BODY_HOLD + END.len() - 1);
    let mut at = ceil_char(body, from.saturating_sub(BEGIN.len()).min(window));
    // Where the line read now starts, and what its text before the
    // marker read so far holds, kept as the search moves on so no text is
    // read twice: a marker starts its line after at most spaces or what a
    // search or the read tool puts before each line (a path or a number
    // ending in `:`, `-` or a tab, with no space inside it).
    let mut line = body[..at].rfind('\n').map_or(0, |n| n + 1);
    let (mut scan, mut word, mut gap) = (line, false, false);
    while let Some(n) = find_dashes(&body[at..window]) {
        let dashes = at + n;
        if let Some(n) = body[at..dashes].rfind('\n') {
            line = at + n + 1;
            (scan, word, gap) = (line, false, false);
        }
        let tail = &body[dashes..];
        let (end, begin) = (tail.starts_with(END), tail.starts_with(BEGIN));
        if end || begin {
            for &b in &body.as_bytes()[scan..dashes] {
                match b {
                    b' ' => gap |= word,
                    _ => word = true,
                }
            }
            scan = dashes;
            // One inside text, such as an armored key's `Comment:`, is
            // that text.
            let starts = !gap && (!word || body[line..dashes].ends_with([':', '-', '\t']));
            if end {
                // Hidden to that line's start when only such a prefix is
                // before it, so that shows.
                return Some(Some(if starts { line } else { dashes }));
            }
            if starts {
                return Some(None);
            }
        }
        if !last && (END.starts_with(tail) || BEGIN.starts_with(tail)) {
            return None;
        }
        at = dashes + 1;
    }
    match last || window < body.len() || body.len() >= BODY_HOLD + END.len() - 1 {
        true => Some(None),
        false => None,
    }
}

/// The most backslashes a key's escaped line end is read with: JSON
/// inside JSON, and deeper, far past any real one.
const PEM_ESCAPES: usize = 16;

/// A key's block as a JSON string holds it, all on one line.
enum Escaped {
    /// Its header runs to `header`, the escaped line end that closes it
    /// from `escape`; its body to `end`, where its END marker starts.
    Key {
        escape: usize,
        header: usize,
        end: usize,
    },
    /// No such block.
    No,
    /// More text may still tell.
    More,
}

/// The escaped PEM block at `begin`, as JSON holds a key: its header's
/// five dashes, an escaped line end right after them (`\n` or `\r\n`,
/// escaped once or more), a body that stays inside its string (no quote
/// that is not escaped), then its END marker, all on the line. A header
/// with no END after it on its line, or a string's end between, is a
/// pattern or a quote, not a key.
fn pem_escaped(text: &str, begin: usize, last: bool) -> Escaped {
    const BEGIN: &str = "-----BEGIN ";
    const END: &str = "-----END ";
    let more = match last {
        true => Escaped::No,
        false => Escaped::More,
    };
    let from = begin + BEGIN.len();
    let Some(dashes) = find_dashes(&text[from..]).map(|n| from + n + 5) else {
        return Escaped::No;
    };
    if text[from..dashes].contains('\n') {
        return Escaped::No;
    }
    // Spaces or tabs may end its header, as a key's reader allows.
    let spaces = text.as_bytes()[dashes..]
        .iter()
        .take_while(|&&b| b == b' ' || b == b'\t')
        .count();
    if spaces > PEM_SPACES {
        return Escaped::No;
    }
    let dashes = dashes + spaces;
    let bytes = &text.as_bytes()[dashes..];
    let slashes = bytes.iter().take_while(|&&b| b == b'\\').count();
    if slashes > PEM_ESCAPES {
        return Escaped::No;
    }
    let escaped = match (slashes, bytes.get(slashes)) {
        (0, Some(_)) => return Escaped::No,
        (_, None) => return more,
        (_, Some(b'n')) => slashes + 1,
        (_, Some(b'r')) => {
            // `\r\n`, the `\n` escaped as the `\r` is.
            let after = &bytes[slashes + 1..];
            let n = slashes + 1;
            let ends = |b: &u8, i: usize| if i < slashes { *b == b'\\' } else { *b == b'n' };
            if after.len() < n && after.iter().enumerate().all(|(i, b)| ends(b, i)) {
                return more;
            }
            match after.len() >= n && after[..n].iter().enumerate().all(|(i, b)| ends(b, i)) {
                true => slashes + 1 + n,
                false => slashes + 1,
            }
        }
        _ => return Escaped::No,
    };
    let header = dashes + escaped;
    let bytes = text.as_bytes();
    // Its line, read to at most a line's worth with room for an END that
    // starts at its end: a line's end is code, not a key.
    let region = &text[header..text.floor_char_boundary(header + PEM_LINE + END.len())];
    let mut line = region;
    if let Some(n) = line.find('\n') {
        line = &line[..n];
    }
    if let Some(n) = line.find('\r') {
        line = &line[..n];
    }
    let ended = line.len() < region.len() || text.len() >= header + PEM_LINE + END.len();
    let end = match find_marker(line, END) {
        Some(0) => find_marker(&line[1..], END).map(|n| n + 1),
        end => end,
    };
    if end.is_some_and(|n| n > PEM_LINE) {
        return Escaped::No;
    }
    let upto = end.unwrap_or(line.len());
    // A quote after an even run of backslashes ends the string, so what
    // follows is code, not the key.
    let quoted = line[..upto].match_indices('"').any(|(q, _)| {
        let run = line.as_bytes()[..q]
            .iter()
            .rev()
            .take_while(|&&b| b == b'\\');
        run.count() % 2 == 0
    });
    // Another block's BEGIN at an escaped line's start ends this one; one
    // inside a line, such as an armored key's `Comment:`, is that line's
    // text.
    let mut begins = false;
    let mut at = 0;
    while let Some(p) = find_marker(&line[at..], BEGIN).map(|n| at + n) {
        if p >= upto {
            break;
        }
        if matches!(bytes[header + p - 1], b'n' | b'r') && bytes[header + p - 2] == b'\\' {
            begins = true;
            break;
        }
        at = p + 1;
    }
    match end {
        _ if quoted || begins => Escaped::No,
        Some(end) => Escaped::Key {
            escape: dashes,
            header,
            end: header + end,
        },
        None if ended => Escaped::No,
        None => more,
    }
}

/// The most of one line a key's body is judged on: a line whose first
/// this many bytes are base64 is a body's line, whatever follows.
const PEM_LINE: usize = 64 * 1024;

/// `line` as a key's body is judged: its end's `\r` taken off, and the
/// number the read tool puts before each line it shows.
fn pem_text(line: &str) -> &str {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let numbered = line.trim_start_matches(' ');
    let digits = numbered.len()
        - numbered
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .len();
    match numbered[digits..].strip_prefix('\t') {
        Some(rest) if digits > 0 => rest,
        _ => line,
    }
}

/// Whether `line`, its end's `\r` and the spaces or tabs around it taken
/// off, is a line of a key's body: base64 with any spaces or tabs, the RFC
/// 1421 headers of an encrypted key, or empty, as a key's reader takes
/// them. What a search or a listing puts before each line it shows (a
/// path, a number, `:`, `-` or a tab between them) is not read: base64
/// holds none of those, so a body's line is what follows the last of
/// them. A marker's dashes are never a body's.
fn pem_line(line: &str) -> bool {
    let line = pem_text(line).trim_matches([' ', '\t']);
    let head = &line[..line.floor_char_boundary(PEM_LINE)];
    if find_dashes(head).is_some() {
        return false;
    }
    if line.len() <= PEM_LINE && (head.contains("Proc-Type:") || head.contains("DEK-Info:")) {
        return true;
    }
    let body = head.rsplit([':', '-', '\t']).next().unwrap_or("");
    let base64 = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b' ');
    body.as_bytes().iter().all(base64)
}

/// A secret block's body with no END marker within [`BODY_HOLD`] of its
/// header, read line by line after its header: the lines
/// that read as a key's are told as one `[REDACTED]` line, and the first
/// that does not, an `END` marker or anything else, is told as it is. So a
/// key cut short hides only itself, and a line that merely looks like a
/// header hides nothing after it.
#[derive(Default)]
struct PemBody {
    /// Still looking for its END marker ([`pem_end`]), all of it held.
    searching: bool,
    /// How much of the held body was looked through for its END.
    searched: usize,
    /// Some of its lines were taken.
    any: bool,
    /// Inside a line too long to hold that reads as a key's: dropped up to
    /// its end.
    long: bool,
    /// The start of a line not yet ended, held to judge it whole.
    line: String,
}

impl PemBody {
    /// Read `text`, which starts a line: how far the body runs, and what it
    /// is told as, once a line ends it or `last` ends the text; `None`
    /// when all of `text` was taken and more may follow.
    fn take(&mut self, text: &str, last: bool) -> Option<(usize, &'static str)> {
        let told = |any: bool| if any { "[REDACTED]\n" } else { "" };
        let mut at = 0;
        loop {
            let rest = &text[at..];
            if self.long {
                match rest.find('\n') {
                    Some(n) => {
                        self.long = false;
                        at += n + 1;
                        continue;
                    }
                    None if last => return Some((text.len(), "[REDACTED]")),
                    None => return None,
                }
            }
            match rest.find('\n') {
                Some(n) if pem_line(&rest[..n]) => {
                    self.any = true;
                    at += n + 1;
                }
                Some(_) => return Some((at, told(self.any))),
                None if rest.is_empty() => {
                    return last.then(|| (at, told(self.any)));
                }
                None if last => {
                    return Some(match pem_line(rest) {
                        true => (text.len(), "[REDACTED]"),
                        false => (at, told(self.any)),
                    });
                }
                None if rest.len() > PEM_LINE => {
                    if !pem_line(rest) {
                        return Some((at, told(self.any)));
                    }
                    (self.long, self.any) = (true, true);
                    return None;
                }
                None => {
                    self.line = rest.to_owned();
                    return None;
                }
            }
        }
    }
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
    /// These and `other`'s, as one: the files removed together unless the
    /// store records them.
    pub fn join(self, other: Files) -> Files {
        match (self.0, other.0) {
            (None, files) | (files, None) => Files(files),
            (Some(first), Some(second)) => {
                let files = (first.files.iter().chain(&second.files).cloned()).collect();
                // Their removal is the joined set's now.
                for owned in [&first, &second] {
                    owned
                        .recorded
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Files::new(files)
            }
        }
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
            lasting_owed: Arc::default(),
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
    /// The lasting slot of a detached command found again: a free one, or
    /// with none free, its place in line for the next one freed, which it
    /// takes before any new command can, counted as running meanwhile. A
    /// bound lowered while it ran so holds new ones back until enough end.
    #[cfg(unix)]
    fn lasting_found(&self) -> Box<dyn Send> {
        if let Ok(permit) = self.lasting_slots.clone().try_acquire_owned() {
            return Box::new(permit);
        }
        struct Owed(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Owed {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        struct Waiting(tokio::task::JoinHandle<()>);
        impl Drop for Waiting {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        self.lasting_owed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (slots, owed) = (self.lasting_slots.clone(), Owed(self.lasting_owed.clone()));
        Box::new(Waiting(tokio::spawn(async move {
            let permit = slots.acquire_owned().await;
            drop(owed);
            // Held until the command ends, which aborts this.
            let _permit = permit;
            std::future::pending::<()>().await;
        })))
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
            + self.lasting_owed.load(std::sync::atomic::Ordering::Relaxed)
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
                let Detached { pid, .. } = detach(
                    self.cord(),
                    &command,
                    &folder,
                    self,
                    environment,
                    sandbox,
                    None,
                )
                .await?;
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
                    None,
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
                // A removal happens here, outside the sandbox: the name
                // removed, a link too, must be one the bot may write.
                for path in patch.removed() {
                    removable(sandbox.clone(), &resolve(workspace, path), path).await?;
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
        let mut files = Files::default();
        let mut preview = |name: &'static str, mut captured: Captured| {
            let spilled = captured.spilled.take();
            let total = captured.len() as u64 + captured.dropped;
            let (bytes, dropped) = captured.finish(spilled.is_some());
            // Reuse the pipe buffer for ordinary UTF-8 output. Invalid
            // bytes still get the same lossy decoding as before.
            let text = String::from_utf8(bytes)
                .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned());
            let mut text = self.redact(text);
            // Written whole to a file as it ran: the file is the stream's
            // retained copy, and the bytes here only what the model sees.
            if let Some(spilled) = spilled {
                let retained = match total.saturating_sub(spilled.saved) {
                    0 => "the full output is retained".to_owned(),
                    short => format!(
                        "its first {} bytes are retained, and the {short} after them were not, past the {} GiB a retained stream holds",
                        spilled.saved,
                        SPILL_BYTES >> 30
                    ),
                };
                files = std::mem::take(&mut files).join(spilled.files);
                return truncated(&text, &retained);
            }
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
            files,
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
        spill: Option<Spill>,
    ) -> impl Future<Output = Result<Outcome>> + Send + 'static {
        let registry = self.clone();
        async move {
            let (mut stdout, mut stderr, exit, elapsed) = {
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
                    spill.as_ref(),
                )
                .await?
            };
            // The streams' files are finished with no process slot held.
            tokio::join!(stdout.settle(), stderr.settle());
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
        owner: i64,
        command: String,
        workspace: PathBuf,
        timeout_ms: Option<u64>,
        environment: Vec<(String, String)>,
        sandbox: Option<Arc<Policy>>,
        queued: Queued,
        done: tokio::sync::oneshot::Sender<Result<Outcome>>,
        spill: Option<Spill>,
    ) {
        let registry = self.clone();
        let mut queued = queued;
        let mut stopping = self.stopping.subscribe();
        let mut pulls = self.pulls.subscribe();
        self.own(&stopper, owner);
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
                let (mut stdout, mut stderr, exit, elapsed) = {
                    let _slot = slot.map_err(|_| Error::new("tool_scheduler_closed"))?;
                    let _held = crate::sandbox::hold_for(sandbox.as_deref()).await;
                    shell(
                        &command,
                        &workspace,
                        timeout_ms.map(Duration::from_millis),
                        &registry,
                        &environment,
                        sandbox.as_deref(),
                        spill.as_ref(),
                    )
                    .await?
                };
                // The streams' files are finished with no process slot held.
                tokio::join!(stdout.settle(), stderr.settle());
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
                Ok(kill) = stop => {
                    let _ = done.send(Err(stopped(kill)));
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
        let entry = Stop {
            sender,
            owner: None,
            used: 0,
        };
        self.stops.lock().unwrap().running.insert(id, entry);
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
                Ok(kill) = stop => Err(stopped(kill)),
                result = run => result,
            }
        }
    }

    /// Kill background command `id`, dropping it, which kills its group:
    /// false when none of that handle runs. Whose it is, the caller checks.
    pub fn stop(&self, id: i64) -> bool {
        let stop = self.stops.lock().unwrap().running.remove(&id);
        stop.is_some_and(|stop| stop.sender.send(Kill::Agent).is_ok())
    }

    /// Count `stopper`'s command toward bot `owner`'s share, as just used.
    /// The share is the process bound (64 by default, Codex's number for a
    /// session's processes): one past it kills the bot's command it used
    /// least recently, as Codex prunes, so an agent that loses track of
    /// what it started cannot hold the whole bound forever. With no bound,
    /// nothing is pruned.
    pub fn own(&self, stopper: &Stopper, owner: i64) {
        let pruned = {
            let mut stops = self.stops.lock().unwrap();
            let used = stops.tick();
            let Some(entry) = stops.running.get_mut(&stopper.listed.id) else {
                return;
            };
            (entry.owner, entry.used) = (Some(owner), used);
            let own = (stops.running.iter())
                .filter(|(_, stop)| stop.owner == Some(owner))
                .map(|(id, stop)| (*id, stop.used));
            let least = match self.budget > 0 && own.clone().count() > self.budget {
                true => own.min_by_key(|(_, used)| *used).map(|(id, _)| id),
                false => None,
            };
            least.and_then(|id| stops.running.remove(&id))
        };
        if let Some(pruned) = pruned {
            let _ = pruned.sender.send(Kill::Pruned);
        }
    }

    /// Background commands `ids` were just used: a wait named them.
    pub fn used(&self, ids: &[i64]) {
        let mut stops = self.stops.lock().unwrap();
        for id in ids {
            let used = stops.tick();
            if let Some(entry) = stops.running.get_mut(id) {
                entry.used = used;
            }
        }
    }

    /// Start detached command `command` in `workspace` as background
    /// command `id`, whose `stopper` stops it: it outlives turns and the
    /// daemon, and its end goes to `done` once nothing it left in its
    /// group runs, if this daemon still does. One that does not start
    /// sends its error there too.
    #[allow(clippy::too_many_arguments)]
    pub async fn detached(
        &self,
        cord: Cord,
        stopper: Stopper,
        command: &str,
        workspace: &Path,
        environment: &[(String, String)],
        sandbox: Option<Arc<Policy>>,
        done: tokio::sync::oneshot::Sender<Result<Outcome>>,
    ) -> Result<Detached> {
        let tracked = Tracked { stopper, done };
        detach(
            cord,
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
        let permit = self.lasting_found();
        let started = std::time::Instant::now()
            .checked_sub(elapsed)
            .unwrap_or_else(std::time::Instant::now);
        // Adopted only with its start known, which identifies its leader.
        let Some(since) = detached.since else {
            return false;
        };
        tokio::spawn(watch_detached(
            self.pulls.subscribe(),
            Leader::Found(since),
            detached.pid,
            sandboxed,
            started,
            Some(Tracked { stopper, done }),
            permit,
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

    /// Resolves once the daemon stops.
    pub fn until_stopping(&self) -> impl Future<Output = ()> + use<> {
        let mut stopping = self.stopping.subscribe();
        async move {
            let _ = stopping.wait_for(|stopping| *stopping).await;
        }
    }

    /// End every background command still running, as the daemon stops.
    pub fn end_background(&self) {
        self.stopping.send_replace(true);
    }
}

/// How a background command that was killed answers: by its agent, or
/// pruned as its agent started one past its share.
fn stopped(kill: Kill) -> Error {
    Error::with(
        "stopped",
        match kill {
            Kill::Agent => {
                "its agent killed this command before it finished, so it may have had effects"
            }
            Kill::Pruned => {
                "its agent started more background commands than the daemon's process bound, so this one, the one it used least recently, was killed before it finished; it may have had effects"
            }
        },
    )
}

/// Why a background command was killed.
#[derive(Clone, Copy, Debug)]
enum Kill {
    /// Its agent killed it by its handle.
    Agent,
    /// Its agent started one past its share, and it was the one used least
    /// recently.
    Pruned,
}

/// The background commands running, by proc handle.
#[derive(Default)]
struct Stops {
    running: HashMap<i64, Stop>,
    /// Orders uses: each start and each wait takes the next.
    ticks: u64,
}

impl Stops {
    fn tick(&mut self) -> u64 {
        self.ticks += 1;
        self.ticks
    }
}

/// What kills one background command, whose bot it counts for once its
/// command runs, and when that bot last used it.
struct Stop {
    sender: tokio::sync::oneshot::Sender<Kill>,
    owner: Option<i64>,
    used: u64,
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
    stop: tokio::sync::oneshot::Receiver<Kill>,
    listed: Stoppable,
}

impl Stopper {
    /// Refuse a start its agent killed before.
    pub fn check(&mut self) -> Result<()> {
        match self.stop.try_recv() {
            Ok(_) => Err(stopped_before_start()),
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
    stops: Arc<std::sync::Mutex<Stops>>,
    id: i64,
}

impl Drop for Stoppable {
    fn drop(&mut self) {
        self.stops.lock().unwrap().running.remove(&self.id);
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

/// `page_lines` of a retained stream's file, read a line at a time up to
/// the page and counted past it, so a file of a gigabyte is never held
/// whole. None when it is gone or no longer a file; links are not followed.
#[cfg(unix)]
pub fn page_file(file: &str, offset: usize, limit: usize) -> Result<Option<String>> {
    use std::io::BufRead;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(opened) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(file)
    else {
        return Ok(None);
    };
    let found = opened.metadata()?;
    if !found.is_file() {
        return Ok(None);
    }
    // One that is larger was not written here; one that grows after is
    // read only as far as it was.
    if found.len() > FILE_RETAINED_BYTES {
        return fail("storage_error");
    }
    let mut reader =
        std::io::BufReader::with_capacity(256 << 10, std::io::Read::take(opened, found.len()));
    // The page's lines, as `str::lines` reads them; each kept to just past
    // the page's bound, which is all `page` needs to refuse a longer one,
    // and none kept once the page is full.
    let first = offset - 1;
    let (mut window, mut kept) = (Vec::<String>::new(), 0);
    let (mut total, mut line, mut partial) = (0usize, Vec::new(), false);
    let shown =
        |total: usize, kept: usize| total >= first && total < first + limit && kept < PREVIEW_BYTES;
    let ended =
        |line: &mut Vec<u8>, total: &mut usize, kept: &mut usize, window: &mut Vec<String>| {
            if shown(*total, *kept) {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let text = String::from_utf8_lossy(line).into_owned();
                *kept += text.len();
                window.push(text);
            }
            line.clear();
            *total += 1;
        };
    loop {
        let read = reader.fill_buf()?;
        if read.is_empty() {
            break;
        }
        let (end, newline) = match read.iter().position(|&b| b == b'\n') {
            Some(at) => (at, true),
            None => (read.len(), false),
        };
        if shown(total, kept) {
            let room = (PREVIEW_BYTES + 1).saturating_sub(line.len());
            line.extend_from_slice(&read[..end.min(room)]);
        }
        reader.consume(end + usize::from(newline));
        partial = !newline;
        if newline {
            ended(&mut line, &mut total, &mut kept, &mut window);
        }
    }
    if partial {
        ended(&mut line, &mut total, &mut kept, &mut window);
    }
    let lines = (0..total).map(|index| {
        (index.checked_sub(first))
            .and_then(|at| window.get(at))
            .map_or("", String::as_str)
    });
    page(lines, offset, limit, PREVIEW_BYTES, false).map(Some)
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
    allowed(sandbox, target, path, Policy::can_write).await
}

/// [`writable`], for removing `target` itself rather than writing through it.
async fn removable(sandbox: Option<Arc<Policy>>, target: &Path, path: &str) -> Result<()> {
    allowed(sandbox, target, path, Policy::can_remove).await
}

async fn allowed(
    sandbox: Option<Arc<Policy>>,
    target: &Path,
    path: &str,
    check: fn(&Policy, &Path) -> bool,
) -> Result<()> {
    let Some(policy) = sandbox else {
        return Ok(());
    };
    let target = target.to_owned();
    let allowed = tokio::task::spawn_blocking(move || check(&policy, &target))
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
    let retained = if dropped == 0 {
        "the full output is retained".to_owned()
    } else {
        format!(
            "{dropped} more past 1 MiB were not kept, and its first and last {KEPT_END} bytes are retained"
        )
    };
    truncated(text, &retained)
}

/// Head and tail of an oversized output, saying what is `retained`. One
/// that redaction left within a preview is shown whole, with the same word.
fn truncated(text: &str, retained: &str) -> String {
    if text.len() <= PREVIEW_BYTES {
        return format!(
            "{text}\n[... {retained}: see this result's artifacts and read them with the read tool ...]"
        );
    }
    let half = PREVIEW_BYTES / 2;
    let head = &text[..boundary(text, half)];
    let tail_start = boundary(text, text.len() - half);
    let omitted = text.len() - head.len() - (text.len() - tail_start);
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
/// that its first and last `KEPT_END` bytes and a count of the rest. With
/// a [`Spill`], a stream that passes the bound is written whole to a file
/// as it is read, and these bytes are only what the model is shown.
#[derive(Default)]
struct Captured {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    dropped: u64,
    /// Where the whole stream goes once it passes the bound, the stream's
    /// name, and what redacts it there.
    spill: Option<(Spill, &'static str, Credentials)>,
    /// What writes it there, once it has passed.
    writer: Option<Writer>,
    /// How many of a command's streams are waiting for their writer to
    /// take what they read: waiting on the disk, not on the pipe.
    waiting: Arc<std::sync::atomic::AtomicUsize>,
    /// The file that holds it, once its writer is done.
    spilled: Option<Spilled>,
}

/// The most a retained stream's file holds: `SPILL_BYTES` of output, each
/// invalid byte made at most three, as a stored copy's are.
pub const FILE_RETAINED_BYTES: u64 = 3 * SPILL_BYTES;

/// Where a command's streams go whole as it runs, once one passes
/// `ARTIFACT_BYTES`: a file each, `FOLDER/NAME-STREAM`, in the bot's
/// outputs folder. What a stream within the bound writes never reaches it.
#[derive(Clone)]
pub struct Spill {
    pub folder: PathBuf,
    pub name: String,
    /// Whether the folder may hold them, asked on a blocking worker as a
    /// stream first passes the bound: not while the bot's commands may
    /// write there, since one could put a link there for the daemon.
    pub usable: Arc<dyn Fn() -> bool + Send + Sync>,
}

/// A stream's writer: what was read goes to it in pieces, written in
/// batches on blocking workers, so the runtime's thread never waits on the
/// disk. A full line of pieces makes the reader wait, and the command with
/// it.
struct Writer {
    /// Closed once the writer stops taking more: at its bound, or failed.
    pieces: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    /// Read and not yet taken by the writer: kept here while the reader
    /// waits, so a wait cut short loses none of it.
    pending: Option<Vec<u8>>,
    done: tokio::task::JoinHandle<Option<Spilled>>,
}

impl Writer {
    /// Hand the pending piece to the writer once it has room, counted in
    /// `waiting` meanwhile. Cut short, the piece stays pending.
    async fn hand(&mut self, waiting: &std::sync::atomic::AtomicUsize) {
        use std::sync::atomic::Ordering;
        struct Waiting<'a>(&'a std::sync::atomic::AtomicUsize);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let (Some(pieces), Some(_)) = (&self.pieces, &self.pending) else {
            return;
        };
        waiting.fetch_add(1, Ordering::Relaxed);
        let _waiting = Waiting(waiting);
        let closed = match pieces.reserve().await {
            Ok(room) => {
                room.send(self.pending.take().unwrap_or_default());
                false
            }
            Err(_) => true,
        };
        if closed {
            self.pieces = None;
            self.pending = None;
        }
    }
}

/// A stream written to its file: `saved` bytes of it, all of it unless
/// it passed `SPILL_BYTES`.
struct Spilled {
    files: Files,
    saved: u64,
}

/// Pieces of a stream waiting for its writer: a few reads' worth.
const SPILL_PIECES: usize = 16;

impl Captured {
    fn spilling(
        spill: Option<&Spill>,
        stream: &'static str,
        registry: &Registry,
        waiting: &Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Captured {
            spill: spill.map(|spill| (spill.clone(), stream, registry.credentials.clone())),
            waiting: waiting.clone(),
            ..Captured::default()
        }
    }

    /// Hand `bytes`, just read, to the writer, starting it with all read
    /// before when they take the stream past the bound.
    async fn spill(&mut self, bytes: &[u8]) {
        if self.writer.is_none() {
            if self.len() + bytes.len() <= ARTIFACT_BYTES {
                return;
            }
            let Some((spill, stream, credentials)) = self.spill.take() else {
                return;
            };
            // Past the bound nothing was dropped yet: the head and tail are
            // all of it so far.
            let mut first = Vec::with_capacity(self.len());
            first.extend_from_slice(&self.head);
            first.extend(self.tail.iter());
            let (pieces, received) = tokio::sync::mpsc::channel(SPILL_PIECES);
            let done = tokio::spawn(spill_stream(spill, stream, credentials, first, received));
            self.writer = Some(Writer {
                pieces: Some(pieces),
                pending: None,
                done,
            });
        }
        let Some(writer) = &mut self.writer else {
            return;
        };
        if writer.pieces.is_some() {
            writer.pending = Some(bytes.to_vec());
            writer.hand(&self.waiting).await;
        }
    }

    /// Wait for the writer, when the stream had one, to take what it was
    /// still to be handed and finish its file.
    async fn settle(&mut self) {
        if let Some(mut writer) = self.writer.take() {
            writer.hand(&self.waiting).await;
            drop(writer.pieces);
            self.spilled = writer.done.await.ok().flatten();
        }
    }

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
    /// many bytes that was. `filed` says the whole stream is in its file,
    /// where the middle is.
    fn finish(self, filed: bool) -> (Vec<u8>, u64) {
        let Captured {
            mut head,
            mut tail,
            mut dropped,
            ..
        } = self;
        // Output after the last trim can leave a dropped tail long, short of
        // the room the marker needs.
        if dropped > 0 && tail.len() > KEPT_END {
            let cut = tail.len() - KEPT_END;
            tail.drain(..cut);
            dropped += cut as u64;
        }
        if dropped > 0 {
            let marker = match filed {
                true => format!(
                    "\n[... {dropped} bytes not shown here: they are in this stream's file ...]\n"
                ),
                false => format!(
                    "\n[... {dropped} bytes dropped: past 1 MiB only the first and last {KEPT_END} bytes of this stream are kept ...]\n"
                ),
            };
            head.extend_from_slice(marker.as_bytes());
        }
        head.extend(tail);
        (head, dropped)
    }
}

/// Write a stream whole to its file in `spill`'s folder, redacted as it
/// goes: `first`, then each piece as it comes, up to `SPILL_BYTES`. None
/// when the folder may not hold it or a write fails, the file removed, and
/// the stream's bounded copy is kept as before. The disk is used only on
/// blocking workers, a batch of pieces at a time: none is held while the
/// command writes nothing, so idle streams never fill the blocking pool.
async fn spill_stream(
    spill: Spill,
    stream: &'static str,
    credentials: Credentials,
    first: Vec<u8>,
    mut pieces: tokio::sync::mpsc::Receiver<Vec<u8>>,
) -> Option<Spilled> {
    let folder = spill.folder.clone();
    let mut file =
        tokio::task::spawn_blocking(move || SpillFile::open(&spill, stream, credentials))
            .await
            .ok()??;
    let mut batch = vec![first];
    loop {
        let (back, full) = tokio::task::spawn_blocking(move || {
            let full = file.write(&batch)?;
            Some((file, full))
        })
        .await
        .ok()??;
        file = back;
        if full {
            break;
        }
        let Some(piece) = pieces.recv().await else {
            break;
        };
        batch = vec![piece];
        while batch.len() < SPILL_PIECES
            && let Ok(piece) = pieces.try_recv()
        {
            batch.push(piece);
        }
    }
    // Past the bound the reader stops sending once this end closes.
    pieces.close();
    let (path, saved) = tokio::task::spawn_blocking(move || file.close(&folder))
        .await
        .ok()??;
    Some(Spilled {
        files: Files::new(vec![(stream, path.to_str()?.to_owned())]),
        saved,
    })
}

/// A stream's file while it is written, used on blocking workers only.
struct SpillFile {
    path: PathBuf,
    out: std::io::BufWriter<std::fs::File>,
    redacting: Redacting,
    saved: u64,
}

impl SpillFile {
    fn open(spill: &Spill, stream: &'static str, credentials: Credentials) -> Option<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        if !(spill.usable)() {
            return None;
        }
        let path = spill.folder.join(format!("{}-{stream}", spill.name));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .ok()?;
        Some(SpillFile {
            path,
            out: std::io::BufWriter::with_capacity(256 << 10, file),
            redacting: Redacting::new(credentials),
            saved: 0,
        })
    }

    /// Write `batch`, redacted, up to `SPILL_BYTES`: whether that bound
    /// was reached, or None, the file removed, when a write failed.
    fn write(&mut self, batch: &[Vec<u8>]) -> Option<bool> {
        use std::io::Write;
        for bytes in batch {
            let fits = (bytes.len() as u64).min(SPILL_BYTES - self.saved) as usize;
            let told = self.redacting.feed(&bytes[..fits]);
            if self.out.write_all(told.as_bytes()).is_err() {
                let _ = std::fs::remove_file(&self.path);
                return None;
            }
            self.saved += fits as u64;
            if fits < bytes.len() {
                return Some(true);
            }
        }
        Some(false)
    }

    /// Finish the file, on disk with its name in `folder` before the store
    /// records it: its path and how many bytes of the stream it holds.
    fn close(self, folder: &Path) -> Option<(PathBuf, u64)> {
        use std::io::Write;
        let SpillFile {
            path,
            mut out,
            redacting,
            saved,
        } = self;
        let written = (|| {
            out.write_all(redacting.finish().as_bytes())?;
            out.flush()?;
            out.get_ref().sync_all()?;
            std::fs::File::open(folder)?.sync_all()
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&path);
            return None;
        }
        Some((path, saved))
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
        if captured.spill.is_some() || captured.writer.is_some() {
            captured.spill(&chunk[..read]).await;
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
    cord: Cord,
    command: &str,
    workspace: &Path,
    registry: &Registry,
    environment: &[(String, String)],
    sandbox: Option<Arc<Policy>>,
    mut tracked: Option<Tracked>,
) -> Result<Detached> {
    // The cord as it was when the start was decided: a pull since, during
    // the setup too, ends the command as soon as its watcher has it.
    let Cord(pulls, _) = cord;
    let stopper = tracked.as_mut().map(|tracked| &mut tracked.stopper);
    let spawned = match Cord(pulls.clone(), registry.stopping.subscribe()).check() {
        Ok(()) => spawn_detached(command, workspace, registry, environment, sandbox, stopper).await,
        Err(error) => Err(error),
    };
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
    // The watcher owns the child from here, before anything is awaited:
    // a caller dropped meanwhile leaves no command that nothing watches.
    let (read, since) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        // Its leader's start, read off the runtime thread as every file is.
        let pid = detached.pid;
        let since = tokio::task::spawn_blocking(move || leader_since(pid))
            .await
            .ok()
            .flatten();
        let _ = read.send(since);
        watch_detached(
            pulls,
            Leader::Child(child),
            pid,
            sandboxed,
            started,
            tracked,
            held,
        )
        .await;
    });
    Ok(Detached {
        since: since.await.ok().flatten(),
        ..detached
    })
}

/// A detached command started: its child, its pid (its leader's start is
/// read once its watcher owns it), whether a sandbox holds it, when it
/// started, and what it holds while it runs.
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
    stopper: Option<&mut Stopper>,
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
    // A stop on its handle during the setup: it never runs.
    if let Some(stopper) = stopper {
        stopper.check()?;
    }
    let started = std::time::Instant::now();
    let child = process.spawn().map_err(|e| not_started(e, workspace))?;
    let pid = child.id().ok_or(Error::new("shell_spawn_failed"))?;
    let held: Box<dyn Send> = Box::new((permit, held, listening));
    Ok((
        child,
        Detached { pid, since: None },
        sandboxed,
        started,
        held,
    ))
}

/// The leader of a detached command's group, as its watcher knows it.
#[cfg(unix)]
enum Leader {
    /// This daemon started it: its child, whose exit it waits on.
    Child(tokio::process::Child),
    /// An earlier daemon did: its start time, read once a second while it
    /// runs, one file, never a walk of every process.
    Found(u64),
}

/// Watch detached command `pid` until nothing it left in its group runs:
/// its `leader`, the child this daemon started or one an earlier daemon
/// did, found again, then what it left. A pull of the cord or a stop on its handle kills the
/// group, and with `sandboxed` the pid namespace it runs in; `held` (its
/// lasting slot, what its sandbox keeps in place) goes with its end, which
/// goes to `tracked`'s `done`.
#[cfg(unix)]
async fn watch_detached(
    mut pulls: watch::Receiver<u64>,
    leader: Leader,
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
    if let Leader::Found(since) = leader {
        let runs = move || async move {
            tokio::task::spawn_blocking(move || leader_runs(pid, since))
                .await
                .unwrap_or(false)
        };
        while ended.is_none() && runs().await {
            tokio::select! {
                Ok(()) = pulls.changed() => {
                    end().await;
                    ended = Some(Err(stopped_by_pause()));
                }
                kill = stop_sent(&mut stop) => {
                    end().await;
                    ended = Some(Err(stopped(kill)));
                }
                () = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
            }
        }
    } else if let Leader::Child(mut child) = leader {
        ended = tokio::select! {
            // Its end is the group's, below; the leader's status is what
            // it says of how it went.
            status = child.wait() => Some(Ok(status.ok())),
            Ok(()) = pulls.changed() => {
                end().await;
                let _ = child.wait().await;
                Some(Err(stopped_by_pause()))
            }
            kill = stop_sent(&mut stop) => {
                end().await;
                let _ = child.wait().await;
                Some(Err(stopped(kill)))
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
            kill = stop_sent(&mut stop) => {
                end().await;
                ended = Some(Err(stopped(kill)));
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
async fn stop_sent(stop: &mut Option<tokio::sync::oneshot::Receiver<Kill>>) -> Kill {
    if let Some(receiver) = stop {
        let got = receiver.await;
        // A finished receiver is never polled again.
        *stop = None;
        if let Ok(kill) = got {
            return kill;
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

/// Whether process `pid`, which started at `since`, still runs: on Linux
/// not a zombie, which an init that does not reap orphans leaves for good,
/// unless another of its threads runs. Elsewhere init reaps orphans.
#[cfg(target_os = "linux")]
fn leader_runs(pid: u32, since: u64) -> bool {
    leader_since(pid) == Some(since)
        && stat(Path::new(&format!("/proc/{pid}/stat")))
            .is_some_and(|(state, _)| !exited(state) || threads_run(&pid.to_string()))
}
#[cfg(all(unix, not(target_os = "linux")))]
fn leader_runs(pid: u32, since: u64) -> bool {
    leader_since(pid) == Some(since)
}

/// Whether detached command `detached`, which an earlier daemon started,
/// still runs as itself: its leader is the process that started then, by
/// its start time, not a later one given its pid, has not exited, and its
/// group has a member. A group whose leader has gone is not taken for it: while no
/// daemon watched, the group may have ended and its id gone to another,
/// and a stop on the handle would kill that one.
#[cfg(unix)]
fn still_runs(detached: Detached) -> bool {
    let Some(since) = detached.since else {
        return false;
    };
    leader_runs(detached.pid, since) && !matches!(group_members(detached.pid as i32), Members::Gone)
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
    _: Cord,
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
#[allow(clippy::too_many_arguments)]
async fn shell(
    command: &str,
    workspace: &Path,
    timeout: Option<Duration>,
    registry: &Registry,
    environment: &[(String, String)],
    sandbox: Option<&Policy>,
    spill: Option<&Spill>,
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
    let waiting = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (mut out, mut err) = (
        Captured::spilling(spill, "stdout", registry, &waiting),
        Captured::spilling(spill, "stderr", registry, &waiting),
    );
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
        // A stream waiting for its file to take what it read is waiting on
        // the disk, not on its pipe, and is waited for.
        while !read {
            match tokio::time::timeout(PIPES_END, &mut reads).await {
                Ok(done) => {
                    done?;
                    read = true;
                }
                Err(_) if waiting.load(std::sync::atomic::Ordering::Relaxed) > 0 => {}
                Err(_) => break,
            }
        }
        status
    };
    let exit = status.map_or(Exit::TimedOut, Exit::Status);
    let elapsed = started.elapsed();
    // A stream's file is finished by its caller, once the command's slot
    // is given back: see `Captured::settle`.
    Ok((out, err, exit, elapsed))
}

#[cfg(not(unix))]
async fn shell(
    _: &str,
    _: &Path,
    _: Option<Duration>,
    _: &Registry,
    _: &[(String, String)],
    _: Option<&Policy>,
    _: Option<&Spill>,
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
        assert!(registry.stops.lock().unwrap().running.contains_key(&7));
        registry.end_running();
        let ended = running.await.unwrap().unwrap_err();
        assert_eq!(ended.code, "daemon_paused");
        assert!(registry.stops.lock().unwrap().running.is_empty());
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
                None,
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
        let (bytes, dropped) = whole.finish(false);
        assert_eq!((bytes.len(), dropped), (ARTIFACT_BYTES, 0));

        let mut over = Captured::default();
        over.head.extend(vec![b'h'; KEPT_END]);
        for _ in 0..40 {
            over.keep(&vec![b'm'; 64 * 1024]);
        }
        over.keep(b"end");
        let (bytes, dropped) = over.finish(false);
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
        // A header that doesn't end its line is a scanner's pattern, not a
        // key: every line after it is kept.
        let scanner = "re = re.compile(\"-----BEGIN RSA PRIVATE KEY-----\")\nfor line in lines:\n    scan(line)\n";
        assert_eq!(redact(scanner), scanner);
        let quoted = "keys = ['-----BEGIN PRIVATE KEY-----']\nprint(keys)\n";
        assert_eq!(redact(quoted), quoted);
        // With no END, only the key's own lines are hidden: the first line
        // that can't be in a key's body ends it.
        assert_eq!(
            redact(
                "KEY = \"\"\"\n-----BEGIN PRIVATE KEY-----\nMIIE+/a=\nAAAA\n\"\"\"\nprint(KEY)\n"
            ),
            "KEY = \"\"\"\n-----BEGIN PRIVATE KEY-----\n[REDACTED]\n\"\"\"\nprint(KEY)\n"
        );
        // A body line past the longest a line is read is still dropped
        // whole, and the code after it kept.
        let long = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\nprint(KEY)\n",
            "A".repeat(70_000)
        );
        assert_eq!(
            redact(&long),
            "-----BEGIN PRIVATE KEY-----\n[REDACTED]\nprint(KEY)\n"
        );
        let mut redacting = Redacting::new(credentials.clone());
        let mut out = String::new();
        for piece in long.as_bytes().chunks(4096) {
            out.push_str(&redacting.feed(piece));
        }
        out.push_str(&redacting.finish());
        assert_eq!(out, redact(&long));
        // As the read tool shows it, each line numbered.
        let numbered = "     1\t-----BEGIN PRIVATE KEY-----\n     2\tMIIE+/a=\n     3\tAAAA\n     4\t-----END PRIVATE KEY-----\n";
        assert_eq!(
            redact(numbered),
            "     1\t-----BEGIN PRIVATE KEY-----\n[REDACTED]\n     4\t-----END PRIVATE KEY-----\n"
        );
        // An encrypted key's too.
        let numbered = "     1\t-----BEGIN RSA PRIVATE KEY-----\n     2\tProc-Type: 4,ENCRYPTED\n     3\tDEK-Info: AES-128-CBC,00\n     4\t\n     5\tMIIE\n     6\t-----END RSA PRIVATE KEY-----\n";
        assert_eq!(
            redact(numbered),
            "     1\t-----BEGIN RSA PRIVATE KEY-----\n[REDACTED]\n     6\t-----END RSA PRIVATE KEY-----\n"
        );
        // Spaces after the header, a blank line after it, and indented
        // body lines, as a key's reader takes them.
        assert_eq!(
            redact(
                "-----BEGIN PRIVATE KEY-----   \n\n  MIIE\n\tAAAA \nMIIEvQIB ADAN\tBgkq\n-----END PRIVATE KEY-----\n"
            ),
            "-----BEGIN PRIVATE KEY-----   \n[REDACTED]\n-----END PRIVATE KEY-----\n"
        );
        // A search's numbers and paths before each line, as `rg -n` and
        // `grep -n -A` print them: the body is still hidden to its END,
        // and the line after it shows.
        for (head, sep) in [("", ":"), ("keys/id.pem:", ":"), ("keys/id.pem-", "-")] {
            let prefix = |n: usize| format!("{head}{n}{sep}");
            let end = "after";
            let text = format!(
                "{}-----BEGIN OPENSSH PRIVATE KEY-----\n{}b3BlbnNzaC1rZXktdjEAAAAA\n{}QyNTUxOQAAACA=\n\
                 {}-----END OPENSSH PRIVATE KEY-----\n{}{end}\n",
                prefix(1),
                prefix(2),
                prefix(3),
                prefix(4),
                prefix(5)
            );
            let hidden = redact(&text);
            assert!(
                !hidden.contains("b3Blbn") && !hidden.contains("QyNTUx"),
                "{hidden}"
            );
            assert!(
                hidden.ends_with(&format!(
                    "[REDACTED]\n{}-----END OPENSSH PRIVATE KEY-----\n{}{end}\n",
                    prefix(4),
                    prefix(5)
                )),
                "{hidden}"
            );
        }
        // A long run of dashes is read once.
        let run = format!(
            "{}\n-----BEGIN PRIVATE KEY-----\nMIIE\n{}",
            "-".repeat(1 << 20),
            "-".repeat(1 << 20)
        );
        let began = std::time::Instant::now();
        let mut redacting = Redacting::new(credentials.clone());
        let mut out = String::new();
        for piece in run.as_bytes().chunks(8192) {
            out.push_str(&redacting.feed(piece));
        }
        out.push_str(&redacting.finish());
        assert_eq!(out, redact(&run));
        assert!(began.elapsed() < std::time::Duration::from_secs(5));
        // Many starts on one long line read it once, whole or in pieces.
        let starts = "-----BEGIN ".repeat(40_000) + "\nend\n";
        let began = std::time::Instant::now();
        assert_eq!(redact(&starts), starts);
        let mut redacting = Redacting::new(credentials.clone());
        let mut out = String::new();
        for piece in starts.as_bytes().chunks(8192) {
            out.push_str(&redacting.feed(piece));
        }
        out.push_str(&redacting.finish());
        assert_eq!(out, starts);
        assert!(began.elapsed() < std::time::Duration::from_secs(5));
        // An encrypted key's headers and their blank line are its body.
        assert_eq!(
            redact(
                "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00\n\nMIIE\n-----END RSA PRIVATE KEY-----\n"
            ),
            "-----BEGIN RSA PRIVATE KEY-----\n[REDACTED]\n-----END RSA PRIVATE KEY-----\n"
        );
        // A key as JSON holds it, its line ends escaped, under any name or
        // none, and escaped again inside another string.
        for (key, hidden) in [
            (
                r#"{"value": "-----BEGIN PRIVATE KEY-----\nMIIEvQIB+/a=\nAAAA\n-----END PRIVATE KEY-----\n", "n": 1}"#,
                r#"{"value": "-----BEGIN PRIVATE KEY-----\n[REDACTED]\n-----END PRIVATE KEY-----\n", "n": 1}"#,
            ),
            (
                r#""-----BEGIN OPENSSH PRIVATE KEY-----\r\nb3BlbnNzaC1rZXk\r\n-----END OPENSSH PRIVATE KEY-----""#,
                r#""-----BEGIN OPENSSH PRIVATE KEY-----\r\n[REDACTED]\r\n-----END OPENSSH PRIVATE KEY-----""#,
            ),
            (
                r#"{"body": "{\"key\": \"-----BEGIN RSA PRIVATE KEY-----\\nProc-Type: 4,ENCRYPTED\\nDEK-Info: AES-128-CBC,00\\n\\nMIIE\\n-----END RSA PRIVATE KEY-----\\n\"}"}"#,
                r#"{"body": "{\"key\": \"-----BEGIN RSA PRIVATE KEY-----\\n[REDACTED]\\n-----END RSA PRIVATE KEY-----\\n\"}"}"#,
            ),
            (
                "{\"value\": \"-----BEGIN PRIVATE KEY-----  \t\\nMIIE\\n-----END PRIVATE KEY-----\\n\"}",
                "{\"value\": \"-----BEGIN PRIVATE KEY-----  \t\\n[REDACTED]\\n-----END PRIVATE KEY-----\\n\"}",
            ),
            (
                r#""-----BEGIN PGP PRIVATE KEY BLOCK-----\nComment: example -----BEGIN CERTIFICATE-----\n\nlQOYBGfake\n-----END PGP PRIVATE KEY BLOCK-----\n""#,
                r#""-----BEGIN PGP PRIVATE KEY BLOCK-----\n[REDACTED]\n-----END PGP PRIVATE KEY BLOCK-----\n""#,
            ),
            (
                r#"{"value": "-----BEGIN PGP PRIVATE KEY BLOCK-----\nComment: Fixture (no identity) <x@y> it's \"quoted\"\n\nlQOYBGfake\n=AbCd\n-----END PGP PRIVATE KEY BLOCK-----\n"}"#,
                r#"{"value": "-----BEGIN PGP PRIVATE KEY BLOCK-----\n[REDACTED]\n-----END PGP PRIVATE KEY BLOCK-----\n"}"#,
            ),
        ] {
            assert_eq!(redact(key), hidden);
            for size in 1..=key.len() {
                let mut redacting = Redacting::new(credentials.clone());
                let mut out = String::new();
                for piece in key.as_bytes().chunks(size) {
                    out.push_str(&redacting.feed(piece));
                }
                out.push_str(&redacting.finish());
                assert_eq!(out, hidden, "in pieces of {size}");
            }
        }
        // A whole block is hidden through its END, whatever its lines
        // hold: an armored key's headers among them.
        let armored = "-----BEGIN PGP PRIVATE KEY BLOCK-----\nComment: A fixture (no identity) <x@y>\n\nlQOYBGfake\n=AbCd\n-----END PGP PRIVATE KEY BLOCK-----\nafter\n";
        let hidden = "-----BEGIN PGP PRIVATE KEY BLOCK-----\n[REDACTED]\n-----END PGP PRIVATE KEY BLOCK-----\nafter\n";
        assert_eq!(redact(armored), hidden);
        for size in 1..=armored.len() {
            let mut redacting = Redacting::new(credentials.clone());
            let mut out = String::new();
            for piece in armored.as_bytes().chunks(size) {
                out.push_str(&redacting.feed(piece));
            }
            out.push_str(&redacting.finish());
            assert_eq!(out, hidden, "in pieces of {size}");
        }
        // A BEGIN inside an armored key's `Comment:` is that comment's
        // text: the key is still hidden through its own END.
        let commented = "-----BEGIN PGP PRIVATE KEY BLOCK-----\nComment: example -----BEGIN CERTIFICATE-----\n\nlQOYBGfake\n-----END PGP PRIVATE KEY BLOCK-----\nafter\n";
        let hidden = "-----BEGIN PGP PRIVATE KEY BLOCK-----\n[REDACTED]\n-----END PGP PRIVATE KEY BLOCK-----\nafter\n";
        assert_eq!(redact(commented), hidden);
        for size in 1..=commented.len() {
            let mut redacting = Redacting::new(credentials.clone());
            let mut out = String::new();
            for piece in commented.as_bytes().chunks(size) {
                out.push_str(&redacting.feed(piece));
            }
            out.push_str(&redacting.finish());
            assert_eq!(out, hidden, "in pieces of {size}");
        }
        // A long body line of dashes and BEGINs inside it is read once.
        let dashed = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            "a -----x a -----BEGIN x ".repeat(8_000)
        );
        let began = std::time::Instant::now();
        assert_eq!(
            redact(&dashed),
            "-----BEGIN PRIVATE KEY-----\n[REDACTED]\n-----END PRIVATE KEY-----\n"
        );
        let mut redacting = Redacting::new(credentials.clone());
        let mut out = String::new();
        for piece in dashed.as_bytes().chunks(8192) {
            out.push_str(&redacting.feed(piece));
        }
        out.push_str(&redacting.finish());
        assert_eq!(out, redact(&dashed));
        assert!(began.elapsed() < std::time::Duration::from_secs(2));
        // A body line run into its END is hidden with it.
        assert_eq!(
            redact("-----BEGIN PRIVATE KEY-----\nMIIE\nAAAA-----END PRIVATE KEY-----\n"),
            "-----BEGIN PRIVATE KEY-----\n[REDACTED]\n-----END PRIVATE KEY-----\n"
        );
        // A header then a run of backslashes is read once, and held no
        // longer than an escape is.
        let slashes = format!("-----BEGIN PRIVATE KEY-----{}", "\\".repeat(4 << 20));
        let began = std::time::Instant::now();
        let mut redacting = Redacting::new(credentials.clone());
        let mut out = String::new();
        for piece in slashes.as_bytes().chunks(8192) {
            out.push_str(&redacting.feed(piece));
        }
        out.push_str(&redacting.finish());
        assert_eq!(out, slashes);
        assert!(began.elapsed() < std::time::Duration::from_secs(5));
        // Many headers with no END read their text once, whole or in
        // pieces: each looks for its END only up to the next BEGIN.
        let headers = "-----BEGIN PRIVATE KEY-----\nprint(1)\n".repeat(40_000);
        let began = std::time::Instant::now();
        let whole = redact(&headers);
        let mut redacting = Redacting::new(credentials.clone());
        let mut out = String::new();
        for piece in headers.as_bytes().chunks(8192) {
            out.push_str(&redacting.feed(piece));
        }
        out.push_str(&redacting.finish());
        assert_eq!(out, whole);
        assert_eq!(whole, headers);
        assert!(began.elapsed() < std::time::Duration::from_secs(5));
        // An escaped header with no END on its line, or code between it
        // and one, is a pattern: kept.
        for kept in [
            r#"re.compile("-----BEGIN PRIVATE KEY-----\n")"#,
            r#"a = "-----BEGIN PRIVATE KEY-----\n"; b = "-----END PRIVATE KEY-----""#,
            "\"-----BEGIN PRIVATE KEY-----\\nMIIE\n-----END PRIVATE KEY-----\"",
        ] {
            assert_eq!(redact(kept), kept);
        }
    }
    /// A bot keeps at most the process bound's number of background
    /// commands, as Codex keeps a session's processes: starting one past
    /// them kills the one its bot used least recently, a wait counting as
    /// a use. Another bot's and a command no bot owns, such as a detached
    /// one, never count, and with no bound nothing is pruned.
    #[test]
    fn a_bot_past_its_share_loses_the_command_it_used_least_recently() {
        let registry = Registry::new("shell").unwrap().with_process_budget(4);
        let pruned = |stopper: &mut Stopper| matches!(stopper.stop.try_recv(), Ok(Kill::Pruned));
        let mut own: Vec<Stopper> = (1..=4)
            .map(|id| {
                let stopper = registry.stopper(id);
                registry.own(&stopper, 7);
                stopper
            })
            .collect();
        let mut other = registry.stopper(1000);
        registry.own(&other, 8);
        let mut unowned = registry.stopper(2000);
        assert!(own.iter_mut().all(|stopper| !pruned(stopper)));
        // The oldest was waited on since, so the next oldest goes.
        registry.used(&[1]);
        let mut next = registry.stopper(5);
        registry.own(&next, 7);
        assert!(!pruned(&mut own[0]));
        assert!(pruned(&mut own[1]));
        assert!(own[2..].iter_mut().all(|stopper| !pruned(stopper)));
        assert!(!pruned(&mut other) && !pruned(&mut unowned) && !pruned(&mut next));
        // Gone from those its agent can kill: a kill by its handle finds none.
        assert!(!registry.stop(2));
        assert!(registry.stop(3));
        let unbounded = Registry::new("shell").unwrap().with_process_budget(0);
        let mut first = unbounded.stopper(1);
        unbounded.own(&first, 7);
        let _rest: Vec<Stopper> = (2..=200)
            .map(|id| {
                let stopper = unbounded.stopper(id);
                unbounded.own(&stopper, 7);
                stopper
            })
            .collect();
        assert!(!pruned(&mut first));
    }
    /// A running background command its bot pruned is killed, and says why.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pruned_background_command_is_killed_and_says_so() {
        let registry = Registry::new("shell").unwrap().with_process_budget(4);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        registry.background(
            registry.stopper(1),
            7,
            "sleep 100".to_owned(),
            std::env::temp_dir(),
            None,
            Vec::new(),
            None,
            registry.queue().unwrap(),
            sender,
            None,
        );
        let _later: Vec<Stopper> = (2..=5)
            .map(|id| {
                let stopper = registry.stopper(id);
                registry.own(&stopper, 7);
                stopper
            })
            .collect();
        let ended = tokio::time::timeout(Duration::from_secs(10), receiver)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(ended.code, "stopped");
        assert!(
            ended
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("used least recently"),
            "{ended:?}"
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
                1,
                command,
                std::env::temp_dir(),
                timeout_ms,
                Vec::new(),
                None,
                registry.queue().unwrap(),
                sender,
                None,
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

    /// A stream redacted piece by piece reads as the whole redacted at
    /// once, however it is cut: a credential, a secret field's value and a
    /// secret PEM block that span pieces are still redacted.
    #[test]
    fn a_stream_redacted_in_pieces_reads_as_the_whole_redacted() {
        let credentials = Credentials::default();
        credentials.set("TOKEN", "tok3n-fake-fake-fake9");
        let fields = ["password", "api_key", "SessionToken", "sesame"].map(String::from);
        credentials.redact_also(&fields, &["PRIVATE KEY".to_owned()]);
        let text = "start tok3n-fake-fake-fake9 é\npassword: fake8fake8fake8\n\
            {\"api_key\": \"fake9fake9fake9x\", \"n\": 1}\nexport AWS_SESSION_TOKEN=abc123def456ghi\n\
            -----BEGIN RSA PRIVATE KEY-----\nMIIEsecret\nAAAA\n-----END RSA PRIVATE KEY-----\n\
            -----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n\
            sesame=\"abc123def\\\"ghi456hidden789suffix\" shown\nend tok3n-fake-fake-fake9";
        let whole = credentials.redact(text.to_owned());
        assert!(!whole.contains("fake8") && !whole.contains("fake9") && !whole.contains("abc123"));
        assert!(!whole.contains("hidden789") && whole.contains("shown"));
        assert!(!whole.contains("secret") && whole.contains("MIIB"));
        for size in 1..=text.len() {
            let mut redacting = Redacting::new(credentials.clone());
            let mut out = String::new();
            for piece in text.as_bytes().chunks(size) {
                out.push_str(&redacting.feed(piece));
            }
            out.push_str(&redacting.finish());
            assert_eq!(out, whole, "in pieces of {size}");
        }
    }
    /// Text cut anywhere reads as the whole: a character cut at a piece's
    /// end is finished by the next, whatever invalid bytes came before.
    #[test]
    fn a_stream_cut_inside_a_character_reads_as_the_whole() {
        let credentials = Credentials::default();
        let text = b"before\xff after\xc3\xa9 end \xf0\x9f\x98\x80 \xe2\x82";
        let whole = String::from_utf8_lossy(text).into_owned();
        for size in 1..=text.len() {
            let mut redacting = Redacting::new(credentials.clone());
            let mut out = String::new();
            for piece in text.chunks(size) {
                out.push_str(&redacting.feed(piece));
            }
            out.push_str(&redacting.finish());
            assert_eq!(out, whole, "in pieces of {size}");
        }
    }
    /// Texts built from the pieces of secret fields, quotes, escapes, line
    /// ends and long runs read the same redacted piece by piece as whole,
    /// however they are cut.
    #[test]
    fn mixed_texts_redact_in_pieces_as_whole() {
        let credentials = Credentials::default();
        credentials.set("TOKEN", "tok3n-fake-fake-fake9");
        credentials.set("LONG", "abc123def456");
        credentials.set("SHORT", "Xabc123");
        credentials.redact_also(
            &["password".to_owned(), "api_key".to_owned()],
            &["PRIVATE KEY".to_owned()],
        );
        let parts: [&str; 35] = [
            "password=",
            "\"api_key\": \"",
            "password: '",
            "\"",
            "'",
            "\\",
            "\n",
            " ",
            ",",
            "k7k7k7k7",
            "é",
            "word ",
            "=",
            "x",
            "tok3n-fake-",
            "fake-fake9",
            "-----BEGIN RSA PRIVATE KEY-----\n",
            "-----END RSA PRIVATE KEY-----",
            "-----BEGIN ",
            "CERTIFICATE-----\r\n",
            "Xabc",
            "123",
            "def456",
            "-----BEGIN PRIVATE KEY-----\")",
            "-----BEGIN PRIVATE KEY-----",
            "Proc-Type: 4,ENCRYPTED\n",
            "DEK-Info: AES\n",
            "\r",
            "MIIE+/a=\n",
            "print(KEY)\n",
            "   ",
            "\t",
            "\"-----BEGIN PRIVATE KEY-----\\n",
            "MIIE+/a=\\r\\n",
            "-----END PRIVATE KEY-----\\n\"",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..100 {
            let mut text = String::new();
            for _ in 0..(next() % 40) {
                match next() % 10 {
                    0 => text.push_str(&"k7".repeat(1 + (next() % 4000) as usize)),
                    1 => text.push_str(&"y".repeat((next() % 6000) as usize)),
                    _ => text.push_str(parts[(next() % parts.len() as u64) as usize]),
                }
            }
            let whole = credentials.redact(text.clone());
            for size in [
                1 + (next() % 7) as usize,
                64 + (next() % 4096) as usize,
                9000,
            ] {
                let mut redacting = Redacting::new(credentials.clone());
                let mut out = String::new();
                for piece in text.as_bytes().chunks(size) {
                    out.push_str(&redacting.feed(piece));
                }
                out.push_str(&redacting.finish());
                assert!(out == whole, "in pieces of {size}: {text:?}");
            }
        }
    }
    /// A credential that begins a longer one cut across pieces waits for
    /// it: the longer is redacted first, as in the whole text.
    #[test]
    fn a_credential_inside_a_longer_one_cut_across_pieces_waits_for_it() {
        let credentials = Credentials::default();
        credentials.set("LONG", "abc123def456");
        credentials.set("SHORT", "Xabc123");
        for text in ["Xabc123def456", "Xabc123 then Xabc123def456 end"] {
            let whole = credentials.redact(text.to_owned());
            for size in 1..=text.len() {
                let mut redacting = Redacting::new(credentials.clone());
                let mut out = String::new();
                for piece in text.as_bytes().chunks(size) {
                    out.push_str(&redacting.feed(piece));
                }
                out.push_str(&redacting.finish());
                assert_eq!(out, whole, "in pieces of {size}");
            }
        }
    }
    /// A long value of letters alone costs each byte the same: it is held
    /// to its bound, then told as redacted.
    #[test]
    fn a_long_value_not_yet_secret_is_read_in_linear_time() {
        let credentials = Credentials::default();
        credentials.redact_also(&["password".to_owned()], &[]);
        let text = format!("password={} after", "k".repeat(2 << 20));
        let mut redacting = Redacting::new(credentials.clone());
        let mut out = String::new();
        for piece in text.as_bytes().chunks(64 << 10) {
            out.push_str(&redacting.feed(piece));
        }
        out.push_str(&redacting.finish());
        assert_eq!(out, "password=[REDACTED] after");
    }
    /// A piece handed to a stream's writer while it has no room stays
    /// pending when the wait is cut short, and is handed once it has room;
    /// the wait is counted as one on the disk meanwhile.
    #[tokio::test]
    async fn a_piece_waiting_for_its_writer_is_kept_when_the_wait_is_cut() {
        use std::sync::atomic::Ordering;
        let (pieces, mut received) = tokio::sync::mpsc::channel(1);
        pieces.send(b"first".to_vec()).await.unwrap();
        let mut writer = Writer {
            pieces: Some(pieces),
            pending: Some(b"second".to_vec()),
            done: tokio::spawn(async { None }),
        };
        let waiting = std::sync::atomic::AtomicUsize::new(0);
        {
            let hand = writer.hand(&waiting);
            tokio::pin!(hand);
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut hand)
                    .await
                    .is_err()
            );
            assert_eq!(waiting.load(Ordering::Relaxed), 1);
        }
        assert_eq!(waiting.load(Ordering::Relaxed), 0);
        assert_eq!(writer.pending.as_deref(), Some(&b"second"[..]));
        assert_eq!(received.recv().await.unwrap(), b"first");
        writer.hand(&waiting).await;
        assert_eq!(writer.pending, None);
        assert_eq!(received.recv().await.unwrap(), b"second");
    }
    /// A secret field's value longer than the window a field is looked for
    /// in is redacted whole however it is cut, as the whole text's is.
    #[test]
    fn a_long_secret_value_cut_across_pieces_is_redacted_whole() {
        let credentials = Credentials::default();
        credentials.redact_also(&["password".to_owned()], &[]);
        let value: String = (0..100_000)
            .map(|n| if n % 7 == 0 { '7' } else { 'k' })
            .collect();
        for text in [
            format!("{}\npassword={value} after\nend", "x".repeat(1 << 20)),
            format!("a \"password\": \"{value}\\\"{value}\", \"n\": 1\nend"),
            format!(
                "{}\npassword=\"{}\" after\nend",
                "x".repeat(1 << 20),
                "kkkkkkk7'k\\\"".repeat(10_000)
            ),
            format!(
                "{}\npassword=\"{}\\\n{}kkkkkkk7suffix\" after\nend",
                "x".repeat((1 << 20) - 6_000),
                "kkkkkkk7".repeat(675),
                "k".repeat(600)
            ),
        ] {
            let whole = credentials.redact(text.clone());
            assert!(!whole.contains("kkkkkkk7"), "whole");
            for sizes in [vec![1 << 20, 64 << 10], vec![4096], vec![1000], vec![97]] {
                let mut redacting = Redacting::new(credentials.clone());
                let (mut out, mut at) = (String::new(), 0);
                for size in sizes.iter().chain(std::iter::repeat(sizes.last().unwrap())) {
                    if at >= text.len() {
                        break;
                    }
                    let end = (at + size).min(text.len());
                    out.push_str(&redacting.feed(&text.as_bytes()[at..end]));
                    at = end;
                }
                out.push_str(&redacting.finish());
                let differs = (out.bytes().zip(whole.bytes())).position(|(a, b)| a != b);
                assert!(
                    out == whole,
                    "in pieces of {sizes:?}: {} vs {} bytes, from {differs:?}: {:?} vs {:?}",
                    out.len(),
                    whole.len(),
                    String::from_utf8_lossy(
                        &out.as_bytes()[differs.unwrap_or(0).saturating_sub(20)..]
                    )
                    .chars()
                    .take(80)
                    .collect::<String>(),
                    String::from_utf8_lossy(
                        &whole.as_bytes()[differs.unwrap_or(0).saturating_sub(20)..]
                    )
                    .chars()
                    .take(80)
                    .collect::<String>()
                );
            }
        }
    }
    #[test]
    fn joined_files_are_removed_together_unless_recorded() {
        let dir = std::env::temp_dir().join(format!("agent-files-join-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str| {
            let path = dir.join(name);
            std::fs::write(&path, "x").unwrap();
            path.to_str().unwrap().to_owned()
        };
        let (a, b) = (file("a"), file("b"));
        let joined =
            Files::new(vec![("stdout", a.clone())]).join(Files::new(vec![("stderr", b.clone())]));
        assert_eq!(joined.len(), 2);
        drop(joined);
        assert!(!Path::new(&a).exists() && !Path::new(&b).exists());
        let (c, d) = (file("c"), file("d"));
        let joined =
            Files::new(vec![("stdout", c.clone())]).join(Files::new(vec![("stderr", d.clone())]));
        joined.recorded();
        drop(joined);
        assert!(Path::new(&c).exists() && Path::new(&d).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// A file's line pages read as a stored copy's, without holding it.
    #[cfg(unix)]
    #[test]
    fn a_file_pages_as_its_text_does() {
        let dir = std::env::temp_dir().join(format!("agent-page-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("out");
        let long = "x".repeat(PREVIEW_BYTES);
        let texts = [
            String::new(),
            "\n".to_owned(),
            "one".to_owned(),
            "one\r\ntwo\n\nfour".to_owned(),
            (1..=3000)
                .map(|n| format!("line {n} {}\n", "é".repeat(n % 40)))
                .collect(),
            format!("a\n{long}\nb\n"),
        ];
        for text in &texts {
            std::fs::write(&file, text).unwrap();
            let path = file.to_str().unwrap();
            for (offset, limit) in [
                (1, 1),
                (1, 5000),
                (2, 2),
                (4, 1),
                (1500, 10),
                (2999, 5),
                (4000, 3),
            ] {
                let want = page_lines(text, offset, limit).map_err(|error| error.code);
                let got = page_file(path, offset, limit).map_err(|error| error.code);
                assert_eq!(
                    got,
                    want.map(Some),
                    "{offset} {limit} of {:?}",
                    &text[..text.len().min(40)]
                );
            }
        }
        assert_eq!(
            page_file(dir.join("gone").to_str().unwrap(), 1, 1).unwrap(),
            None
        );
        // One larger than any retained file was not written here: refused
        // without a read.
        let large = dir.join("large");
        std::fs::File::create(&large)
            .unwrap()
            .set_len(FILE_RETAINED_BYTES + 1)
            .unwrap();
        let refused = page_file(large.to_str().unwrap(), 1, 1).unwrap_err();
        assert_eq!(refused.code, "storage_error");
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// Many blocks in one piece read as the whole: each is passed once.
    #[test]
    fn many_blocks_in_one_piece_redact_as_whole() {
        let credentials = Credentials::default();
        credentials.redact_also(&[], &["PRIVATE KEY".to_owned()]);
        let block = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n\
            -----BEGIN PRIVATE KEY-----\nMIIEsecret\n-----END PRIVATE KEY-----\n";
        let text = block.repeat((1 << 20) / block.len());
        let whole = credentials.redact(text.clone());
        assert!(!whole.contains("MIIEsecret"));
        for size in [1 << 20, 64 << 10, 4099] {
            let mut redacting = Redacting::new(credentials.clone());
            let mut out = String::new();
            for piece in text.as_bytes().chunks(size) {
                out.push_str(&redacting.feed(piece));
            }
            out.push_str(&redacting.finish());
            assert!(out == whole, "in pieces of {size}");
        }
    }
    /// A stream past the bound goes whole to its file in the bot's outputs
    /// folder as the command runs, redacted, every byte of its middle with
    /// it; the result names the file and keeps no cut copy. A folder its
    /// commands may write gets nothing, the cut copy kept as before.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stream_past_the_bound_goes_whole_to_its_file() {
        let dir = std::env::temp_dir().join(format!("agent-spill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("outputs")).unwrap();
        let registry = Registry::new("shell").unwrap();
        registry.credentials.set("TOKEN", "tok3n-fake-fake-fake9");
        // 3 MiB of numbered lines, a credential among them.
        let command = "for i in $(seq 1 30000); do printf 'line %06d %0100d\\n' $i 0; \
            [ $i = 15000 ] && echo tok3n-fake-fake-fake9; done; echo err >&2";
        let spill = |usable: bool| Spill {
            folder: dir.join("outputs"),
            name: format!("7-call{usable}"),
            usable: Arc::new(move || usable),
        };
        let run = |spill: Spill| {
            registry.foreground(
                command.into(),
                dir.clone(),
                Some(60_000),
                Vec::new(),
                None,
                registry.place(),
                tokio::sync::oneshot::channel().0,
                Some(spill),
            )
        };
        let outcome = run(spill(true)).await.unwrap();
        assert!(
            outcome.artifacts.is_empty(),
            "{:?}",
            outcome.artifacts.len()
        );
        let [(stream, file)] = &outcome.files[..] else {
            panic!("{:?}", &outcome.files[..]);
        };
        assert_eq!(*stream, "stdout");
        assert!(file.ends_with("outputs/7-calltrue-stdout"), "{file}");
        let whole = std::fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = whole.lines().collect();
        assert_eq!(lines.len(), 30001);
        assert_eq!(lines[15000], "[REDACTED]");
        assert!(
            lines[..15000]
                .iter()
                .enumerate()
                .all(|(n, line)| line.starts_with(&format!("line {:06} ", n + 1)))
        );
        assert!(!whole.contains("tok3n"));
        let value: serde_json::Value = serde_json::from_str(&outcome.output).unwrap();
        let shown = value["stdout"].as_str().unwrap();
        assert!(
            shown.contains("bytes omitted; the full output is retained"),
            "{}",
            &shown[..200]
        );
        assert!(shown.starts_with("line 000001"));
        assert!(shown.ends_with(&format!("line 030000 {}\n", "0".repeat(100))));
        assert_eq!(value["stderr"], "err\n");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Unrecorded, the file goes with its outcome.
        let file = file.clone();
        drop(outcome);
        for _ in 0..100 {
            if !Path::new(&file).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!Path::new(&file).exists());
        // Redaction that leaves less than a preview shows it whole, the
        // file still named.
        registry
            .credentials
            .redact_also(&["sesame".to_owned()], &[]);
        let secrets = "for i in $(seq 1 1200); do printf 'sesame=a%01000d\\n' $i; done";
        let outcome = registry
            .foreground(
                secrets.into(),
                dir.clone(),
                Some(60_000),
                Vec::new(),
                None,
                registry.place(),
                tokio::sync::oneshot::channel().0,
                Some(spill(true)),
            )
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&outcome.output).unwrap();
        let shown = value["stdout"].as_str().unwrap();
        assert!(shown.len() < PREVIEW_BYTES, "{}", shown.len());
        assert!(shown.starts_with("sesame=[REDACTED]\n"), "{}", &shown[..60]);
        assert!(shown.contains("bytes not shown here: they are in this stream's file"));
        assert!(shown.ends_with("the full output is retained: see this result's artifacts and read them with the read tool ...]"));
        let [(_, file)] = &outcome.files[..] else {
            panic!("{:?}", &outcome.files[..]);
        };
        let whole = std::fs::read_to_string(file).unwrap();
        assert_eq!(whole, "sesame=[REDACTED]\n".repeat(1200));
        let file = file.clone();
        drop(outcome);
        for _ in 0..100 {
            if !Path::new(&file).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // A folder the bot's commands may write holds nothing.
        let outcome = run(spill(false)).await.unwrap();
        assert!(outcome.files.is_empty());
        assert_eq!(outcome.artifacts.len(), 1);
        assert!(outcome.output.contains("more past 1 MiB were not kept"));
        assert_eq!(std::fs::read_dir(dir.join("outputs")).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
