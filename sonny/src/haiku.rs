//! A haiku from the sky's busiest agent (the sky design's tenth call).
//! Busy is messages sent and told plus tool calls over a window, ten
//! minutes by default; the title passes only to a challenger half again
//! as busy once the holder has had it ten minutes, a thermostat's margin
//! and hold, so it never flickers. Each new holder writes one: a copy of
//! it, forked where it is and allowed no call, so the request reads as
//! its next would and its prompt cache is warm, as the composer's guess
//! is written. The daemon deletes the copy once idle two minutes should
//! Sonny not get to, and the sky draws no copy the daemon will throw
//! away. The last one, and how many were written today, are kept in
//! `~/.agent/sonny/haiku.json`, so every sky shows the same one and a
//! day's cap, when the person sets one, holds across them. Each one
//! written is added to `~/.agent/sonny/haikus.jsonl` too, kept as long as
//! `[sonny] sky_haiku_keep` says, so a replay shows the one that was up
//! then: a line added each time, never the whole rewritten, and read by a
//! replay alone, so keeping them all costs the live sky nothing.
use agent_client::Client;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long the copy may take; a later haiku is not shown.
const WAIT: Duration = Duration::from_secs(60);
/// A copy left behind is deleted once idle this long.
const EXPIRE: Duration = Duration::from_secs(120);
/// The longest line kept: the chat log's column, less a margin.
pub const WIDTH: usize = 34;

const ASK: &str = "[HAIKU MODE: Sonny's sky, where the person watches their agents work, finds you the busiest agent right now.]
Write a haiku about what you have been doing these last minutes, in your own voice: three lines of five, seven and five syllables.
Plain words anyone passing could read: no file paths, code, names of people, secrets or private details.
Call no tool. Reply with ONLY the three lines, no title, quotes or explanation.";

/// The rule, as `[sonny]` sets it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rule {
    /// Whether haikus are written at all.
    pub on: bool,
    /// The most a day; 0 for no limit, the hold bounding them.
    pub daily: u64,
    /// How long one is kept for replays; none keeps them all.
    pub keep: Option<Duration>,
    pub window: Duration,
    /// How much busier, in percent, a challenger must be.
    pub margin: u64,
    pub hold: Duration,
}

impl Rule {
    /// The person's, or the defaults when the config can't be read.
    pub fn load() -> Self {
        let sonny = agent_client::config::load()
            .map(|config| config.sonny)
            .unwrap_or_default();
        Rule::of(&sonny)
    }

    fn of(sonny: &agent_client::config::Sonny) -> Self {
        Rule {
            on: sonny.sky_haikus,
            daily: sonny.sky_haiku_daily,
            keep: (sonny.sky_haiku_keep > 0)
                .then(|| Duration::from_secs(sonny.sky_haiku_keep.saturating_mul(3600))),
            window: Duration::from_secs(sonny.sky_haiku_window.max(1) * 60),
            margin: sonny.sky_haiku_margin,
            hold: Duration::from_secs(sonny.sky_haiku_hold * 60),
        }
    }
}

impl Default for Rule {
    fn default() -> Self {
        Rule::of(&agent_client::config::Sonny::default())
    }
}

/// Who holds the busiest title, by the rule.
#[derive(Debug)]
pub struct Title {
    pub rule: Rule,
    /// Each message and tool call of the window: when, and whose, by
    /// its place in `names`.
    busy: VecDeque<(Instant, u32)>,
    /// Who has a message or call in the window, and how many, kept as
    /// `busy` changes: weighing goes through who is busy, not through all
    /// they did, and the window keeps no name of its own.
    names: Vec<(String, u64)>,
    places: HashMap<String, u32>,
    /// Places whose count came to none, to give out again.
    free: Vec<u32>,
    /// The holder, by name, and since when.
    pub holder: Option<(String, Instant)>,
    /// When watching began: the first title waits a whole window, so it
    /// goes to who is busiest, not who spoke first.
    opened: Instant,
}

impl Title {
    pub fn new(rule: Rule, now: Instant) -> Self {
        Title {
            rule,
            busy: VecDeque::new(),
            names: Vec::new(),
            places: HashMap::new(),
            free: Vec::new(),
            holder: None,
            opened: now,
        }
    }

    /// `at` sent or was told a message, or called a tool. Nothing is kept
    /// when no haiku is written, nor past the window.
    pub fn note(&mut self, now: Instant, name: &str) {
        if !self.rule.on {
            return;
        }
        self.forget(now);
        let place = match self.places.get(name) {
            Some(&place) => place,
            None => {
                let place = match self.free.pop() {
                    Some(place) => {
                        self.names[place as usize].0 = name.to_owned();
                        place
                    }
                    None => {
                        self.names.push((name.to_owned(), 0));
                        (self.names.len() - 1) as u32
                    }
                };
                self.places.insert(name.to_owned(), place);
                place
            }
        };
        self.names[place as usize].1 += 1;
        self.busy.push_back((now, place));
    }

    /// Let go of what is older than the window.
    fn forget(&mut self, now: Instant) {
        let window = self.rule.window;
        while (self.busy.front()).is_some_and(|(at, _)| now.duration_since(*at) > window) {
            let Some((_, place)) = self.busy.pop_front() else {
                break;
            };
            let (name, count) = &mut self.names[place as usize];
            *count -= 1;
            if *count == 0 {
                self.places.remove(name.as_str());
                name.clear();
                self.free.push(place);
            }
        }
    }

    /// Who the title passes to now, if it passes: the busiest, when it is
    /// the first or beats the holder by the margin once the hold is up.
    /// `gone` says who is no longer here, whose title is free.
    pub fn crown(&mut self, now: Instant, gone: impl Fn(&str) -> bool) -> Option<String> {
        let window = self.rule.window;
        self.forget(now);
        if self.holder.as_ref().is_some_and(|(name, _)| gone(name)) {
            self.holder = None;
        }
        let holder = self.holder.as_ref().map(|(name, _)| name.as_str());
        // The busiest; on a tie, the holder keeps it, else the first by name.
        let (best, most) = (self.names.iter())
            .filter(|(name, n)| *n > 0 && !gone(name))
            .map(|(name, n)| (name.as_str(), *n))
            .max_by_key(|&(name, n)| (n, Some(name) == holder, std::cmp::Reverse(name)))?;
        let passes = match &self.holder {
            None => now.duration_since(self.opened) >= window,
            Some((name, _)) if name == best => false,
            Some((name, since)) => {
                let held = (self.places.get(name.as_str()))
                    .map_or(0, |&place| self.names[place as usize].1);
                now.duration_since(*since) >= self.rule.hold
                    && most > held
                    && most * 100 >= held * (100 + self.rule.margin)
            }
        };
        let best = best.to_owned();
        passes.then(|| {
            self.holder = Some((best.clone(), now));
            best
        })
    }
}

