//! Templates: Markdown with blanks Sonny fills, as Jinja writes them
//! (MiniJinja), from what it already keeps: the person's sources, files
//! in the agent's folder, the lead's tasks and the clock. A blank never
//! runs a command, reads the environment or reaches the network; the only
//! data is what [`Data`] hands it.
//!
//! A render says what it read ([`Reads`]), so a window watches only that
//! and fills the blanks again only once one of those changed ([`stamp`]).
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
/// The most a render writes, cut on a character's edge.
const OUTPUT: usize = 64 * 1024;

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

fn stamp_of(path: &Path) -> Stamp {
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
        .map_err(|e| said(&e))
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
            false => Err(said(&e)),
        })
        .map(|()| cut(String::from_utf8_lossy(&out.0).into_owned(), OUTPUT));
    let reads = reads.lock().map(|r| r.clone()).unwrap_or_default();
    (rendered, reads)
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
pub fn changed(reads: &Reads) -> bool {
    (reads.seen.iter()).any(|(path, was)| stamp_of(path) != *was)
}

/// The environment every template is read in: a blank used that names
/// nothing fails the render, while `{% if %}` may still ask about one.
fn environment() -> Environment<'static> {
    let mut env = Environment::new();
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
    env.add_function("source", move |name: String| -> Result<Value, Error> {
        read(&r, &|reads| {
            reads.sources.insert(name.clone());
        });
        if !d.sources.contains(&name) {
            let yours = match d.sources.is_empty() {
                true => "you have none set up".to_owned(),
                false => format!("yours are {}", d.sources.join(", ")),
            };
            return Err(failed(format!("no source named {name}: {yours}")));
        }
        let log = (d.store.as_deref()).map_or_else(Default::default, |store| {
            let file = recent_file(store, &name);
            let (stamp, log) = log(&file);
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
            ("up", Value::from(log.down.is_none())),
            ("down_since", log.down.map_or(Value::from(()), Value::from)),
            ("latest", log.events.first().map_or(Value::from(()), value)),
            ("events", log.events.iter().map(value).collect()),
        ];
        Ok(Value::from_pairs(fields))
    });
    let (r, d) = (reads.clone(), data.clone());
    env.add_function("file", move |path: String| -> Result<String, Error> {
        let path = d.folder.join(path.trim());
        let stamp = stamp_of(&path);
        read(&r, &|reads| {
            reads.files.insert(path.clone());
            reads.seen.insert(path.clone(), stamp);
        });
        crate::margin::read(&path).map_err(failed)
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
fn failed(why: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidOperation, why.into())
}

/// A template's error in one line, where it is in the template: the kind
/// of error, its detail and the line.
fn said(error: &Error) -> String {
    let what = match error.detail() {
        Some(detail) => detail.to_owned(),
        None => error.kind().to_string(),
    };
    match error.line() {
        Some(line) => format!("line {line}: {what}"),
        None => what,
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
    /// When it started failing, in seconds since the epoch, while the
    /// lookout's newest word on it is that it is down.
    down: Option<i64>,
}

/// The source log `file`, read again only once it changed, and how it
/// stood before it was read.
fn log(file: &Path) -> (Stamp, Arc<Log>) {
    type Cache = HashMap<PathBuf, (Stamp, Arc<Log>)>;
    static READ: OnceLock<Mutex<Cache>> = OnceLock::new();
    let stamp = stamp_of(file);
    if stamp.1.is_none() {
        return (stamp, Arc::default());
    }
    let mut cache = READ
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some((at, log)) = cache.get(file)
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
    let events: Vec<Json> = kept()
        .filter(|kept| kept["lookout"] != true)
        .take(EVENTS)
        .map(|kept| {
            json!({
                "text": kept["text"],
                "id": kept["id"],
                "topic": kept["topic"],
                "from": kept["from"],
                "url": kept["url"],
                "at": kept["at_ms"].as_u64().map(|ms| ms / 1000),
                "fields": kept["fields"],
            })
        })
        .collect();
    let log = Arc::new(Log { events, down });
    cache.insert(file.to_owned(), (stamp, log.clone()));
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
            "line 1: no source named cl: yours are ci"
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
        assert!(text.unwrap_err().starts_with("line 2: "));
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
