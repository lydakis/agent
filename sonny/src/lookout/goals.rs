//! Goals: an agent keeps working until a command says its work is done.
//! A goal is what done looks like, in words, and a check: a command that
//! exits 0 once it is. Each time a turn of the agent that has one
//! completes, the lookout runs the check in the agent's folder. While it
//! fails, the agent gets its output and works again, and its lead hears
//! nothing of those turns; once it passes, or the agent gives up, or the
//! goal's budget is spent, or two turns in a row the goal started call no
//! tool, the goal ends and the lead hears that turn with how it ended.
//! No model judges: a command says when the work is done, as it does in
//! CI.
//!
//! Each agent's goal is one file, `STORE.lookout/goals/BOT.json`, which
//! its `goal` tool writes (a lead's for its tasks too) and the lookout
//! reads again when a call changed it, and removes when the goal ends. A
//! check runs as the person, outside every sandbox, as a trigger's `if`
//! does, without the provider keys an agent's shell goes without:
//! setting one waits for a verdict where the agent's other calls do.
use agent_client::Client;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a check may run unless the goal says, and at most.
pub const TIMEOUT_S: u64 = 10 * 60;
pub const TIMEOUT_MAX_S: u64 = 60 * 60;
/// How much of a failing check's output the agent is shown: its end.
const OUTPUT_LINES: usize = 60;
const OUTPUT_BYTES: usize = 8 * 1024;
/// Turns in a row the goal started that called no tool, after which it
/// stops: the agent only answers, and another turn would too.
const IDLE_TURNS: u32 = 2;
/// What a turn the goal started carries as its request ID, before the
/// agent and turn it follows: not the lookout's own note, since the
/// turn is the agent's work and its end is news.
pub(crate) const REQUEST: &str = "goal-";

#[derive(Clone, Debug, PartialEq)]
pub struct Goal {
    pub bot: String,
    pub bot_id: i64,
    pub done_when: String,
    pub check: String,
    /// The agent's folder, where the check runs.
    pub workspace: PathBuf,
    pub set_by: String,
    /// When it was set: a goal set again since is another goal.
    pub set_ms: u64,
    /// Tokens the agent may spend on it, counted from `tokens_at_set`.
    pub budget_tokens: Option<u64>,
    pub tokens_at_set: u64,
    pub timeout_s: u64,
    /// Why the agent gave up, told once its turn ends.
    pub blocked: Option<String>,
}

impl Goal {
    fn to_json(&self) -> Value {
        json!({"bot": self.bot, "bot_id": self.bot_id, "done_when": self.done_when, "check": self.check,
            "workspace": self.workspace, "set_by": self.set_by, "set_ms": self.set_ms,
            "budget_tokens": self.budget_tokens, "tokens_at_set": self.tokens_at_set,
            "timeout_s": self.timeout_s, "blocked": self.blocked})
    }

    fn from_json(value: &Value) -> Option<Goal> {
        let text = |key: &str| value[key].as_str().map(str::to_owned);
        Some(Goal {
            bot: text("bot")?,
            bot_id: value["bot_id"].as_i64()?,
            done_when: text("done_when")?,
            check: text("check")?,
            workspace: PathBuf::from(text("workspace")?),
            set_by: text("set_by")?,
            set_ms: value["set_ms"].as_u64()?,
            budget_tokens: value["budget_tokens"].as_u64(),
            tokens_at_set: value["tokens_at_set"].as_u64().unwrap_or(0),
            timeout_s: value["timeout_s"].as_u64().unwrap_or(TIMEOUT_S),
            blocked: text("blocked"),
        })
    }

    /// What the agent and its lead are shown of it.
    fn shown(&self) -> Value {
        json!({"bot": self.bot, "done_when": self.done_when, "check": self.check,
            "budget_tokens": self.budget_tokens, "timeout_s": self.timeout_s, "set_by": self.set_by,
            "blocked": self.blocked})
    }
}

pub fn dir(store: &Path) -> PathBuf {
    super::subs::dir(store).join("goals")
}

fn file(store: &Path, bot: &str) -> PathBuf {
    dir(store).join(format!("{bot}.json"))
}

pub fn read(store: &Path, bot: &str) -> Option<Goal> {
    let text = std::fs::read_to_string(file(store, bot)).ok()?;
    Goal::from_json(&serde_json::from_str(&text).ok()?)
}

pub fn all(store: &Path) -> HashMap<String, Goal> {
    let Ok(entries) = std::fs::read_dir(dir(store)) else {
        return HashMap::new();
    };
    entries
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().to_string_lossy().into_owned();
            let bot = name.strip_suffix(".json")?;
            Some((bot.to_owned(), read(store, bot)?))
        })
        .collect()
}

