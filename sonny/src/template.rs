//! Templates: Markdown with blanks Sonny fills, as Jinja writes them
//! (MiniJinja), from what it already keeps: the person's sources, files
//! in the agent's folder, the lead's tasks and the clock. A blank never
//! runs a command, reads the environment or reaches the network; the only
//! data is what [`Data`] hands it.
//!
//! A render says what it read ([`Reads`]), so a window watches only that
//! and fills the blanks again only once one of those changed ([`stamp`]).
//!
//! Every live template is one thing, wherever it is written: Markdown or
//! a message given with `live` true, checked once as it is written by
//! [`refusal`], which refuses it as `live_template` ([`REFUSED`]). Where
//! its fill goes ([`To`]) sets the one rule that differs: one the person
//! sees in a window (a margin note) is filled again whenever what it
//! read changes, and may read the tasks; one that goes into a prompt (a
//! memory fact, an automation's message) is filled once as it goes and
//! never again, so a prompt never changes under the model, and reads no
//! tasks, which it outlives. How much of a fill is kept:
//!
//! | where it goes | most kept |
//! |---|---|
//! | any render | [`OUTPUT`], 64 KiB, the render stopped there |
//! | a margin note | its template [`crate::margin`]'s `NOTE_BYTES`; its fill the note's lines |
//! | a prompt | `agent_client::memory::cut_filled`, 32 KiB, and what it cut |
//! | a memory fact's index line | 300 bytes, one line |
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use minijinja::{Environment, Error, ErrorKind, UndefinedBehavior, Value};
use serde_json::{Value as Json, json};

/// The most steps a render takes: a loop past it stops the render, in
/// well under a millisecond.
const FUEL: u64 = 100_000;
/// How deep templates may nest blocks and calls.
const DEPTH: usize = 50;
/// The most events a source's blank holds, newest first.
const EVENTS: usize = 50;
/// The most topics a source's blank holds, each its newest event.
const TOPICS: usize = 200;
/// The most a render writes, cut on a character's edge.
pub const OUTPUT: usize = 64 * 1024;

/// What a render read: a window watches these and nothing else.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reads {
    pub sources: BTreeSet<String>,
    pub files: BTreeSet<PathBuf>,
    pub tasks: bool,
    pub clock: bool,
    /// The sources it read that the lookout says are down, each with when
    /// it started failing, in seconds since the epoch.
    pub down: BTreeMap<String, i64>,
    /// Each source's log and each file as it stood when it was read, by
    /// size and time: [`changed`] compares them with now.
    pub seen: BTreeMap<PathBuf, Stamp>,
}

/// A file's size and time; none for a file that is not there.
pub type Stamp = (u64, Option<SystemTime>);

/// `path`'s size and time now.
pub fn stamp(path: &Path) -> Stamp {
    std::fs::metadata(path).map_or((u64::MAX, None), |m| (m.len(), m.modified().ok()))
}

/// What blanks are filled from.
#[derive(Clone, Debug, Default)]
pub struct Data {
    /// The store whose lookout keeps the sources' events; none, no source
    /// reads.
    pub store: Option<PathBuf>,
    /// The sources the person set up, by name.
    pub sources: Arc<Vec<String>>,
    /// The folder a file blank's path is read from.
    pub folder: PathBuf,
    /// What `tasks()` gives: the agents as `/status` last read them.
    pub tasks: Arc<Vec<Json>>,
    /// Seconds since the epoch.
    pub now: i64,
}

/// Why `template` does not read as one, or nothing: what a note is
/// refused for before any blank is filled.
pub fn check(template: &str) -> Result<(), String> {
    let env = environment();
    env.template_from_str(template)
        .map(drop)
        .map_err(|e| said(&e, template))
}

/// `template` with its blanks filled from `data`, or why it could not be,
/// and what it read either way.
pub fn render(template: &str, data: &Data) -> (Result<String, String>, Reads) {
    let reads = Arc::new(Mutex::new(Reads::default()));
    let mut env = environment();
    blanks(&mut env, data, &reads);
    let mut out = Capped(Vec::new());
    let rendered = (env.template_from_str(template))
        .and_then(|t| t.render_captured_to((), &mut out).map(drop))
        .or_else(|e| match out.full() {
            true => Ok(()),
            false => Err(said(&e, template)),
        })
        .map(|()| cut(String::from_utf8_lossy(&out.0).into_owned(), OUTPUT));
    let reads = reads.lock().map(|r| r.clone()).unwrap_or_default();
    (rendered, reads)
}

/// The sources the person set up whose events a blank reads: the commons
/// keeps none.
pub fn configured_sources() -> Arc<Vec<String>> {
    Arc::new(
        (crate::lookout::sources::configured()
            .unwrap_or_default()
            .into_iter())
        .filter(|source| !source.command.is_empty())
        .map(|source| source.name)
        .collect(),
    )
}

