//! Task updates: the lead hears, in one message, which tasks it started
//! ended turns or wait for an approval, as its role says it will. This is
//! the app's coordinator wake (`app/ui/app.js`), told the same way, so the
//! lead gets the same message whichever client is open, and the same news
//! from both is one message: the request ID is the app's, made from the
//! news alone.
//!
//! Only live turns count, while Sonny is attached. Once the lead rests, and
//! at most every ten minutes, one message lists each task's latest turn of
//! each kind, by the handle the wait tool reads it with: turns the lead,
//! another agent or a trigger asked for, and approvals, to act on; turns the
//! person asked for in the task themselves, listed apart. A turn the lead
//! waits on is not news, nor its own forks. What changed in memory rides
//! along, and never sends a message by itself.
use agent_client::Client;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const WAKE: Duration = Duration::from_secs(10 * 60);
const WAKE_TASKS: usize = 32;
/// How long a trigger has to pass a task's answer on before the turn is
/// news after all.
const FORWARD: Duration = Duration::from_secs(15);
const MEMORY_LINES: usize = 20;
const ACTIVE: [&str; 5] = ["running", "waiting", "paced", "queued", "ready"];
const APPROVAL: &str = "waiting for approval";

/// A task's latest turn of one kind, and how many came since the first.
#[derive(Clone, Debug, PartialEq)]
struct Turn {
    first: i64,
    turn: i64,
    status: String,
    by: Option<String>,
    count: u32,
    approval: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct News {
    act: Option<Turn>,
    theirs: Option<Turn>,
}

impl News {
    fn merge(&mut self, news: News) {
        fn one(had: Option<Turn>, new: Option<Turn>) -> Option<Turn> {
            match (had, new) {
                (Some(a), Some(t)) => Some(Turn {
                    first: a.first.min(t.first),
                    count: a.count + t.count,
                    ..if t.turn >= a.turn { t } else { a }
                }),
                (a, t) => a.or(t),
            }
        }
        self.act = one(self.act.take(), news.act);
        self.theirs = one(self.theirs.take(), news.theirs);
    }
}

#[derive(Default)]
struct Bot {
    id: Option<i64>,
    parent: Option<String>,
    parent_id: Option<i64>,
    status: String,
    running: Option<i64>,
    waiting_on: Vec<String>,
}

/// A message for the lead, and the news it carries, to put back if it
/// cannot be sent.
pub struct Wake {
    pub request_id: String,
    pub prompt: String,
    sent: Vec<(String, News)>,
    memory: Option<HashMap<String, u64>>,
}

type Key = (String, i64);

pub struct Updates {
    lead: String,
    project: String,
    /// `~/.agents/memory`, where the person's and the project's facts are.
    memory: Option<PathBuf>,
    pub live: bool,
    bots: HashMap<String, Bot>,
    from: HashMap<Key, String>,
    origin: HashMap<Key, String>,
    answer_to: HashMap<Key, i64>,
    /// Turns whose answer a trigger passes to the lead: when they become
    /// news if it does not, and the news.
    forwarding: HashMap<Key, (Instant, String, Option<String>, Option<String>)>,
    /// Each task's news, in the order it first came.
    tasks: Vec<(String, News)>,
    last: Option<Instant>,
    memory_seen: Option<HashMap<String, u64>>,
    opened: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The project a lead heads: `shop` for `shop.lead`.
fn lead_project(name: &str) -> Option<&str> {
    name.strip_suffix(".lead").filter(|p| !p.is_empty())
}

impl Updates {
    pub fn new(lead: &str, project: &str, memory: Option<PathBuf>) -> Self {
        Self {
            lead: lead.to_owned(),
            project: project.to_owned(),
            memory,
            live: false,
            bots: HashMap::new(),
            from: HashMap::new(),
            origin: HashMap::new(),
            answer_to: HashMap::new(),
            forwarding: HashMap::new(),
            tasks: Vec::new(),
            last: None,
            memory_seen: None,
            opened: now_ms(),
        }
    }

