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
/// task_done_check`; none when it is off.
pub fn after() -> Option<Duration> {
    let hours = agent_client::config::load().map_or(8, |c| c.sonny.task_done_check);
    (hours > 0).then(|| Duration::from_secs(hours * 3600))
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

/// Each task's newest turn as a sweep last read it, by name: its
/// history's head then, the turn, and when it ended (ms).
type Read = Mutex<HashMap<String, (i64, i64, u64)>>;

/// How often a window that found the check off looks again whether it
/// was turned on.
const OFF_LOOK: Duration = Duration::from_secs(60);

/// A lead's window's look for tasks idle past the threshold: one sweep at
/// a time, on its own task, so the window never waits on it.
#[derive(Default)]
pub struct Sweep {
    /// When it last looked.
    looked: Option<Instant>,
    /// Whether a sweep is under way.
    running: Arc<AtomicBool>,
    /// Each task's newest turn as last read, so a task is read once per
    /// turn.
    read: Arc<Read>,
}

impl Sweep {
    /// Look at the tasks matching `glob` in `store`, if it is time to.
    pub fn look(&mut self, client: &Arc<Client>, store: &Path, glob: &str) {
        if self.looked.is_some_and(|at| at.elapsed() < OFF_LOOK) {
            return;
        }
        let Some(after) = after() else {
            self.looked = Some(Instant::now());
            return;
        };
        if self.looked.is_some_and(|at| at.elapsed() < every(after))
            || self.running.swap(true, Ordering::AcqRel)
        {
            return;
        }
        self.looked = Some(Instant::now());
        let (client, store, glob) = (client.clone(), store.to_owned(), glob.to_owned());
        let (running, read) = (self.running.clone(), self.read.clone());
        tokio::spawn(async move {
            sweep(&client, &store, &glob, after, &read).await;
            running.store(false, Ordering::Release);
        });
    }
}

/// Ask each task matching `glob` that sits idle past `after` whether it
/// is done, one at a time.
async fn sweep(client: &Client, store: &Path, glob: &str, after: Duration, read: &Read) {
    let mut listed = Vec::new();
    let mut from: Option<String> = None;
    loop {
        let mut asked = json!({"name": glob, "limit": 256});
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
    let now = crate::home::now_ms() as u64;
    let mut names = Vec::new();
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
        {
            continue;
        }
        names.push(name.to_owned());
        let known = read.lock().ok().and_then(|read| read.get(name).copied());
        let (turn, ended) = match known {
            Some((at, turn, ended)) if at == head => (turn, ended),
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
                let ended = match last["status"] == "completed" {
                    true => ended,
                    false => u64::MAX,
                };
                if let Ok(mut read) = read.lock() {
                    read.insert(name.to_owned(), (head, turn, ended));
                }
                (turn, ended)
            }
        };
        if ended == u64::MAX || !due(store, (name, bot_id), turn, ended, now, after) {
            continue;
        }
        // Resolved by its own call, or asking the person, it is no idle
        // work.
        match crate::needs::scan(client, name, Some(turn)).await {
            Some(seen) if !seen.itself && seen.ask.is_none() => {}
            _ => continue,
        }
        let _ = ask(client, store, (name, bot_id), turn).await;
    }
    if let Ok(mut read) = read.lock() {
        read.retain(|name, _| names.contains(name));
    }
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
    /// too short a while, nor, on the next sweep, the one it asked.
    #[tokio::test]
    async fn a_sweep_asks_each_task_idle_long_enough_once() {
        let asked = Arc::new(Mutex::new(Vec::<Value>::new()));
        let seen = asked.clone();
        let now = crate::home::now_ms() as u64;
        let client = crate::tests::daemon_answering(move |request| {
            seen.lock().unwrap().push(request.clone());
            let ended = |hours: u64| now - hours * 3_600_000;
            match request["op"].as_str() {
                Some("bots") => json!({"bots": [
                    {"name": "calc.lead", "bot_id": 1, "head": 1, "running_turn": null, "expire_ms": null},
                    {"name": "calc.old", "bot_id": 2, "head": 2, "running_turn": null, "expire_ms": null},
                    {"name": "calc.new", "bot_id": 3, "head": 3, "running_turn": null, "expire_ms": null},
                    {"name": "calc.busy", "bot_id": 4, "head": 4, "running_turn": 9, "expire_ms": null}]}),
                Some("turns") => {
                    let hours = match request["bot"].as_str() {
                        Some("calc.old") => 9,
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
        let read = Mutex::new(HashMap::new());
        let after = Duration::from_secs(8 * 3600);
        sweep(&client, &store, "calc.*", after, &read).await;
        let forks: Vec<Value> = (asked.lock().unwrap().iter())
            .filter(|r| r["op"] == "fork")
            .cloned()
            .collect();
        assert_eq!(forks.len(), 1);
        assert_eq!(forks[0]["source"], "calc.old");
        assert_eq!(forks[0]["allow"], json!([]));
        assert!(read_answer(&store, "calc.old"));
        assert_eq!(crate::resolved::read(&store, "calc.old"), Some(5));
        // Asked once: the next sweep reads no turn again and asks no one.
        asked.lock().unwrap().clear();
        sweep(&client, &store, "calc.*", after, &read).await;
        let again = asked.lock().unwrap().clone();
        assert!(
            !again
                .iter()
                .any(|r| r["op"] == "fork" || r["op"] == "turns")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn read_answer(store: &Path, bot: &str) -> bool {
        read(store, bot, 2).is_some_and(|answer| answer.done && answer.turn == 5)
    }
}
