//! What went out as the person: a journal of the writes agents make
//! through Sonny's tools, kept by the program that makes them, written
//! before each write leaves and again when it ends, so a write is never
//! lost from it however the program stops.
//!
//! One JSON line per change, in `~/.agent/sonny/sent/YYYY-MM-DD.jsonl`
//! (UTC), which only the person reads. A write has an identity that stays
//! across attempts at its call (`TURN:CALL/N`, its place among the call's
//! writes), the attempt it was made in (each answering of the call is
//! one), and a state: `dispatched` once it may have left, then
//! `completed`, `failed`, or `unknown` when nothing will say whether it
//! took effect. Reads are not kept: they may always be made again. What
//! a line keeps of a write's arguments has its credentials redacted.
//!
//! `sonny sent` lists them. It sees only what goes through Sonny's tools:
//! a write a shell command makes is not here, and an approval is a
//! decision, not proof that a write was sent.
use serde_json::{Value, json};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The most of a write's arguments a line keeps, so a line stays one
/// atomic append.
const TARGET_CHARS: usize = 120;

/// What `sonny sent` says before what it lists, whatever it lists.
pub const COVERAGE: &str = "Only what agents did through Sonny's tools: calls to MCP servers \
     that do not say they only read. Writes made by shell commands (a CLI posting a comment) are \
     not seen. An approval is a decision, not proof a write was sent: the state says what \
     happened.";

fn folder() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent/sonny/sent"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The UTC day `ms` falls on, as `YYYY-MM-DD`.
fn day(ms: u64) -> String {
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Who a call's writes are made for: its agent, turn and call, as the
/// daemon names them. A call over a link names the agent of the machine
/// that made it, and no turn here.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Owner {
    pub bot: String,
    pub turn: i64,
    pub call_id: String,
}

/// Answerings begun in this process, so two attempts differ.
static ATTEMPTS: AtomicU64 = AtomicU64::new(0);

/// The writes of one answering of a call, numbered in the order they go.
#[derive(Debug, Default)]
pub struct Writes {
    owner: Owner,
    /// This answering of the call: its writes' identities may repeat in
    /// another, which is a separate attempt at them.
    attempt: String,
    next: AtomicU64,
    /// The journal's folder; none when HOME is not set.
    folder: Option<PathBuf>,
}

impl Writes {
    /// `owner`'s writes, noted in the person's journal.
    pub fn of(owner: Owner) -> Self {
        Self::to(owner, folder())
    }

    /// `owner`'s writes, noted in the journal in `folder`.
    pub fn to(owner: Owner, folder: Option<PathBuf>) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let attempt = format!(
            "{nanos:x}.{:x}.{:x}",
            std::process::id(),
            ATTEMPTS.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            owner,
            attempt,
            next: AtomicU64::new(0),
            folder,
        }
    }

    /// Note `what`, called with `arguments`, as about to leave: what to end
    /// it with. Kept on disk before this returns. A write not ended ends
    /// `unknown` when what holds it is dropped.
    pub fn dispatched(&self, what: &str, arguments: &Value) -> Open {
        let n = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let Owner { bot, turn, call_id } = &self.owner;
        let line = json!({"op": format!("{turn}:{call_id}/{n}"), "attempt": self.attempt,
            "bot": bot, "turn": turn, "call_id": call_id, "n": n, "what": what,
            "target": target(arguments)});
        if let Some(folder) = &self.folder {
            note(folder, &line, "dispatched", true);
        }
        Open {
            line,
            folder: self.folder.clone(),
            ended: false,
        }
    }
}

/// A write that may have left and has not ended.
pub struct Open {
    line: Value,
    folder: Option<PathBuf>,
    ended: bool,
}

/// How a write ended.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ended {
    /// Its destination answered that it was done.
    Completed,
    /// Its destination refused it.
    Failed,
    /// Nothing will say whether it took effect.
    Unknown,
}

impl Open {
    pub fn end(mut self, how: Ended) {
        self.ended = true;
        let state = match how {
            Ended::Completed => "completed",
            Ended::Failed => "failed",
            Ended::Unknown => "unknown",
        };
        if let Some(folder) = &self.folder {
            note(folder, &self.line, state, false);
        }
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        if !self.ended
            && let Some(folder) = &self.folder
        {
            note(folder, &self.line, "unknown", false);
        }
    }
}

