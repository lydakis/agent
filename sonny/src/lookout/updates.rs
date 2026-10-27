//! Task updates: what the lead hears of its tasks unjudged, as its role
//! says it will. A turn it asked for and did not wait on, a turn another
//! agent or a trigger asked for, and an approval only the person can give
//! are always news; turns the person asked for in the task themselves are
//! listed apart, as theirs. A turn the lead waits on is not news, nor its
//! own forks.
//!
//! Only live turns count. The lookout sends the news with its next note,
//! or on its own once the lead rests, at most every ten minutes; one note
//! names each task's latest turn of each kind, by the handle the wait tool
//! reads it with. What changed in memory rides along, and never sends a
//! note by itself.
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
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
const APPROVAL: &str = "waiting for the person's approval";
/// An approval is news once the approver has had its say: a call the
/// approver answers waits a few seconds first, the person's longer.
const APPROVAL_AFTER: Duration = super::APPROVAL_AFTER;

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

/// The news one note carries, and what to put back if it cannot be sent.
#[derive(Default)]
pub struct Told {
    /// One line a turn, the person's apart; each with its handle.
    pub items: Vec<Line>,
    /// Tasks whose news waits for the next note.
    pub more: usize,
    /// What changed in memory, a paragraph of its own.
    pub memory: Option<String>,
    sent: Vec<(String, News)>,
    seen: Option<HashMap<String, u64>>,
}

pub struct Line {
    pub handle: String,
    /// Asked for by the person in the task: theirs, not the lead's.
    pub theirs: bool,
    pub text: String,
    /// The news itself: the same news twice is one note.
    pub key: String,
}

type Key = (String, i64);