/// Written whole and moved in, so the lookout never reads half of one.
fn write(store: &Path, goal: &Goal) -> Result<(), String> {
    let dir = dir(store);
    super::make_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = file(store, &goal.bot);
    let partial = dir.join(format!(".{}.{}.partial", goal.bot, std::process::id()));
    super::write_moved(&partial, &path, &goal.to_json().to_string())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Where how a goal ended is noted, by the turn that ended it:
/// `STORE.lookout/goals/ended/BOT.json`. The roster and `/status` read it
/// to tell a task that did what was asked from one that only stopped, and
/// a lookout that restarts before the lead heard the turn reads how it
/// ended from it, since the goal itself is gone, as a Kubernetes Job keeps
/// its terminal condition after its pods go.
fn ended_file(store: &Path, bot: &str) -> PathBuf {
    dir(store).join("ended").join(format!("{bot}.json"))
}

/// How `goal` ended at `turn`, written whole and moved in before the goal
/// goes, then the goal ends.
fn ended(store: &Path, goal: &Goal, turn: i64, outcome: &str, met: bool) {
    let path = ended_file(store, &goal.bot);
    if let Some(dir) = path.parent() {
        let _ = super::make_dir(dir);
        let partial = dir.join(format!(".{}.{}.partial", goal.bot, std::process::id()));
        let text = json!({"turn": turn, "bot_id": goal.bot_id, "check": goal.check, "met": met,
            "outcome": outcome})
        .to_string();
        let _ = super::write_moved(&partial, &path, &text);
    }
    end(store, goal);
}

/// What the ended note says of `bot`'s `turn`, when that turn ended its goal.
fn ending(store: &Path, bot: &str, turn: i64) -> Option<Value> {
    let text = std::fs::read_to_string(ended_file(store, bot)).ok()?;
    let noted: Value = serde_json::from_str(&text).ok()?;
    (noted["turn"].as_i64() == Some(turn)).then_some(noted)
}

/// How a task whose turn `turn` completed stands with its goal: `checking`
/// while it has one, which that turn has not met yet; `checked` when that
/// turn met it; nothing when it has none.
pub fn standing(store: &Path, bot: &str, turn: Option<i64>) -> Option<&'static str> {
    if read(store, bot).is_some() {
        return Some(crate::started::CHECKING);
    }
    let noted = ending(store, bot, turn?)?;
    (noted["met"] == true).then_some(crate::started::CHECKED)
}