/// Live memory facts' blanks, filled as a read or a new agent finds them:
/// the person's sources, files in the agent's folder and the clock. Not
/// tasks, which a fact outlives.
pub struct Live;

impl Live {
    fn data(folder: &Path) -> Data {
        Data {
            store: (crate::STORE.get().cloned())
                .or_else(|| crate::daemon_paths(None, None).ok().map(|(store, _)| store)),
            sources: configured_sources(),
            folder: folder.to_owned(),
            tasks: Default::default(),
            now: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64),
        }
    }
}

/// The blanks and filters a window's live template has, as the tools
/// that write one say them.
pub const BLANKS: &str = "Blanks: source('name') or source('name:topic'): .latest, .events newest \
    first, .topics each topic's newest; an event has text, at, topic, from, url, fields. file('path'), \
    modified('path'), tasks(), task('name'), now(). Filters: ago, time, fromjson, and Jinja's own \
    (selectattr, sort, map, join, tojson).";

/// The code every live template is refused with, wherever it is written.
pub const REFUSED: &str = "live_template";

/// Where a live template's fill goes, which sets whether it may read the
/// tasks: a prompt outlives them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum To {
    /// Shown in a window, and filled again there as what it read changes.
    Window,
    /// Into a prompt, filled once as it goes.
    Prompt,
}

const TASKS: &str = "tasks() and task() fill only what a window shows: a prompt outlives its tasks";

/// The sources `template` names by a literal, `source('NAME')`, and
/// whether it calls `tasks()` or `task()`, in its blanks and tags alone:
/// text outside them, `{% raw %}` blocks, comments and quoted words are
/// words.
fn named(template: &str) -> (Vec<String>, bool) {
    let (mut sources, mut tasks) = (Vec::new(), false);
    let mut rest = template;
    let mut raw = false;
    while let Some(at) = rest.find('{') {
        let after = &rest[at..];
        let (close, quoted) = match after.get(..2) {
            Some("{{") if !raw => ("}}", true),
            Some("{%") => ("%}", true),
            Some("{#") if !raw => ("#}", false),
            _ => {
                rest = &after[1..];
                continue;
            }
        };
        let Some(end) = closing(&after[2..], close, quoted) else {
            break;
        };
        let code = &after[2..2 + end];
        rest = &after[2 + end + 2..];
        if close == "#}" {
            continue;
        }
        let tag = code.trim_matches(|c: char| c == '-' || c == '+' || c.is_whitespace());
        if close == "%}" && (tag == "raw" || tag == "endraw") {
            raw = tag == "raw";
            continue;
        }
        if raw {
            continue;
        }
        calls(code, |name, arg| match name {
            "tasks" | "task" => tasks = true,
            "source" => {
                sources.extend(arg.map(|named| crate::lookout::sources::named(named).0.to_owned()))
            }
            _ => {}
        });
    }
    (sources, tasks)
}

/// Where `close` first ends `code`, outside quotes when `quoted`.
fn closing(code: &str, close: &str, quoted: bool) -> Option<usize> {
    if !quoted {
        return code.find(close);
    }
    let mut chars = code.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '\'' | '"' => skip_quoted(&mut chars, c),
            _ if code[at..].starts_with(close) => return Some(at),
            _ => {}
        }
    }
    None
}

/// Past the end of a word quoted by `quote`, its escapes included.
fn skip_quoted(chars: &mut std::str::CharIndices<'_>, quote: char) {
    while let Some((_, c)) = chars.next() {
        match c {
            '\\' => _ = chars.next(),
            _ if c == quote => break,
            _ => {}
        }
    }
}

/// Each call of a global in the blank or tag `code`, by its name and its
/// first argument when that is a plain quoted word: quoted words and
/// attributes' calls (`x.task()`) are not calls of one.
fn calls(code: &str, mut call: impl FnMut(&str, Option<&str>)) {
    let mut chars = code.char_indices();
    let mut attribute = false;
    while let Some((at, c)) = chars.next() {
        match c {
            '\'' | '"' => {
                skip_quoted(&mut chars, c);
                attribute = false;
            }
            '.' => attribute = true,
            c if c.is_alphabetic() || c == '_' => {
                let end = code[at..]
                    .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .map_or(code.len(), |len| at + len);
                let after = code[end..].trim_start();
                if !attribute && let Some(args) = after.strip_prefix('(') {
                    call(&code[at..end], first_word(args.trim_start()));
                }
                attribute = false;
                for _ in code[at + c.len_utf8()..end].chars() {
                    chars.next();
                }
            }
            c if c.is_whitespace() => {}
            _ => attribute = false,
        }
    }
}

