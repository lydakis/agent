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
//! does, without the provider keys in ~/.agent/env: setting one waits for
//! a verdict where the agent's other calls do.
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
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = file(store, &goal.bot);
    let partial = dir.join(format!(".{}.{}.partial", goal.bot, std::process::id()));
    std::fs::write(&partial, goal.to_json().to_string())
        .and_then(|()| std::fs::rename(&partial, &path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&partial);
            format!("{}: {e}", path.display())
        })
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
            with nothing given, the result lists your goals and your tasks'.",
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
    /// Agents whose goal's check runs: a turn that ends meanwhile is
    /// plain news, and the check's own turn is checked in its place.
    checking: HashSet<String>,
    /// Turns in a row the goal started that called no tool.
    idle: HashMap<String, u32>,
}

impl Goals {
    pub fn new(store: &Path, socket: &Path) -> Goals {
        Goals {
            store: store.to_owned(),
            socket: socket.to_owned(),
            goals: all(store),
            called: HashSet::new(),
            continued: HashSet::new(),
            checking: HashSet::new(),
            idle: HashMap::new(),
        }
    }

    /// The check of `bot` said how its goal stands.
    pub fn settled(&mut self, bot: &str) {
        self.checking.remove(bot);
    }

    /// Checks under way answer a connection since dropped, never the next.
    pub fn reconnected(&mut self) {
        self.checking.clear();
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
        done: &tokio::sync::mpsc::Sender<Settled>,
    ) -> Option<(String, i64)> {
        let bot = event["bot"].as_str()?;
        let data = &event["data"];
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
                if let Some(goal) = self.goals.remove(bot) {
                    end(&self.store, &goal);
                }
                None
            }
            "turn_finished" if live => {
                let turn = event["turn"].as_i64()?;
                let status = data["status"].as_str().unwrap_or_default();
                // Its goal as it is now, which its lead may have set or
                // cleared in the meantime.
                match read(&self.store, bot) {
                    Some(goal) => self.goals.insert(bot.to_owned(), goal),
                    None => self.goals.remove(bot),
                };
                let goal = self.goals.get(bot)?.clone();
                // Left by an agent deleted while the lookout was away, whose
                // name another has since.
                if bot_id.is_some_and(|id| id != goal.bot_id) {
                    end(&self.store, &goal);
                    self.goals.remove(bot);
                    return None;
                }
                if self.checking.contains(bot) {
                    return None;
                }
                // An interrupted or failed turn pauses it: the next turn
                // that completes is checked. A steer's is part of another.
                if status != "completed" && goal.blocked.is_none() {
                    return None;
                }
                let idle = match self.continued.contains(bot) && !self.called.contains(bot) {
                    true => self.idle.get(bot).copied().unwrap_or(0) + 1,
                    false => 0,
                };
                self.idle.insert(bot.to_owned(), idle);
                let (client, done, store, socket) = (
                    client.clone(),
                    done.clone(),
                    self.store.clone(),
                    self.socket.clone(),
                );
                let held = (bot.to_owned(), turn);
                let bot = bot.to_owned();
                self.checking.insert(bot.clone());
                tokio::spawn(async move {
                    let outcome = pursue(&client, &store, &socket, &goal, turn, idle).await;
                    let _ = done.send(Settled { bot, turn, outcome }).await;
                });
                Some(held)
            }
            _ => None,
        }
    }
}

/// After a turn of an agent with a goal: whether it works again, or how
/// the goal ended.
async fn pursue(
    client: &Client,
    store: &Path,
    socket: &Path,
    goal: &Goal,
    turn: i64,
    idle: u32,
) -> Option<String> {
    if let Some(why) = &goal.blocked {
        end(store, goal);
        return Some(format!(", and gave up its goal: {why}"));
    }
    let check = format!("`{}`", goal.check);
    let (code, output) = run(goal, store, socket).await;
    // Cleared or set again while the check ran: this turn is plain news;
    // given up meanwhile, it ends so.
    match read(store, &goal.bot) {
        Some(now) if now.set_ms == goal.set_ms => {
            if let Some(why) = &now.blocked {
                end(store, &now);
                return Some(format!(", and gave up its goal: {why}"));
            }
        }
        _ => return Some(String::new()),
    }
    if code == Some(0) {
        end(store, goal);
        return Some(format!(", and met its goal: {check} exits 0"));
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
            end(store, goal);
            return Some(format!(
                ", and stopped at its goal's budget of {budget} tokens: {check} still {exits}"
            ));
        }
    }
    if idle >= IDLE_TURNS {
        end(store, goal);
        return Some(format!(
            ", and stopped pursuing its goal: {IDLE_TURNS} turns in a row it started called no tool, \
             and {check} still {exits}"
        ));
    }
    let prompt = format!(
        "Your goal is not met yet: {check} {exits}.\n```\n{output}\n```\nKeep working until {}. When no way \
         remains, give the goal up with the goal tool's blocked, saying why.",
        goal.done_when
    );
    let asked = json!({"bot": goal.bot, "bot_id": goal.bot_id, "prompt": prompt, "delivery": "queue",
        "origin": "goal", "request_id": format!("{REQUEST}{}-{turn}", goal.bot)});
    match client.request("submit", asked).await {
        Ok(_) => None,
        Err(error) => Some(format!(
            ", and could not go on with its goal: {}",
            error.code
        )),
    }
}

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
    // The provider keys in ~/.agent/env are the daemon's: no agent's own
    // command has them, nor does its check.
    if let Some(file) = agent_client::env_file::path() {
        for (key, _) in agent_client::env_file::read(&file).unwrap_or_default() {
            command.env_remove(key);
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return (None, format!("the check did not start: {error}")),
    };
    let group = child.id();
    // Read as it prints, keeping only the end, so a check that prints
    // without end holds a few KB, not all of it.
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let ran = async {
        let (stdout, stderr) = tokio::join!(last(stdout), last(stderr));
        (child.wait().await, stdout, stderr)
    };
    let (status, stdout, stderr) =
        match tokio::time::timeout(Duration::from_secs(goal.timeout_s), ran).await {
            Ok((Ok(status), stdout, stderr)) => (status, stdout, stderr),
            Ok((Err(error), ..)) => return (None, format!("the check failed to run: {error}")),
            Err(_) => {
                if let Some(group) = group {
                    // SAFETY: a signal to the process group the check leads.
                    unsafe { libc::kill(-(group as i32), libc::SIGKILL) };
                }
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
