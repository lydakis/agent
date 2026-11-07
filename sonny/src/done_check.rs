//! Tasks that sit idle long enough are asked whether they are done, as
//! threads are resolved by whoever sees the work finished. Once a task's
//! last turn ended `[sonny] task_done_check` hours ago (8 by default, 0
//! never), with nothing marking it done or waiting on the person, its
//! lead's window asks a copy of it, with no tools, whether the work it was
//! given is done ([`ask`]). A yes marks it done as `mark_done` does, the
//! mark naming the check as its maker; either answer is kept in
//! `STORE.done-check/BOT.json` with the bot's id, the turn it covers and
//! the copy's reason ([`Answer`]), so `/status` says the check marked it, when and
//! why, or that it found work left; and the task is not asked again until
//! another of its turns ends. Leads are never asked: marking a lead done
//! stays the person's. New work reopens a task as it reopens any.
//!
//! It is the window's own work, as only a window shows it ([`Sweep`]): a
//! lead's window looks every fiftieth of the threshold, between a minute
//! and ten, reading each task's newest turn once per turn; with no window
//! of the project open, nothing looks. Two windows ask one copy, as the
//! needs-you check does ([`crate::waiting`]).
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_client::Client;
use serde_json::{Value, json};

/// The call a mark the check made names, beside the lead's call ids.
pub const CALL: &str = "done-check";
/// The most of the copy's reason an answer keeps, in characters.
const WHY_CHARS: usize = 200;
/// What the copy is asked.
const ASK: &str = "[A CHECK FROM SONNY, answered in JSON alone; nothing here reaches the person.] \
    You have been idle a while. Is the work you were given done: finished, handed on, or dropped \
    by the person, with nothing left for you to do in it? Set done to true or false, and say why in \
    `why`, in one short plain line, such as \"its pull request merged\" or \"the tests still fail\".";

/// What the copy answered, for the turn it read up to.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// The task's newest turn when it was asked: a later one lets it go.
    pub turn: i64,
    /// Whether the work is done.
    pub done: bool,
    /// Why, in a line.
    pub why: String,
    /// When it answered, in seconds since the epoch.
    pub at: i64,
}

impl Answer {
    /// The answer as a status row says it, `now` in seconds.
    pub fn said(&self, now: i64) -> String {
        let when = crate::asks::clock(self.at, now);
        match self.done {
            true => format!("marked done by the check at {when}: {}", self.why),
            false => format!("checked at {when}, work left: {}", self.why),
        }
    }
}

fn dir(store: &Path) -> PathBuf {
    let mut dir = store.as_os_str().to_owned();
    dir.push(".done-check");
    PathBuf::from(dir)
}

fn file(store: &Path, bot: &str) -> PathBuf {
    dir(store).join(format!("{bot}.json"))
}

/// The answer kept for `bot`, while `bot_id` is the bot it was kept for.
pub fn read(store: &Path, bot: &str, bot_id: i64) -> Option<Answer> {
    let kept: Value = serde_json::from_slice(&std::fs::read(file(store, bot)).ok()?).ok()?;
    (kept["bot_id"].as_i64() == Some(bot_id)).then_some(())?;
    Some(Answer {
        turn: kept["turn"].as_i64()?,
        done: kept["done"].as_bool()?,
        why: kept["why"].as_str()?.to_owned(),
        at: kept["at"].as_i64()?,
    })
}

/// Keep `answer` for `bot`, as `bot_id`, and with a yes mark it done up
/// to its turn: unless an answer for a later turn is kept already, as by
/// another window's check that ended first.
pub fn keep(store: &Path, bot: &str, bot_id: i64, answer: &Answer) -> Result<(), String> {
    crate::waiting::locked(&dir(store), || {
        if read(store, bot, bot_id).is_some_and(|kept| kept.turn > answer.turn) {
            return Ok(());
        }
        let kept = json!({"bot_id": bot_id, "turn": answer.turn, "done": answer.done,
            "why": answer.why, "at": answer.at});
        crate::waiting::replace(&file(store, bot), kept.to_string().as_bytes())?;
        match answer.done {
            true => crate::resolved::keep(store, bot, answer.turn, CALL),
            false => Ok(()),
        }
    })
}