/// The quoted word `args` starts with, when it is the whole first
/// argument and has no escapes.
fn first_word(args: &str) -> Option<&str> {
    let quote = args.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let end = 1 + args[1..].find(quote)?;
    let word = &args[1..end];
    // The whole argument, not the first part of a name built from parts.
    let whole = args[end + 1..].trim_start().starts_with([')', ',']);
    (whole && !word.contains('\\')).then_some(word)
}

/// Why a blank cannot read `name`: no source of that name is set up.
fn no_source(name: &str, sources: &[String]) -> String {
    let yours = match sources.is_empty() {
        true => "you have none set up".to_owned(),
        false => format!("yours are {}", sources.join(", ")),
    };
    format!("no source named {name}: {yours}")
}

/// A live template going into a prompt filled from `data`, or why not.
fn live(template: &str, data: &Data) -> Result<String, String> {
    let (filled, reads) = render(template, data);
    if reads.tasks {
        return Err(TASKS.into());
    }
    filled
}

/// Why `template` cannot be a live one whose fill goes `to`: it does not
/// read as one, names a source never set up, or, going into a prompt,
/// the tasks. Shown in a window, it shows now, so a fill that fails now
/// refuses it too, unless it reads the tasks, which only the window
/// knows. Going into a prompt, what a later fill may find, such as a file
/// not written yet, does not refuse it. The one check every live template
/// is written past, refused as [`REFUSED`].
pub fn refusal(template: &str, data: &Data, to: To) -> Result<(), String> {
    check(template)?;
    // Named in the text, each is checked whether or not a render gets to
    // it now: a blank before it may wait on a file not written yet.
    let (sources, tasks) = named(template);
    if tasks && to == To::Prompt {
        return Err(TASKS.into());
    }
    if let Some(name) = sources.iter().find(|name| !data.sources.contains(name)) {
        return Err(no_source(name, &data.sources));
    }
    let (filled, reads) = render(template, data);
    match filled {
        Err(why) if why.contains("no source named ") => Err(why),
        _ if reads.tasks && to == To::Prompt => Err(TASKS.into()),
        Err(why) if to == To::Window && !reads.tasks => Err(why),
        _ => Ok(()),
    }
}

/// An automation's live message filled from `folder`, sources read
/// where `store`'s lookout keeps them (this Sonny's store when none): the
/// blanks a live fact has.
pub fn fill_message(message: &str, folder: &Path, store: Option<&Path>) -> Result<String, String> {
    live(message, &message_data(folder, store))
}

/// Why an automation's `message` could never fill as a live one, or
/// nothing: [`refusal`]'s, as a prompt's.
pub fn check_message(message: &str, folder: &Path, store: Option<&Path>) -> Result<(), String> {
    refusal(message, &message_data(folder, store), To::Prompt)
}

fn message_data(folder: &Path, store: Option<&Path>) -> Data {
    let mut data = Live::data(folder);
    if let Some(store) = store {
        data.store = Some(store.to_owned());
    }
    data
}

impl agent_client::memory::Fill for Live {
    fn check(&self, template: &str, folder: &Path) -> Result<(), String> {
        refusal(template, &Self::data(folder), To::Prompt)
    }

    fn fill(&self, template: &str, folder: &Path) -> Result<String, String> {
        live(template, &Self::data(folder))
    }
}

/// Where a render writes: it stops the render once past [`OUTPUT`], so a
/// loop that prints much never holds more than that.
struct Capped(Vec<u8>);

impl Capped {
    /// Past the most a render writes, by a whole character's room, so the
    /// cut lands inside what was kept.
    const ROOM: usize = OUTPUT + 4;

    fn full(&self) -> bool {
        self.0.len() >= Self::ROOM
    }
}