/// The goal ends: its file goes, unless it was set again since.
fn end(store: &Path, goal: &Goal) {
    if read(store, &goal.bot).is_some_and(|now| now.set_ms == goal.set_ms) {
        let _ = std::fs::remove_file(file(store, &goal.bot));
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The `goal` tool, as `sonny tool goal` runs it for `caller`: set one on
/// itself or on a task it made, give its own up, clear one, or list the
/// goals it and its tasks have.
pub async fn call(
    store: &Path,
    client: &Client,
    caller: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let target = |bot: &Value| bot.as_str().unwrap_or(caller).to_owned();
    // A goal goes on the caller or on an agent it made, whose record says
    // so by id, since a name can be another bot's since: the record also
    // gives its folder, id and spend.
    let inspect = async |bot: &str| -> Result<Value, String> {
        (client.request("inspect", json!({"bot": bot})).await)
            .map_err(|e| format!("bot_not_found: {bot}: {e}"))
    };
    let record = async |bot: &str| -> Result<Value, String> {
        let record = inspect(bot).await?;
        if bot != caller && record["created_by_id"] != inspect(caller).await?["bot_id"] {
            return Err(format!(
                "not_your_task: {bot} was not made by {caller}; a goal goes on you or on a task you made"
            ));
        }
        Ok(record)
    };
    if let Some(set) = arguments.get("set") {
        let bot = target(&set["bot"]);
        let record = record(&bot).await?;
        let text = |key: &str| set[key].as_str().map(str::trim).unwrap_or_default();
        let (done_when, check) = (text("done_when"), text("check"));
        if done_when.is_empty() || check.is_empty() {
            return Err("invalid_goal: done_when and check are both needed".into());
        }
        let timeout_s = set["timeout_s"].as_u64().unwrap_or(TIMEOUT_S);
        if !(1..=TIMEOUT_MAX_S).contains(&timeout_s) {
            return Err(format!("invalid_goal: timeout_s is 1 to {TIMEOUT_MAX_S}"));
        }
        let budget_tokens = set["budget_tokens"].as_u64().filter(|b| *b > 0);
        let workspace = record["workspace"].as_str().unwrap_or("/");
        let goal = Goal {
            bot: bot.clone(),
            bot_id: record["bot_id"].as_i64().unwrap_or_default(),
            done_when: done_when.to_owned(),
            check: check.to_owned(),
            workspace: PathBuf::from(workspace),
            set_by: caller.to_owned(),
            set_ms: now_ms(),
            budget_tokens,
            tokens_at_set: record["tokens_used"].as_u64().unwrap_or(0),
            timeout_s,
            blocked: None,
        };
        // The same goal again changes nothing, its budget still counted
        // from when it was first set.
        if let Some(had) = read(store, &bot).filter(|had| {
            (
                had.bot_id,
                &had.done_when,
                &had.check,
                had.budget_tokens,
                had.timeout_s,
            ) == (
                goal.bot_id,
                &goal.done_when,
                &goal.check,
                goal.budget_tokens,
                goal.timeout_s,
            ) && had.blocked.is_none()
        }) {
            return Ok(json!({"goal": had.shown(), "duplicate": true}));
        }
        write(store, &goal)?;
        return Ok(json!({"goal": goal.shown(), "duplicate": false}));
    }
    if let Some(why) = arguments.get("blocked") {
        let why = why.as_str().map(str::trim).unwrap_or_default();
        if why.is_empty() {
            return Err("invalid_goal: blocked says why".into());
        }
        let Some(mut goal) = read(store, caller) else {
            return Err("no_goal: you have no goal to give up".into());
        };
        goal.blocked = Some(why.to_owned());
        write(store, &goal)?;
        return Ok(json!({"goal": goal.shown(), "ends": "when this turn ends"}));
    }
    if let Some(bot) = arguments.get("clear") {
        let bot = target(bot);
        record(&bot).await?;
        let had = read(store, &bot);
        if let Some(goal) = &had {
            end(store, goal);
        }
        return Ok(json!({"bot": bot, "cleared": had.is_some()}));
    }
    // Its own, and its tasks', whoever set them.
    let me = inspect(caller).await?["bot_id"].clone();
    let mut mine = Vec::new();
    for goal in all(store).into_values() {
        if goal.bot == caller
            || inspect(&goal.bot)
                .await
                .is_ok_and(|task| task["created_by_id"] == me)
        {
            mine.push(goal.shown());
        }
    }
    Ok(json!({"goals": mine}))
}

/// The `goal` tool's declaration.
pub fn tool(name: &str, tag: &str) -> Value {
    json!({
        "name": name,
        "description": "A goal keeps an agent working until a command says its work is done. Set one on \
            yourself, or on a task you made, with what done looks like and a check: a shell command, run in \
            that agent's folder, that exits 0 once it is (tests passing, a build, a file count). Each time \
            its turn completes, the check runs; while it fails, the agent is shown its output and works \
            again, and its lead hears nothing of those turns. The goal ends once the check passes, the \
            budget is spent, two turns it started call no tool, or the agent gives up with blocked and why, \
            and its lead hears that turn with how it ended. Setting the same goal again changes nothing; \
            with nothing given, the result lists your goals and your tasks'. A person's message that \
            starts with /goal asks you to set one on yourself for what it says.",
        "parameters": {
            "type": "object",
            "properties": {
                "set": {
                    "type": "object",
                    "properties": {
                        "done_when": {"type": "string", "minLength": 1,
                            "description": "What done looks like, in words."},
                        "check": {"type": "string", "minLength": 1,
                            "description": "The command, as sh -c runs it, that exits 0 once it is."},
                        "bot": {"type": "string", "minLength": 1,
                            "description": "A task you made; you when left out."},
                        "budget_tokens": {"type": "integer", "minimum": 1,
                            "description": "Tokens the agent may spend on it before it stops."},
                        "timeout_s": {"type": "integer", "minimum": 1, "maximum": TIMEOUT_MAX_S,
                            "description": "How long the check may run; 600 when left out."}
                    },
                    "required": ["done_when", "check"],
                    "additionalProperties": false
                },
                "blocked": {"type": "string", "minLength": 1,
                    "description": "Give your own goal up, saying why no way remains."},
                "clear": {"type": "string", "minLength": 1,
                    "description": "Clear the goal of a task you made, or your own by your name."}
            },
            "additionalProperties": false
        },
        "answered_by": tag
    })
}

/// How a turn the lookout held for a goal ends: `None` when the agent
/// works again and the turn is no news; else what its lead hears after
/// the turn's status, empty when the goal went meanwhile.
#[derive(Debug, PartialEq)]
pub struct Settled {
    pub bot: String,
    pub turn: i64,
    pub outcome: Option<String>,
    /// The turn the agent works again in, when it does.
    pub continued: Option<i64>,
}

/// The lookout's goals, and what it follows of each agent's turns.
pub struct Goals {
    store: PathBuf,
    socket: PathBuf,
    goals: HashMap<String, Goal>,
    /// Agents whose running turn called a tool.
    called: HashSet<String>,
    /// Agents whose running turn the goal started.
    continued: HashSet<String>,
    /// Agents whose goal's check runs.
    checking: HashMap<String, Check>,
    /// The newest turn of each agent that completed while its check ran,
    /// held until that check settles, as a work queue keeps an item added
    /// again while it is processed and processes it once more after.
    later: HashMap<String, Later>,
    /// Turns in a row the goal started that called no tool.
    /// Keyed by the goal's `set_ms` and the last turn counted, so a turn
    /// replayed after a reconnect is not counted again.
    idle: HashMap<String, (u64, i64, u32)>,
    /// The folder each turn was sent to: a later turn can move its agent,
    /// and the check runs where the turn worked.
    worked: HashMap<(String, i64), PathBuf>,
}

/// A goal's check under way.
struct Check {
    pursuit: tokio::task::AbortHandle,
    /// The cursor of the turn's end, which a restart replays.
    at: Option<i64>,
    turn: i64,
    /// The goal checked: one set since is checked afresh.
    set_ms: u64,
    /// The idle count from before this turn and any that wait for it
    /// counted, which a replay counts again.
    idle: Option<(u64, i64, u32)>,
}

/// A turn that completed while its goal's check ran.
struct Later {
    turn: i64,
    at: Option<i64>,
    goal: Goal,
    idle: u32,
    before: Option<(u64, i64, u32)>,
}

impl Goals {
    pub fn new(store: &Path, socket: &Path) -> Goals {
        Goals {
            store: store.to_owned(),
            socket: socket.to_owned(),
            goals: all(store),
            called: HashSet::new(),
            continued: HashSet::new(),
            checking: HashMap::new(),
            later: HashMap::new(),
            idle: HashMap::new(),
            worked: HashMap::new(),
        }
    }

    /// The check of `bot`'s `turn` said how its goal stands. A turn that
    /// completed meanwhile is checked now, unless the agent works again
    /// after it, whose turn is checked when it ends: after a restart the
    /// turn the check went on in may be one that already ended, which no
    /// one would check again. Else it is plain news. Returns the turn whose
    /// check starts now.
    pub fn settled(
        &mut self,
        settled: &Settled,
        client: &std::sync::Arc<Client>,
        done: &tokio::sync::mpsc::UnboundedSender<Settled>,
    ) -> Option<i64> {
        if !(self.checking.get(&settled.bot)).is_some_and(|check| check.turn == settled.turn) {
            return None;
        }
        self.checking.remove(&settled.bot);
        let later = self.later.remove(&settled.bot)?;
        let goes_on = settled.outcome.is_none()
            && settled.continued.is_none_or(|turn| turn <= later.turn)
            && read(&self.store, &settled.bot).is_some_and(|now| now.set_ms == later.goal.set_ms);
        if goes_on {
            let turn = later.turn;
            self.check(
                later.goal,
                turn,
                later.idle,
                later.at,
                later.before,
                client,
                done,
            );
            return Some(turn);
        }
        let _ = done.send(Settled {
            bot: settled.bot.clone(),
            turn: later.turn,
            outcome: Some(String::new()),
            continued: None,
        });
        None
    }

    /// Checks under way answer a connection since dropped, never the next:
    /// they stop, and the replay runs them again, counting idle turns again
    /// from where they stood before the first of them.
    pub fn reconnected(&mut self) {
        for (bot, check) in self.checking.drain() {
            check.pursuit.abort();
            match check.idle {
                Some(idle) => self.idle.insert(bot, idle),
                None => self.idle.remove(&bot),
            };
        }
        self.later.clear();
    }

    /// The end of the oldest turn whose check is under way or waits for
    /// one: what the lookout keeps for the next one is from before it, so
    /// a restart, or one with no subscriber to hold the turn, runs its
    /// check again.
    pub fn floor(&self) -> Option<i64> {
        (self.checking.values().map(|check| check.at))
            .chain(self.later.values().map(|later| later.at))
            .flatten()
            .min()
    }

    pub fn reread(&mut self) {
        self.goals = all(&self.store);
        self.idle.retain(|bot, _| self.goals.contains_key(bot));
    }

    /// An event the lookout read: the turn it holds for a goal, whose news
    /// waits for `done` to settle it, when its end is one.
    /// `bot_id` is the id of the agent the event names, as the lookout
    /// knows it now.
    pub fn event(
        &mut self,
        event: &Value,
        bot_id: Option<i64>,
        live: bool,
        client: &std::sync::Arc<Client>,
        done: &tokio::sync::mpsc::UnboundedSender<Settled>,
    ) -> Option<(String, i64)> {
        let bot = event["bot"].as_str()?;
        let data = &event["data"];
        if let (Some("accepted" | "queued"), Some(turn), Some(workspace)) = (
            event["event"].as_str(),
            event["turn"].as_i64(),
            data["workspace"].as_str(),
        ) {
            self.worked
                .insert((bot.to_owned(), turn), PathBuf::from(workspace));
        }
        match event["event"].as_str()? {
            "accepted" => {
                self.called.remove(bot);
                match data["request_id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with(REQUEST))
                {
                    true => self.continued.insert(bot.to_owned()),
                    false => self.continued.remove(bot),
                };
                None
            }
            "tool_started" => {
                self.called.insert(bot.to_owned());
                None
            }
            // A new agent under the name has no goal of the last one's.
            "deleted" => {
                self.worked.retain(|(name, _), _| name != bot);
                if let Some(check) = self.checking.remove(bot) {
                    check.pursuit.abort();
                }
                self.later.remove(bot);
                if let Some(goal) = self.goals.remove(bot) {
                    end(&self.store, &goal);
                }
                let _ = std::fs::remove_file(ended_file(&self.store, bot));
                None
            }
            "turn_finished" => {
                let turn = event["turn"].as_i64()?;
                let worked = self.worked.remove(&(bot.to_owned(), turn));
                if !live {
                    return None;
                }
                let status = data["status"].as_str().unwrap_or_default();
                // Its goal as it is now, which its lead may have set or
                // cleared in the meantime.
                match read(&self.store, bot) {
                    Some(goal) => self.goals.insert(bot.to_owned(), goal),
                    None => self.goals.remove(bot),
                };
                let Some(mut goal) = self.goals.get(bot).cloned() else {
                    return self.told_before(bot, turn, bot_id, done);
                };
                if let Some(worked) = worked {
                    goal.workspace = worked;
                }
                // Left by an agent deleted while the lookout was away, whose
                // name another has since.
                if bot_id.is_some_and(|id| id != goal.bot_id) {
                    end(&self.store, &goal);
                    self.goals.remove(bot);
                    return None;
                }
                // A set again goal's check answers for a goal that is
                // gone: its turn is plain news, and this one is checked for
                // the new goal.
                if let Some(check) = self.checking.get(bot)
                    && check.set_ms != goal.set_ms
                {
                    let check = self.checking.remove(bot)?;
                    check.pursuit.abort();
                    let _ = done.send(Settled {
                        bot: bot.to_owned(),
                        turn: check.turn,
                        outcome: Some(String::new()),
                        continued: None,
                    });
                    if let Some(later) = self.later.remove(bot) {
                        let _ = done.send(Settled {
                            bot: bot.to_owned(),
                            turn: later.turn,
                            outcome: Some(String::new()),
                            continued: None,
                        });
                    }
                }
                // An interrupted or failed turn pauses it: the next turn
                // that completes is checked. A steer's is part of another.
                if status != "completed" && goal.blocked.is_none() {
                    return None;
                }
                let idle = match self.continued.contains(bot) && !self.called.contains(bot) {
                    // Counted for this goal only: one set since starts at 0.
                    true => match self.idle.get(bot) {
                        Some((set_ms, at, idle)) if *set_ms == goal.set_ms && *at == turn => *idle,
                        Some((set_ms, _, idle)) if *set_ms == goal.set_ms => idle + 1,
                        _ => 1,
                    },
                    false => 0,
                };
                let before = self.idle.insert(bot.to_owned(), (goal.set_ms, turn, idle));
                let at = event["cursor"].as_i64();
                // One under way: this turn waits for it, and one that
                // waited before it is plain news.
                if self.checking.contains_key(bot) {
                    let later = Later {
                        turn,
                        at,
                        goal,
                        idle,
                        before,
                    };
                    if let Some(older) = self.later.insert(bot.to_owned(), later) {
                        let _ = done.send(Settled {
                            bot: bot.to_owned(),
                            turn: older.turn,
                            outcome: Some(String::new()),
                            continued: None,
                        });
                    }
                    return Some((bot.to_owned(), turn));
                }
                self.check(goal, turn, idle, at, before, client, done);
                Some((bot.to_owned(), turn))
            }
            _ => None,
        }
    }

    /// Run `goal`'s check for `turn`, whose end is at `at`. `before` is
    /// the idle count from before `turn` counted.
    #[allow(clippy::too_many_arguments)]
    fn check(
        &mut self,
        goal: Goal,
        turn: i64,
        idle: u32,
        at: Option<i64>,
        before: Option<(u64, i64, u32)>,
        client: &std::sync::Arc<Client>,
        done: &tokio::sync::mpsc::UnboundedSender<Settled>,
    ) {
        let (client, done, store, socket) = (
            client.clone(),
            done.clone(),
            self.store.clone(),
            self.socket.clone(),
        );
        let set_ms = goal.set_ms;
        let bot = goal.bot.clone();
        let pursuit = tokio::spawn(async move {
            let (outcome, continued) = pursue(&client, &store, &socket, &goal, turn, idle).await;
            let bot = goal.bot;
            let settled = Settled {
                bot,
                turn,
                outcome,
                continued,
            };
            let _ = done.send(settled);
        });
        let check = Check {
            pursuit: pursuit.abort_handle(),
            at,
            turn,
            set_ms,
            idle: before,
        };
        self.checking.insert(bot, check);
    }

    /// A turn of an agent with no goal now: when it is the one that ended
    /// its goal, which a restart reads again before the lead heard how, it
    /// is told so again.
    fn told_before(
        &mut self,
        bot: &str,
        turn: i64,
        bot_id: Option<i64>,
        done: &tokio::sync::mpsc::UnboundedSender<Settled>,
    ) -> Option<(String, i64)> {
        let noted = ending(&self.store, bot, turn)?;
        if bot_id.is_some_and(|id| noted["bot_id"].as_i64() != Some(id)) {
            return None;
        }
        let outcome = noted["outcome"].as_str()?.to_owned();
        let _ = done.send(Settled {
            bot: bot.to_owned(),
            turn,
            outcome: Some(outcome),
            continued: None,
        });
        Some((bot.to_owned(), turn))
    }
}

/// After a turn of an agent with a goal: how the goal ended, else `None`
/// with the turn it works again in.
async fn pursue(
    client: &Client,
    store: &Path,
    socket: &Path,
    goal: &Goal,
    turn: i64,
    idle: u32,
) -> (Option<String>, Option<i64>) {
    // What the lead hears is the lookout's own words only: the reason an
    // agent gives and the check it may have set are its text, which the
    // lead reads in the turn itself.
    if goal.blocked.is_some() {
        ended(store, goal, turn, GAVE_UP, false);
        return (Some(GAVE_UP.to_owned()), None);
    }
    let check = format!("`{}`", goal.check);
    let (code, output) = run(goal, store, socket).await;
    // Cleared or set again while the check ran: this turn is plain news;
    // given up meanwhile, it ends so.
    match read(store, &goal.bot) {
        Some(now) if now.set_ms == goal.set_ms => {
            if now.blocked.is_some() {
                ended(store, &now, turn, GAVE_UP, false);
                return (Some(GAVE_UP.to_owned()), None);
            }
        }
        _ => return (Some(String::new()), None),
    }
    let over = |outcome: String, met: bool| {
        ended(store, goal, turn, &outcome, met);
        (Some(outcome), None)
    };
    if code == Some(0) {
        return over(", and met its goal: its check exits 0".to_owned(), true);
    }
    let exits = match code {
        Some(code) => format!("exits {code}"),
        None => "did not finish".to_owned(),
    };
    if let Some(budget) = goal.budget_tokens {
        let record = client.request("inspect", json!({"bot": goal.bot})).await;
        let used = (record.ok())
            .and_then(|r| r["tokens_used"].as_u64())
            .unwrap_or(0);
        if used.saturating_sub(goal.tokens_at_set) >= budget {
            return over(
                format!(
                    ", and stopped at its goal's budget of {budget} tokens: its check still {exits}"
                ),
                false,
            );
        }
    }
    if idle >= IDLE_TURNS {
        return over(
            format!(
                ", and stopped pursuing its goal: {IDLE_TURNS} turns in a row it started called no tool, \
                 and its check still {exits}"
            ),
            false,
        );
    }
    let prompt = format!(
        "Your goal is not met yet: {check} {exits}.\n```\n{output}\n```\nKeep working until {}. When no way \
         remains, give the goal up with the goal tool's blocked, saying why.",
        goal.done_when
    );
    let asked = json!({"bot": goal.bot, "bot_id": goal.bot_id, "prompt": prompt, "delivery": "queue",
        "origin": "goal", "request_id": format!("{REQUEST}{}-{turn}", goal.bot)});
    // The turn it goes on in: after a restart, the one this turn's own
    // request ID made before, which may have ended already. One made
    // before from other output than the check printed now is that turn
    // too, which the daemon names.
    match client.request("submit", asked).await {
        Ok(made) => (None, made["turn"].as_i64()),
        Err(error) if error.code == "idempotency_conflict" => {
            (None, error.facts.as_ref().and_then(|f| f["turn"].as_i64()))
        }
        Err(error) => (
            Some(format!(
                ", and could not go on with its goal: {}",
                error.code
            )),
            None,
        ),
    }
}

const GAVE_UP: &str = ", and gave up its goal, saying why in that turn";

/// The check, in its own process group in the agent's folder, for up to
/// its timeout: its exit code, `None` when it did not finish, and the end
/// of what it printed.
async fn run(goal: &Goal, store: &Path, socket: &Path) -> (Option<i32>, String) {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(&goal.check)
        .current_dir(&goal.workspace)
        .env("AGENT_STORE", store)
        .env("AGENT_SOCKET", socket)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    // The provider keys are the daemon's: no agent's own command has them,
    // nor does its check.
    for key in crate::task::kept_keys() {
        command.env_remove(key);
    }
    // A place among the processes Sonny runs for agents, held while the
    // check runs; waiting for one counts against its timeout.
    let started = std::time::Instant::now();
    let Ok(_place) = tokio::time::timeout(
        Duration::from_secs(goal.timeout_s),
        crate::budget::processes().admit(&goal.bot),
    )
    .await
    else {
        return (
            None,
            "the check waited its whole timeout for one of Sonny's processes (sonny.tool_processes)"
                .into(),
        );
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return (None, format!("the check did not start: {error}")),
    };
    // Whatever the check started goes with it, however it ends: done, past
    // its timeout, or its pursuit stopped.
    let _group = child.id().map(|id| Group::new(id as i32));
    // Read as it prints, keeping only the end, so a check that prints
    // without end holds a few KB, not all of it.
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let ran = async {
        let (stdout, stderr) = tokio::join!(last(stdout), last(stderr));
        (child.wait().await, stdout, stderr)
    };
    let (status, stdout, stderr) = match tokio::time::timeout(
        Duration::from_secs(goal.timeout_s).saturating_sub(started.elapsed()),
        ran,
    )
    .await
    {
        Ok((Ok(status), stdout, stderr)) => (status, stdout, stderr),
        Ok((Err(error), ..)) => return (None, format!("the check failed to run: {error}")),
        Err(_) => {
            return (
                None,
                format!("the check ran past {} s and was stopped", goal.timeout_s),
            );
        }
    };
    let mut text = String::from_utf8_lossy(&stdout).into_owned();
    let errors = String::from_utf8_lossy(&stderr);
    if !errors.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&errors);
    }
    (status.code(), tail(&text))
}

/// The process groups of the checks running now.
static GROUPS: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());