/// Whether `answer` still covers its task: its turn is the newest,
/// `newest`, and none runs.
pub fn holds(answer: &Answer, newest: Option<i64>, running: bool) -> bool {
    !running && newest == Some(answer.turn)
}

/// How long a task sits idle before it is asked, from `[sonny]
/// task_done_check`, none when it is off; and whether `[sonny]
/// needs_check` is on, for the turns no window heard end.
fn settings() -> (Option<Duration>, bool) {
    let config = agent_client::config::load().ok();
    let hours = config.as_ref().map_or(8, |c| c.sonny.task_done_check);
    let needs = config.as_ref().is_none_or(|c| c.sonny.needs_check);
    (
        (hours > 0).then(|| Duration::from_secs(hours * 3600)),
        needs,
    )
}

/// How often a lead's window looks for tasks idle past `after`: a
/// fiftieth of it, between a minute and ten.
pub fn every(after: Duration) -> Duration {
    (after / 50).clamp(Duration::from_secs(60), Duration::from_secs(600))
}

/// Whether `bot`'s turn `turn`, ended at `ended` (ms), is to be asked at
/// `now` (ms): it ended `after` ago or more, and nothing kept says it is
/// done, waits on the person, or was asked already.
pub fn due(
    store: &Path,
    (bot, bot_id): (&str, i64),
    turn: i64,
    ended: u64,
    now: u64,
    after: Duration,
) -> bool {
    now.saturating_sub(ended) >= after.as_millis() as u64
        && read(store, bot, bot_id).is_none_or(|answer| answer.turn < turn)
        && !crate::resolved::holds(crate::resolved::read(store, bot), Some(turn), false)
        && crate::waiting::read(store, bot).is_none_or(|mark| mark.turn != turn)
}

/// The JSON Schema the copy's reply follows.
fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "done": {"type": "boolean"},
            "why": {"type": "string"}
        },
        "required": ["done", "why"],
        "additionalProperties": false
    })
}

