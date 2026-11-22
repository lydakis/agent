//! A script's journal, so that a script the tool host was running when it
//! stopped (a crash, an upgrade, a host that gave way) runs on from where
//! it was rather than being answered that its outcome is unknown, as Pi
//! Durable's code mode does.
//!
//! The script is run again from the start, and every result it was given
//! before is given to it again, in the order it was given, without the
//! call running a second time. A script's JavaScript runs only when the
//! host hands it a result, and sees no clock or random number the
//! journal does not hold, so the same results in the same order make the
//! same calls with the same arguments. Once the journal is used up, the
//! script goes on live. A call that was running when the host stopped is
//! not run again: the script is told it was cut off and may have done its
//! work, as the daemon tells a model of a call a crash cut off.
//!
//! One file a call, beside the store, written as the script goes, one line
//! a record: `start` (what the script began with), `call` (a call it made,
//! written before the call starts), `result` (what a call returned,
//! written before the script sees it), `load` (what `load()` read),
//! `yielded` (the call was handed back to the model) and `end` (the answer,
//! once the script ended). Each is written whole by one `write`, which a
//! process that dies leaves in the file: only a stop of the machine itself
//! can lose one, and then the daemon gives up the call anyway.
//!
//! The file is locked while its script runs, so a host that finds it
//! locked knows another host still runs the script, and lets the call be.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The version of everything that turns a journal into what a script
/// sees: the prelude, its globals, how results are handed over. Bump it
/// when that changes, so a script begun before an upgrade is not replayed
/// by different code.
pub const VERSION: u64 = 1;

/// The folder of the journals of the daemon of `store`, beside it, as a
/// workflow's runs are.
pub fn folder(store: &Path) -> PathBuf {
    let store = agent_client::socket::canonical_store(store).unwrap_or_else(|_| store.to_owned());
    let mut dir = store.into_os_string();
    dir.push(".exec");
    PathBuf::from(dir)
}

/// The journal of the call `call_id` of `turn`, in `folder`: the call's id
/// in hex, so any id makes a name, and the name gives the call back.
pub fn path(folder: &Path, turn: i64, call_id: &str) -> PathBuf {
    let hex: String = call_id.bytes().map(|b| format!("{b:02x}")).collect();
    folder.join(format!("{turn}-{hex}.jsonl"))
}

/// The call a journal's name names.
fn call_of(name: &str) -> Option<(i64, String)> {
    let (turn, hex) = name.strip_suffix(".jsonl")?.split_once('-')?;
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    Some((turn.parse().ok()?, String::from_utf8(bytes).ok()?))
}