/// The last haiku and today's count, as every sky keeps them; the ones
/// before it are in the [`Past`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Kept {
    /// The daemon's socket, so another daemon's haiku isn't shown.
    pub socket: String,
    /// The local day the count is for, `2026-10-31`.
    pub day: String,
    pub written: u64,
    /// The last haiku written: who wrote it and its three lines, never
    /// none while one was ever written.
    pub by: String,
    pub lines: Vec<String>,
    /// When it was written, and since when its writer held the title.
    pub at_ms: i64,
    pub since_ms: i64,
    /// The hold the last claim was for, `(by, since_ms)`: the haiku being
    /// written for it, or the one written. The last haiku shows until a
    /// newer one is written, so a claim that writes none leaves it.
    pub claim: (String, i64),
    /// Why the last claim wrote no haiku, when it wrote none.
    pub unwritten: Option<Unwritten>,
}

/// Why a claim wrote no haiku. The haiku before it shows on.
#[derive(Clone, Debug, PartialEq)]
pub struct Unwritten {
    pub by: String,
    pub at_ms: i64,
    pub why: Failed,
}

/// How writing a haiku failed.
#[derive(Clone, Debug, PartialEq)]
pub enum Failed {
    /// The daemon could not be reached, or would not copy the agent: what
    /// it said.
    Unreached(String),
    /// The copy's turn did not end in time, failed or was cut short: how
    /// it ended.
    Unanswered(String),
    /// It answered with something other than three short lines, an empty
    /// reply among them.
    NotAHaiku,
}

impl Failed {
    fn code(&self) -> &'static str {
        match self {
            Failed::Unreached(_) => "unreached",
            Failed::Unanswered(_) => "unanswered",
            Failed::NotAHaiku => "not_a_haiku",
        }
    }

    fn detail(&self) -> Option<&str> {
        match self {
            Failed::Unreached(detail) | Failed::Unanswered(detail) => Some(detail),
            Failed::NotAHaiku => None,
        }
    }

    fn read(kept: &Value) -> Option<Failed> {
        let detail = || kept["detail"].as_str().unwrap_or("").to_owned();
        match kept["why"].as_str()? {
            "unreached" => Some(Failed::Unreached(detail())),
            "unanswered" => Some(Failed::Unanswered(detail())),
            "not_a_haiku" => Some(Failed::NotAHaiku),
            _ => None,
        }
    }
}

impl Unwritten {
    fn read(kept: &Value) -> Option<Unwritten> {
        Some(Unwritten {
            by: kept["by"].as_str()?.to_owned(),
            at_ms: kept["at_ms"].as_i64().unwrap_or(0),
            why: Failed::read(kept)?,
        })
    }

    fn json(&self) -> Value {
        json!({"by": self.by, "at_ms": self.at_ms, "why": self.why.code(),
            "detail": self.why.detail()})
    }
}

/// A haiku as kept: who wrote it, its lines, when, and since when its
/// writer held the title.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Poem {
    pub by: String,
    pub lines: Vec<String>,
    pub at_ms: i64,
    pub since_ms: i64,
}

impl Poem {
    pub(crate) fn read(kept: &Value) -> Poem {
        Poem {
            by: kept["by"].as_str().unwrap_or("").to_owned(),
            lines: (kept["lines"].as_array().into_iter().flatten())
                .filter_map(|line| line.as_str().map(str::to_owned))
                .collect(),
            at_ms: kept["at_ms"].as_i64().unwrap_or(0),
            since_ms: kept["since_ms"].as_i64().unwrap_or(0),
        }
    }

    /// Its line in the past, for `socket`'s daemon.
    fn line(socket: &str, poem: &Poem) -> String {
        json!({"socket": socket, "by": poem.by, "lines": poem.lines,
            "at_ms": poem.at_ms, "since_ms": poem.since_ms})
        .to_string()
    }
}

/// The past as a replay reads it, from `from` (Unix ms) on and the hour
/// before, the one up then among them: read where it left off each time,
/// so a haiku another sky writes while it plays shows when the replay
/// reaches it, and a replay holds only its own span.
#[derive(Debug)]
pub struct Past {
    socket: String,
    from: i64,
    /// Where the file was read to, and which file it was, should it be
    /// rewritten under it.
    read: u64,
    file: Option<(u64, u64)>,
    pub poems: Vec<Poem>,
}

impl Past {
    pub fn new(socket: &Path, from: i64) -> Past {
        Past {
            socket: socket.display().to_string(),
            from: from - 3_600_000,
            read: 0,
            file: None,
            poems: Vec::new(),
        }
    }

    /// What was added since the last read; the whole again when the file
    /// was rewritten, as when old ones were let go.
    pub fn read(&mut self, store: &Store) {
        use std::os::unix::fs::MetadataExt;
        let Some(path) = store.past() else {
            return;
        };
        let Ok(mut file) = std::fs::File::open(&path) else {
            return;
        };
        let Ok(meta) = file.metadata() else {
            return;
        };
        let id = (meta.dev(), meta.ino());
        if self.file != Some(id) || meta.len() < self.read {
            (self.file, self.read) = (Some(id), 0);
            self.poems.clear();
        }
        if meta.len() == self.read || file.seek(std::io::SeekFrom::Start(self.read)).is_err() {
            return;
        }
        let mut added = Vec::new();
        if file.read_to_end(&mut added).is_err() {
            return;
        }
        // A line still being written waits for its end.
        let whole = added
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |end| end + 1);
        self.read += whole as u64;
        for line in added[..whole].split(|&b| b == b'\n') {
            let Ok(kept) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if kept["socket"].as_str() != Some(self.socket.as_str()) {
                continue;
            }
            let poem = Poem::read(&kept);
            if poem.lines.len() == 3 && poem.at_ms >= self.from {
                self.poems.push(poem);
            }
        }
    }
}

/// The upgrade migration that keeps each daemon's claim apart from the
/// haiku shown, in `haiku.json` (0.8.1).
pub const CLAIMS: &str = "haiku-claims";