/// `arguments` cut to what a line keeps, its credentials redacted first,
/// as the approver redacts what it is shown: the values of the
/// environment's secret variables, keys, tokens, and passwords in URLs.
fn target(arguments: &Value) -> String {
    let redactor = agent_client::approver::Redactor::from_env(std::env::vars());
    let text = match arguments {
        Value::Null => String::new(),
        other => redactor.redact_value(other).to_string(),
    };
    match text.char_indices().nth(TARGET_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// Append `line` in `state` to today's file in `folder`, kept on disk
/// before returning when `kept`. A journal that cannot be written loses
/// the line, never the write: the call goes on.
fn note(folder: &Path, line: &Value, state: &str, kept: bool) {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let at = now_ms();
    let mut line = line.clone();
    line["at"] = json!(at);
    line["state"] = json!(state);
    let mut text = line.to_string();
    text.push('\n');
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(folder);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(folder.join(format!("{}.jsonl", day(at))));
    if let Ok(mut file) = file
        && std::io::Write::write_all(&mut file, text.as_bytes()).is_ok()
        && kept
    {
        let _ = file.sync_data();
    }
}

/// One write as its lines left it: its latest state.
#[derive(Clone, Debug, PartialEq)]
pub struct Noted {
    pub op: String,
    pub attempt: String,
    pub bot: String,
    pub turn: i64,
    pub call_id: String,
    pub what: String,
    pub target: String,
    pub state: String,
    pub at: u64,
}

/// The writes noted in `folder` from `since` on, oldest first, each at its
/// latest state, of `bot` alone when it names one.
pub fn writes_in(folder: &Path, since: u64, bot: Option<&str>) -> Vec<Noted> {
    let first = day(since);
    let mut days: Vec<PathBuf> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension().is_some_and(|e| e == "jsonl")
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem >= first.as_str())
        })
        .collect();
    days.sort();
    let mut order = Vec::new();
    let mut latest: std::collections::HashMap<String, Noted> = Default::default();
    for path in days {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        // Line by line, so a line left torn, as by a full disk, costs
        // only itself.
        for line in bytes.split(|&b| b == b'\n') {
            let Ok(said) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            let at = said["at"].as_u64().unwrap_or_default();
            let named = |key: &str| said[key].as_str().unwrap_or_default().to_owned();
            let write = Noted {
                op: named("op"),
                attempt: named("attempt"),
                bot: named("bot"),
                turn: said["turn"].as_i64().unwrap_or_default(),
                call_id: named("call_id"),
                what: named("what"),
                target: named("target"),
                state: named("state"),
                at,
            };
            if at < since || bot.is_some_and(|bot| bot != write.bot) {
                continue;
            }
            // Its first line's time is when it went; a later one, its end.
            let key = format!("{} {}", write.attempt, write.op);
            match latest.get_mut(&key) {
                Some(kept) => kept.state = write.state,
                None => {
                    order.push(key.clone());
                    latest.insert(key, write);
                }
            }
        }
    }
    order
        .into_iter()
        .filter_map(|key| latest.remove(&key))
        .collect()
}