impl std::io::Write for Capped {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let room = Self::ROOM - self.0.len().min(Self::ROOM);
        self.0.extend_from_slice(&bytes[..bytes.len().min(room)]);
        match self.full() {
            true => Err(std::io::Error::other("the note is full")),
            false => Ok(bytes.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Whether a source's log or a file a render read changed since it read
/// it: stamped before each read, so a change while it filled is a change.
#[cfg(test)]
pub fn changed(reads: &Reads) -> bool {
    (reads.seen.iter()).any(|(path, was)| stamp(path) != *was)
}

/// The environment every template is read in: a blank used that names
/// nothing fails the render, while `{% if %}` may still ask about one.
fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    // Spans to quote, not the debug dump a test build would print.
    env.set_debug(false);
    env.set_undefined_behavior(UndefinedBehavior::SemiStrict);
    env.set_fuel(Some(FUEL));
    env.set_recursion_limit(DEPTH);
    env
}

/// The blanks a template may use, each noting what it read in `reads`.
fn blanks(env: &mut Environment<'static>, data: &Data, reads: &Arc<Mutex<Reads>>) {
    let read = |reads: &Arc<Mutex<Reads>>, f: &dyn Fn(&mut Reads)| {
        if let Ok(mut reads) = reads.lock() {
            f(&mut reads);
        }
    };
    let (r, d) = (reads.clone(), data.clone());
    env.add_function("source", move |named: String| -> Result<Value, Error> {
        let (name, topic) = crate::lookout::sources::named(&named);
        let name = name.to_owned();
        read(&r, &|reads| {
            reads.sources.insert(name.clone());
        });
        if !d.sources.contains(&name) {
            return Err(failed(no_source(&name, &d.sources)));
        }
        let log = (d.store.as_deref()).map_or_else(Default::default, |store| {
            let file = recent_file(store, &name);
            let (stamp, log) = log(&file, topic);
            read(&r, &|reads| {
                reads.seen.insert(file.clone(), stamp);
            });
            log
        });
        if let Some(since) = log.down {
            read(&r, &|reads| {
                reads.down.insert(name.clone(), since);
            });
        }
        let fields = [
            ("name", Value::from(name.as_str())),
            ("topic", topic.map_or(Value::from(()), Value::from)),
            ("up", Value::from(log.down.is_none())),
            ("down_since", log.down.map_or(Value::from(()), Value::from)),
            ("latest", log.events.first().map_or(Value::from(()), value)),
            ("events", log.events.iter().map(value).collect()),
            ("topics", log.topics.iter().map(value).collect()),
        ];
        Ok(Value::from_pairs(fields))
    });
    let (r, d) = (reads.clone(), data.clone());
    // The folders kept from agents, read once a render at its first file:
    // an agent writes the template, and Sonny reads outside its sandbox.
    let kept = Arc::new(std::sync::OnceLock::new());
    env.add_function("file", move |path: String| -> Result<String, Error> {
        let path = d.folder.join(path.trim());
        let stamp = stamp(&path);
        read(&r, &|reads| {
            reads.files.insert(path.clone());
            reads.seen.insert(path.clone(), stamp);
        });
        let hidden = kept.get_or_init(hidden).as_ref().map_err(failed)?;
        crate::margin::read(&path, hidden).map_err(failed)
    });
    let (r, d) = (reads.clone(), data.clone());
    let kept = Arc::new(std::sync::OnceLock::new());
    env.add_function("modified", move |path: String| -> Result<Value, Error> {
        let path = d.folder.join(path.trim());
        let stamp = stamp(&path);
        read(&r, &|reads| {
            reads.files.insert(path.clone());
            reads.seen.insert(path.clone(), stamp);
        });
        let hidden = kept.get_or_init(hidden).as_ref().map_err(failed)?;
        modified(&path, hidden).map_err(failed)
    });
    let (r, d) = (reads.clone(), data.clone());
    env.add_function("tasks", move || {
        read(&r, &|reads| reads.tasks = true);
        d.tasks.iter().map(value).collect::<Value>()
    });
    let (r, d) = (reads.clone(), data.clone());
    env.add_function("task", move |name: String| -> Result<Value, Error> {
        read(&r, &|reads| reads.tasks = true);
        (d.tasks.iter())
            .find(|task| task["name"] == name.as_str())
            .map(value)
            .ok_or_else(|| failed(format!("no task named {name}")))
    });
    let (r, now) = (reads.clone(), data.now);
    env.add_function("now", move || {
        read(&r, &|reads| reads.clock = true);
        now
    });
    let (r, now) = (reads.clone(), data.now);
    env.add_filter("ago", move |at: i64| {
        read(&r, &|reads| reads.clock = true);
        ago(now.saturating_sub(at))
    });
    let (r, now) = (reads.clone(), data.now);
    env.add_filter("time", move |at: i64| {
        read(&r, &|reads| reads.clock = true);
        crate::asks::clock(at, now)
    });
    env.add_filter("fromjson", |text: String| -> Result<Value, Error> {
        serde_json::from_str::<Json>(&text)
            .map(|json| value(&json))
            .map_err(|e| failed(format!("not JSON: {e}")))
    });
}

/// `json` as a template reads it: numbers as numbers, whatever way
/// serde_json keeps them.
fn value(json: &Json) -> Value {
    match json {
        Json::Null => Value::from(()),
        Json::Bool(b) => Value::from(*b),
        Json::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(n), ..) => Value::from(n),
            (_, Some(n), _) => Value::from(n),
            (.., Some(n)) => Value::from(n),
            _ => Value::from(n.to_string()),
        },
        Json::String(text) => Value::from(text.as_str()),
        Json::Array(items) => items.iter().map(value).collect(),
        Json::Object(fields) => {
            Value::from_pairs(fields.iter().map(|(k, v)| (k.as_str(), value(v))))
        }
    }
}