/// `haiku.json` as Sonny 0.8.0 wrote it, made current once for its HOME,
/// recorded in `migrated` beside it: each daemon's claim was its `by` and
/// `since_ms`, and a claim cleared the haiku, so one that wrote nothing
/// left none; the last of the day's shows in its place.
pub fn upgrade(store: &Store) {
    let Some(file) = store.file.clone() else {
        return;
    };
    let Some(record) = file.parent().map(|dir| dir.join("migrated")) else {
        return;
    };
    crate::upgrade::once(&record, CLAIMS, || {
        if !file.exists() {
            return true;
        }
        let Some(_lock) = store.lock() else {
            return false;
        };
        // A file that can't be read as one is left as it is, and tried
        // again at the next sky, rather than written over empty.
        let Some(Value::Object(mut all)) = (std::fs::read(&file).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        else {
            return false;
        };
        for kept in all.values_mut().filter_map(Value::as_object_mut) {
            if kept.contains_key("claim") {
                continue;
            }
            let claim = json!({"by": kept.get("by").cloned().unwrap_or(json!("")),
                "since_ms": kept.get("since_ms").cloned().unwrap_or(json!(0))});
            kept.insert("claim".into(), claim);
            let three = |poem: &Value| poem["lines"].as_array().is_some_and(|l| l.len() == 3);
            if !three(&Value::Object(kept.clone())) {
                let past = kept.get("past").and_then(Value::as_array);
                let last = (past.and_then(|past| past.iter().rev().find(|poem| three(poem))))
                    .cloned()
                    .unwrap_or_else(|| json!({"by": "", "lines": [], "at_ms": 0, "since_ms": 0}));
                for field in ["by", "lines", "at_ms", "since_ms"] {
                    kept.insert(field.into(), last[field].clone());
                }
            }
        }
        store.write(&Value::Object(all)).is_ok()
    });
    crate::upgrade::once(&record, PAST, || {
        if !file.exists() {
            return true;
        }
        let Some(_lock) = store.lock() else {
            return false;
        };
        let Some(Value::Object(all)) = (std::fs::read(&file).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        else {
            return false;
        };
        move_past(store, all)
    });
}

/// The upgrade migration that moves each daemon's day of haikus out of
/// `haiku.json`, which every sky reads each second, into `haikus.jsonl`,
/// which only a replay reads (0.9.0).
pub const PAST: &str = "haiku-past";

/// `all`, `haiku.json` as read, with each daemon's `past` moved to the
/// past file, among what it holds, oldest first: a run cut short and run
/// again adds none twice. Whether it is done.
fn move_past(store: &Store, mut all: serde_json::Map<String, Value>) -> bool {
    let Some(path) = store.past() else {
        return false;
    };
    let mut poems: Vec<(String, Poem)> = Vec::new();
    let mut moved = false;
    for (socket, kept) in all.iter_mut() {
        let Some(Value::Array(past)) = kept.as_object_mut().and_then(|kept| kept.remove("past"))
        else {
            continue;
        };
        moved = true;
        poems.extend(
            (past.iter().map(Poem::read))
                .filter(|poem| poem.lines.len() == 3)
                .map(|poem| (socket.clone(), poem)),
        );
    }
    if !moved {
        return true;
    }
    let had = match std::fs::read_to_string(&path) {
        Ok(had) => had,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return false,
    };
    for kept in had
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        if let Some(socket) = kept["socket"].as_str() {
            poems.push((socket.to_owned(), Poem::read(&kept)));
        }
    }
    poems.sort_by_key(|(socket, poem)| (poem.at_ms, socket.clone()));
    poems.dedup_by(|a, b| a.0 == b.0 && a.1.at_ms == b.1.at_ms);
    let text: String = (poems.iter())
        .map(|(socket, poem)| Poem::line(socket, poem) + "\n")
        .collect();
    let part = path.with_extension(format!("jsonl.{}", std::process::id()));
    let written = std::fs::write(&part, text).and_then(|()| std::fs::rename(&part, &path));
    written.is_ok() && store.write(&Value::Object(all)).is_ok()
}

/// The upgrade migration that names `[sonny] sky_haikus`, a count in
/// 0.8, as the switch and the day's most it now is (0.9.0): `0` turned
/// haikus off, any other count is `sky_haiku_daily`.
pub const DAILY: &str = "haiku-daily";

/// At start, once, before the config is read: a count in `sky_haikus`
/// moved to where it means the same now.
pub fn at_start() {
    let (Some(home), Some(path)) = (
        std::env::var_os("HOME").map(PathBuf::from),
        agent_client::config::path(),
    ) else {
        return;
    };
    // No config, nothing to move: and nothing made, not even a HOME gone.
    if !path.exists() {
        return;
    }
    crate::upgrade::once(&home.join(".agent/sonny/migrated"), DAILY, || {
        config_daily(&path)
    });
}

/// The config at `path` with a count in `sky_haikus` moved, each other
/// line as the person wrote it. Whether it is done: one it can't move
/// cleanly is left as it is, for the config's own error to name the key,
/// and tried again next time.
fn config_daily(path: &Path) -> bool {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return e.kind() == std::io::ErrorKind::NotFound,
    };
    match dailied(&text) {
        Err(()) => false,
        Ok(None) => true,
        Ok(Some(moved)) => {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).map_or(0o600, |m| m.permissions().mode() & 0o777);
            agent_client::files::replace_mode(path, moved.as_bytes(), mode).is_ok()
        }
    }
}

/// `text` as [`config_daily`] writes it: none when `[sonny] sky_haikus`
/// is no count. The line written must parse to the config with only
/// that key moved, so a string holding `sky_haikus =` is never touched,
/// and a key lines can't move (a dotted key, an inline table) is an
/// error.
fn dailied(text: &str) -> Result<Option<String>, ()> {
    use toml::Value;
    let old: toml::Table = text.parse().map_err(drop)?;
    let count = (old.get("sonny").and_then(Value::as_table))
        .and_then(|sonny| sonny.get("sky_haikus"))
        .and_then(Value::as_integer);
    let Some(count) = count else {
        return Ok(None);
    };
    let mut expected = old.clone();
    let Some(Value::Table(sonny)) = expected.get_mut("sonny") else {
        return Err(());
    };
    let line = match count {
        0 => {
            sonny.insert("sky_haikus".into(), Value::Boolean(false));
            "sky_haikus = false".to_owned()
        }
        n => {
            sonny.remove("sky_haikus");
            sonny.insert("sky_haiku_daily".into(), Value::Integer(n));
            format!("sky_haiku_daily = {n}")
        }
    };
    let mut table = String::new();
    let mut moved = false;
    let mut lines = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
            table = trimmed.trim_matches(['[', ']']).trim().to_owned();
        }
        let key = trimmed.split_once('=').map(|(key, _)| key.trim());
        if table == "sonny" && key == Some("sky_haikus") && !moved {
            let indent = &raw[..raw.len() - raw.trim_start().len()];
            lines.push(format!("{indent}{line}"));
            moved = true;
            continue;
        }
        lines.push(raw.to_owned());
    }
    let mut new = lines.join("\n");
    if text.ends_with('\n') {
        new.push('\n');
    }
    match new.parse::<toml::Table>() {
        Ok(parsed) if moved && parsed == expected => Ok(Some(new)),
        _ => Err(()),
    }
}

/// Where every sky keeps its haikus: the last one and the day's count in
/// `~/.agent/sonny/haiku.json`, every one written in `haikus.jsonl` beside
/// it.
#[derive(Clone, Debug)]
pub struct Store {
    file: Option<PathBuf>,
}

impl Store {
    #[cfg(not(test))]
    pub fn new() -> Store {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Store {
            file: home.map(|home| home.join(".agent/sonny/haiku.json")),
        }
    }

    /// Tests keep their haikus apart from the person's, and from each
    /// other's.
    #[cfg(test)]
    pub fn new() -> Store {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sonny-haiku-{}-{n}", std::process::id()));
        Store {
            file: Some(dir.join("haiku.json")),
        }
    }

    /// Every haiku written, a line each, oldest first.
    fn past(&self) -> Option<PathBuf> {
        Some(self.file.as_ref()?.with_file_name("haikus.jsonl"))
    }

