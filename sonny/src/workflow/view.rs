//! What `/status` shows of the workflows that run: one row a run, read
//! from its journal, as the end message is. A window reads only what the
//! journal gained since its last look, and never reads a run that ended
//! again; it stops a run by leaving a `stop` file in the run's folder,
//! which the tool host running it acts on.
use ratatui::text::Span;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use unicode_width::UnicodeWidthStr;

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

    /// Its row's state: `Verify · 12 working, 31 finished, 2 failed`.
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

/// Past this many workers a strip is a bar of [`BAR`] cells.
pub const STRIP_MOST: usize = 24;

/// The cells of a bar.
pub const BAR: usize = 12;

/// One cell of a strip: a worker that came back, failed, or works.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cell {
    Back,
    Failed,
    Working,
}

impl Cell {
    /// Its mark in its hue, from the one state table: `✓` muted, `✗` red,
    /// `●` live, so each says its state with no colour.
    fn sign(self) -> Span<'static> {
        crate::status::sign(match self {
            Cell::Back => crate::started::DONE,
            Cell::Failed => "failed",
            Cell::Working => "working",
        })
    }
}

/// A run's workers as cells, a phase at a time in the order the phases
/// came, each phase's back, then failed, then working, so it fills from
/// the left; `None` parts two phases.
pub fn cells(phases: &[(String, Counts)]) -> Vec<Option<Cell>> {
    let mut out = Vec::new();
    for (_, counts) in phases.iter().filter(|(_, c)| total(*c) > 0) {
        if !out.is_empty() {
            out.push(None);
        }
        for (n, cell) in [
            (counts.done, Cell::Back),
            (counts.failed, Cell::Failed),
            (counts.running, Cell::Working),
        ] {
            out.extend(std::iter::repeat_n(Some(cell), n));
        }
    }
    out
}

fn total(counts: Counts) -> usize {
    counts.done + counts.failed + counts.running
}

/// `counts` as a bar of [`BAR`] cells in proportion, back, failed, then
/// working, where a state that has any keeps a cell: one failure in three
/// hundred still shows. Shares by the largest remainder.
pub fn bar(counts: Counts) -> Vec<Cell> {
    let states = [
        (counts.done, Cell::Back),
        (counts.failed, Cell::Failed),
        (counts.running, Cell::Working),
    ];
    let all = total(counts);
    if all == 0 {
        return Vec::new();
    }
    // Each state's whole cells, at least one when it has any, and what
    // its share had left over, in BARths of a worker.
    let mut shares: Vec<(usize, usize, Cell)> = (states.iter())
        .map(|&(n, cell)| {
            let whole = n * BAR / all;
            // A state lifted to its one cell has had more than its share.
            match (n, whole) {
                (0, _) => (0, 0, cell),
                (_, 0) => (1, 0, cell),
                _ => (whole, (n * BAR) % all, cell),
            }
        })
        .collect();
    let mut given: usize = shares.iter().map(|(n, ..)| n).sum();
    while given < BAR {
        // The largest remainder of those that have any takes the next.
        let at = (0..shares.len())
            .filter(|&at| states[at].0 > 0)
            .max_by_key(|&at| (shares[at].1, std::cmp::Reverse(at)))
            .expect("some state has workers");
        shares[at].0 += 1;
        shares[at].1 = 0;
        given += 1;
    }
    while given > BAR {
        // Taken from the largest share, never below one.
        let at = (0..shares.len())
            .filter(|&at| shares[at].0 > 1)
            .min_by_key(|&at| (shares[at].1, std::cmp::Reverse(shares[at].0)))
            .expect("a share above one");
        shares[at].0 -= 1;
        given -= 1;
    }
    (shares.into_iter())
        .flat_map(|(n, _, cell)| std::iter::repeat_n(cell, n))
        .collect()
}

/// A run's strip in at most `room` columns: each worker a mark, phases a
/// space apart; past [`STRIP_MOST`] workers, or wider than both `room`
/// and the bar, a bar of [`BAR`] cells and the counts after it, the counts left out where
/// they do not fit. `starting` before any worker.
pub fn strip(phases: &[(String, Counts)], room: usize) -> Vec<Span<'static>> {
    let all = phases.iter().fold(Counts::default(), |all, (_, c)| Counts {
        running: all.running + c.running,
        done: all.done + c.done,
        failed: all.failed + c.failed,
    });
    if total(all) == 0 {
        return vec![Span::raw(counted(all))];
    }
    // Cells a worker only up to STRIP_MOST, so a big run's row costs what
    // the bar does. A strip no wider than the bar stays a strip, room or not.
    if total(all) <= STRIP_MOST {
        let cells = cells(phases);
        if cells.len() <= room || cells.len() <= BAR {
            return (cells.into_iter())
                .map(|cell| cell.map_or_else(|| Span::raw(" "), Cell::sign))
                .collect();
        }
    }
    let mut out: Vec<Span<'static>> = bar(all).into_iter().map(Cell::sign).collect();
    let counts = format!("  {}", counted(all));
    if BAR + counts.width() <= room {
        out.push(Span::raw(counts));
    }
    out
}

