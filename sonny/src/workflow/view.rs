//! What `/status` shows of the workflows that run: one row a run, read
//! from its journal, as the end message is. A window reads only what the
//! journal gained since its last look, and never reads a run that ended
//! again; it stops a run by leaving a `stop` file in the run's folder,
//! which the tool host running it acts on.
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// What stands between a row's caller and its run in the row's `bot`:
/// `shop.lead#wf-1a2b3c4d`, which no agent's name holds.
pub const MARK: &str = "#wf-";

/// The file a window leaves in a run's folder to stop it.
pub const STOP: &str = "stop";

/// A run that has not ended, as its journal says so far.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Live {
    pub id: String,
    /// The script's `meta.name`.
    pub name: String,
    pub caller: String,
    pub project: String,
    pub started_ms: u64,
    /// The phase `phase()` set last.
    pub phase: String,
    /// Agents running, done (a value, some from an earlier run) and
    /// failed (null), in all.
    pub running: usize,
    pub done: usize,
    pub failed: usize,
    /// Each phase's agents, in the order the phases first came.
    pub phases: Vec<(String, Counts)>,
    /// Output tokens this run's own agents spent.
    pub tokens: u64,
    /// The newest `log()` line.
    pub log: String,
    /// Whether a stop is asked for and not yet carried out.
    pub stopping: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub running: usize,
    pub done: usize,
    pub failed: usize,
}

impl Live {
    /// The row's `bot`: the caller's, marked with the run.
    pub fn bot(&self) -> String {
        format!("{}{MARK}{}", self.caller, self.id)
    }

    /// Its row's state: `Verify · 12 running, 31 done, 2 failed`.
    pub fn state(&self) -> String {
        let mut said = match self.phase.as_str() {
            "" => String::new(),
            phase => format!("{phase} · "),
        };
        said.push_str(&counted(Counts {
            running: self.running,
            done: self.done,
            failed: self.failed,
        }));
        if self.stopping {
            said.push_str(" · stopping");
        }
        said
    }

    /// What it says on a key: each phase's agents, then its last log
    /// line and the tokens so far.
    pub fn more(&self) -> String {
        let mut parts: Vec<String> = (self.phases.iter())
            .map(|(phase, counts)| match phase.as_str() {
                "" => counted(*counts),
                phase => format!("{phase}: {}", counted(*counts)),
            })
            .collect();
        if !self.log.is_empty() {
            parts.push(self.log.clone());
        }
        parts.push(format!("{} output tokens", self.tokens));
        parts.join(" · ")
    }
}