/// Remove the days in `folder` before `keep_days` ago; 0 keeps all.
fn prune_in(folder: &Path, keep_days: u64) {
    if keep_days == 0 {
        return;
    }
    let first = day(now_ms().saturating_sub(keep_days.saturating_sub(1) * 86_400_000));
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        let path = entry.path();
        let old = path.extension().is_some_and(|e| e == "jsonl")
            && (path.file_stem().and_then(|stem| stem.to_str()))
                .is_some_and(|stem| stem < first.as_str());
        if old {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Keep `[sonny] sent_days` of the journal, as the tool host starts.
pub fn prune() {
    let keep = agent_client::config::load().map_or(30, |config| config.sonny.sent_days);
    if let Some(folder) = folder() {
        prune_in(&folder, keep);
    }
}

/// How the state of a write reads in a list.
fn said(state: &str) -> &str {
    match state {
        "dispatched" => "attempted",
        "unknown" => "uncertain",
        other => other,
    }
}

const USAGE: &str = "usage: sonny sent [--since 1d|12h|30m] [--agent NAME] [--json]";

/// `sonny sent`: what went out as the person, today unless `--since`
/// says, with what it cannot see first.
pub fn cli(args: &[String]) -> Result<(), String> {
    let (mut since, mut agent, mut as_json) = (None, None, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--since" => since = Some(args.next().ok_or(USAGE)?.clone()),
            "--agent" => agent = Some(args.next().ok_or(USAGE)?.clone()),
            "--json" => as_json = true,
            _ => return Err(USAGE.into()),
        }
    }
    let now = now_ms();
    let since = match since {
        Some(text) => now.saturating_sub(span_ms(&text).ok_or(USAGE)?),
        None => now - now % 86_400_000,
    };
    let folder = folder().ok_or("HOME is not set")?;
    let writes = writes_in(&folder, since, agent.as_deref());
    if as_json {
        let listed: Vec<Value> = (writes.iter())
            .map(|w| {
                json!({"op": w.op, "attempt": w.attempt, "agent": w.bot, "turn": w.turn, "call_id": w.call_id,
                    "what": w.what, "target": w.target, "state": said(&w.state), "at": w.at})
            })
            .collect();
        let out = json!({"coverage": COVERAGE, "writes": listed});
        println!("{out}");
        return Ok(());
    }
    println!("{COVERAGE}\n");
    if writes.is_empty() {
        println!("Nothing went out as you in that time.");
    }
    for w in &writes {
        let at = w.at / 1000 % 86_400;
        println!(
            "{:02}:{:02} UTC  {}  {}  {}  {}",
            at / 3600,
            at / 60 % 60,
            w.bot,
            w.what,
            said(&w.state),
            w.target
        );
    }
    Ok(())
}

/// `30m`, `12h` or `2d` in milliseconds.
fn span_ms(text: &str) -> Option<u64> {
    let (number, unit) = text.split_at(text.len().checked_sub(1)?);
    let n: u64 = number.parse().ok()?;
    let unit = match unit {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    n.checked_mul(unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let mut bytes = [0u8; 8];
        let _ = std::fs::File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes));
        let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        std::env::temp_dir().join(format!("sent-{name}"))
    }

    #[test]
    fn a_day_is_its_utc_date() {
        assert_eq!(day(0), "1970-01-01");
        assert_eq!(day(1_762_387_200_000), "2025-11-06");
        assert_eq!(day(951_782_400_000), "2000-02-29");
    }

    #[test]
    fn each_write_lists_once_at_its_latest_state_and_one_dropped_is_uncertain() {
        let folder = scratch();
        let owner = Owner {
            bot: "p.lead".into(),
            turn: 7,
            call_id: "c1".into(),
        };
        let writes = Writes::to(owner, Some(folder.clone()));
        let comment = json!({"text": "hi"});
        let first = writes.dispatched("tools.tracker.comment", &comment);
        // Kept before it leaves, so a stop now leaves it attempted.
        let listed = writes_in(&folder, 0, None);
        assert_eq!((listed.len(), said(&listed[0].state)), (1, "attempted"));
        first.end(Ended::Completed);
        writes
            .dispatched("tools.tracker.comment", &comment)
            .end(Ended::Failed);
        drop(writes.dispatched("tools.tracker.close", &Value::Null));
        let listed = writes_in(&folder, 0, None);
        let states: Vec<(&str, &str, &str)> = (listed.iter())
            .map(|w| (w.op.as_str(), w.what.as_str(), said(&w.state)))
            .collect();
        assert_eq!(
            states,
            [
                ("7:c1/1", "tools.tracker.comment", "completed"),
                ("7:c1/2", "tools.tracker.comment", "failed"),
                ("7:c1/3", "tools.tracker.close", "uncertain")
            ]
        );
        assert_eq!(listed[0].target, r#"{"text":"hi"}"#);
        assert!(writes_in(&folder, 0, Some("q.lead")).is_empty());
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn another_attempt_at_a_call_lists_apart_and_a_torn_line_costs_only_itself() {
        let folder = scratch();
        let owner = Owner {
            bot: "p.lead".into(),
            turn: 7,
            call_id: "c1".into(),
        };
        let (first, again) = (
            Writes::to(owner.clone(), Some(folder.clone())),
            Writes::to(owner, Some(folder.clone())),
        );
        first
            .dispatched("tools.a", &Value::Null)
            .end(Ended::Unknown);
        // A line torn inside a character, as a full disk leaves one.
        let file = folder.join(format!("{}.jsonl", day(now_ms())));
        let mut torn = std::fs::read(&file).unwrap();
        torn.extend_from_slice(b"{\"op\":\"7:c1/9\",\"target\":\"\xc3");
        std::fs::write(&file, torn).unwrap();
        again
            .dispatched("tools.a", &Value::Null)
            .end(Ended::Completed);
        let listed = writes_in(&folder, 0, None);
        let states: Vec<(&str, &str)> = (listed.iter())
            .map(|w| (w.op.as_str(), said(&w.state)))
            .collect();
        assert_eq!(states, [("7:c1/1", "uncertain"), ("7:c1/1", "completed")]);
        assert_ne!(listed[0].attempt, listed[1].attempt);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_target_keeps_no_credentials() {
        let kept = target(&json!({"url": "https://me:hunter2-long-pass@example.com/x",
            "api_key": "sk-abcdefghijklmnopqrstuvwxyz0123456789"}));
        assert!(
            !kept.contains("hunter2") && !kept.contains("sk-abcdef"),
            "{kept}"
        );
    }

    #[test]
    fn days_past_the_kept_ones_go_and_zero_keeps_all() {
        let folder = scratch();
        std::fs::create_dir_all(&folder).unwrap();
        let old = folder.join("2000-01-01.jsonl");
        let today = folder.join(format!("{}.jsonl", day(now_ms())));
        std::fs::write(&old, "").unwrap();
        std::fs::write(&today, "").unwrap();
        prune_in(&folder, 0);
        assert!(old.exists());
        prune_in(&folder, 30);
        assert!(!old.exists() && today.exists());
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_long_target_is_cut_on_a_character() {
        let long = json!({"text": "é".repeat(500)});
        let cut = target(&long);
        assert_eq!(cut.chars().count(), TARGET_CHARS + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(span_ms("2d"), Some(172_800_000));
        assert_eq!(span_ms("x"), None);
    }
}