    /// Add `poem`, written for `socket`'s daemon, to the past, and let go
    /// of what is older than `keep`, if anything is: a line added, the
    /// file rewritten only when its first line has aged out, under the
    /// lock the caller holds.
    pub(crate) fn remember(
        &self,
        socket: &str,
        poem: &Poem,
        keep: Option<Duration>,
    ) -> std::io::Result<()> {
        let path = self.past().ok_or(std::io::ErrorKind::NotFound)?;
        if let Some(keep) = keep {
            let since = poem.at_ms - keep.as_millis() as i64;
            self.forget(&path, since)?;
        }
        let mut line = Poem::line(socket, poem);
        line.push('\n');
        let mut file = (std::fs::OpenOptions::new().create(true).append(true)).open(&path)?;
        file.write_all(line.as_bytes())
    }

    /// The past at `path` with only what was written after `since`: left
    /// as it is when its first haiku is, so most writes read a line.
    fn forget(&self, path: &Path, since: i64) -> std::io::Result<()> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut lines = std::io::BufReader::new(file).lines();
        let at = |line: &str| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|poem| poem["at_ms"].as_i64())
        };
        match lines.next().transpose()? {
            Some(first) if at(&first).is_some_and(|at| at <= since) => {}
            _ => return Ok(()),
        }
        let mut kept = String::new();
        for line in lines {
            let line = line?;
            if at(&line).is_some_and(|at| at > since) {
                kept.push_str(&line);
                kept.push('\n');
            }
        }
        let part = path.with_extension(format!("jsonl.{}", std::process::id()));
        std::fs::write(&part, kept)?;
        std::fs::rename(&part, path)
    }

    /// The file's lock, held until it is dropped: one sky reads, changes
    /// and writes it at a time.
    fn lock(&self) -> Option<std::fs::File> {
        let lock = self.file.as_ref()?.with_extension("json.lock");
        std::fs::create_dir_all(lock.parent()?).ok()?;
        let lock = (std::fs::OpenOptions::new().create(true).truncate(false))
            .write(true)
            .open(lock)
            .ok()?;
        lock.lock().ok()?;
        Some(lock)
    }

    /// Write `all` as the file, whole or not at all, should a sky stop
    /// halfway.
    fn write(&self, all: &Value) -> std::io::Result<()> {
        let file = self.file.as_ref().ok_or(std::io::ErrorKind::NotFound)?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let part = file.with_extension(format!("json.{}", std::process::id()));
        std::fs::write(&part, all.to_string())?;
        std::fs::rename(&part, file)
    }

    /// Every daemon's haiku, by socket, as the file keeps them.
    fn all(&self) -> serde_json::Map<String, Value> {
        (self.file.as_ref())
            .and_then(|file| std::fs::read(file).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|all| match all {
                Value::Object(all) => Some(all),
                _ => None,
            })
            .unwrap_or_default()
    }
}

impl Kept {
    /// What the file says for `socket`, or nothing kept.
    pub fn load(store: &Store, socket: &Path) -> Kept {
        let socket = socket.display().to_string();
        let kept = store.all().remove(&socket).filter(Value::is_object);
        let Some(kept) = kept else {
            return Kept {
                socket,
                ..Kept::default()
            };
        };
        let shown = Poem::read(&kept);
        let claim = (
            kept["claim"]["by"].as_str().unwrap_or("").to_owned(),
            kept["claim"]["since_ms"].as_i64().unwrap_or(0),
        );
        Kept {
            day: kept["day"].as_str().unwrap_or("").to_owned(),
            written: kept["written"].as_u64().unwrap_or(0),
            by: shown.by,
            lines: shown.lines,
            at_ms: shown.at_ms,
            since_ms: shown.since_ms,
            claim,
            unwritten: Unwritten::read(&kept["unwritten"]),
            socket,
        }
    }

    /// Read, change and write the file for `socket` as one step, no other
    /// sky reading or writing it between: what `change` returns, and the
    /// file as it then is. `None` when the file can't be locked or written,
    /// so nothing counted is spent.
    pub fn update<T>(
        store: &Store,
        socket: &Path,
        change: impl FnOnce(&mut Kept) -> T,
    ) -> Option<(T, Kept)> {
        // Held until the end of the step, and let go as the file closes.
        let _lock = store.lock()?;
        let mut kept = Kept::load(store, socket);
        let changed = change(&mut kept);
        kept.save(store).ok()?;
        Some((changed, kept))
    }

    /// Keep `lines` as the haiku `by` wrote for its claim since `since_ms`,
    /// at `now_ms`, if that claim is still the one kept: whether it was.
    /// A newer claim that took over while it was written keeps its own.
    /// Added to the past too, for replays, kept as long as `keep` says.
    pub fn publish(
        store: &Store,
        socket: &Path,
        by: &str,
        since_ms: i64,
        lines: &[String; 3],
        now_ms: i64,
        keep: Option<Duration>,
    ) -> bool {
        let poem = Poem {
            by: by.to_owned(),
            lines: lines.to_vec(),
            at_ms: now_ms,
            since_ms,
        };
        Kept::update(store, socket, |kept| {
            let current = kept.claim == (by.to_owned(), since_ms);
            if current {
                kept.by = by.to_owned();
                kept.since_ms = since_ms;
                kept.lines = lines.to_vec();
                kept.at_ms = now_ms;
                kept.unwritten = None;
                // Shown however the past fares: a replay misses it at most.
                let _ = store.remember(&kept.socket, &poem, keep);
            }
            current
        })
        .is_some_and(|(current, _)| current)
    }

    /// Note that the claim `by` held since `since_ms` wrote no haiku, at
    /// `now_ms`, if it is still the one kept. The haiku before shows on,
    /// and the claim stays counted, so a copy that keeps failing costs no
    /// more than the day's cap.
    pub fn unwritten(
        store: &Store,
        socket: &Path,
        by: &str,
        since_ms: i64,
        why: Failed,
        now_ms: i64,
    ) {
        Kept::update(store, socket, |kept| {
            if kept.claim == (by.to_owned(), since_ms) {
                kept.unwritten = Some(Unwritten {
                    by: by.to_owned(),
                    at_ms: now_ms,
                    why,
                });
            }
        });
    }

    fn save(&self, store: &Store) -> std::io::Result<()> {
        // Each daemon's apart, by its socket; the others' as they are.
        let mut all = store.all();
        let mut kept = json!({"day": self.day, "written": self.written, "by": self.by,
            "lines": self.lines, "at_ms": self.at_ms, "since_ms": self.since_ms,
            "claim": {"by": self.claim.0, "since_ms": self.claim.1},
            "unwritten": self.unwritten.as_ref().map(Unwritten::json)});
        // A day of past haikus [`PAST`] has not moved yet stays where it
        // is, for that migration to move when it runs again.
        if let Some(past) = all.get(&self.socket).and_then(|old| old.get("past")) {
            kept["past"] = past.clone();
        }
        all.insert(self.socket.clone(), kept);
        store.write(&Value::Object(all))
    }

    /// Who holds the title at `now_ms`, and since when: the last claim's
    /// agent, whether or not it wrote its haiku, while that claim is
    /// younger than `within`.
    pub fn holder(&self, now_ms: i64, within: std::time::Duration) -> Option<(String, i64)> {
        let (holder, since_ms) = &self.claim;
        (!holder.is_empty() && now_ms - since_ms < within.as_millis() as i64)
            .then(|| (holder.clone(), *since_ms))
    }