/// `2 running, 3 done, 1 failed`, leaving out what is none; `starting`
/// before any agent.
fn counted(counts: Counts) -> String {
    let said: Vec<String> = [
        (counts.running, "running"),
        (counts.done, "done"),
        (counts.failed, "failed"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect();
    match said.is_empty() {
        true => "starting".into(),
        false => said.join(", "),
    }
}

/// The caller of a row's `bot`, and its run when it is a run's.
pub fn split(bot: &str) -> (&str, Option<&str>) {
    match bot.split_once(MARK) {
        Some((caller, run)) => (caller, Some(run)),
        None => (bot, None),
    }
}

/// What a window knows of one run between looks.
#[derive(Default)]
struct Fold {
    /// How far into the journal it read, to a line's end.
    read: u64,
    live: Live,
    /// The keys running, with their phase.
    asking: HashMap<String, String>,
    by_phase: BTreeMap<usize, (String, Counts)>,
    ended: bool,
}

impl Fold {
    fn take(&mut self, line: &Value) {
        let phase_of = |line: &Value| line["phase"].as_str().unwrap_or("").to_owned();
        match line["t"].as_str() {
            Some("phase") => self.live.phase = line["title"].as_str().unwrap_or("").to_owned(),
            Some("log") => self.live.log = line["text"].as_str().unwrap_or("").to_owned(),
            Some("asking") => {
                let key = line["key"].as_str().unwrap_or("").to_owned();
                self.asking.insert(key, phase_of(line));
            }
            Some("agent") => {
                let key = line["key"].as_str().unwrap_or("");
                let phase = self.asking.remove(key).unwrap_or_else(|| phase_of(line));
                let counts = self.counts(phase);
                match line["value"].is_null() {
                    true => counts.failed += 1,
                    false => counts.done += 1,
                }
                if line["earlier"] != true {
                    self.live.tokens += line["output_tokens"].as_u64().unwrap_or(0);
                }
            }
            // The script runs again from the top: what ran is asked again.
            Some("replay") => self.asking.clear(),
            Some("stop") => self.live.stopping = true,
            Some("end") => self.ended = true,
            _ => {}
        }
    }

    fn counts(&mut self, phase: String) -> &mut Counts {
        let at = (self.by_phase.iter())
            .find(|(_, (named, _))| *named == phase)
            .map(|(at, _)| *at)
            .unwrap_or(self.by_phase.len());
        &mut self
            .by_phase
            .entry(at)
            .or_insert((phase, Counts::default()))
            .1
    }

    /// The run as it stands.
    fn live(&mut self) -> Live {
        let asking: Vec<String> = self.asking.values().cloned().collect();
        let mut phases: Vec<(String, Counts)> = self.by_phase.values().cloned().collect();
        for phase in asking {
            match phases.iter_mut().find(|(named, _)| *named == phase) {
                Some((_, counts)) => counts.running += 1,
                None => phases.push((
                    phase,
                    Counts {
                        running: 1,
                        ..Default::default()
                    },
                )),
            }
        }
        let mut live = self.live.clone();
        live.running = phases.iter().map(|(_, c)| c.running).sum();
        live.done = phases.iter().map(|(_, c)| c.done).sum();
        live.failed = phases.iter().map(|(_, c)| c.failed).sum();
        live.phases = phases;
        live
    }
}

/// What this process read of each run, and the runs it saw end.
type Folds = (HashMap<PathBuf, Fold>, HashSet<PathBuf>);
static FOLDS: LazyLock<Mutex<Folds>> = LazyLock::new(Mutex::default);

/// The runs of `project` under `store` that have not ended, the newest
/// first.
pub fn live(store: &Path, project: &str) -> Vec<Live> {
    let Ok(entries) = std::fs::read_dir(super::runs(store)) else {
        return Vec::new();
    };
    let mut folds = FOLDS.lock().unwrap();
    let (folds, ended) = &mut *folds;
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if ended.contains(&dir) {
            continue;
        }
        if dir.join("result.json").exists() {
            folds.remove(&dir);
            ended.insert(dir);
            continue;
        }
        if !folds.contains_key(&dir) {
            let Ok(plan) = super::read_plan(&dir) else {
                continue;
            };
            let live = Live {
                id: plan.id,
                name: plan.meta["name"].as_str().unwrap_or("workflow").to_owned(),
                caller: plan.caller,
                project: plan.project,
                started_ms: plan.started_ms,
                ..Default::default()
            };
            folds.insert(
                dir.clone(),
                Fold {
                    live,
                    ..Default::default()
                },
            );
        }
        let fold = folds.get_mut(&dir).expect("inserted");
        if fold.live.project != project {
            continue;
        }
        read_on(&dir.join("events.jsonl"), fold);
        if fold.ended {
            continue;
        }
        if !fold.live.stopping {
            fold.live.stopping = dir.join(STOP).exists();
        }
        out.push(fold.live());
    }
    out.sort_by_key(|live| std::cmp::Reverse(live.started_ms));
    out
}

/// Fold what `journal` gained since `fold` last read it, to its last
/// whole line.
fn read_on(journal: &Path, fold: &mut Fold) {
    let Ok(mut file) = std::fs::File::open(journal) else {
        return;
    };
    if file.seek(SeekFrom::Start(fold.read)).is_err() {
        return;
    }
    let mut gained = Vec::new();
    if file.read_to_end(&mut gained).is_err() {
        return;
    }
    let Some(whole) = gained.iter().rposition(|byte| *byte == b'\n') else {
        return;
    };
    for line in gained[..=whole].split(|byte| *byte == b'\n') {
        if let Ok(line) = serde_json::from_slice::<Value>(line) {
            fold.take(&line);
        }
    }
    fold.read += whole as u64 + 1;
}

/// Ask the host running `run` under `store` to stop it.
pub fn stop(store: &Path, run: &str) -> Result<(), String> {
    let dir = super::runs(store).join(super::valid_id(run)?);
    if dir.join("result.json").exists() {
        return Err(format!("workflow run {run} has ended"));
    }
    std::fs::write(dir.join(STOP), b"").map_err(|e| e.to_string())
}

/// A run of `caller`'s named `name` under `store`, with `journal` its
/// lines so far, as a host leaves one: for tests of what windows show.
#[cfg(test)]
pub fn plant(store: &Path, id: &str, caller: &str, name: &str, journal: &[Value]) -> PathBuf {
    let dir = super::runs(store).join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let plan = super::Plan {
        id: id.into(),
        caller: caller.into(),
        caller_id: 1,
        turn: 1,
        project: crate::project::lead_project(caller).map_or_else(
            || caller.split('.').next().unwrap_or("").into(),
            str::to_owned,
        ),
        roles: PathBuf::new(),
        workspace: PathBuf::new(),
        model: String::new(),
        effort: None,
        mode: String::new(),
        state: None,
        args: Value::Null,
        budget: None,
        meta: serde_json::json!({"name": name, "description": "-"}),
        resumed_from: None,
        started_ms: 1,
        max_agents: 1000,
        at_once: 2,
    };
    std::fs::write(dir.join("run.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let lines: String = journal.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(dir.join("events.jsonl"), lines).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_run_s_row_counts_its_agents_by_phase_from_its_journal() {
        let mut fold = Fold::default();
        for line in [
            json!({"t": "phase", "title": "Find"}),
            json!({"t": "asking", "key": "a", "phase": "Find"}),
            json!({"t": "asking", "key": "b", "phase": "Find"}),
            json!({"t": "agent", "key": "a", "phase": "Find", "value": "x", "output_tokens": 7}),
            json!({"t": "agent", "key": "c", "phase": "Find", "value": "y", "earlier": true,
                "output_tokens": 9}),
            json!({"t": "phase", "title": "Verify"}),
            json!({"t": "asking", "key": "d", "phase": "Verify"}),
            json!({"t": "agent", "key": "d", "phase": "Verify", "value": null, "output_tokens": 3}),
            json!({"t": "asking", "key": "e", "phase": "Verify"}),
            json!({"t": "log", "text": "1 of 2 checked"}),
        ] {
            fold.take(&line);
        }
        let live = fold.live();
        assert_eq!((live.running, live.done, live.failed), (2, 2, 1));
        assert_eq!(live.state(), "Verify · 2 running, 2 done, 1 failed");
        assert_eq!(
            live.more(),
            "Find: 1 running, 2 done · Verify: 1 running, 1 failed · 1 of 2 checked · 10 output tokens"
        );
        // Run again from the top, what was running is asked again.
        fold.take(&json!({"t": "replay"}));
        assert_eq!(fold.live().running, 0);
        assert_eq!(Live::default().state(), "starting");
    }

    #[test]
    fn a_window_reads_only_what_the_journal_gained_to_a_line_s_end() {
        let dir = std::env::temp_dir().join(format!("sonny-wf-view-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let journal = dir.join("events.jsonl");
        std::fs::write(
            &journal,
            "{\"t\":\"phase\",\"title\":\"A\"}\n{\"t\":\"log\",\"te",
        )
        .unwrap();
        let mut fold = Fold::default();
        read_on(&journal, &mut fold);
        assert_eq!(
            (fold.live.phase.as_str(), fold.live.log.as_str()),
            ("A", "")
        );
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .unwrap();
        std::io::Write::write_all(&mut file, b"xt\":\"half\"}\n").unwrap();
        read_on(&journal, &mut fold);
        assert_eq!(fold.live.log, "half");
        assert_eq!(split("shop.lead#wf-1a2b"), ("shop.lead", Some("1a2b")));
        assert_eq!(split("shop.lead"), ("shop.lead", None));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
