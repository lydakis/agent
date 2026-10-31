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
//! `~/.agent/sonny/haiku.json`, so every sky shows the same one and the
//! daily cap holds across them.
use agent_client::Client;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
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
    /// Haikus a day; 0 for none.
    pub daily: u64,
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
        Rule {
            daily: sonny.sky_haikus,
            window: Duration::from_secs(sonny.sky_haiku_window.max(1) * 60),
            margin: sonny.sky_haiku_margin,
            hold: Duration::from_secs(sonny.sky_haiku_hold * 60),
        }
    }
}

impl Default for Rule {
    fn default() -> Self {
        let sonny = agent_client::config::Sonny::default();
        Rule {
            daily: sonny.sky_haikus,
            window: Duration::from_secs(sonny.sky_haiku_window * 60),
            margin: sonny.sky_haiku_margin,
            hold: Duration::from_secs(sonny.sky_haiku_hold * 60),
        }
    }
}

/// Who holds the busiest title, by the rule.
#[derive(Debug)]
pub struct Title {
    pub rule: Rule,
    /// Each message and tool call of the window: when, and whose.
    busy: VecDeque<(Instant, String)>,
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
            holder: None,
            opened: now,
        }
    }

    /// `at` sent or was told a message, or called a tool. Nothing is kept
    /// when no haiku is written, nor past the window.
    pub fn note(&mut self, now: Instant, name: &str) {
        if self.rule.daily == 0 {
            return;
        }
        self.forget(now);
        self.busy.push_back((now, name.to_owned()));
    }

    /// Let go of what is older than the window.
    fn forget(&mut self, now: Instant) {
        let window = self.rule.window;
        while (self.busy.front()).is_some_and(|(at, _)| now.duration_since(*at) > window) {
            self.busy.pop_front();
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
        let mut counts: HashMap<&str, u64> = HashMap::new();
        for (_, name) in &self.busy {
            *counts.entry(name).or_default() += 1;
        }
        let holder = self.holder.as_ref().map(|(name, _)| name.as_str());
        // The busiest; on a tie, the holder keeps it, else the first seen.
        let (best, most) = (counts.iter())
            .filter(|&(&name, _)| !gone(name))
            .map(|(&name, &n)| (name, n))
            .max_by_key(|&(name, n)| (n, Some(name) == holder, std::cmp::Reverse(name)))?;
        let passes = match &self.holder {
            None => now.duration_since(self.opened) >= window,
            Some((name, _)) if name == best => false,
            Some((name, since)) => {
                let held = counts.get(name.as_str()).copied().unwrap_or(0);
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

/// The last haiku and today's count, as every sky keeps them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Kept {
    /// The daemon's socket, so another daemon's haiku isn't shown.
    pub socket: String,
    /// The local day the count is for, `2026-10-31`.
    pub day: String,
    pub written: u64,
    pub by: String,
    pub lines: Vec<String>,
    /// When it was written, and since when its writer held the title.
    pub at_ms: i64,
    pub since_ms: i64,
}

impl Kept {
    /// What the file says for `socket`, or nothing kept.
    pub fn load(socket: &Path) -> Kept {
        let socket = socket.display().to_string();
        let kept = all().remove(&socket).filter(Value::is_object);
        let Some(kept) = kept else {
            return Kept {
                socket,
                ..Kept::default()
            };
        };
        let text = |key: &str| kept[key].as_str().unwrap_or("").to_owned();
        Kept {
            day: text("day"),
            written: kept["written"].as_u64().unwrap_or(0),
            by: text("by"),
            lines: (kept["lines"].as_array().into_iter().flatten())
                .filter_map(|line| line.as_str().map(str::to_owned))
                .collect(),
            at_ms: kept["at_ms"].as_i64().unwrap_or(0),
            since_ms: kept["since_ms"].as_i64().unwrap_or(0),
            socket,
        }
    }

    /// Read, change and write the file for `socket` as one step, no other
    /// sky reading or writing it between: what `change` returns, and the
    /// file as it then is. `None` when the file can't be locked or written,
    /// so nothing counted is spent.
    pub fn update<T>(socket: &Path, change: impl FnOnce(&mut Kept) -> T) -> Option<(T, Kept)> {
        let lock = file(&home()?).with_extension("json.lock");
        std::fs::create_dir_all(lock.parent()?).ok()?;
        // Held until the end of the step, and let go as the file closes.
        let lock = (std::fs::OpenOptions::new().create(true).truncate(false))
            .write(true)
            .open(lock)
            .ok()?;
        lock.lock().ok()?;
        let mut kept = Kept::load(socket);
        let changed = change(&mut kept);
        kept.save().ok()?;
        Some((changed, kept))
    }

    /// Keep `lines` as the haiku `by` wrote for its claim since `since_ms`,
    /// at `now_ms`, if that claim is still the one kept: whether it was.
    /// A newer claim that took over while it was written keeps its own.
    pub fn publish(socket: &Path, by: &str, since_ms: i64, lines: &[String], now_ms: i64) -> bool {
        Kept::update(socket, |kept| {
            let current = kept.by == by && kept.since_ms == since_ms;
            if current {
                kept.lines = lines.to_vec();
                kept.at_ms = now_ms;
            }
            current
        })
        .is_some_and(|(current, _)| current)
    }

    fn save(&self) -> std::io::Result<()> {
        let home = home().ok_or(std::io::ErrorKind::NotFound)?;
        let file = file(&home);
        // Each daemon's apart, by its socket; the others' as they are.
        let mut all = all();
        all.insert(
            self.socket.clone(),
            json!({"day": self.day, "written": self.written, "by": self.by,
                "lines": self.lines, "at_ms": self.at_ms, "since_ms": self.since_ms}),
        );
        let kept = Value::Object(all);
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Whole or not at all, should a sky stop halfway.
        let part = file.with_extension(format!("json.{}", std::process::id()));
        std::fs::write(&part, kept.to_string())?;
        std::fs::rename(&part, &file)
    }

    /// Whether a haiku may be written now, at `now_ms`, by `by`, holder
    /// since `since_ms`: today's count is under the cap, and no sky wrote
    /// one for this same hold already. Counts it, when it may.
    pub fn claim(&mut self, rule: &Rule, by: &str, since_ms: i64, now_ms: i64) -> bool {
        let today = day(now_ms);
        if self.day != today {
            self.day = today;
            self.written = 0;
        }
        let same = self.by == by && (since_ms - self.since_ms).abs() < rule.hold.as_millis() as i64;
        if same || self.written >= rule.daily {
            return false;
        }
        self.written += 1;
        self.by = by.to_owned();
        self.since_ms = since_ms;
        self.lines.clear();
        self.at_ms = now_ms;
        true
    }
}

/// Every daemon's haiku, by socket, as the file keeps them.
fn all() -> serde_json::Map<String, Value> {
    home()
        .and_then(|home| std::fs::read(file(&home)).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|all| match all {
            Value::Object(all) => Some(all),
            _ => None,
        })
        .unwrap_or_default()
}

/// The local day of `ms`, as the count is kept by.
fn day(ms: i64) -> String {
    let local = crate::trigger::local(ms.div_euclid(1000));
    format!("{}-{:02}-{:02}", local.year, local.month, local.day)
}

fn file(home: &Path) -> PathBuf {
    home.join(".agent/sonny/haiku.json")
}

#[cfg(not(test))]
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Tests keep their haiku apart from the person's, and from each other's.
#[cfg(test)]
fn home() -> Option<PathBuf> {
    let thread = format!("{:?}", std::thread::current().id());
    let thread: String = thread.chars().filter(char::is_ascii_digit).collect();
    Some(std::env::temp_dir().join(format!("sonny-haiku-{}-{thread}", std::process::id())))
}

/// The haiku a copy of `bot` writes, or `None` when it writes none worth
/// showing.
pub async fn write(socket: PathBuf, bot: String) -> Option<Vec<String>> {
    let (client, _events) = Client::connect(&socket).await.ok()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let name = format!("{bot}.haiku-{:x}", stamp % 0x100_0000);
    let forked = client
        .request(
            "fork",
            json!({"source": bot, "bot": name, "allow": [],
                "expire_ms": EXPIRE.as_millis() as u64, "request_id": name}),
        )
        .await
        .ok()?;
    let reply = ask(&client, &name, &forked["bot_id"]).await;
    let _ = client
        .request("delete", json!({"bot": name, "bot_id": forked["bot_id"]}))
        .await;
    clean(&reply?)
}

/// The copy's reply, interrupting a turn that runs past `WAIT`.
async fn ask(client: &Client, name: &str, id: &Value) -> Option<String> {
    let submitted = client
        .request(
            "submit",
            json!({"bot": name, "bot_id": id, "request_id": format!("{name}.1"),
                "prompt": ASK, "delivery": "reject"}),
        )
        .await
        .ok()?;
    let handle = submitted["handle"].as_str()?;
    let waited = client
        .request(
            "wait",
            json!({"handles": [handle], "timeout_ms": WAIT.as_millis() as u64}),
        )
        .await
        .ok()?;
    let outcome = &waited["results"][handle];
    if outcome["pending"] == true {
        if let Some(turn) = submitted["turn"].as_i64() {
            let _ = client
                .request("interrupt", json!({"bot": name, "turn": turn}))
                .await;
        }
        return None;
    }
    (outcome["status"] == "completed" && outcome["text_truncated"] != true)
        .then(|| outcome["text"].as_str().map(str::to_owned))
        .flatten()
}

/// The reply as three lines, or `None` when it is not a haiku: more or
/// fewer lines, or one too long for the column.
pub fn clean(reply: &str) -> Option<Vec<String>> {
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
    (lines.len() == 3 && lines.iter().all(fits)).then_some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> Rule {
        Rule {
            daily: 8,
            window: Duration::from_secs(600),
            margin: 50,
            hold: Duration::from_secs(600),
        }
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
        let mut off = Title::new(Rule { daily: 0, ..rule() }, start);
        off.note(at(1), "a");
        assert!(off.busy.is_empty());
    }

    #[test]
    fn a_claim_is_read_counted_and_written_as_one_step() {
        let rule = Rule { daily: 1, ..rule() };
        let socket = Path::new("/tmp/agent-claim.sock");
        let ms = 1_793_400_000_000;
        let (first, kept) = Kept::update(socket, |kept| kept.claim(&rule, "a", ms, ms)).unwrap();
        assert!(first);
        assert_eq!(kept.written, 1);
        // Another sky, after: the file already counts the first.
        let (second, _) =
            Kept::update(socket, |kept| kept.claim(&rule, "b", ms + 1, ms + 1)).unwrap();
        assert!(!second);
        assert_eq!(Kept::load(socket).by, "a");
        // A haiku is kept only for the claim it was written for.
        let lines = vec!["x".to_owned(), "y".into(), "z".into()];
        assert!(!Kept::publish(socket, "a", ms - 1, &lines, ms + 2));
        assert!(Kept::load(socket).lines.is_empty());
        assert!(Kept::publish(socket, "a", ms, &lines, ms + 3));
        let kept = Kept::load(socket);
        assert_eq!((kept.lines, kept.at_ms), (lines, ms + 3));
        let _ = std::fs::remove_dir_all(home().unwrap());
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
        assert!(clean("a\nb\nc\nd").is_none());
        assert!(clean("a\nthis line runs far too long for the column beside\nc").is_none());
        assert!(clean("# Haiku\nb\nc").is_none());
    }

    #[test]
    fn a_haiku_is_written_once_a_hold_and_within_the_day_s_cap() {
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
        kept.save().unwrap();
        assert_eq!(Kept::load(Path::new("/tmp/agent.sock")), kept);
        assert!(Kept::load(Path::new("/tmp/other.sock")).lines.is_empty());
        // Another daemon's claim leaves this one's as it was.
        Kept::update(Path::new("/tmp/other.sock"), |other| {
            other.claim(&rule, "x", ms, ms)
        });
        assert_eq!(Kept::load(Path::new("/tmp/agent.sock")), kept);
        assert_eq!(Kept::load(Path::new("/tmp/other.sock")).by, "x");
        let _ = std::fs::remove_dir_all(home().unwrap());
    }
}