    /// A bot as `bots` lists it.
    pub fn seat(&mut self, record: &Value) {
        let Some(name) = record["name"].as_str() else {
            return;
        };
        let status = match record["status"].as_str() {
            Some("completed") | None => "idle",
            Some(status) => status,
        };
        self.bots.insert(
            name.to_owned(),
            Bot {
                id: record["bot_id"].as_i64(),
                parent: record["created_by"].as_str().map(str::to_owned),
                parent_id: record["created_by_id"].as_i64(),
                status: status.to_owned(),
                running: record["running_turn"].as_i64(),
                waiting_on: Vec::new(),
            },
        );
    }

    /// Who made the bot, when the bot holding that name now is the one
    /// that did.
    fn creator(&self, name: &str) -> Option<&str> {
        let bot = self.bots.get(name)?;
        let parent = bot.parent.as_deref()?;
        (bot.parent_id.is_some() && self.bots.get(parent)?.id == bot.parent_id).then_some(parent)
    }

    pub fn event(&mut self, event: &Value, now: Instant) {
        let kind = event["event"].as_str().unwrap_or("");
        if kind == "follow_live" {
            self.live = true;
            return;
        }
        let Some(name) = event["bot"].as_str() else {
            return;
        };
        let turn = event["turn"].as_i64();
        let data = &event["data"];
        let key = |turn: i64| (name.to_owned(), turn);
        match kind {
            "created" | "forked" => {
                let mut record = data.clone();
                record["name"] = json!(name);
                self.seat(&record);
            }
            "deleted" => {
                self.bots.remove(name);
                self.tasks.retain(|(task, _)| task != name);
            }
            "accepted" | "queued" => {
                let Some(turn) = turn else { return };
                if let Some(by) = data["from"]["bot"].as_str() {
                    self.from.insert(key(turn), by.to_owned());
                    // The answer a trigger was to pass on, passed on.
                    if let (Some(task), Some(of)) =
                        (data["from"]["bot"].as_str(), data["from"]["turn"].as_i64())
                    {
                        self.forwarding.remove(&(task.to_owned(), of));
                    }
                } else if let Some(origin) = data["origin"].as_str() {
                    self.origin.insert(key(turn), origin.to_owned());
                }
                // A trigger's fire with --reply-to ends its request ID
                // `.to.ID`: the bot the turn's answer goes to.
                if let Some(to) = data["request_id"]
                    .as_str()
                    .filter(|id| id.starts_with("trigger_"))
                    .and_then(|id| id.rsplit_once(".to."))
                    .and_then(|(_, id)| id.parse().ok())
                {
                    self.answer_to.insert(key(turn), to);
                }
                let Some(bot) = self.bots.get_mut(name) else {
                    return;
                };
                if kind == "accepted" {
                    bot.status = "running".into();
                    bot.running = Some(turn);
                    bot.waiting_on.clear();
                } else if bot.running.is_none() && !ACTIVE.contains(&bot.status.as_str()) {
                    // Behind nothing of its own: waiting for a slot.
                    bot.status = data["status"].as_str().unwrap_or("queued").to_owned();
                    bot.running = Some(turn);
                }
            }
            "turn_waiting" => {
                if let Some(bot) = self.bots.get_mut(name) {
                    bot.status = "waiting".into();
                    bot.waiting_on = data["handles"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                }
                if data["approval"] == true
                    && self.live
                    && let Some(turn) = turn
                {
                    let call = data["call_id"].as_str().map(str::to_owned);
                    self.tell(name, turn, APPROVAL, None, call, None);
                }
            }
            "turn_paced" => {
                if let Some(bot) = self.bots.get_mut(name) {
                    bot.status = "paced".into();
                }
            }
            "turn_resumed" => {
                if let Some(bot) = self.bots.get_mut(name) {
                    bot.status = "running".into();
                    bot.waiting_on.clear();
                }
            }
            "turn_finished" => {
                let Some(turn) = turn else { return };
                let status = data["status"].as_str().unwrap_or("?").to_owned();
                let from = self.from.remove(&key(turn));
                let origin = self.origin.remove(&key(turn));
                let answered = self.answer_to.remove(&key(turn));
                if let Some(bot) = self.bots.get_mut(name)
                    && bot.running.is_none_or(|running| running == turn)
                {
                    bot.running = None;
                    bot.waiting_on.clear();
                    bot.status = match status.as_str() {
                        "completed" | "steered" => "idle".into(),
                        _ => status.clone(),
                    };
                }
                // A steer's turn is part of the turn it joined.
                if !self.live || status == "steered" {
                    return;
                }
                let creator = self
                    .creator(name)
                    .and_then(|c| self.bots.get(c))
                    .and_then(|c| c.id);
                if answered.is_some() && answered == creator {
                    self.forwarding
                        .insert(key(turn), (now + FORWARD, status, from, origin));
                } else {
                    self.tell(name, turn, &status, from, None, origin);
                }
            }
            _ => {}
        }
    }

    fn tell(
        &mut self,
        name: &str,
        turn: i64,
        status: &str,
        from: Option<String>,
        approval: Option<String>,
        origin: Option<String>,
    ) {
        if self.creator(name) != Some(self.lead.as_str())
            || lead_project(name).is_some()
            || name.starts_with(&format!("{}-", self.lead))
        {
            return;
        }
        let handle = format!("turn:{name}/{turn}");
        if self.bots[&self.lead].waiting_on.contains(&handle) {
            return;
        }
        let by = match status {
            APPROVAL => None,
            _ if from.as_deref() == Some(self.lead.as_str()) => Some("you".to_owned()),
            _ => Some(from.or(origin).unwrap_or_else(|| "the person".into())),
        };
        let at = match self.tasks.iter().position(|(task, _)| task == name) {
            Some(at) => at,
            None => {
                self.tasks.push((name.to_owned(), News::default()));
                self.tasks.len() - 1
            }
        };
        let news = &mut self.tasks[at].1;
        let answered = news
            .act
            .as_ref()
            .is_some_and(|asked| asked.turn == turn && asked.status == APPROVAL);
        let theirs = by.as_deref() == Some("the person") && !answered;
        let turn = Turn {
            first: turn,
            turn,
            status: status.to_owned(),
            by,
            count: 1,
            approval,
        };
        news.merge(match theirs {
            true => News {
                theirs: Some(turn),
                ..News::default()
            },
            false => News {
                act: Some(turn),
                ..News::default()
            },
        });
    }

    /// Turns a trigger did not pass on in time, as news.
    pub fn expire(&mut self, now: Instant) {
        let late: Vec<Key> = self
            .forwarding
            .iter()
            .filter(|(_, (at, ..))| *at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in late {
            if let Some((_, status, from, origin)) = self.forwarding.remove(&key) {
                self.tell(&key.0, key.1, &status, from, None, origin);
            }
        }
    }

    /// Whether the lead is to hear now: there is news, it rests, and it
    /// last heard long enough ago.
    pub fn due(&self, now: Instant) -> bool {
        self.live
            && !self.tasks.is_empty()
            && self
                .bots
                .get(&self.lead)
                .is_some_and(|lead| lead.id.is_some() && !ACTIVE.contains(&lead.status.as_str()))
            && self.last.is_none_or(|last| now >= last + WAKE)
    }

    /// The message for the lead, taking the news it carries.
    pub fn take(&mut self, now: Instant) -> Option<Wake> {
        let lead_id = self.bots.get(&self.lead)?.id?;
        let mut tasks = std::mem::take(&mut self.tasks);
        // Those with news to act on first; the rest wait for the next.
        tasks.sort_by_key(|(_, news)| news.act.is_none());
        let rest = tasks.split_off(tasks.len().min(WAKE_TASKS));
        self.tasks = rest;
        self.last = Some(now);
        let items: Vec<(&str, &str, &Turn)> = tasks
            .iter()
            .flat_map(|(name, news)| {
                [("act", &news.act), ("theirs", &news.theirs)]
                    .into_iter()
                    .filter_map(move |(kind, turn)| Some((name.as_str(), kind, turn.as_ref()?)))
            })
            .collect();
        let mut prompt = wake_text(&items, self.tasks.len());
        let (memory, seen) = match self.memory_news() {
            Some((text, seen)) => (Some(text), Some(seen)),
            None => (None, None),
        };
        if let Some(text) = memory {
            prompt.push_str("\n\n");
            prompt.push_str(&text);
        }
        let mut keyed: Vec<Value> = items
            .iter()
            .map(|(name, kind, turn)| json!([name, kind, turn.turn, turn.status, turn.approval]))
            .collect();
        let key = |v: &Value| {
            format!(
                "{}\u{0}{}",
                v[0].as_str().unwrap_or(""),
                v[1].as_str().unwrap_or("")
            )
        };
        keyed.sort_by_key(key);
        let request_id = format!(
            "app-wake-{lead_id}-{}",
            digest(&Value::Array(keyed).to_string())
        );
        Some(Wake {
            request_id,
            prompt,
            sent: tasks,
            memory: seen,
        })
    }

    /// The message was sent: what it said of memory has been told.
    pub fn sent(&mut self, wake: Wake) {
        if let Some(seen) = wake.memory {
            self.memory_seen = Some(seen);
        }
    }

    /// The message could not be sent: its news comes first next time.
    pub fn unsent(&mut self, wake: Wake) {
        let later = std::mem::replace(&mut self.tasks, wake.sent);
        for (name, news) in later {
            match self.tasks.iter_mut().find(|(task, _)| *task == name) {
                Some((_, had)) => had.merge(news),
                None => self.tasks.push((name, news)),
            }
        }
    }

    /// The lead was deleted, or another took its name.
    pub fn forget(&mut self) {
        self.tasks.clear();
    }

    /// What was saved to or removed from the person's and the project's
    /// memory since the lead last heard (or since Sonny opened), less what
    /// the lead saved itself; and what it will have heard.
    fn memory_news(&mut self) -> Option<(String, HashMap<String, u64>)> {
        let root = self.memory.as_ref()?;
        let dirs = [root.clone(), root.join("projects").join(&self.project)];
        let read: Vec<(String, Vec<Fact>)> = dirs
            .iter()
            .filter_map(|dir| Some((dir.to_string_lossy().into_owned(), facts(dir)?)))
            .collect();
        let now: HashMap<&str, &Fact> = read
            .iter()
            .flat_map(|(_, facts)| facts.iter().map(|f| (f.path.as_str(), f)))
            .collect();
        let in_read = |path: &str| {
            let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            read.iter()
                .any(|(read, _)| read.trim_end_matches('/') == dir)
        };
        let own = format!("turn:{}/", self.lead);
        let seen = self.memory_seen.as_ref();
        let mut saved: Vec<&Fact> = now
            .values()
            .copied()
            .filter(|f| match seen {
                Some(seen) => seen.get(&f.path) != Some(&f.modified),
                None => f.modified > self.opened,
            })
            .filter(|f| !f.source.starts_with(&own))
            .collect();
        saved.sort_by_key(|f| std::cmp::Reverse(f.modified));
        let mut gone: Vec<&String> = seen
            .into_iter()
            .flat_map(|seen| seen.keys())
            .filter(|path| !now.contains_key(path.as_str()) && in_read(path))
            .collect();
        gone.sort();
        let told: HashMap<String, u64> = seen
            .into_iter()
            .flatten()
            .filter(|(path, _)| !in_read(path))
            .map(|(path, at)| (path.clone(), *at))
            .chain(now.iter().map(|(path, f)| (path.to_string(), f.modified)))
            .collect();
        let lines: Vec<String> = gone
            .iter()
            .map(|path| format!("- removed: {path}"))
            .chain(
                saved
                    .iter()
                    .map(|f| format!("- saved: {}: {}", f.path, f.description)),
            )
            .collect();
        if lines.is_empty() {
            self.memory_seen = Some(told);
            return None;
        }
        let mut text = format!(
            "Memory changed since you last heard; each fact is a file, read before relying on it:\n{}",
            lines[..lines.len().min(MEMORY_LINES)].join("\n")
        );
        if lines.len() > MEMORY_LINES {
            text.push_str(&format!(
                "\n- {} more: ~/.agent/memory show lists every fact",
                lines.len() - MEMORY_LINES
            ));
        }
        Some((text, told))
    }
}

/// The lead's message, as the app writes it.
fn wake_text(items: &[(&str, &str, &Turn)], more: usize) -> String {
    let line = |(name, _, t): &(&str, &str, &Turn)| {
        let mut line = format!("- {name}: turn:{name}/{} {}", t.turn, t.status);
        if let Some(by) = &t.by {
            line.push_str(&format!(", asked by {by}"));
        }
        if t.count > 1 {
            line.push_str(&format!(
                ", and {} earlier since turn:{name}/{}",
                t.count - 1,
                t.first
            ));
        }
        line
    };
    let mut lines: Vec<String> = items.iter().filter(|i| i.1 == "act").map(line).collect();
    let theirs: Vec<String> = items.iter().filter(|i| i.1 == "theirs").map(line).collect();
    if !theirs.is_empty() {
        lines.push(
            "The person asked for these turns in the task themselves, so they are theirs:".into(),
        );
        lines.extend(theirs);
    }
    if more > 0 {
        lines.push(format!("- {more} more tasks in the next update"));
    }
    format!(
        "Task updates: since you last heard, tasks you started ended turns or wait for an approval. The wait tool reads each one's final reply by its handle.\n{}",
        lines.join("\n")
    )
}

/// FNV-1a over the text's UTF-16 units, 64 bits as hex, as the app makes
/// it.
fn digest(text: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for unit in text.encode_utf16() {
        hash = (hash ^ unit as u64).wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

struct Fact {
    path: String,
    description: String,
    source: String,
    modified: u64,
}

/// The facts in a memory folder, none in one not there; None when one of
/// them cannot be read, as the app counts such a folder unread.
fn facts(dir: &Path) -> Option<Vec<Fact>> {
    let entries = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        entries => entries.ok()?,
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.ok()?;
        let file = entry.file_name().to_string_lossy().into_owned();
        if file == "MEMORY.md" || file.starts_with('.') || !file.ends_with(".md") {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let text = std::fs::read_to_string(&path).ok()?;
        let head = text.strip_prefix("---\n")?.split_once("\n---\n")?.0;
        let field = |key: &str| {
            head.lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix(": "))
                .map(str::to_owned)
        };
        let modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as u64);
        found.push(Fact {
            path: path.to_string_lossy().into_owned(),
            description: field("description")?,
            source: field("source")?,
            modified,
        });
    }
    Some(found)
}

/// Follow the daemon for the lead's tasks and tell it their news, for as
/// long as Sonny runs, connecting again when the daemon goes.
pub async fn run(socket: PathBuf, lead: String, project: String) {
    let memory = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agents/memory"));
    let mut updates = Updates::new(&lead, &project, memory);
    loop {
        if let Ok((client, mut events)) = Client::connect(&socket).await
            && seat_all(&client, &mut updates).await
            && client
                .request("follow", json!({"bot": "*", "after": 0}))
                .await
                .is_ok()
        {
            updates.live = false;
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Some(event) => updates.event(&event, Instant::now()),
                        None => break,
                    },
                    _ = tick.tick() => {}
                }
                let now = Instant::now();
                updates.expire(now);
                if updates.due(now) {
                    tell(&client, &mut updates, &lead, now).await;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn seat_all(client: &Client, updates: &mut Updates) -> bool {
    let mut after = Value::Null;
    loop {
        let Ok(page) = client
            .request("bots", json!({"after": after, "limit": 256}))
            .await
        else {
            return false;
        };
        for record in page["bots"].as_array().into_iter().flatten() {
            updates.seat(record);
        }
        after = page["next_after"].clone();
        if after.is_null() {
            return true;
        }
    }
}

async fn tell(client: &Client, updates: &mut Updates, lead: &str, now: Instant) {
    let Some(wake) = updates.take(now) else {
        return;
    };
    let lead_id = updates.bots.get(lead).and_then(|b| b.id);
    let sent = client
        .request(
            "submit",
            json!({"bot": lead, "bot_id": lead_id, "request_id": wake.request_id,
                "prompt": wake.prompt, "delivery": "queue", "origin": "tasks"}),
        )
        .await;
    match sent {
        Ok(_) => updates.sent(wake),
        Err(error) => match error.code.as_str() {
            "bot_not_found" => updates.forget(),
            // Another client told it first.
            "idempotency_conflict" => {}
            _ => updates.unsent(wake),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(bot: &str, turn: Option<i64>, kind: &str, data: Value) -> Value {
        json!({"bot": bot, "turn": turn, "event": kind, "data": data})
    }

    fn fleet() -> Updates {
        let mut updates = Updates::new("shop.lead", "shop", None);
        let now = Instant::now();
        for (name, id, by, by_id) in [
            ("shop.lead", 1, None, None),
            ("shop.fix", 2, Some("shop.lead"), Some(1)),
            ("shop.docs", 3, Some("shop.lead"), Some(1)),
            ("shop.fix.review", 4, Some("shop.fix"), Some(2)),
        ] {
            updates.event(
                &ev(
                    name,
                    None,
                    "created",
                    json!({"bot_id": id, "status": "idle",
                    "created_by": by, "created_by_id": by_id}),
                ),
                now,
            );
        }
        updates.event(&json!({"event": "follow_live"}), now);
        updates
    }

    fn turn(updates: &mut Updates, bot: &str, turn: i64, from: Value, status: &str) {
        let now = Instant::now();
        updates.event(&ev(bot, Some(turn), "accepted", from), now);
        updates.event(
            &ev(bot, Some(turn), "turn_finished", json!({"status": status})),
            now,
        );
    }

    #[test]
    fn the_lead_hears_its_tasks_once_it_rests_in_the_apps_words() {
        let mut updates = fleet();
        let now = Instant::now();
        updates.event(
            &ev(
                "shop.lead",
                Some(9),
                "accepted",
                json!({"origin": "person"}),
            ),
            now,
        );
        turn(
            &mut updates,
            "shop.fix",
            4,
            json!({"from": {"bot": "shop.lead", "turn": 9}}),
            "completed",
        );
        turn(
            &mut updates,
            "shop.fix",
            5,
            json!({"from": {"bot": "shop.lead", "turn": 9}}),
            "failed",
        );
        turn(&mut updates, "shop.docs", 2, json!({}), "completed");
        // A task's own helper is that task's to hear of.
        turn(
            &mut updates,
            "shop.fix.review",
            1,
            json!({"from": {"bot": "shop.fix", "turn": 4}}),
            "completed",
        );
        assert!(!updates.due(now), "the lead is working");
        updates.event(
            &ev(
                "shop.lead",
                Some(9),
                "turn_finished",
                json!({"status": "completed"}),
            ),
            now,
        );
        assert!(updates.due(now));
        let wake = updates.take(now).unwrap();
        assert_eq!(
            wake.prompt,
            "Task updates: since you last heard, tasks you started ended turns or wait for an approval. The wait tool reads each one's final reply by its handle.\n\
             - shop.fix: turn:shop.fix/5 failed, asked by you, and 1 earlier since turn:shop.fix/4\n\
             The person asked for these turns in the task themselves, so they are theirs:\n\
             - shop.docs: turn:shop.docs/2 completed, asked by the person"
        );
        // The app's ID for the same news, so either client's message is the one.
        // As app.js's digest makes it of the same news.
        assert_eq!(wake.request_id, "app-wake-1-3db47f6e6680999f");
        updates.sent(wake);
        // Ten minutes apart at most.
        turn(
            &mut updates,
            "shop.docs",
            3,
            json!({"from": {"bot": "shop.lead", "turn": 10}}),
            "completed",
        );
        assert!(!updates.due(now + Duration::from_secs(60)));
        assert!(updates.due(now + WAKE));
    }

    #[test]
    fn what_the_lead_waits_on_and_what_it_cannot_send_are_handled() {
        let mut updates = fleet();
        let now = Instant::now();
        updates.event(
            &ev(
                "shop.lead",
                Some(9),
                "turn_waiting",
                json!({"handles": ["turn:shop.fix/4"]}),
            ),
            now,
        );
        turn(
            &mut updates,
            "shop.fix",
            4,
            json!({"from": {"bot": "shop.lead", "turn": 9}}),
            "completed",
        );
        updates.event(&ev("shop.docs", Some(2), "accepted", json!({})), now);
        updates.event(
            &ev(
                "shop.docs",
                Some(2),
                "turn_waiting",
                json!({"approval": true, "call_id": "c1"}),
            ),
            now,
        );
        updates.event(
            &ev(
                "shop.lead",
                Some(9),
                "turn_finished",
                json!({"status": "completed"}),
            ),
            now,
        );
        let wake = updates.take(now).unwrap();
        assert!(
            wake.prompt
                .ends_with("\n- shop.docs: turn:shop.docs/2 waiting for approval"),
            "{}",
            wake.prompt
        );
        assert!(
            !wake.prompt.contains("shop.fix"),
            "the lead read it with its wait"
        );
        // Refused for another reason: the news comes back for next time.
        updates.unsent(wake);
        let again = updates.take(now + WAKE).unwrap();
        assert!(again.prompt.contains("shop.docs/2 waiting for approval"));
    }

    #[test]
    fn memory_changes_ride_along_less_the_leads_own() {
        let root = std::env::temp_dir().join(format!("sonny-updates-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join("projects/shop");
        std::fs::create_dir_all(&project).unwrap();
        let fact = |dir: &Path, name: &str, source: &str| {
            std::fs::write(
                dir.join(format!("{name}.md")),
                format!("---\nname: {name}\ndescription: about {name}\ntype: project\nsource: {source}\nverified: 2026-10-26\n---\n\nbody\n"),
            )
            .unwrap();
        };
        fact(&root, "old", "person");
        let mut updates = fleet();
        updates.memory = Some(root.clone());
        updates.opened = 0;
        let news = |updates: &mut Updates| {
            let now = Instant::now() + WAKE * 10;
            turn(
                updates,
                "shop.fix",
                4,
                json!({"from": {"bot": "shop.lead", "turn": 9}}),
                "completed",
            );
            let wake = updates.take(now).unwrap();
            let prompt = wake.prompt.clone();
            updates.sent(wake);
            prompt
        };
        assert!(
            news(&mut updates).contains(&format!("- saved: {}/old.md: about old", root.display()))
        );
        fact(&project, "flow", "turn:shop.fix/4");
        fact(&project, "mine", "turn:shop.lead/9");
        std::fs::remove_file(root.join("old.md")).unwrap();
        let told = news(&mut updates);
        let memory = told.split_once("\n\n").unwrap().1;
        assert_eq!(
            memory,
            format!(
                "Memory changed since you last heard; each fact is a file, read before relying on it:\n\
                 - removed: {root}/old.md\n\
                 - saved: {root}/projects/shop/flow.md: about flow",
                root = root.display()
            )
        );
        // Told once.
        assert!(!news(&mut updates).contains("Memory changed"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn digest_is_the_apps() {
        // FNV-1a 64 of "a": the app's digest('a').
        assert_eq!(digest("a"), "af63dc4c8601ec8c");
    }
}