/// What the copy's reply says, for `turn` at `at`: none when it is not
/// the JSON asked for.
pub fn answer(reply: &str, turn: i64, at: i64) -> Option<Answer> {
    let said: Value = serde_json::from_str(reply.trim()).ok()?;
    let why: String = (said["why"].as_str()?.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(WHY_CHARS)
        .collect();
    Some(Answer {
        turn,
        done: said["done"].as_bool()?,
        why: match why.is_empty() {
            true => "it said no more".to_owned(),
            false => why,
        },
        at,
    })
}

/// Ask a copy of `bot`, as `bot_id`, idle since its turn `turn`, whether
/// its work is done, and keep what it answers.
pub async fn ask(
    client: &Client,
    store: &Path,
    (bot, bot_id): (&str, i64),
    turn: i64,
) -> Result<Answer, String> {
    // Named apart from the needs-you check's copy of the same turn.
    let stamp = 0x80_0000 | (turn as u128 % 0x80_0000);
    let reply = crate::waiting::ask_copy(client, bot, turn, stamp, ASK, schema()).await?;
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let answer = answer(&reply, turn, at)
        .ok_or_else(|| format!("the check's reply was not the JSON asked for: {reply}"))?;
    keep(store, bot, bot_id, &answer)?;
    Ok(answer)
}

/// Each task as a sweep last read it, by name.
type Read = Mutex<HashMap<String, Known>>;

/// A task's newest turn as a sweep last read it, and how far the sweep
/// got with it: while its history's head stays, a sweep reads nothing of
/// it again, on disk or from the daemon, but the clock.
#[derive(Clone, Copy)]
struct Known {
    /// Its history's head then.
    head: i64,
    /// The turn that ran last.
    turn: i64,
    /// When it ended (ms), or never, for one that did not complete.
    ended: u64,
    /// Whether what that turn left the person is kept, or needs none.
    needs: bool,
    /// Whether it was asked whether it is done, or needs not be.
    asked: bool,
}

/// How often a window that found the check off looks again whether it
/// was turned on.
const OFF_LOOK: Duration = Duration::from_secs(60);

/// A lead's window's look for tasks idle past the threshold: one sweep at
/// a time, on its own task, so the window never waits on it.
#[derive(Default)]
pub struct Sweep {
    /// When it looks next: the config is read only then, never per tick.
    next: Option<Instant>,
    /// Whether a sweep is under way.
    running: Arc<AtomicBool>,
    /// Each task's newest turn as last read, so a task is read once per
    /// turn.
    read: Arc<Read>,
    /// Tasks the check found done since the window last took them.
    done: Arc<Mutex<Vec<String>>>,
}

impl Sweep {
    /// Look at the tasks matching `glob` in `store`, if it is time to.
    pub fn look(&mut self, client: &Arc<Client>, store: &Path, scope: crate::background::Scope) {
        let now = Instant::now();
        if self.next.is_some_and(|at| now < at) {
            return;
        }
        let (after, needs) = settings();
        if (after.is_none() && !needs) || self.running.swap(true, Ordering::AcqRel) {
            self.next = Some(now + OFF_LOOK);
            return;
        }
        // With the done check off, missed turns are looked for as often
        // as at its default.
        self.next = Some(now + every(after.unwrap_or(Duration::from_secs(8 * 3600))));
        let (client, store) = (client.clone(), store.to_owned());
        let (running, read, done) = (self.running.clone(), self.read.clone(), self.done.clone());
        tokio::spawn(async move {
            sweep(&client, &store, &scope, (after, needs), (&read, &done)).await;
            running.store(false, Ordering::Release);
        });
    }

    /// The tasks the check found done since last asked, for the window to
    /// resolve as a `mark_done` would.
    pub fn done(&self) -> Vec<String> {
        self.done
            .lock()
            .map(|mut done| std::mem::take(&mut *done))
            .unwrap_or_default()
    }
}

/// Ask each of the lead's tasks in `scope` that sits idle past `after`
/// whether it is done, one at a time; and, for any idle task's last turn
/// no window heard end, what it left waiting on the person, first.
async fn sweep(
    client: &Client,
    store: &Path,
    scope: &crate::background::Scope,
    (after, needs): (Option<Duration>, bool),
    (read, done): (&Read, &Mutex<Vec<String>>),
) {
    let mut listed = Vec::new();
    let mut from: Option<String> = None;
    loop {
        let mut asked = json!({"name": scope.glob, "limit": 256});
        if let Some(from) = &from {
            asked["after"] = json!(from);
        }
        let Ok(page) = client.request("bots", asked).await else {
            return;
        };
        listed.extend(page["bots"].as_array().cloned().unwrap_or_default());
        match page["next_after"].as_str() {
            Some(next) if from.as_deref() != Some(next) => from = Some(next.to_owned()),
            _ => break,
        }
    }
    // With other leads in the folder, their tasks are theirs to ask, as
    // are a project's inside it (`shop.web` in `shop`).
    let mine = crate::owned(&listed, &scope.project, &scope.lead);
    let listed_names = (listed.iter()).filter_map(|bot| bot["name"].as_str());
    let inner: Vec<String> = (crate::inner_projects(listed_names, &scope.project).into_iter())
        .map(|inner| format!("{inner}."))
        .collect();
    let now = crate::home::now_ms() as u64;
    let mut names = std::collections::HashSet::new();
    for bot in &listed {
        let (Some(name), Some(head), Some(bot_id)) = (
            bot["name"].as_str(),
            bot["head"].as_i64(),
            bot["bot_id"].as_i64(),
        ) else {
            continue;
        };
        // Side chats and checks' copies expire; a task does not.
        if crate::project::is_lead(name)
            || !bot["expire_ms"].is_null()
            || !bot["running_turn"].is_null()
            || mine.as_ref().is_some_and(|mine| !mine.contains(name))
            || inner.iter().any(|inner| name.starts_with(inner))
        {
            continue;
        }
        names.insert(name.to_owned());
        let known = read.lock().ok().and_then(|read| read.get(name).copied());
        let mut known = match known {
            Some(known) if known.head == head => known,
            _ => {
                // The newest turn that ran: a steer it took in never did.
                let Ok(last) = crate::last_ran(client, name, None).await else {
                    continue;
                };
                let (Some(turn), Some(ended)) =
                    (last["turn"].as_i64(), last["finished_ms"].as_u64())
                else {
                    continue;
                };
                // One that did not complete is not idle work to judge.
                let completed = last["status"] == "completed";
                Known {
                    head,
                    turn,
                    ended: if completed { ended } else { u64::MAX },
                    needs: !completed,
                    asked: !completed,
                }
            }
        };
        let turn = known.turn;
        // A turn that ended while no window was open to hear it was never
        // checked for what it left the person: it is first, unless the
        // task is resolved past it.
        if needs && !known.needs {
            let resolved =
                || crate::resolved::holds(crate::resolved::read(store, name), Some(turn), false);
            if !crate::waiting::answered(store, name, turn) && !resolved() {
                crate::waiting::checked(client, store, name, turn).await;
            }
            known.needs = crate::waiting::answered(store, name, turn) || resolved();
        }
        // Not yet idle long enough reads nothing on disk.
        let idle =
            after.filter(|after| now.saturating_sub(known.ended) >= after.as_millis() as u64);
        if let Some(after) = idle.filter(|_| !known.asked) {
            let when = (known.ended, now, after);
            known.asked = asked(client, store, (name, bot_id), turn, when, done).await;
        }
        if let Ok(mut read) = read.lock() {
            read.insert(name.to_owned(), known);
        }
    }
    if let Ok(mut read) = read.lock() {
        read.retain(|name, _| names.contains(name));
    }
}

/// Ask `bot` whether its turn `turn`, ended at `ended` (ms), is done, once
/// idle past `after` at `now` (ms), unless nothing need be: resolved, waiting on the person, asking
/// or answered already. The window hears of a yes, while no turn ran
/// since. Whether nothing is left to ask of that turn.
async fn asked(
    client: &Client,
    store: &Path,
    (bot, bot_id): (&str, i64),
    turn: i64,
    (ended, now, after): (u64, u64, Duration),
    done: &Mutex<Vec<String>>,
) -> bool {
    if !due(store, (bot, bot_id), turn, ended, now, after) {
        return true;
    }
    // Resolved by its own call, or asking the person, it is no idle work.
    match crate::needs::scan(client, bot, Some(turn)).await {
        Some(seen) if !seen.itself && seen.ask.is_none() => {}
        Some(_) => return true,
        None => return false,
    }
    // Its window closes only while the answer still covers it: no turn
    // ran since the one asked about.
    let Ok(answer) = ask(client, store, (bot, bot_id), turn).await else {
        return false;
    };
    if answer.done
        && crate::last_ran(client, bot, None)
            .await
            .is_ok_and(|last| last["turn"].as_i64() == Some(turn) && last["status"] == "completed")
        && let Ok(mut done) = done.lock()
    {
        done.push(bot.to_owned());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_copys_reply_says_whether_the_work_is_done_and_why() {
        let got = answer(
            r#"{"done": true, "why": "its  pull request\nmerged"}"#,
            7,
            100,
        );
        assert_eq!(
            got,
            Some(Answer {
                turn: 7,
                done: true,
                why: "its pull request merged".into(),
                at: 100
            })
        );
        assert_eq!(
            answer(r#"{"done": false, "why": " "}"#, 7, 1).unwrap().why,
            "it said no more"
        );
        assert_eq!(answer("Yes, done.", 7, 1), None);
    }

    #[test]
    fn an_answer_kept_late_never_undoes_a_newer_one_nor_reaches_a_new_bot() {
        let dir = std::env::temp_dir().join(format!("sonny-done-late-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = dir.join("state.sqlite");
        let answer = |turn: i64, done: bool| Answer {
            turn,
            done,
            why: "w".into(),
            at: 1,
        };
        keep(&store, "calc.fix", 1, &answer(7, false)).unwrap();
        // Another window's check of an older turn ends after.
        keep(&store, "calc.fix", 1, &answer(5, true)).unwrap();
        assert_eq!(read(&store, "calc.fix", 1), Some(answer(7, false)));
        // A bot made again under the name has no answer of its own.
        assert_eq!(read(&store, "calc.fix", 2), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_yes_marks_the_task_done_and_a_later_turn_asks_again() {
        let dir = std::env::temp_dir().join(format!("sonny-done-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = dir.join("state.sqlite");
        let hour = Duration::from_secs(3600);
        let (ended, now) = (0, 2 * 3600 * 1000);
        assert!(
            !due(&store, ("calc.fix", 1), 4, ended, 1000, hour),
            "not idle long enough"
        );
        assert!(due(&store, ("calc.fix", 1), 4, ended, now, hour));
        let no = Answer {
            turn: 4,
            done: false,
            why: "tests fail".into(),
            at: 1,
        };
        keep(&store, "calc.fix", 1, &no).unwrap();
        assert!(
            !due(&store, ("calc.fix", 1), 4, ended, now, hour),
            "asked once a turn"
        );
        assert!(crate::resolved::read(&store, "calc.fix").is_none());
        assert!(
            due(&store, ("calc.fix", 1), 5, ended, now, hour),
            "a later turn asks again"
        );
        let yes = Answer {
            turn: 5,
            done: true,
            why: "merged".into(),
            at: 2,
        };
        keep(&store, "calc.fix", 1, &yes).unwrap();
        assert_eq!(crate::resolved::read(&store, "calc.fix"), Some(5));
        assert!(crate::resolved::made_by(&store, "calc.fix", CALL));
        assert_eq!(read(&store, "calc.fix", 1), Some(yes));
        // One waiting on the person is not asked.
        let mark = crate::waiting::Mark {
            turn: 9,
            waiting_for: "a branch".into(),
        };
        crate::waiting::keep(&store, "calc.web", 9, Some(&mark)).unwrap();
        assert!(!due(&store, ("calc.web", 1), 9, ended, now, hour));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_window_looks_a_fiftieth_as_often_within_a_minute_and_ten() {
        assert_eq!(
            every(Duration::from_secs(8 * 3600)),
            Duration::from_secs(576)
        );
        assert_eq!(every(Duration::from_secs(3600)), Duration::from_secs(72));
        assert_eq!(every(Duration::from_secs(60)), Duration::from_secs(60));
        assert_eq!(
            every(Duration::from_secs(1000 * 3600)),
            Duration::from_secs(600)
        );
    }

    /// A window's sweep asks the idle task past the threshold, through a
    /// tool-less copy, and only it: not the lead, a running task, one idle
    /// too short a while, another lead's task, a project's inside it, nor,
    /// on the next sweep, the one it asked.
    #[tokio::test]
    async fn a_sweep_asks_each_task_idle_long_enough_once() {
        let asked = Arc::new(Mutex::new(Vec::<Value>::new()));
        let seen = asked.clone();
        let now = crate::home::now_ms() as u64;
        let client = crate::tests::daemon_answering(move |request| {
            seen.lock().unwrap().push(request.clone());
            let ended = |hours: u64| now - hours * 3_600_000;
            match request["op"].as_str() {
                // One bot's record, as a check reads it.
                Some("bots") if request["limit"] == 1 => {
                    let name = request["name"].as_str().unwrap_or("");
                    let id = name.len() as i64;
                    json!({"bots": [{"name": name, "bot_id": id, "expire_ms": null}]})
                }
                Some("bots") => json!({"bots": [
                    {"name": "calc.lead", "bot_id": 1, "head": 1, "running_turn": null, "expire_ms": null},
                    {"name": "calc.old", "bot_id": 2, "head": 2, "running_turn": null, "expire_ms": null},
                    {"name": "calc.new", "bot_id": 3, "head": 3, "running_turn": null, "expire_ms": null},
                    {"name": "calc.busy", "bot_id": 4, "head": 4, "running_turn": 9, "expire_ms": null},
                    {"name": "calc.lead.ada", "bot_id": 5, "head": 5, "running_turn": null, "expire_ms": null},
                    {"name": "calc.theirs", "bot_id": 6, "head": 6, "running_turn": null, "expire_ms": null,
                        "created_by": "calc.lead.ada"},
                    {"name": "calc.web.lead", "bot_id": 7, "head": 7, "running_turn": null, "expire_ms": null},
                    {"name": "calc.web.fix", "bot_id": 8, "head": 8, "running_turn": null, "expire_ms": null}]}),
                Some("turns") => {
                    let hours = match request["bot"].as_str() {
                        Some("calc.old" | "calc.theirs" | "calc.web.fix") => 9,
                        _ => 1,
                    };
                    json!({"turns": [{"turn": 5, "status": "completed",
                        "finished_ms": ended(hours)}]})
                }
                Some("history_nodes") => json!({"nodes": [{"node": 20, "turn": 5}]}),
                Some("fork") => json!({"bot_id": 9}),
                Some("submit") => json!({"handle": "turn:copy/1", "turn": 1}),
                Some("wait") => json!({"results": {"turn:copy/1": {"status": "completed",
                    "text": r#"{"done": true, "why": "its pull request merged"}"#}}}),
                _ => json!({}),
            }
        })
        .await;
        let dir = std::env::temp_dir().join(format!("sonny-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = dir.join("state.sqlite");
        let (read, done) = (Mutex::new(HashMap::new()), Mutex::new(Vec::new()));
        let after = (Some(Duration::from_secs(8 * 3600)), true);
        let scope = scope();
        sweep(&client, &store, &scope, after, (&read, &done)).await;
        let forks: Vec<Value> = (asked.lock().unwrap().iter())
            .filter(|r| r["op"] == "fork")
            .cloned()
            .collect();
        assert_eq!(forks.len(), 1);
        assert_eq!(forks[0]["source"], "calc.old");
        assert_eq!(forks[0]["allow"], json!([]));
        assert!(read_answer(&store, "calc.old"));
        assert_eq!(crate::resolved::read(&store, "calc.old"), Some(5));
        // Each idle task's last turn, which no window heard end, was
        // checked for what it left the person, the one too new to ask
        // whether it is done among them.
        assert!(!crate::waiting::due(&store, "calc.new", 5));
        assert!(!crate::waiting::due(&store, "calc.old", 5));
        // The window hears of it, to close its window as a mark_done would.
        assert_eq!(std::mem::take(&mut *done.lock().unwrap()), ["calc.old"]);
        // Asked once: the next sweep reads no turn again and asks no one.
        asked.lock().unwrap().clear();
        sweep(&client, &store, &scope, after, (&read, &done)).await;
        let again = asked.lock().unwrap().clone();
        // Nothing moved: the listing is all it asks the daemon, and it
        // reads nothing on disk, its marks gone or not.
        assert!(
            again.iter().all(|r| r["op"] == "bots" && r["limit"] == 256),
            "{again:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        sweep(&client, &store, &scope, after, (&read, &done)).await;
        assert!(!dir.exists(), "read or wrote beside the store");
        assert!(done.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scope() -> crate::background::Scope {
        crate::background::Scope {
            project: "calc".into(),
            lead: "calc.lead".into(),
            glob: "calc.*".into(),
        }
    }

    /// A turn that ran while the copy answered keeps the task's window:
    /// the yes covers only the turn asked about.
    #[tokio::test]
    async fn a_turn_run_while_the_check_answered_keeps_the_window() {
        let forked = Arc::new(AtomicBool::new(false));
        let seen = forked.clone();
        let now = crate::home::now_ms() as u64;
        let client = crate::tests::daemon_answering(move |request| {
            match request["op"].as_str() {
                Some("bots") => json!({"bots": [
                    {"name": "calc.late", "bot_id": 2, "head": 2, "running_turn": null, "expire_ms": null}]}),
                // Turn 6 ran once the copy was made.
                Some("turns") if seen.load(Ordering::Acquire) => json!({"turns": [
                    {"turn": 6, "status": "completed", "finished_ms": now}]}),
                Some("turns") => json!({"turns": [{"turn": 5, "status": "completed",
                    "finished_ms": now - 9 * 3_600_000}]}),
                Some("history_nodes") => json!({"nodes": [{"node": 20, "turn": 5}]}),
                Some("fork") => {
                    seen.store(true, Ordering::Release);
                    json!({"bot_id": 9})
                }
                Some("submit") => json!({"handle": "turn:copy/1", "turn": 1}),
                Some("wait") => json!({"results": {"turn:copy/1": {"status": "completed",
                    "text": r#"{"done": true, "why": "its pull request merged"}"#}}}),
                _ => json!({}),
            }
        })
        .await;
        let dir = std::env::temp_dir().join(format!("sonny-sweep-ran-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = dir.join("state.sqlite");
        let (read, done) = (Mutex::new(HashMap::new()), Mutex::new(Vec::new()));
        let after = (Some(Duration::from_secs(8 * 3600)), true);
        sweep(&client, &store, &scope(), after, (&read, &done)).await;
        assert!(forked.load(Ordering::Acquire));
        assert!(done.lock().unwrap().is_empty());
        // Its mark covers turn 5 alone, so turn 6 leaves it open.
        assert_eq!(crate::resolved::read(&store, "calc.late"), Some(5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What a sweep costs a window opened on 1000 idle tasks, all checked
    /// before: `cargo test -p sonny --release sweep_cost -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn sweep_cost() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        let now = crate::home::now_ms() as u64;
        let bots: Vec<Value> = (0..1000)
            .map(|i| {
                json!({"name": format!("calc.t{i}"), "bot_id": i + 10, "head": 1,
                "running_turn": null, "expire_ms": null})
            })
            .collect();
        let client = crate::tests::daemon_answering(move |request| {
            counted.fetch_add(1, Ordering::Relaxed);
            match request["op"].as_str() {
                Some("bots") => json!({"bots": bots.clone()}),
                Some("turns") => json!({"turns": [{"turn": 5, "status": "completed",
                    "finished_ms": now - 9 * 3_600_000}]}),
                _ => json!({}),
            }
        })
        .await;
        let dir = std::env::temp_dir().join(format!("sonny-sweep-cost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = dir.join("state.sqlite");
        for i in 0..1000 {
            let name = format!("calc.t{i}");
            crate::waiting::keep(&store, &name, 5, None).unwrap();
            let no = Answer {
                turn: 5,
                done: false,
                why: "work left".into(),
                at: 0,
            };
            keep(&store, &name, i + 10, &no).unwrap();
        }
        let (read, done) = (Mutex::new(HashMap::new()), Mutex::new(Vec::new()));
        let after = (Some(Duration::from_secs(8 * 3600)), true);
        let scope = scope();
        println!("seeded");
        for pass in ["open", "steady"] {
            calls.store(0, Ordering::Relaxed);
            let began = Instant::now();
            sweep(&client, &store, &scope, after, (&read, &done)).await;
            let took = began.elapsed();
            let calls = calls.load(Ordering::Relaxed);
            println!("{pass}: {took:?}, {calls} daemon calls");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn read_answer(store: &Path, bot: &str) -> bool {
        read(store, bot, 2).is_some_and(|answer| answer.done && answer.turn == 5)
    }
}