/// Stop every check under way: the lookout is giving way, and the one
/// that takes over runs them again.
pub fn stop() {
    for group in GROUPS.lock().map(|g| g.clone()).unwrap_or_default() {
        // SAFETY: a signal to a process group a check leads.
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
}

/// A check's process group, killed when dropped.
struct Group(i32);

impl Group {
    fn new(id: i32) -> Group {
        if let Ok(mut groups) = GROUPS.lock() {
            groups.push(id);
        }
        Group(id)
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        // SAFETY: a signal to the process group the check leads.
        unsafe { libc::killpg(self.0, libc::SIGKILL) };
        if let Ok(mut groups) = GROUPS.lock() {
            groups.retain(|g| *g != self.0);
        }
    }
}

/// The last `OUTPUT_BYTES` a pipe gave, read until it closes.
async fn last(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let (mut kept, mut chunk) = (Vec::new(), [0u8; 8192]);
    while let Ok(read) = pipe.read(&mut chunk).await
        && read > 0
    {
        kept.extend_from_slice(&chunk[..read]);
        if kept.len() > 2 * OUTPUT_BYTES {
            kept.drain(..kept.len() - OUTPUT_BYTES);
        }
    }
    let from = kept.len().saturating_sub(OUTPUT_BYTES);
    kept.split_off(from)
}

/// The end of a check's output: its last lines, within a few KB.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let mut kept = lines[lines.len().saturating_sub(OUTPUT_LINES)..].join("\n");
    if kept.len() > OUTPUT_BYTES {
        let mut from = kept.len() - OUTPUT_BYTES;
        while !kept.is_char_boundary(from) {
            from += 1;
        }
        kept = kept[from..].to_owned();
    }
    // Fences in the output would end the block the agent reads it in.
    kept.replace("```", "'''")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sonny-goals-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn goal(store: &Path, bot: &str) -> Goal {
        Goal {
            bot: bot.into(),
            bot_id: 7,
            done_when: "tests pass".into(),
            check: "true".into(),
            workspace: store.parent().unwrap().to_owned(),
            set_by: "p.lead".into(),
            set_ms: 1,
            budget_tokens: Some(10),
            tokens_at_set: 3,
            timeout_s: 5,
            blocked: None,
        }
    }

    #[test]
    fn a_task_stands_checking_while_its_goal_is_set_and_checked_by_the_turn_that_met_it() {
        let dir = scratch("standing");
        let store = dir.join("state.sqlite");
        let set = goal(&store, "p.fix");
        assert_eq!(standing(&store, "p.fix", Some(4)), None);
        write(&store, &set).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&super::dir(&store)), 0o700);
            assert_eq!(mode(&file(&store, "p.fix")), 0o600);
        }
        assert_eq!(standing(&store, "p.fix", Some(4)), Some("checking"));
        ended(
            &store,
            &set,
            4,
            ", and met its goal: its check exits 0",
            true,
        );
        assert_eq!(standing(&store, "p.fix", Some(4)), Some("checked"));
        // How it ended is kept for that turn, for a lookout that restarts
        // before the lead heard.
        assert_eq!(
            ending(&store, "p.fix", 4).unwrap()["outcome"],
            ", and met its goal: its check exits 0"
        );
        assert_eq!(ending(&store, "p.fix", 5), None);
        // A later turn is plain news, and so is a goal set again.
        assert_eq!(standing(&store, "p.fix", Some(5)), None);
        write(&store, &set).unwrap();
        assert_eq!(standing(&store, "p.fix", Some(4)), Some("checking"));
        // The goals list never takes the note for a goal.
        assert_eq!(all(&store).len(), 1);
        // One that ended any other way was never met.
        end(&store, &set);
        ended(&store, &set, 6, GAVE_UP, false);
        assert_eq!(standing(&store, "p.fix", Some(6)), None);
        assert_eq!(ending(&store, "p.fix", 6).unwrap()["outcome"], GAVE_UP);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_replay_after_a_drop_counts_idle_turns_from_before_the_check_it_runs_again() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = scratch("replay");
        let store = dir.join("state.sqlite");
        let mut set = goal(&store, "p.fix");
        set.check = "sleep 30".into();
        write(&store, &set).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let socket = dir.join("agent.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL});
                write.write_all(format!("{ready}\n").as_bytes()).await.unwrap();
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let asked: Value = serde_json::from_str(&line).unwrap();
                    let answer = json!({"id": asked["id"], "result": {}});
                    write.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
                }
            });
            let client = Client::connect(&socket).await.unwrap().0;
            let (done, _settled) = tokio::sync::mpsc::unbounded_channel();
            let mut goals = Goals::new(&store, &socket);
            let turn = |goals: &mut Goals, turn: i64, tool: bool| {
                let ev = |event: &str, data: Value| {
                    json!({"bot": "p.fix", "turn": turn, "event": event, "data": data, "cursor": turn})
                };
                let asked = json!({"request_id": format!("{REQUEST}p.fix-{turn}")});
                goals.event(&ev("accepted", asked), None, true, &client, &done);
                if tool {
                    goals.event(&ev("tool_started", json!({})), None, true, &client, &done);
                }
                let finished = ev("turn_finished", json!({"status": "completed"}));
                goals.event(&finished, None, true, &client, &done)
            };
            // Goal turns with no call, a call, and no call, while the first
            // one's check runs.
            assert_eq!(turn(&mut goals, 1, false), Some(("p.fix".into(), 1)));
            turn(&mut goals, 2, true);
            turn(&mut goals, 3, false);
            assert_eq!(goals.idle["p.fix"], (1, 3, 1));
            // The connection drops, and the replay starts from turn 1: it
            // is one idle turn, not one more after turn 3.
            goals.reconnected();
            turn(&mut goals, 1, false);
            assert_eq!(goals.idle["p.fix"], (1, 1, 1));
            goals.reconnected();
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_goal_reads_back_as_written_and_ends_only_as_set() {
        let dir = scratch("read");
        let store = dir.join("state.sqlite");
        let first = goal(&store, "p.fix");
        write(&store, &first).unwrap();
        assert_eq!(read(&store, "p.fix"), Some(first.clone()));
        assert_eq!(all(&store).len(), 1);
        // Set again since: the old one's end leaves the new one.
        let again = Goal {
            set_ms: 2,
            ..first.clone()
        };
        write(&store, &again).unwrap();
        end(&store, &first);
        assert_eq!(read(&store, "p.fix"), Some(again.clone()));
        end(&store, &again);
        assert_eq!(read(&store, "p.fix"), None);
    }

    #[test]
    fn a_check_says_its_code_and_the_end_of_what_it_printed() {
        let dir = scratch("run");
        let store = dir.join("state.sqlite");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut failing = goal(&store, "p.fix");
        failing.check = "seq 1 100; echo '```' >&2; exit 3".into();
        let (code, output) = runtime.block_on(run(&failing, &store, &store));
        assert_eq!(code, Some(3));
        assert!(output.starts_with("42\n"), "{output}");
        assert!(output.ends_with("100\n'''"), "{output}");
        // One that prints without end keeps only the end of it.
        let mut loud = goal(&store, "p.fix");
        loud.check = "head -c 50000000 /dev/zero | tr '\\0' x; echo; echo last".into();
        // What is kept is under test, not how fast 50 MB goes through `tr`:
        // a slow runner takes more than the 5 s the others get.
        loud.timeout_s = TIMEOUT_S;
        let (code, output) = runtime.block_on(run(&loud, &store, &store));
        assert_eq!(code, Some(0));
        assert!(
            output.len() <= OUTPUT_BYTES && output.ends_with("\nlast"),
            "{}",
            output.len()
        );
        let mut slow = goal(&store, "p.fix");
        slow.check = "sleep 30".into();
        slow.timeout_s = 1;
        let (code, output) = runtime.block_on(run(&slow, &store, &store));
        assert_eq!(code, None);
        assert!(output.contains("ran past 1 s"), "{output}");
        let mut here = goal(&store, "p.fix");
        here.check = format!("test \"$AGENT_STORE\" = '{}' && pwd", store.display());
        let (code, output) = runtime.block_on(run(&here, &store, &store));
        assert_eq!(code, Some(0));
        assert_eq!(
            std::fs::canonicalize(output.trim()).unwrap(),
            std::fs::canonicalize(&dir).unwrap()
        );
    }

    #[test]
    fn the_tail_keeps_the_last_lines_within_its_bytes() {
        let long = "x".repeat(OUTPUT_BYTES * 2);
        assert_eq!(tail(&long).len(), OUTPUT_BYTES);
        let wide = "é".repeat(OUTPUT_BYTES);
        assert!(tail(&wide).len() <= OUTPUT_BYTES);
        assert_eq!(tail("a\nb\n\n"), "a\nb");
    }
}