/// `2 working, 3 finished, 1 failed`, leaving out what is none;
/// `starting` before any agent.
fn counted(counts: Counts) -> String {
    let said: Vec<String> = [
        (counts.running, "working"),
        (counts.done, "finished"),
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
            // A call that threw ends as failed, though a replay asks again.
            Some("thrown") => {
                let key = line["key"].as_str().unwrap_or("");
                let phase = self.asking.remove(key).unwrap_or_else(|| phase_of(line));
                self.counts(phase).failed += 1;
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
        held: Value::Null,
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
            json!({"t": "asking", "key": "f", "phase": "Verify"}),
            json!({"t": "thrown", "key": "f", "phase": "Verify", "error": "no such profile"}),
        ] {
            fold.take(&line);
        }
        let live = fold.live();
        assert_eq!((live.running, live.done, live.failed), (2, 2, 2));
        assert_eq!(live.state(), "Verify · 2 working, 2 finished, 2 failed");
        assert_eq!(
            live.more(),
            "Find: 1 working, 2 finished · Verify: 1 working, 2 failed · 1 of 2 checked · 10 output tokens"
        );
        // Run again from the top, what was running is asked again.
        fold.take(&json!({"t": "replay"}));
        assert_eq!(fold.live().running, 0);
        assert_eq!(Live::default().state(), "starting");
        // A stop its caller or the lead asked for shows as the window's does.
        fold.take(&json!({"t": "stop"}));
        assert!(fold.live().state().ends_with(" · stopping"));
    }

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn counts(done: usize, failed: usize, running: usize) -> Counts {
        Counts {
            running,
            done,
            failed,
        }
    }

    #[test]
    fn a_strip_is_each_worker_s_mark_a_phase_at_a_time() {
        let phases = vec![
            ("Find".to_owned(), counts(4, 0, 0)),
            ("Score".to_owned(), counts(1, 1, 2)),
        ];
        assert_eq!(text(&strip(&phases, 80)), "✓✓✓✓ ✓✗●●");
        // A phase no worker has reached yet takes no room.
        let phases = vec![
            ("Find".to_owned(), counts(0, 0, 3)),
            ("Score".to_owned(), Counts::default()),
        ];
        assert_eq!(text(&strip(&phases, 80)), "●●●");
        assert_eq!(text(&strip(&[], 80)), "starting");
        // Each mark keeps its state's hue, from the one table.
        let marks = strip(&[(String::new(), counts(1, 1, 1))], 80);
        assert_eq!(marks[0].style, crate::status::sign("done").style);
        assert_eq!(marks[1].style, crate::status::sign("failed").style);
        assert_eq!(marks[2].style, crate::status::sign("working").style);
    }

    #[test]
    fn past_24_workers_a_strip_is_a_bar_of_12_and_its_counts() {
        let phases = vec![("Verify".to_owned(), counts(31, 2, 12))];
        assert_eq!(
            text(&strip(&phases, 80)),
            "✓✓✓✓✓✓✓✓✗●●●  12 working, 31 finished, 2 failed"
        );
        // 24 is still a strip; 25 is a bar.
        assert_eq!(strip(&[(String::new(), counts(24, 0, 0))], 80).len(), 24);
        assert_eq!(
            text(&strip(&[(String::new(), counts(25, 0, 0))], 80)),
            "✓✓✓✓✓✓✓✓✓✓✓✓  25 finished"
        );
        // A state lifted to its one cell takes no more on its remainder:
        // 2 of 25 failed is one cell, not two.
        let lifted: String = (bar(counts(11, 2, 12)).into_iter())
            .map(|cell| cell.sign().content.into_owned())
            .collect();
        assert_eq!(lifted, "✓✓✓✓✓✗●●●●●●");
        // One failure in three hundred keeps its cell.
        let one = bar(counts(299, 1, 0));
        assert_eq!(one.len(), BAR);
        assert_eq!(one.iter().filter(|c| **c == Cell::Failed).count(), 1);
        for (done, failed, running) in [(1, 1, 1), (0, 0, 1000), (5, 300, 2), (13, 13, 13)] {
            let bar = bar(counts(done, failed, running));
            assert_eq!(bar.len(), BAR, "{done} {failed} {running}");
            for (n, cell) in [
                (done, Cell::Back),
                (failed, Cell::Failed),
                (running, Cell::Working),
            ] {
                assert_eq!(bar.contains(&cell), n > 0, "{done} {failed} {running}");
            }
        }
    }

    #[test]
    fn a_strip_too_wide_for_its_row_is_a_bar_and_then_drops_its_counts() {
        let phases = vec![("Find".to_owned(), counts(20, 0, 0))];
        assert_eq!(text(&strip(&phases, 20)).chars().count(), 20);
        assert_eq!(text(&strip(&phases, 19)), "✓✓✓✓✓✓✓✓✓✓✓✓");
        let phases = vec![("Find".to_owned(), counts(40, 0, 0))];
        assert_eq!(text(&strip(&phases, 25)), "✓✓✓✓✓✓✓✓✓✓✓✓  40 finished");
        assert_eq!(text(&strip(&phases, 24)), "✓✓✓✓✓✓✓✓✓✓✓✓");
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