pub struct Updates {
    lead: String,
    /// The memory folders whose facts are news: the person's and the
    /// project's, as the subscriber's own memory indexes name them.
    memory: Vec<PathBuf>,
    /// Bots with subscriptions of their own: no one's tasks here.
    subscribers: HashSet<String>,
    pub live: bool,
    bots: HashMap<String, Bot>,
    from: HashMap<Key, String>,
    origin: HashMap<Key, String>,
    answer_to: HashMap<Key, i64>,
    /// Turns whose answer a trigger passes to the lead: when they become
    /// news if it does not, and the news.
    forwarding: HashMap<Key, (Instant, String, Option<String>, Option<String>)>,
    /// Approvals not yet answered: when they become news, and the call.
    approvals: HashMap<Key, (Instant, Option<String>)>,
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

impl Updates {
    pub fn new(lead: &str, memory: Vec<PathBuf>) -> Self {
        Self {
            lead: lead.to_owned(),
            memory,
            subscribers: HashSet::new(),
            live: false,
            bots: HashMap::new(),
            from: HashMap::new(),
            origin: HashMap::new(),
            answer_to: HashMap::new(),
            forwarding: HashMap::new(),
            approvals: HashMap::new(),
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
        // Another bot under the name: the news was for the last one.
        if self
            .bots
            .get(name)
            .is_some_and(|had| had.id != record["bot_id"].as_i64())
        {
            self.gone(name);
        }
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

    /// Every bot is seated: one no longer listed was deleted while the lookout
    /// was away, which no event will say.
    pub fn seated(&mut self, listed: &HashSet<String>) {
        let gone: Vec<String> = (self.bots.keys())
            .filter(|name| !listed.contains(*name))
            .cloned()
            .collect();
        for name in gone {
            self.bots.remove(&name);
            self.gone(&name);
        }
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
                self.gone(name);
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
                    self.approvals
                        .entry(key(turn))
                        .or_insert((now + APPROVAL_AFTER, call));
                }
            }
            "turn_paced" => {
                if let Some(bot) = self.bots.get_mut(name) {
                    bot.status = "paced".into();
                }
            }
            // Answered: no longer news.
            "tool_completed" => {
                if let Some(turn) = turn {
                    self.approvals.remove(&key(turn));
                }
            }
            "turn_resumed" => {
                if let Some(turn) = turn {
                    self.approvals.remove(&key(turn));
                }
                if let Some(bot) = self.bots.get_mut(name) {
                    bot.status = "running".into();
                    bot.waiting_on.clear();
                }
            }
            "turn_finished" => {
                let Some(turn) = turn else { return };
                self.approvals.remove(&key(turn));
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
            || self.subscribers.contains(name)
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
        let waited: Vec<Key> = (self.approvals.iter())
            .filter(|(_, (at, _))| *at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in waited {
            if let Some((_, call)) = self.approvals.remove(&key) {
                self.tell(&key.0, key.1, APPROVAL, None, call, None);
            }
        }
    }

    /// The lead read these turns with its wait tool: what they said is no
    /// news to it any more.
    pub fn read(&mut self, handles: &HashSet<String>) {
        for (name, news) in &mut self.tasks {
            for kind in [&mut news.act, &mut news.theirs] {
                if kind.as_ref().is_some_and(|t| {
                    t.status != APPROVAL && handles.contains(&format!("turn:{name}/{}", t.turn))
                }) {
                    *kind = None;
                }
            }
        }
        self.tasks
            .retain(|(_, news)| news.act.is_some() || news.theirs.is_some());
        self.forwarding
            .retain(|(name, turn), _| !handles.contains(&format!("turn:{name}/{turn}")));
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

    /// The news for the next note, taken: every task's when the lead
    /// heard nothing for a while, those with news to act on first.
    pub fn take(&mut self, now: Instant) -> Told {
        let mut tasks = std::mem::take(&mut self.tasks);
        tasks.sort_by_key(|(_, news)| news.act.is_none());
        self.tasks = tasks.split_off(tasks.len().min(WAKE_TASKS));
        self.last = Some(now);
        let mut items: Vec<Line> = tasks
            .iter()
            .flat_map(|(name, news)| {
                [(false, &news.act), (true, &news.theirs)]
                    .into_iter()
                    .filter_map(move |(theirs, turn)| Some(line(name, theirs, turn.as_ref()?)))
            })
            .collect();
        // The person's own turns come last, under a line of their own.
        items.sort_by_key(|item| item.theirs);
        let (memory, seen) = match self.memory_news() {
            Some((text, seen)) => (Some(text), Some(seen)),
            None => (None, None),
        };
        Told {
            items,
            more: self.tasks.len(),
            memory,
            sent: tasks,
            seen,
        }
    }

    /// The note was sent: what it said of memory has been told.
    pub fn sent(&mut self, told: Told) {
        if let Some(seen) = told.seen {
            self.memory_seen = Some(seen);
        }
    }

    /// The note could not be sent: its news comes first next time.
    pub fn unsent(&mut self, told: Told) {
        let later = std::mem::replace(&mut self.tasks, told.sent);
        for (name, news) in later {
            match self.tasks.iter_mut().find(|(task, _)| *task == name) {
                Some((_, had)) => had.merge(news),
                None => self.tasks.push((name, news)),
            }
        }
    }

    /// The lead's ID, to send it the note and no later bot of its name.
    pub fn lead_id(&self) -> Option<i64> {
        self.bots.get(&self.lead)?.id
    }

    /// The lead was deleted, or another took its name: what was news for
    /// it is no one's.
    pub fn forget(&mut self) {
        self.tasks.clear();
        self.forwarding.clear();
        self.approvals.clear();
    }

    /// The bots with subscriptions of their own, now: what was news of
    /// them is no longer.
    pub fn subscribed(&mut self, subscribers: HashSet<String>) {
        let new: Vec<String> = (subscribers.difference(&self.subscribers))
            .cloned()
            .collect();
        for name in new {
            self.drop_news(&name);
        }
        self.subscribers = subscribers;
    }

    /// Task updates were just asked for: news from now on, the bots'
    /// states kept.
    pub fn begin(&mut self) {
        self.forget();
        self.memory_seen = None;
        self.opened = now_ms();
    }

    /// The bot under `name` is gone: drop its news, or all of it when it
    /// was the lead.
    fn gone(&mut self, name: &str) {
        if name == self.lead {
            return self.forget();
        }
        self.drop_news(name);
    }

    fn drop_news(&mut self, name: &str) {
        self.tasks.retain(|(task, _)| task != name);
        self.forwarding.retain(|(task, _), _| task != name);
        self.approvals.retain(|(task, _), _| task != name);
    }

    /// What was saved to or removed from the person's and the project's
    /// memory since the lead last heard (or since the lookout started), less what
    /// the lead saved itself; and what it will have heard.
    fn memory_news(&mut self) -> Option<(String, HashMap<String, u64>)> {
        let read: Vec<(String, Vec<Fact>)> = (self.memory)
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
            .chain(saved.iter().map(|f| format!("- saved: {}", f.path)))
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

/// A task's latest turn of one kind, as the note names it.
fn line(name: &str, theirs: bool, t: &Turn) -> Line {
    let mut text = format!("- {name}: turn:{name}/{} {}", t.turn, t.status);
    if let Some(by) = &t.by {
        text.push_str(&format!(", asked by {by}"));
    }
    if t.count > 1 {
        text.push_str(&format!(
            ", and {} earlier since turn:{name}/{}",
            t.count - 1,
            t.first
        ));
    }
    let approval = t.approval.as_deref().unwrap_or("");
    Line {
        handle: format!("turn:{name}/{}", t.turn),
        theirs,
        text,
        key: format!("{name}/{}/{}/{theirs}/{approval}", t.turn, t.status),
    }
}

struct Fact {
    path: String,
    source: String,
    modified: u64,
}

/// The facts in a memory folder, none in one not there; None when one of
/// them cannot be read.
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
            source: field("source")?,
            modified,
        });
    }
    Some(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(bot: &str, turn: Option<i64>, kind: &str, data: Value) -> Value {
        json!({"bot": bot, "turn": turn, "event": kind, "data": data})
    }

    fn fleet() -> Updates {
        let mut updates = Updates::new("shop.lead", Vec::new());
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

    /// The note's lines and its memory paragraph.
    fn text(told: &Told) -> String {
        let mut text = (told.items.iter().map(|i| i.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(memory) = &told.memory {
            text.push_str("\n\n");
            text.push_str(memory);
        }
        text
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
    fn the_lead_hears_its_tasks_once_it_rests() {
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
        let told = updates.take(now);
        assert_eq!(
            text(&told),
            "- shop.fix: turn:shop.fix/5 failed, asked by you, and 1 earlier since turn:shop.fix/4\n\
             - shop.docs: turn:shop.docs/2 completed, asked by the person"
        );
        let theirs: Vec<bool> = told.items.iter().map(|i| i.theirs).collect();
        assert_eq!(theirs, [false, true]);
        assert_eq!(told.items[0].handle, "turn:shop.fix/5");
        updates.sent(told);
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
        // The approver has its say first.
        updates.expire(now);
        assert!(!updates.due(now), "the lead read shop.fix with its wait");
        updates.expire(now + APPROVAL_AFTER);
        let told = updates.take(now);
        assert_eq!(
            text(&told),
            "- shop.docs: turn:shop.docs/2 waiting for the person's approval"
        );
        // Refused: the news comes back for next time.
        updates.unsent(told);
        let again = updates.take(now + WAKE);
        assert!(text(&again).contains("shop.docs/2 waiting for the person's approval"));
    }

    #[test]
    fn an_approval_answered_in_time_and_a_turn_the_lead_read_are_no_news() {
        let mut updates = fleet();
        let now = Instant::now();
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
        // The approver allowed it.
        updates.event(&ev("shop.docs", Some(2), "turn_resumed", json!({})), now);
        updates.expire(now + APPROVAL_AFTER);
        assert!(updates.take(now).items.is_empty());
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
            "completed",
        );
        // Reading an earlier turn leaves the latest news.
        updates.read(&["turn:shop.fix/4".to_owned()].into());
        assert!(updates.due(now + WAKE));
        updates.read(&["turn:shop.fix/5".to_owned()].into());
        assert!(!updates.due(now + WAKE));
    }

    #[test]
    fn memory_changes_ride_along_less_the_leads_own() {
        let root = std::env::temp_dir().join(format!("lookout-updates-{}", std::process::id()));
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
        updates.memory = vec![root.clone(), project.clone()];
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
            let told = updates.take(now);
            let prompt = text(&told);
            updates.sent(told);
            prompt
        };
        assert!(news(&mut updates).contains(&format!("- saved: {}/old.md", root.display())));
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
                 - saved: {root}/projects/shop/flow.md",
                root = root.display()
            )
        );
        // Told once.
        assert!(!news(&mut updates).contains("Memory changed"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_new_lead_under_the_name_does_not_hear_the_last_ones_news() {
        let now = Instant::now();
        for gone in [
            ev("shop.lead", None, "deleted", json!({})),
            // Seated afresh on reconnecting, with another ID.
            json!({}),
        ] {
            let mut updates = fleet();
            turn(
                &mut updates,
                "shop.fix",
                4,
                json!({"from": {"bot": "shop.lead", "turn": 9}}),
                "completed",
            );
            if gone["event"] == "deleted" {
                updates.event(&gone, now);
                updates.event(
                    &ev(
                        "shop.lead",
                        None,
                        "created",
                        json!({"bot_id": 7, "status": "idle"}),
                    ),
                    now,
                );
            } else {
                updates.seat(&json!({"name": "shop.lead", "bot_id": 7, "status": "idle"}));
            }
            assert!(!updates.due(now), "{gone}");
            assert!(updates.take(now).sent.is_empty());
        }
    }

    #[test]
    fn a_new_task_under_the_name_does_not_inherit_the_last_ones_news() {
        let now = Instant::now();
        let mut updates = fleet();
        turn(
            &mut updates,
            "shop.fix",
            4,
            json!({"from": {"bot": "shop.lead", "turn": 9}}),
            "completed",
        );
        // Deleted and made again while the lookout was away: seated afresh.
        updates.seat(&json!({"name": "shop.fix", "bot_id": 99, "status": "idle"}));
        assert!(!updates.due(now));
        assert!(updates.take(now).sent.is_empty());
    }

    #[test]
    fn a_task_deleted_while_the_lookout_was_away_has_no_news() {
        let now = Instant::now();
        let mut updates = fleet();
        turn(
            &mut updates,
            "shop.fix",
            4,
            json!({"from": {"bot": "shop.lead", "turn": 9}}),
            "completed",
        );
        let listed = ["shop.lead", "shop.docs", "shop.fix.review"]
            .map(str::to_owned)
            .into();
        updates.seated(&listed);
        assert!(!updates.due(now));
        assert!(updates.take(now).sent.is_empty());
    }
}