    /// Whether a haiku may be written now, at `now_ms`, by `by`, holder
    /// since `since_ms`: haikus are on, today's count is under the cap, if
    /// there is one, and no sky wrote one for this same hold already.
    /// Counts it, when it may.
    pub fn claim(&mut self, rule: &Rule, by: &str, since_ms: i64, now_ms: i64) -> bool {
        let today = day(now_ms);
        if self.day != today {
            self.day = today;
            self.written = 0;
        }
        let (held_by, held_since) = &self.claim;
        let same = held_by == by && (since_ms - held_since).abs() < rule.hold.as_millis() as i64;
        if !rule.on || same || (rule.daily > 0 && self.written >= rule.daily) {
            return false;
        }
        self.written += 1;
        self.claim = (by.to_owned(), since_ms);
        self.unwritten = None;
        true
    }
}

/// The local day of `ms`, as the count is kept by.
fn day(ms: i64) -> String {
    let local = crate::trigger::local(ms.div_euclid(1000));
    format!("{}-{:02}-{:02}", local.year, local.month, local.day)
}

/// The haiku a copy of `bot` writes, or why it wrote none worth showing.
pub async fn write(socket: PathBuf, bot: String) -> Result<[String; 3], Failed> {
    let unreached = |e: agent_client::Error| Failed::Unreached(e.to_string());
    let (client, _events) = Client::connect(&socket).await.map_err(unreached)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let name = copy_name(&bot, stamp);
    let forked = client
        .request(
            "fork",
            json!({"source": bot, "bot": name, "allow": [],
                "expire_ms": EXPIRE.as_millis() as u64, "request_id": name}),
        )
        .await
        .map_err(unreached)?;
    let reply = ask(&client, &name, &forked["bot_id"]).await;
    let _ = client
        .request("delete", json!({"bot": name, "bot_id": forked["bot_id"]}))
        .await;
    clean(&reply?).ok_or(Failed::NotAHaiku)
}

/// The copy's name: `bot` cut short enough that the name, and the
/// submit's request id after it, stay within the daemon's 128 bytes.
fn copy_name(bot: &str, stamp: u128) -> String {
    let tail = format!(".haiku-{:06x}", stamp % 0x100_0000);
    // Room for the ".1" the submit's request id adds.
    let mut keep = (126 - tail.len()).min(bot.len());
    while !bot.is_char_boundary(keep) {
        keep -= 1;
    }
    format!("{}{tail}", &bot[..keep])
}

/// The copy's reply, interrupting a turn that runs past `WAIT`.
async fn ask(client: &Client, name: &str, id: &Value) -> Result<String, Failed> {
    let unreached = |e: agent_client::Error| Failed::Unreached(e.to_string());
    let submitted = client
        .request(
            "submit",
            json!({"bot": name, "bot_id": id, "request_id": format!("{name}.1"),
                "prompt": ASK, "delivery": "steer"}),
        )
        .await
        .map_err(unreached)?;
    let handle = (submitted["handle"].as_str())
        .ok_or_else(|| Failed::Unreached(format!("submit answered no handle: {submitted}")))?;
    let waited = client
        .request(
            "wait",
            json!({"handles": [handle], "timeout_ms": WAIT.as_millis() as u64}),
        )
        .await
        .map_err(unreached)?;
    let outcome = &waited["results"][handle];
    if outcome["pending"] == true {
        if let Some(turn) = submitted["turn"].as_i64() {
            let _ = client
                .request("interrupt", json!({"bot": name, "turn": turn}))
                .await;
        }
        return Err(Failed::Unanswered(format!(
            "still running after {}s",
            WAIT.as_secs()
        )));
    }
    let status = outcome["status"].as_str().unwrap_or("unknown");
    if status != "completed" {
        let error = outcome["error"]["code"].as_str().unwrap_or("");
        return Err(Failed::Unanswered(
            format!("{status} {error}").trim().to_owned(),
        ));
    }
    if outcome["text_truncated"] == true {
        return Err(Failed::Unanswered("its reply was cut short".into()));
    }
    Ok(outcome["text"].as_str().unwrap_or("").to_owned())
}