/// What went wrong in a blank, as the template's error.
/// The folders kept from agents ([`agent_client::hidden`]), by their real
/// paths, as the daemon knows them; a config that does not read refuses
/// every file.
fn hidden() -> Result<Vec<PathBuf>, String> {
    let config = agent_client::config::load()?;
    Ok((agent_client::hidden::folders(&config).into_iter())
        .map(|folder| std::fs::canonicalize(&folder).unwrap_or(folder))
        .collect())
}

/// When the file at `path` last changed, in seconds, none while it is not
/// there; refused, as `file()` is, when what was opened is in one of
/// `hidden`, so a blank can't tell when a key changed either.
fn modified(path: &Path, hidden: &[PathBuf]) -> Result<Value, String> {
    use std::os::unix::fs::OpenOptionsExt;
    // A pipe opened without O_NONBLOCK would hold the window up.
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    else {
        return Ok(Value::from(()));
    };
    if agent_client::hidden::holds(hidden, &file) {
        return Err(format!(
            "read_denied: {} is kept unreadable from agents",
            path.display()
        ));
    }
    let since = |at: SystemTime| at.duration_since(SystemTime::UNIX_EPOCH).ok();
    Ok(
        match file
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(since)
        {
            Some(at) => Value::from(at.as_secs() as i64),
            None => Value::from(()),
        },
    )
}

fn failed(why: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidOperation, why.into())
}

/// The most of a failing expression an error quotes.
const QUOTED: usize = 80;

/// A template's error in one line, where it is in `template`: the line,
/// the expression that failed as written there, and the kind of error or
/// its detail.
fn said(error: &Error, template: &str) -> String {
    let what = match error.detail() {
        Some(detail) => detail.to_owned(),
        None => error.kind().to_string(),
    };
    // The detail may name it already, as an undefined name's does.
    let at = (error.range())
        .and_then(|range| template.get(range))
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|text| !text.is_empty() && !what.contains(text.as_str()))
        .map(|text| match text.len() > QUOTED {
            true => format!("{}…", cut(text, QUOTED)),
            false => text,
        });
    match (error.line(), at) {
        (Some(line), Some(at)) => format!("line {line}, `{at}`: {what}"),
        (Some(line), None) => format!("line {line}: {what}"),
        (None, Some(at)) => format!("`{at}`: {what}"),
        (None, None) => what,
    }
}