/// The hash of `text`, in hex.
pub fn hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Lock `file` for this process alone, or say it is locked.
fn lock(file: &std::fs::File) -> bool {
    // SAFETY: flock on a descriptor `file` owns and keeps open; closing
    // it lets the lock go.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// A journal of one call's script, locked while it is open.
pub struct Journal {
    path: PathBuf,
    /// None once a write failed: the file is gone then, so no host
    /// replays a journal that misses what the script was given.
    file: Mutex<Option<std::fs::File>>,
    /// What an earlier run of the script recorded, taken as it is given
    /// to the script again; empty for a script that starts afresh.
    pub replay: Mutex<Replay>,
}

/// What an earlier run of a script recorded.
#[derive(Default, Debug)]
pub struct Replay {
    pub start: Option<Value>,
    /// The calls it made, in order: tool and the hash of its arguments.
    pub calls: Vec<(String, String)>,
    /// The results it was given, in the order it was given them.
    pub results: VecDeque<Given>,
    /// The calls that have a result.
    pub answered: HashSet<u64>,
    pub loads: VecDeque<Option<String>>,
    pub yielded: bool,
    pub end: Option<Value>,
}

/// A result a script was given: the call's number, the clock it saw from
/// then, what it was handed, and the server the call counts under.
#[derive(Debug, Clone)]
pub struct Given {
    pub call: u64,
    pub now_ms: u64,
    pub raw: String,
    pub counted: Option<String>,
}

/// Whether a journal could be picked up.
pub enum Claim {
    /// Read, and locked for this host.
    Taken(Box<Journal>),
    /// Another host holds it: it still runs the script.
    Held,
    /// There is none, or it cannot be replayed.
    None,
}

impl Journal {
    /// A journal for a script that starts afresh, or none when the file
    /// cannot be made: the script runs without one, as before journals.
    pub fn begin(path: PathBuf) -> Option<Journal> {
        std::fs::create_dir_all(path.parent()?).ok()?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        if !lock(&file) {
            return None;
        }
        file.set_len(0).ok()?;
        Some(Journal {
            path,
            file: Mutex::new(Some(file)),
            replay: Mutex::default(),
        })
    }

    /// The journal at `path`, to run its script again: locked, and read.
    pub fn claim(path: &Path) -> Claim {
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
        else {
            return Claim::None;
        };
        if !lock(&file) {
            return Claim::Held;
        }
        let Some(replay) = read(&file) else {
            return Claim::None;
        };
        if replay
            .start
            .as_ref()
            .is_none_or(|start| start["v"] != VERSION)
        {
            return Claim::None;
        }
        Claim::Taken(Box::new(Journal {
            path: path.to_owned(),
            file: Mutex::new(Some(file)),
            replay: Mutex::new(replay),
        }))
    }

    /// Record `lines`, in one write. A journal that cannot be written is
    /// removed, never left missing a record.
    fn write(&self, lines: &[Value]) {
        let mut file = self.file.lock().unwrap();
        let Some(open) = file.as_mut() else {
            return;
        };
        let mut text = String::new();
        for line in lines {
            text.push_str(&line.to_string());
            text.push('\n');
        }
        if open.write_all(text.as_bytes()).is_err() {
            let _ = std::fs::remove_file(&self.path);
            *file = None;
        }
    }

    pub fn start(&self, start: Value) {
        self.write(&[start]);
    }

    /// The calls a script made together, numbered, before they start.
    pub fn calls(&self, calls: &[(u64, String, String)]) {
        let lines: Vec<Value> = (calls.iter())
            .map(|(n, tool, args)| json!({"t": "call", "n": n, "tool": tool, "args": args}))
            .collect();
        self.write(&lines);
    }

    pub fn result(&self, given: &Given) {
        self.write(&[
            json!({"t": "result", "n": given.call, "now_ms": given.now_ms,
            "raw": given.raw, "counted": given.counted}),
        ]);
    }

    pub fn load(&self, value: &Option<String>) {
        self.write(&[json!({"t": "load", "value": value})]);
    }

    pub fn yielded(&self) {
        self.write(&[json!({"t": "yielded"})]);
    }

    pub fn end(&self, answer: &Value) {
        self.write(&[json!({"t": "end", "answer": answer})]);
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Read a journal. A last line cut short, as a process that died while
/// writing it leaves, is no record; any other line that does not read
/// makes the journal unreadable.
fn read(file: &std::fs::File) -> Option<Replay> {
    let mut replay = Replay::default();
    let mut lines = std::io::BufReader::new(file).lines().peekable();
    while let Some(line) = lines.next() {
        let line = line.ok()?;
        let record: Value = match serde_json::from_str(&line) {
            Ok(record) => record,
            Err(_) if lines.peek().is_none() => break,
            Err(_) => return None,
        };
        match record["t"].as_str()? {
            "start" => replay.start = Some(record),
            "call" => {
                if record["n"].as_u64()? != replay.calls.len() as u64 + 1 {
                    return None;
                }
                let tool = record["tool"].as_str()?.to_owned();
                replay
                    .calls
                    .push((tool, record["args"].as_str()?.to_owned()));
            }
            "result" => {
                let call = record["n"].as_u64()?;
                if call > replay.calls.len() as u64 || !replay.answered.insert(call) {
                    return None;
                }
                replay.results.push_back(Given {
                    call,
                    now_ms: record["now_ms"].as_u64()?,
                    raw: record["raw"].as_str()?.to_owned(),
                    counted: record["counted"].as_str().map(str::to_owned),
                });
            }
            "load" => replay
                .loads
                .push_back(record["value"].as_str().map(str::to_owned)),
            "yielded" => replay.yielded = true,
            "end" => replay.end = Some(record["answer"].clone()),
            _ => return None,
        }
    }
    Some(replay)
}

/// Remove the journal of a call that is answered or let go.
pub fn forget(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Remove from `folder` the journals of calls no longer waiting, those
/// `waiting` does not hold, that no host has open; and the tools no
/// journal left names.
pub fn sweep(folder: &Path, waiting: &HashSet<(i64, String)>) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    let mut named = HashSet::new();
    let mut tools = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("tools-") {
            tools.push((name, path));
            continue;
        }
        let Some(call) = call_of(&name) else {
            continue;
        };
        let Ok(file) = std::fs::OpenOptions::new().read(true).open(&path) else {
            continue;
        };
        if !waiting.contains(&call) && lock(&file) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let mut first = String::new();
        if std::io::BufReader::new(&file).read_line(&mut first).is_ok()
            && let Ok(start) = serde_json::from_str::<Value>(&first)
            && let Some(hash) = start["tools"].as_str()
        {
            named.insert(format!("tools-{hash}.json"));
        }
    }
    for (name, path) in tools {
        if !named.contains(&name) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Keep the tools a script was shown under their hash, unless kept
/// already, so a script run again is shown the same ones. Whether they
/// are kept.
pub fn keep_tools(folder: &Path, hash: &str, tools: &str) -> bool {
    let path = folder.join(format!("tools-{hash}.json"));
    if path.exists() {
        return true;
    }
    let partial = folder.join(format!(".tools-{hash}-{}", std::process::id()));
    let written = std::fs::create_dir_all(folder)
        .and_then(|()| std::fs::write(&partial, tools))
        .and_then(|()| std::fs::rename(&partial, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written.is_ok()
}

/// The tools a script was shown, by their hash.
pub fn kept_tools(folder: &Path, hash: &str) -> Option<String> {
    std::fs::read_to_string(folder.join(format!("tools-{hash}.json"))).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("journal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_name_gives_its_call_back() {
        let path = path(Path::new("/s.exec"), 42, "call_ab/c-1");
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(call_of(name), Some((42, "call_ab/c-1".to_owned())));
    }

    #[test]
    fn a_journal_reads_back_and_a_line_cut_short_is_no_record() {
        let dir = scratch("read");
        let at = path(&dir, 1, "c");
        let journal = Journal::begin(at.clone()).unwrap();
        // Locked while its script runs.
        assert!(matches!(Journal::claim(&at), Claim::Held));
        journal.start(json!({"t": "start", "v": VERSION, "code": "x"}));
        journal.calls(&[(1, "a".into(), "h1".into()), (2, "b".into(), "h2".into())]);
        journal.result(&Given {
            call: 2,
            now_ms: 5,
            raw: "{\"ok\":1}".into(),
            counted: Some("test".into()),
        });
        journal.load(&Some("7".into()));
        journal.yielded();
        drop(journal);
        let mut file = std::fs::OpenOptions::new().append(true).open(&at).unwrap();
        file.write_all(b"{\"t\":\"res").unwrap();
        let Claim::Taken(again) = Journal::claim(&at) else {
            panic!("not taken");
        };
        let replay = again.replay.lock().unwrap();
        assert_eq!(replay.calls.len(), 2);
        assert_eq!(replay.results[0].call, 2);
        assert_eq!(replay.results[0].counted.as_deref(), Some("test"));
        assert_eq!(replay.loads, [Some("7".to_owned())]);
        assert!(replay.yielded && replay.end.is_none());
    }

    #[test]
    fn a_journal_of_another_version_or_broken_inside_is_not_replayed() {
        let dir = scratch("version");
        let old = path(&dir, 1, "c");
        std::fs::write(&old, "{\"t\":\"start\",\"v\":0}\n").unwrap();
        assert!(matches!(Journal::claim(&old), Claim::None));
        let start = format!("{{\"t\":\"start\",\"v\":{VERSION}}}\n");
        let broken = path(&dir, 2, "c");
        std::fs::write(&broken, format!("{start}not json\n{{}}\n")).unwrap();
        assert!(matches!(Journal::claim(&broken), Claim::None));
        let skipped = path(&dir, 3, "c");
        std::fs::write(
            &skipped,
            format!("{start}{{\"t\":\"call\",\"n\":2,\"tool\":\"a\",\"args\":\"h\"}}\n"),
        )
        .unwrap();
        assert!(matches!(Journal::claim(&skipped), Claim::None));
        assert!(matches!(Journal::claim(&path(&dir, 4, "c")), Claim::None));
    }

    #[test]
    fn a_sweep_keeps_the_waiting_and_the_open_and_the_tools_they_name() {
        let dir = scratch("sweep");
        let write = |turn: i64, tools: &str| {
            let at = path(&dir, turn, "c");
            std::fs::write(
                &at,
                format!("{{\"t\":\"start\",\"v\":1,\"tools\":\"{tools}\"}}\n"),
            )
            .unwrap();
            at
        };
        let (waiting, gone) = (write(1, "aa"), write(2, "bb"));
        let open = Journal::begin(path(&dir, 3, "c")).unwrap();
        open.start(json!({"t": "start", "v": VERSION, "tools": "cc"}));
        for hash in ["aa", "bb", "cc"] {
            assert!(keep_tools(&dir, hash, "[]"));
        }
        sweep(&dir, &[(1, "c".to_owned())].into());
        assert!(waiting.exists() && !gone.exists() && open.path().exists());
        assert!(kept_tools(&dir, "aa").is_some() && kept_tools(&dir, "cc").is_some());
        assert!(kept_tools(&dir, "bb").is_none());
    }
}