/// The reply as three lines, or `None` when it is not a haiku: more or
/// fewer lines, or one too long for the column.
pub fn clean(reply: &str) -> Option<[String; 3]> {
    let lines: Vec<String> = (reply.lines())
        .map(|line| {
            line.trim()
                .trim_matches(|c: char| matches!(c, '*' | '_' | '"' | '“' | '”' | '`'))
                .trim()
                .to_owned()
        })
        .filter(|line| !line.is_empty() && !line.starts_with("```"))
        .collect();
    let fits = |line: &String| line.chars().count() <= WIDTH && !line.contains(['#', '<', '>']);
    let lines: [String; 3] = lines.try_into().ok()?;
    lines.iter().all(fits).then_some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> Rule {
        Rule {
            on: true,
            daily: 8,
            keep: Some(Duration::from_secs(24 * 3600)),
            window: Duration::from_secs(600),
            margin: 50,
            hold: Duration::from_secs(600),
        }
    }

    #[test]
    fn a_copy_s_name_and_request_id_fit_the_daemon_s_names() {
        assert_eq!(copy_name("sky", 0xabc), "sky.haiku-000abc");
        let long = "b".repeat(128);
        let name = copy_name(&long, 0x1234_5678);
        assert_eq!(name.len(), 126);
        assert!(name.ends_with(".haiku-345678"));
        assert!(format!("{name}.1").len() <= 128);
    }

    #[test]
    fn the_title_waits_a_window_then_passes_by_margin_and_hold() {
        let start = Instant::now();
        let mut title = Title::new(rule(), start);
        let at = |s: u64| start + Duration::from_secs(s);
        title.note(at(1), "a");
        title.note(at(2), "b");
        title.note(at(3), "b");
        // Watched less than a window: no title yet.
        assert_eq!(title.crown(at(300), |_| false), None);
        title.note(at(400), "b");
        assert_eq!(title.crown(at(601), |_| false), Some("b".to_owned()));
        assert_eq!(title.crown(at(602), |_| false), None);
        // Busier, but held less than the hold.
        for s in 610..620 {
            title.note(at(s), "a");
        }
        assert_eq!(title.crown(at(700), |_| false), None);
        // Once the hold is up, half again as busy takes it: 10 against 1.
        assert_eq!(title.crown(at(1202), |_| false), Some("a".to_owned()));
        // Not busier by the margin: 0 has 10, 1 has 12, short of 15.
        for s in 1210..1220 {
            title.note(at(s), "a");
        }
        for s in 1210..1222 {
            title.note(at(s), "b");
        }
        assert_eq!(title.crown(at(1802), |_| false), None);
        // A holder that goes leaves the title free for the busiest.
        assert_eq!(
            title.crown(at(1803), |name| name == "a"),
            Some("b".to_owned())
        );
    }

    #[test]
    fn the_title_keeps_no_more_than_its_window_and_nothing_when_off() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let mut title = Title::new(rule(), start);
        for s in 0..100 {
            title.note(at(s * 60), "a");
        }
        assert!(title.busy.len() <= 11, "{}", title.busy.len());
        let mut off = Title::new(
            Rule {
                on: false,
                ..rule()
            },
            start,
        );
        off.note(at(1), "a");
        assert!(off.busy.is_empty());
    }

    #[test]
    fn a_claim_is_read_counted_and_written_as_one_step() {
        let store = Store::new();
        let rule = Rule { daily: 1, ..rule() };
        let socket = Path::new("/tmp/agent-claim.sock");
        let ms = 1_793_400_000_000;
        let (first, kept) =
            Kept::update(&store, socket, |kept| kept.claim(&rule, "a", ms, ms)).unwrap();
        assert!(first);
        assert_eq!(kept.written, 1);
        // Another sky, after: the file already counts the first.
        let (second, _) = Kept::update(&store, socket, |kept| {
            kept.claim(&rule, "b", ms + 1, ms + 1)
        })
        .unwrap();
        assert!(!second);
        assert_eq!(Kept::load(&store, socket).claim, ("a".to_owned(), ms));
        // A haiku is kept only for the claim it was written for.
        let lines = ["x".to_owned(), "y".into(), "z".into()];
        let keep = rule.keep;
        assert!(!Kept::publish(
            &store,
            socket,
            "a",
            ms - 1,
            &lines,
            ms + 2,
            keep
        ));
        assert!(Kept::load(&store, socket).lines.is_empty());
        assert!(Kept::publish(&store, socket, "a", ms, &lines, ms + 3, keep));
        let kept = Kept::load(&store, socket);
        assert_eq!((kept.lines, kept.at_ms), (lines.to_vec(), ms + 3));
        let _ = std::fs::remove_dir_all(store.file.unwrap().parent().unwrap());
    }

    /// Every haiku in the past file, for `socket`, from the start.
    fn past(store: &Store, socket: &Path) -> Vec<Poem> {
        let mut past = Past::new(socket, i64::MIN / 2);
        past.read(store);
        past.poems
    }

    #[test]
    fn haikus_are_kept_for_replays_as_long_as_the_person_says() {
        let store = Store::new();
        let socket = Path::new("/tmp/agent-past.sock");
        let hour = 3_600_000;
        let ms = 1_793_400_000_000;
        let lines = |n: i64| [format!("line {n}"), "y".into(), "z".into()];
        let write = |rule: &Rule, n: i64, by: &str| {
            let at = ms + n * hour;
            Kept::update(&store, socket, |kept| kept.claim(rule, by, at, at)).unwrap();
            assert!(Kept::publish(
                &store,
                socket,
                by,
                at,
                &lines(n),
                at + 1,
                rule.keep
            ));
        };
        let day = rule();
        for (n, by) in [(0, "a"), (1, "b"), (24, "c")] {
            write(&day, n, by);
        }
        // The first went a day after it was written; the last is among them.
        let kept = past(&store, socket);
        let at: Vec<(&str, i64)> = (kept.iter())
            .map(|poem| (poem.by.as_str(), poem.at_ms))
            .collect();
        assert_eq!(at, [("b", ms + hour + 1), ("c", ms + 24 * hour + 1)]);
        assert_eq!(kept[1].lines, lines(24).to_vec());
        assert_eq!(kept[1].since_ms, ms + 24 * hour);
        // Kept all, nothing goes, a year on; kept two hours, only the last.
        let all = Rule {
            keep: None,
            ..rule()
        };
        write(&all, 24 * 365, "d");
        assert_eq!(past(&store, socket).len(), 3);
        let two = Rule {
            keep: Some(Duration::from_secs(7200)),
            ..rule()
        };
        write(&two, 24 * 366, "e");
        let kept = past(&store, socket);
        assert_eq!(
            kept.iter().map(|p| p.by.as_str()).collect::<Vec<_>>(),
            ["e"]
        );
        // The live file keeps only the last: what each sky reads a second.
        let live = std::fs::read_to_string(store.file.as_ref().unwrap()).unwrap();
        assert!(
            !live.contains("line 0") && !live.contains("\"past\""),
            "{live}"
        );
        let _ = std::fs::remove_dir_all(store.file.unwrap().parent().unwrap());
    }

    #[test]
    fn a_replay_reads_only_what_was_added_and_starts_over_on_a_rewrite() {
        let store = Store::new();
        let socket = Path::new("/tmp/agent-tail.sock");
        let ms = 1_793_400_000_000_i64;
        let poem = |by: &str, at: i64| Poem {
            by: by.into(),
            lines: vec!["x".into(), "y".into(), "z".into()],
            at_ms: at,
            since_ms: at,
        };
        let path = store.past().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // One over an hour before the replay, and another daemon's: not its.
        let mine = socket.display().to_string();
        store
            .remember(&mine, &poem("old", ms - 7_200_000), None)
            .unwrap();
        store
            .remember(&mine, &poem("a", ms - 60_000), None)
            .unwrap();
        store
            .remember("/tmp/other.sock", &poem("o", ms), None)
            .unwrap();
        let mut replay = Past::new(socket, ms);
        replay.read(&store);
        assert_eq!(replay.poems, [poem("a", ms - 60_000)]);
        // Half a line, as a sky writing it now leaves it, waits for its end.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let line = Poem::line(&socket.display().to_string(), &poem("b", ms + 1));
        file.write_all(&line.as_bytes()[..10]).unwrap();
        replay.read(&store);
        assert_eq!(replay.poems.len(), 1);
        file.write_all(&line.as_bytes()[10..]).unwrap();
        file.write_all(b"\n").unwrap();
        replay.read(&store);
        assert_eq!(replay.poems, [poem("a", ms - 60_000), poem("b", ms + 1)]);
        // Rewritten, as when old ones are let go: read again from the start.
        let keep = Some(Duration::from_secs(60));
        store
            .remember(&socket.display().to_string(), &poem("c", ms + 2), keep)
            .unwrap();
        replay.read(&store);
        assert_eq!(replay.poems, [poem("b", ms + 1), poem("c", ms + 2)]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn no_limit_writes_one_each_hold_and_a_cap_still_holds() {
        let mut kept = Kept::default();
        let free = Rule { daily: 0, ..rule() };
        let ms = 1_793_400_000_000;
        // Holders in turn, at one time, so no day turns over.
        for n in 0..50 {
            assert!(kept.claim(&free, &format!("agent-{n}"), ms, ms), "{n}");
        }
        assert_eq!(kept.written, 50);
        let capped = Rule {
            daily: 50,
            ..rule()
        };
        assert!(!kept.claim(&capped, "another", ms, ms));
        // Switched off, as the rule a sky reads just before it claims says,
        // nothing is claimed or counted, limit or none.
        let off = Rule { on: false, ..free };
        assert!(!kept.claim(&off, "one more", ms, ms));
        assert_eq!(kept.written, 50);
    }

    #[test]
    fn a_past_not_yet_moved_outlives_a_write_and_moves_once_it_can() {
        let store = Store::new();
        let file = store.file.clone().unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let ms = 1_793_400_000_000_i64;
        let socket = Path::new("/tmp/a.sock");
        let old = json!({"/tmp/a.sock": {"day": "2026-11-05", "written": 1, "by": "a",
            "lines": ["x", "y", "z"], "at_ms": ms, "since_ms": ms,
            "claim": {"by": "a", "since_ms": ms},
            "past": [{"by": "a", "lines": ["x", "y", "z"], "at_ms": ms, "since_ms": ms}]}});
        std::fs::write(&file, old.to_string()).unwrap();
        // The past file can't be read, so the move fails and waits.
        let past_file = store.past().unwrap();
        std::fs::create_dir_all(&past_file).unwrap();
        upgrade(&store);
        Kept::update(&store, socket, |kept| kept.written += 1).unwrap();
        let live = std::fs::read_to_string(&file).unwrap();
        assert!(live.contains("\"past\""), "{live}");
        std::fs::remove_dir(&past_file).unwrap();
        upgrade(&store);
        assert_eq!(past(&store, socket).len(), 1);
        assert!(!std::fs::read_to_string(&file).unwrap().contains("past"));
        assert_eq!(Kept::load(&store, socket).written, 2);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn the_day_s_past_moves_out_of_the_file_every_sky_reads() {
        let store = Store::new();
        let file = store.file.clone().unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let ms = 1_793_400_000_000_i64;
        let poem = |by: &str, at: i64| {
            json!({"by": by, "lines": ["x", "y", "z"],
            "at_ms": at, "since_ms": at})
        };
        let old = json!({
            "/tmp/a.sock": {"day": "2026-11-05", "written": 2, "by": "b",
                "lines": ["x", "y", "z"], "at_ms": ms + 3, "since_ms": ms + 3,
                "claim": {"by": "b", "since_ms": ms + 3},
                "past": [poem("a", ms + 1), poem("b", ms + 3)]},
            "/tmp/b.sock": {"day": "2026-11-05", "written": 1, "by": "c",
                "lines": ["x", "y", "z"], "at_ms": ms + 2, "since_ms": ms + 2,
                "claim": {"by": "c", "since_ms": ms + 2},
                "past": [poem("c", ms + 2), {"by": "e", "lines": [], "at_ms": ms}]}});
        std::fs::write(&file, old.to_string()).unwrap();
        // A run cut short after the past was written, before the file was.
        let past_file = store.past().unwrap();
        let first = Poem::read(&poem("a", ms + 1));
        std::fs::write(&past_file, Poem::line("/tmp/a.sock", &first) + "\n").unwrap();
        upgrade(&store);
        let a = past(&store, Path::new("/tmp/a.sock"));
        assert_eq!(
            a.iter().map(|p| p.by.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        let b = past(&store, Path::new("/tmp/b.sock"));
        assert_eq!(b.iter().map(|p| p.by.as_str()).collect::<Vec<_>>(), ["c"]);
        let text = std::fs::read_to_string(&past_file).unwrap();
        let at: Vec<i64> = (text.lines())
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["at_ms"]
                    .as_i64()
                    .unwrap()
            })
            .collect();
        assert_eq!(at, [ms + 1, ms + 2, ms + 3], "oldest first");
        assert!(!std::fs::read_to_string(&file).unwrap().contains("past"));
        let kept = Kept::load(&store, Path::new("/tmp/a.sock"));
        assert_eq!((kept.by.as_str(), kept.written), ("b", 2));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn a_count_in_sky_haikus_moves_to_where_it_means_the_same() {
        let moved = |text: &str| dailied(text).unwrap();
        // 0 turned haikus off; any other count is the day's most.
        assert_eq!(
            moved("[sonny]\nchime = false\nsky_haikus = 0\n").as_deref(),
            Some("[sonny]\nchime = false\nsky_haikus = false\n")
        );
        assert_eq!(
            moved("[sonny]\n  sky_haikus = 12 # a dozen\nsky_haiku_hold = 5").as_deref(),
            Some("[sonny]\n  sky_haiku_daily = 12\nsky_haiku_hold = 5")
        );
        let moved = moved("[sonny]\nsky_haikus = 3\n").unwrap();
        let dir = std::env::temp_dir().join(format!("sonny-daily-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), &moved).unwrap();
        let config = agent_client::config::read(&dir.join("config.toml")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(config.sonny.sky_haikus);
        assert_eq!(config.sonny.sky_haiku_daily, 3);
        // Nothing to move: a switch already, none, or elsewhere.
        assert_eq!(dailied("[sonny]\nsky_haikus = true\n"), Ok(None));
        assert_eq!(dailied("[sonny]\nchime = true\n"), Ok(None));
        assert_eq!(dailied("[judge]\nmodel = \"x\"\n"), Ok(None));
        // A key lines can't move is left for the config's own error.
        assert_eq!(dailied("sonny.sky_haikus = 4\n"), Err(()));
        assert_eq!(dailied("sonny = { sky_haikus = 4 }\n"), Err(()));
    }

    #[test]
    fn a_reply_is_a_haiku_only_as_three_short_lines() {
        let haiku =
            "Tests fail, then pass green\n\n*the pull request waits for me*\nmorning merges in\n";
        assert_eq!(
            clean(haiku).unwrap(),
            [
                "Tests fail, then pass green",
                "the pull request waits for me",
                "morning merges in"
            ]
        );
        assert!(clean("one line").is_none());
        assert!(clean("").is_none());
        assert!(clean("  \n\n").is_none());
        assert!(clean("a\nb\nc\nd").is_none());
        assert!(clean("a\nthis line runs far too long for the column beside\nc").is_none());
        assert!(clean("# Haiku\nb\nc").is_none());
    }

    #[test]
    fn a_haiku_is_written_once_a_hold_and_within_the_day_s_cap() {
        let store = Store::new();
        let rule = Rule { daily: 2, ..rule() };
        let mut kept = Kept {
            socket: "s".into(),
            ..Kept::default()
        };
        let ms = 1_793_400_000_000;
        assert!(kept.claim(&rule, "shop.lead", ms, ms));
        // Another sky, the same hold: already written.
        assert!(!kept.claim(&rule, "shop.lead", ms + 5_000, ms + 6_000));
        assert!(kept.claim(&rule, "infra.lead", ms + 700_000, ms + 700_000));
        // The cap holds for the day, and opens the next.
        assert!(!kept.claim(&rule, "shop.lead", ms + 1_400_000, ms + 1_400_000));
        assert!(kept.claim(&rule, "shop.lead", ms + 86_400_000, ms + 86_400_000));
        assert_eq!(kept.written, 1);
        // Kept for every sky of the same daemon, and only its.
        kept.lines = vec!["a".into(), "b".into(), "c".into()];
        kept.socket = "/tmp/agent.sock".into();
        kept.save(&store).unwrap();
        assert_eq!(Kept::load(&store, Path::new("/tmp/agent.sock")), kept);
        assert!(
            Kept::load(&store, Path::new("/tmp/other.sock"))
                .lines
                .is_empty()
        );
        // Another daemon's claim leaves this one's as it was.
        Kept::update(&store, Path::new("/tmp/other.sock"), |other| {
            other.claim(&rule, "x", ms, ms)
        });
        assert_eq!(Kept::load(&store, Path::new("/tmp/agent.sock")), kept);
        assert_eq!(
            Kept::load(&store, Path::new("/tmp/other.sock")).claim.0,
            "x"
        );
        let _ = std::fs::remove_dir_all(store.file.unwrap().parent().unwrap());
    }

    #[test]
    fn a_claim_that_writes_no_haiku_leaves_the_one_before_showing() {
        let store = Store::new();
        let rule = Rule { daily: 8, ..rule() };
        let socket = Path::new("/tmp/agent-unwritten.sock");
        let ms = 1_793_400_000_000;
        let first = [
            "frost".to_owned(),
            "a build turns green".into(),
            "dawn".into(),
        ];
        Kept::update(&store, socket, |kept| kept.claim(&rule, "a", ms, ms)).unwrap();
        assert!(Kept::publish(&store, socket, "a", ms, &first, ms + 1, None));
        // The next holder's copy answers nothing: the claim is spent, the
        // haiku before shows on in every sky, and the file says why.
        let next = ms + 700_000;
        Kept::update(&store, socket, |kept| kept.claim(&rule, "b", next, next)).unwrap();
        let kept = Kept::load(&store, socket);
        assert_eq!(
            (kept.by.as_str(), kept.lines.clone()),
            ("a", first.to_vec())
        );
        Kept::unwritten(&store, socket, "b", next, Failed::NotAHaiku, next + 1);
        let kept = Kept::load(&store, socket);
        assert_eq!(
            (kept.by.as_str(), kept.lines.clone()),
            ("a", first.to_vec())
        );
        assert_eq!(kept.written, 2);
        assert_eq!(
            kept.unwritten,
            Some(Unwritten {
                by: "b".into(),
                at_ms: next + 1,
                why: Failed::NotAHaiku
            })
        );
        // The title is b's, whose haiku shows nowhere, for its hold: a
        // sky opened now does not hand it on early.
        let hour = std::time::Duration::from_secs(3600);
        assert_eq!(kept.holder(next + 60_000, hour), Some(("b".into(), next)));
        assert_eq!(kept.holder(next + 3_600_000, hour), None);
        // Not tried again for the same hold.
        let (again, _) = Kept::update(&store, socket, |kept| {
            kept.claim(&rule, "b", next + 5, next + 5)
        })
        .unwrap();
        assert!(!again);
        // A failure for a claim taken over since says nothing of the new one.
        let third = next + 700_000;
        Kept::update(&store, socket, |kept| kept.claim(&rule, "c", third, third)).unwrap();
        let late = Failed::Unanswered("failed provider_error".into());
        Kept::unwritten(&store, socket, "b", next, late, third + 1);
        assert_eq!(Kept::load(&store, socket).unwritten, None);
        let _ = std::fs::remove_dir_all(store.file.unwrap().parent().unwrap());
    }

    #[test]
    fn a_file_whose_claim_cleared_the_haiku_shows_the_last_one_written_once_upgraded() {
        // As Sonny 0.8.0 left it: a claim that wrote nothing saved no lines.
        let store = Store::new();
        let file = store.file.clone().unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let ms = 1_793_400_000_000_i64;
        let old = json!({"/tmp/agent.sock": {"day": "2026-11-02", "written": 2,
            "by": "b", "lines": [], "at_ms": ms + 9, "since_ms": ms + 5,
            "past": [{"by": "a", "lines": ["x", "y", "z"], "at_ms": ms + 1, "since_ms": ms}]}});
        std::fs::write(&file, old.to_string()).unwrap();
        upgrade(&store);
        let kept = Kept::load(&store, Path::new("/tmp/agent.sock"));
        assert_eq!(
            (kept.by.as_str(), kept.at_ms, kept.since_ms),
            ("a", ms + 1, ms)
        );
        assert_eq!(kept.lines, ["x", "y", "z"]);
        // The claim it held stays spent.
        assert_eq!(kept.claim, ("b".to_owned(), ms + 5));
        // Once: the file as written since is left as it is.
        let lines = ["n".to_owned(), "o".into(), "w".into()];
        let socket = Path::new("/tmp/agent.sock");
        assert!(Kept::publish(
            &store,
            socket,
            "b",
            ms + 5,
            &lines,
            ms + 20,
            None
        ));
        upgrade(&store);
        assert_eq!(Kept::load(&store, socket).lines, lines);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn a_file_that_cannot_be_read_is_left_as_it_is_and_upgraded_once_it_can() {
        let store = Store::new();
        let file = store.file.clone().unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"{\"/tmp/agent.sock\": {\"by\"").unwrap();
        upgrade(&store);
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"{\"/tmp/agent.sock\": {\"by\""
        );
        // Not counted done: once the file reads, it is upgraded.
        let ms = 1_793_400_000_000_i64;
        let old = json!({"/tmp/agent.sock": {"day": "2026-11-02", "written": 1,
            "by": "a", "lines": ["x", "y", "z"], "at_ms": ms + 1, "since_ms": ms}});
        std::fs::write(&file, old.to_string()).unwrap();
        upgrade(&store);
        let kept = Kept::load(&store, Path::new("/tmp/agent.sock"));
        assert_eq!(kept.claim, ("a".to_owned(), ms));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }
}

#[cfg(test)]
mod scale {
    use super::*;

    /// The busiest-agent window at fleet scale: a ten-minute window of
    /// messages and tool calls from 10,000 agents, weighed once a second.
    /// `cargo test -p sonny --release haiku::scale -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn the_window_at_fleet_scale() {
        /// Resident memory now, in KiB, what was freed given back first so
        /// this counts what is held; none where it can't be read.
        #[cfg(target_os = "linux")]
        fn rss_kib() -> usize {
            // SAFETY: malloc_trim only returns free heap pages to the system.
            unsafe { libc::malloc_trim(0) };
            std::fs::read_to_string("/proc/self/statm")
                .ok()
                .and_then(|s| s.split_whitespace().nth(1)?.parse::<usize>().ok())
                .map_or(0, |pages| pages * 4)
        }

        #[cfg(not(target_os = "linux"))]
        fn rss_kib() -> usize {
            0
        }
        let names: Vec<String> = (0..10_000)
            .map(|n| format!("p{}.task-{n}", n % 100))
            .collect();
        let rule = Rule {
            on: true,
            daily: 8,
            keep: None,
            window: Duration::from_secs(600),
            margin: 50,
            hold: Duration::from_secs(600),
        };
        let start = Instant::now();
        let before = rss_kib();
        let mut title = Title::new(rule, start);
        // 2,000 a second for the window, weighed each second.
        let (mut noted, mut weighed) = (Duration::ZERO, Duration::ZERO);
        for second in 0..600u64 {
            let now = start + Duration::from_secs(second);
            let clock = Instant::now();
            for n in 0..2_000usize {
                title.note(
                    now,
                    &names[(second as usize * 2_000 + n) * 7919 % names.len()],
                );
            }
            noted += clock.elapsed();
            let clock = Instant::now();
            title.crown(now, |_| false);
            weighed += clock.elapsed();
        }
        let held = rss_kib().saturating_sub(before);
        eprintln!(
            "TITLE 1.2M in window: rss +{held} KiB, note {:.0} ns each, crown {:?} each",
            noted.as_nanos() as f64 / 1.2e6,
            weighed / 600
        );
    }
}