/// How long ago, roughly: `40s ago`, `5m ago`, `3h ago`, `2d ago`;
/// `just now` for a time not yet come, as a clock a little off says.
fn ago(seconds: i64) -> String {
    match seconds {
        ..0 => "just now".to_owned(),
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

/// `text` cut to at most `most` bytes, on a character's edge.
fn cut(mut text: String, most: usize) -> String {
    if text.len() > most {
        let mut end = most;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Where the lookout of `store` keeps what the source `name` printed
/// lately.
fn recent_file(store: &Path, name: &str) -> PathBuf {
    let dir = crate::lookout::beside(store, ".sources");
    crate::lookout::sources::recent_file(&dir, name)
}

/// What a source's log says: what it printed lately and whether the
/// lookout says it is down.
#[derive(Default)]
struct Log {
    /// Newest first, as blanks give them.
    events: Vec<Json>,
    /// Each topic's newest event, newest first: the lookout keeps one for
    /// each however old.
    topics: Vec<Json>,
    /// When it started failing, in seconds since the epoch, while the
    /// lookout's newest word on it is that it is down.
    down: Option<i64>,
}

/// The source log `file`, its events on `topic` alone when one is named,
/// read again only once it changed, and how it stood before it was read.
fn log(file: &Path, topic: Option<&str>) -> (Stamp, Arc<Log>) {
    type Cache = HashMap<(PathBuf, Option<String>), (Stamp, Arc<Log>)>;
    static READ: OnceLock<Mutex<Cache>> = OnceLock::new();
    let stamp = stamp(file);
    if stamp.1.is_none() {
        return (stamp, Arc::default());
    }
    let mut cache = READ
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let key = (file.to_owned(), topic.map(str::to_owned));
    if let Some((at, log)) = cache.get(&key)
        && *at == stamp
    {
        return (stamp, log.clone());
    }
    let text = std::fs::read_to_string(file).unwrap_or_default();
    let kept = || (text.lines().rev()).filter_map(|line| serde_json::from_str::<Json>(line).ok());
    // The lookout's newest word: down, or back again.
    let down = kept()
        .find(|kept| kept["lookout"] == true)
        .filter(|word| (word["id"].as_str()).is_some_and(|id| id.starts_with("lookout.down.")))
        .and_then(|word| word["since_ms"].as_u64().or(word["at_ms"].as_u64()))
        .map(|ms| (ms / 1000) as i64);
    let blank = |kept: &Json| {
        json!({
            "text": kept["text"],
            "id": kept["id"],
            "topic": kept["topic"],
            "from": kept["from"],
            "url": kept["url"],
            "at": kept["at_ms"].as_u64().map(|ms| ms / 1000),
            "fields": kept["fields"],
        })
    };
    let printed = || {
        kept()
            .filter(|kept| kept["lookout"] != true)
            .filter(|kept| topic.is_none_or(|topic| kept["topic"] == topic))
    };
    let events: Vec<Json> = printed().take(EVENTS).map(|kept| blank(&kept)).collect();
    let mut seen = BTreeSet::new();
    let topics: Vec<Json> = printed()
        .filter(|kept| (kept["topic"].as_str()).is_some_and(|topic| seen.insert(topic.to_owned())))
        .take(TOPICS)
        .map(|kept| blank(&kept))
        .collect();
    let log = Arc::new(Log {
        events,
        topics,
        down,
    });
    cache.insert(key, (stamp, log.clone()));
    (stamp, log)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(folder: &Path) -> Data {
        Data {
            store: None,
            sources: Arc::new(vec![]),
            folder: folder.to_owned(),
            tasks: Arc::new(vec![
                json!({"name": "calc.add", "state": "working", "group": "working"}),
                json!({"name": "calc.sub", "state": "failed", "group": "needs_you"}),
            ]),
            now: 1_000_000,
        }
    }

    #[test]
    fn blanks_fill_from_tasks_and_the_clock_and_say_what_they_read() {
        let dir = std::env::temp_dir();
        let template = "{% for t in tasks() %}- {{ t.name }}: \
            {% if t.group == 'needs_you' %}**{{ t.state }}**{% else %}{{ t.state }}{% endif %}\n\
            {% endfor %}checked {{ 999_700 | ago }}";
        let (text, reads) = render(template, &data(&dir));
        assert_eq!(
            text.unwrap(),
            "- calc.add: working\n- calc.sub: **failed**\nchecked 5m ago"
        );
        assert!(reads.tasks && reads.clock);
        assert!(reads.sources.is_empty() && reads.files.is_empty());
    }

    fn refusal_prompt(template: &str, data: &Data) -> Result<(), String> {
        refusal(template, data, To::Prompt)
    }

    #[test]
    fn a_window_may_read_the_tasks_and_is_refused_as_a_prompt_is_otherwise() {
        let dir = tempfile_dir("window");
        let data = data(&dir);
        assert_eq!(refusal("{{ tasks() | length }}", &data, To::Window), Ok(()));
        assert!(
            refusal("{{ source('ci').latest }}", &data, To::Window)
                .unwrap_err()
                .contains("no source named ci")
        );
        assert!(refusal("{% if %}", &data, To::Window).is_err());
        // Shown now, it must fill now: a file not written yet is refused,
        // saying which, though a prompt fills later and takes it.
        let why = refusal("{{ file('later.txt') }}", &data, To::Window).unwrap_err();
        assert!(why.starts_with("line 1, `file('later.txt')`: "), "{why}");
        assert_eq!(
            refusal("{{ file('later.txt') }}", &data, To::Prompt),
            Ok(())
        );
    }

    #[test]
    fn a_live_fact_fills_from_sources_files_and_the_clock_but_never_from_tasks() {
        let dir = tempfile_dir("live-fact");
        let data = Data {
            sources: Arc::new(vec!["ci".into()]),
            ..data(&dir)
        };
        // A file not written yet fills later: the fact is kept, and a read
        // says why it does not fill now.
        let fact = "PRs open: {{ file('prs.txt') }}, as of {{ 999_940 | ago }}";
        assert_eq!(refusal_prompt(fact, &data), Ok(()));
        assert!(live(fact, &data).unwrap_err().contains("prs.txt"));
        std::fs::write(dir.join("prs.txt"), "#886").unwrap();
        assert_eq!(live(fact, &data).unwrap(), "PRs open: #886, as of 1m ago");
        // A source with no events yet fills too.
        assert_eq!(refusal_prompt("{{ source('ci').latest }}", &data), Ok(()));
        // Refused: a source never set up, the tasks, and what is no
        // template at all.
        let refused = refusal_prompt("{{ source('deploys').latest }}", &data).unwrap_err();
        assert!(refused.contains("no source named deploys"), "{refused}");
        for tasks in ["{{ tasks() | length }}", "{{ task('calc.add').state }}"] {
            let refused = refusal_prompt(tasks, &data).unwrap_err();
            assert!(
                refused.starts_with("tasks() and task() fill only what a window shows"),
                "{refused}"
            );
        }
        assert!(refusal_prompt("{{ open", &data).is_err());
        // A file not written yet before them hides neither.
        std::fs::remove_file(dir.join("prs.txt")).unwrap();
        let late = "{{ file('prs.txt') }} {{ source(\"typo\").latest.text }}";
        assert!(
            refusal_prompt(late, &data)
                .unwrap_err()
                .contains("no source named typo")
        );
        let late = "{{ file('prs.txt') }}{% for t in tasks() %}{{ t.name }}{% endfor %}";
        assert!(
            refusal_prompt(late, &data)
                .unwrap_err()
                .starts_with("tasks() and task()")
        );
        // Words outside the blanks, raw blocks and attributes are not calls.
        let words = "source('typo') and tasks() {% raw %}{{ source('typo') }}{% endraw %} \
            {{ file('prs.txt') | length }}";
        assert_eq!(named(words), (vec![], false));
        // Nor are quoted words and comments, whatever their characters.
        let quoted = "{{ file('tasks().txt') }}{# {{ source('missing') }} #}{{ file('café(1).txt') }}\
            {{ \"}} source('x')\" }}{{ 'it\\'s tasks()' }}";
        assert_eq!(named(quoted), (vec![], false));
        assert_eq!(
            named("{{ \"}}\" ~ source('typo') }}"),
            (vec!["typo".to_owned()], false)
        );
        assert_eq!(
            named("{{ x.task(1) }} {{- source( 'ci' ).latest -}}"),
            (vec!["ci".to_owned()], false)
        );
        // A name built from parts is left to the render.
        assert_eq!(
            named("{{ source('re' ~ 'lease').latest }}"),
            (vec![], false)
        );
    }

    #[test]
    fn a_file_blank_reads_from_the_folder_and_json_becomes_fields() {
        let dir = tempfile_dir("file-blank");
        std::fs::write(dir.join("pr.json"), r#"{"state": "open", "checks": 3}"#).unwrap();
        let (text, reads) = render(
            "{% set pr = file('pr.json') | fromjson %}{{ pr.state }} with {{ pr.checks }} checks",
            &data(&dir),
        );
        assert_eq!(text.unwrap(), "open with 3 checks");
        assert_eq!(reads.files, BTreeSet::from([dir.join("pr.json")]));
        assert!(!changed(&reads));
        std::fs::write(dir.join("pr.json"), r#"{"state": "merged", "checks": 3}"#).unwrap();
        assert!(changed(&reads), "a change shows");
        // When a file last changed, which `ago` reads; none for one not there.
        let data = Data {
            now: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                + 120,
            ..data(&dir)
        };
        let (text, reads) = render(
            "{{ modified('pr.json') | ago }} {{ modified('gone.json') is none }}",
            &data,
        );
        assert_eq!(text.unwrap(), "2m ago True");
        assert_eq!(
            reads.files,
            BTreeSet::from([dir.join("pr.json"), dir.join("gone.json")])
        );
        std::fs::write(dir.join("gone.json"), "{}").unwrap();
        assert!(changed(&reads), "a file written since shows");
    }

    #[test]
    fn modified_refuses_a_file_in_a_kept_folder_by_where_it_leads() {
        let dir = tempfile_dir("modified-kept");
        let keys = dir.join("keys");
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::write(keys.join("env"), "KEY=x").unwrap();
        std::fs::write(dir.join("plain"), "x").unwrap();
        let link = dir.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(keys.join("env"), &link).unwrap();
        let hidden = [std::fs::canonicalize(&keys).unwrap()];
        for path in [keys.join("env"), link] {
            let refused = modified(&path, &hidden).unwrap_err();
            assert!(refused.starts_with("read_denied: "), "{refused}");
        }
        assert!(
            modified(&dir.join("plain"), &hidden)
                .unwrap()
                .as_i64()
                .is_some()
        );
        assert!(modified(&dir.join("gone"), &hidden).unwrap().is_none());
    }

    #[test]
    fn a_source_blank_gives_its_newest_events_first_and_an_unknown_one_fails() {
        let store = tempfile_dir("source-blank").join("store.sqlite");
        let dir = crate::lookout::beside(&store, ".sources").join(".recent");
        std::fs::create_dir_all(&dir).unwrap();
        let line = |text: &str, at: u64| {
            json!({"source": "ci", "text": text, "at_ms": at * 1000, "seq": 1}).to_string()
        };
        let log = format!("{}\n{}\n", line("red", 1), line("green", 2));
        std::fs::write(dir.join("ci.jsonl"), &log).unwrap();
        let data = Data {
            store: Some(store.clone()),
            sources: Arc::new(vec!["ci".into()]),
            ..data(Path::new("/"))
        };
        let (text, reads) = render(
            "{{ source('ci').latest.text }} at {{ source('ci').latest.at }}, {{ source('ci').events | length }} kept",
            &data,
        );
        assert_eq!(text.unwrap(), "green at 2, 2 kept");
        // Each topic's newest event, newest first.
        let pr = |n: u64, state: &str, at: u64| {
            json!({"source": "ci", "text": format!("#{n} {state}"), "topic": n.to_string(),
                "fields": {"state": state}, "at_ms": at * 1000, "seq": 1})
            .to_string()
        };
        let prs = [
            pr(1, "open", 1),
            pr(2, "open", 2),
            pr(1, "green", 3),
            pr(2, "merged", 4),
        ];
        std::fs::write(dir.join("ci.jsonl"), prs.join("\n") + "\n" + &log).unwrap();
        let (text, _) = render(
            "{% for pr in source('ci').topics if pr.fields.state != 'merged' %}{{ pr.text }} {% endfor %}\
             {{ source('ci').topics | length }}",
            &data,
        );
        assert_eq!(text.unwrap(), "#1 green 2");
        // One topic's events alone, however busy the others: what a blank
        // reads is the source, as a subscription to NAME:TOPIC is.
        let (text, one) = render(
            "{{ source('ci:1').topic }}: {% for e in source('ci:1').events %}{{ e.text }}, {% endfor %}\
             {{ source('ci:1').latest.text }} {{ source('ci:3').latest is none }}",
            &data,
        );
        assert_eq!(text.unwrap(), "1: #1 green, #1 open, #1 green True");
        assert_eq!(one.sources, BTreeSet::from(["ci".to_owned()]));
        assert_eq!(
            refusal("{{ source('cl:1').latest }}", &data, To::Prompt).unwrap_err(),
            "no source named cl: yours are ci"
        );
        std::fs::write(dir.join("ci.jsonl"), &log).unwrap();
        assert_eq!(reads.sources, BTreeSet::from(["ci".to_owned()]));
        assert!(reads.down.is_empty());
        // The lookout says it is down: the blank and what it read say so,
        // until its word that it is back.
        let word = |kind: &str, at: u64| {
            json!({"source": "ci", "text": kind, "lookout": true, "at_ms": at * 1000,
                "since_ms": 3000, "id": format!("lookout.{kind}.{at}"), "seq": 1})
            .to_string()
        };
        let down = format!("{log}{}\n", word("down", 4));
        std::fs::write(dir.join("ci.jsonl"), &down).unwrap();
        let blank = "{{ 'up' if source('ci').up else 'down' }} {{ source('ci').down_since }} {{ source('ci').latest.text }}";
        let (text, reads) = render(blank, &data);
        assert_eq!(text.unwrap(), "down 3 green");
        assert_eq!(reads.down, BTreeMap::from([("ci".to_owned(), 3)]));
        std::fs::write(dir.join("ci.jsonl"), format!("{down}{}\n", word("back", 5))).unwrap();
        let (text, reads) = render(blank, &data);
        assert_eq!(text.unwrap(), "up None green");
        assert!(reads.down.is_empty());
        let (text, _) = render("{{ source('cl').latest.text }}", &data);
        assert_eq!(
            text.unwrap_err(),
            "line 1, `source('cl')`: no source named cl: yours are ci"
        );
    }

    #[test]
    fn a_runaway_loop_and_a_misspelled_name_fail_and_say_where() {
        let dir = std::env::temp_dir();
        let start = std::time::Instant::now();
        let (text, _) = render(
            "{% for i in range(100000) %}{% for j in range(100000) %}x{% endfor %}{% endfor %}",
            &data(&dir),
        );
        assert!(text.is_err());
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        let (text, _) = render("one\n{{ taks }}", &data(&dir));
        assert_eq!(text.unwrap_err(), "line 2, `taks`: undefined value");
        // Of three blanks on a line, the error quotes the one that failed.
        let (text, _) = render(
            "{{ now() }} {{ (file('x') | fromjson).a }} {{ 1 }}",
            &data(&dir),
        );
        let why = text.unwrap_err();
        assert!(why.starts_with("line 1, `file('x')`: "), "{why}");
        // Asking about a name that is not there is fine.
        let (text, _) = render("{% if missing %}x{% else %}none{% endif %}", &data(&dir));
        assert_eq!(text.unwrap(), "none");
        assert!(check("{% if %}").is_err());
        assert!(check("{{ fine }}").is_ok());
    }

    #[test]
    fn a_render_stops_writing_once_the_note_is_full() {
        let dir = tempfile_dir("full");
        std::fs::write(dir.join("big.txt"), "é".repeat(100_000)).unwrap();
        let start = std::time::Instant::now();
        let (text, _) = render(
            "{% set big = file('big.txt') %}{% for i in range(1000) %}{{ big }}{% endfor %}",
            &data(&dir),
        );
        let text = text.unwrap();
        assert!(text.len() <= OUTPUT && text.len() > OUTPUT - 4);
        assert!(text.chars().all(|c| c == 'é'));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    fn tempfile_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sonny-template-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
