//! Task updates: what the lead hears of its tasks unjudged, as its role
//! says it will. Every message someone other than the lead writes in a
//! task, a turn it asked for and did not wait on, a turn another agent or
//! a trigger asked for, an approval only the person can give, and a
//! running turn that has printed nothing for a long while are always news;
//! turns the person asked for in the task themselves are listed apart, as
//! theirs. The person's words are quoted; anyone else's stay behind the
//! turn's handle, as the note reaches the approver as the person's own. A
//! turn the lead waits on is not news, nor its own forks.
//!
//! Only live turns count. The lookout sends the news with its next note,
//! or on its own as soon as the lead rests; one note
//! names each task's latest turn of each kind, by the handle the wait tool
//! reads it with. What changed in memory rides along, and never sends a
//! note by itself.
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// How long news waits on a lead its events say is busy before the
/// daemon is asked whether it is.
pub const DOUBT: Duration = Duration::from_secs(60);
const WAKE_TASKS: usize = 32;
/// About what one note may take, well under the daemon's prompt limit; a
/// line other than the person's words counts as `LINE_BYTES`.
const NOTE_BYTES: usize = 64 * 1024;
const LINE_BYTES: usize = 160;
/// How many messages a task's news keeps the person's words for.
const PENDING: usize = 128;
/// How long a trigger has to pass a task's answer on before the turn is
/// news after all.
const FORWARD: Duration = Duration::from_secs(15);
const MEMORY_LINES: usize = 20;
const ACTIVE: [&str; 5] = ["running", "waiting", "paced", "queued", "ready"];
const APPROVAL: &str = "waiting for the person's approval";
const QUIET: &str = "has printed nothing for 30 minutes";
/// How long a running turn may print nothing before it is news.
const QUIET_AFTER: Duration = super::QUIET_AFTER;
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
    /// Which call waits on approval, or which silence: each its own news.
    which: Option<String>,
}

/// A message written in a task: who wrote it, and the words, only when
/// the person did.
#[derive(Clone, Debug, PartialEq)]
struct Said {
    turn: i64,
    from: String,
    words: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct News {
    said: Vec<Said>,
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
        self.said.extend(news.said);
        self.said.sort_by_key(|said| said.turn);
        self.said.dedup_by_key(|said| said.turn);
        // A long backlog keeps its count and who wrote, but past PENDING
        // the oldest lose their words, then go: the wait tool reads them.
        let over = self.said.len().saturating_sub(PENDING);
        for said in self.said.iter_mut().take(over) {
            said.words = None;
        }
        if self.said.len() > 2 * PENDING {
            self.said.drain(..self.said.len() - 2 * PENDING);
        }
        self.act = one(self.act.take(), news.act);
        self.theirs = one(self.theirs.take(), news.theirs);
    }

    fn empty(&self) -> bool {
        self.said.is_empty() && self.act.is_none() && self.theirs.is_none()
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
    /// Words written in the task, not how a turn ended.
    pub said: bool,
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
    /// The bots whose news this is, as the lookout's subscriptions with
    /// task updates name them.
    pub watched: HashSet<String>,
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
    /// Each task's running turn, when it last printed anything, and
    /// whether that silence was told already.
    quiet: HashMap<String, (i64, Instant, bool)>,
    silences: u64,
    /// When the daemon last said whether the lead rests.
    asked: Option<Instant>,
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
            watched: HashSet::new(),
            live: false,
            bots: HashMap::new(),
            from: HashMap::new(),
            origin: HashMap::new(),
            answer_to: HashMap::new(),
            forwarding: HashMap::new(),
            approvals: HashMap::new(),
            tasks: Vec::new(),
            quiet: HashMap::new(),
            silences: 0,
            asked: None,
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
        if let Some(turn) = turn {
            self.printed(name, kind, turn, data, now);
        }
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

    /// Keeps each task's running turn and when it last printed anything:
    /// a turn waiting on the person, on other turns or for a slot is not
    /// silent, and neither is one that ended.
    fn printed(&mut self, task: &str, kind: &str, turn: i64, data: &Value, now: Instant) {
        match kind {
            "accepted" | "turn_resumed" => {
                self.quiet.insert(task.to_owned(), (turn, now, false));
            }
            "turn_waiting" | "turn_paced" => {
                self.quiet.remove(task);
            }
            "turn_finished" if data["status"] != "steered" => {
                if (self.quiet.get(task)).is_some_and(|(running, ..)| *running == turn) {
                    self.quiet.remove(task);
                }
            }
            // A turn that was running before its bot was watched is
            // tracked from the first thing it does since.
            "text_delta" | "tool_started" | "tool_completed" if !self.quiet.contains_key(task) => {
                self.quiet.insert(task.to_owned(), (turn, now, false));
            }
            _ => {
                if let Some((running, at, told)) = self.quiet.get_mut(task)
                    && *running == turn
                {
                    *at = now;
                    *told = false;
                }
            }
        }
    }

    /// Whether the bot is one of the lead's tasks: one a subscription with
    /// task updates watches, not a fork of the lead nor a subscriber of its
    /// own.
    fn task(&self, name: &str) -> bool {
        self.watched.contains(name)
            && !self.subscribers.contains(name)
            && !name.starts_with(&format!("{}-", self.lead))
    }

    /// The task's news, made in the order it first came.
    fn news_of(&mut self, name: &str) -> &mut News {
        let at = match self.tasks.iter().position(|(task, _)| task == name) {
            Some(at) => at,
            None => {
                self.tasks.push((name.to_owned(), News::default()));
                self.tasks.len() - 1
            }
        };
        &mut self.tasks[at].1
    }

    /// Someone other than the lead wrote in a task: `from` names them, and
    /// `words` are what the person wrote, when it was the person.
    pub fn said(&mut self, name: &str, turn: i64, from: &str, words: Option<&str>) {
        if !self.live || !self.task(name) {
            return;
        }
        // Only what a note quotes is kept.
        let line = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        let words = words.map(|words| line(&super::cut(&line(words), super::QUOTE_CHARS)));
        let said = Said {
            turn,
            from: from.to_owned(),
            words,
        };
        self.news_of(name).merge(News {
            said: vec![said],
            ..News::default()
        });
    }

    fn tell(
        &mut self,
        name: &str,
        turn: i64,
        status: &str,
        from: Option<String>,
        which: Option<String>,
        origin: Option<String>,
    ) {
        if !self.task(name) {
            return;
        }
        let handle = format!("turn:{name}/{turn}");
        if self.bots[&self.lead].waiting_on.contains(&handle) {
            return;
        }
        let by = match status {
            APPROVAL | QUIET => None,
            _ if from.as_deref() == Some(self.lead.as_str()) => Some("you".to_owned()),
            _ => Some(from.or(origin).unwrap_or_else(|| "the person".into())),
        };
        let news = self.news_of(name);
        let answered = news.act.as_ref().is_some_and(|asked| {
            asked.turn == turn && [APPROVAL, QUIET].contains(&asked.status.as_str())
        });
        let theirs = by.as_deref() == Some("the person") && !answered;
        let turn = Turn {
            first: turn,
            turn,
            status: status.to_owned(),
            by,
            count: 1,
            which,
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

    /// Turns a trigger did not pass on in time, approvals that waited, and
    /// turns silent since `QUIET_AFTER`, once per silence, as news.
    pub fn expire(&mut self, now: Instant) {
        let silent: Vec<(String, i64)> = (self.quiet.iter_mut())
            .filter(|(_, (_, at, told))| !*told && now >= *at + QUIET_AFTER)
            .map(|(task, (turn, _, told))| {
                *told = true;
                (task.clone(), *turn)
            })
            .collect();
        for (task, turn) in silent {
            self.silences += 1;
            let which = Some(format!("silence {}", self.silences));
            if self.live {
                self.tell(&task, turn, QUIET, None, which, None);
            }
        }
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
                    t.status != APPROVAL
                        && t.status != QUIET
                        && handles.contains(&format!("turn:{name}/{}", t.turn))
                }) {
                    *kind = None;
                }
            }
        }
        self.tasks.retain(|(_, news)| !news.empty());
        self.forwarding
            .retain(|(name, turn), _| !handles.contains(&format!("turn:{name}/{turn}")));
    }

    /// Whether there is news for the lead: its own, or `held` other news.
    pub fn news(&self, held: bool) -> bool {
        self.live && (held || !self.tasks.is_empty())
    }

    /// Whether the lead is to hear now: there is news and it rests.
    pub fn due(&self, held: bool) -> bool {
        self.news(held)
            && self
                .bots
                .get(&self.lead)
                .is_some_and(|lead| lead.id.is_some() && !ACTIVE.contains(&lead.status.as_str()))
    }

    /// Whether the lead is in the middle of a turn, as its events say.
    pub fn busy(&self) -> bool {
        (self.bots.get(&self.lead)).is_some_and(|lead| ACTIVE.contains(&lead.status.as_str()))
    }

    /// Whether to ask the daemon if the lead rests: news came `since` and
    /// has waited a while on a lead its events say is busy. A missed event
    /// would otherwise hold the note until the lead's next turn.
    pub fn doubt(&self, now: Instant, held: bool, since: Instant) -> bool {
        self.news(held)
            && now >= since + DOUBT
            && self.asked.is_none_or(|asked| now >= asked + DOUBT)
            && self
                .bots
                .get(&self.lead)
                .is_some_and(|lead| lead.id.is_some() && ACTIVE.contains(&lead.status.as_str()))
    }

    /// The lead as the daemon lists it now: if it rests there, it rests
    /// here. What the events said it was doing comes back, to log.
    pub fn rests(&mut self, now: Instant, record: &Value) -> Option<String> {
        self.asked = Some(now);
        let lead = self.bots.get_mut(&self.lead)?;
        if lead.id != record["bot_id"].as_i64()
            || !record["running_turn"].is_null()
            || !ACTIVE.contains(&lead.status.as_str())
        {
            return None;
        }
        lead.running = None;
        lead.waiting_on.clear();
        Some(std::mem::replace(&mut lead.status, "idle".into()))
    }

    /// The news for the next note, taken: every task's, those with news to
    /// act on first.
    pub fn take(&mut self) -> Told {
        let mut tasks = std::mem::take(&mut self.tasks);
        tasks.sort_by_key(|(_, news)| news.said.is_empty() && news.act.is_none());
        let rest = tasks.split_off(tasks.len().min(WAKE_TASKS));
        let mut quoted = HashMap::new();
        let mut items: Vec<Line> = Vec::new();
        let mut sent = Vec::new();
        // What does not fit in this note's bytes waits for the next.
        let mut later = Vec::new();
        let mut bytes = 0;
        for (name, mut news) in tasks {
            if bytes >= NOTE_BYTES {
                later.push((name, news));
                continue;
            }
            let (mut kept, mut wait) = (Vec::new(), Vec::new());
            for message in std::mem::take(&mut news.said) {
                let size = message.words.as_ref().map_or(0, String::len) + LINE_BYTES;
                let first = items.is_empty() && kept.is_empty();
                if wait.is_empty() && (first || bytes + size <= NOTE_BYTES) {
                    bytes += size;
                    kept.push(message);
                } else {
                    wait.push(message);
                }
            }
            news.said = kept;
            if !wait.is_empty() {
                let waiting = News {
                    said: wait,
                    ..News::default()
                };
                later.push((name.clone(), waiting));
            }
            // A message that started a turn told below as asked by its
            // writer says nothing more; the person's words are still quoted.
            let ended = |s: &&Said| {
                s.words.is_none() && news.act.as_ref().is_some_and(|t| t.turn == s.turn)
            };
            let left: Vec<Said> = news.said.iter().filter(|s| !ended(s)).cloned().collect();
            items.extend(said(&name, &left, &mut quoted));
            let turns = [(false, &news.act), (true, &news.theirs)];
            items.extend(
                (turns.into_iter())
                    .filter_map(|(theirs, t)| Some(line(&name, theirs, t.as_ref()?))),
            );
            bytes += 2 * LINE_BYTES;
            sent.push((name, news));
        }
        later.extend(rest);
        self.tasks = later;
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
            sent,
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
    /// Every bot seated, by name.
    pub fn listed(&self) -> Vec<String> {
        self.bots.keys().cloned().collect()
    }

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
        self.quiet.remove(name);
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
    let which = t.which.as_deref().unwrap_or("");
    Line {
        handle: format!("turn:{name}/{}", t.turn),
        said: false,
        theirs,
        text,
        key: format!("{name}/{}/{}/{theirs}/{which}", t.turn, t.status),
    }
}

/// What was written in a task, as the note says it: each of the person's
/// messages quoted, unless the note quoted the same words for another task
/// already, and everyone else's in one line by its latest turn.
fn said(name: &str, said: &[Said], quoted: &mut HashMap<String, String>) -> Vec<Line> {
    let mut lines = Vec::new();
    let line = |turn: i64, text: String| Line {
        handle: format!("turn:{name}/{turn}"),
        said: true,
        theirs: false,
        text,
        key: format!("{name}/{turn}/said"),
    };
    for message in said {
        let Some(words) = &message.words else {
            continue;
        };
        // Kept as told: one line, cut at the quote's length.
        let handle = format!("turn:{name}/{}", message.turn);
        let text = match quoted.get(words) {
            Some(first) => format!("- {name}: {handle}, the person wrote the same as in {first}"),
            None => format!("- {name}: {handle}, the person wrote: \"{words}\""),
        };
        quoted.entry(words.clone()).or_insert(handle);
        lines.push(line(message.turn, text));
    }
    let others: Vec<&Said> = said.iter().filter(|s| s.words.is_none()).collect();
    if let Some(last) = others.last() {
        let mut from: Vec<&str> = others.iter().map(|s| s.from.as_str()).collect();
        from.sort();
        from.dedup();
        let text = match others.len() {
            1 => format!(
                "- {name}: turn:{name}/{}, {} wrote in it",
                last.turn, last.from
            ),
            n => format!(
                "- {name}: turn:{name}/{}, {n} messages in it since turn:{name}/{}, from {}",
                last.turn,
                others[0].turn,
                from.join(", ")
            ),
        };
        lines.push(line(last.turn, text));
    }
    lines
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
        // The lead's own tasks, as its subscription names them.
        updates.watched = ["shop.fix", "shop.docs"].map(str::to_owned).into();
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
        assert!(!updates.due(false), "the lead is working");
        updates.event(
            &ev(
                "shop.lead",
                Some(9),
                "turn_finished",
                json!({"status": "completed"}),
            ),
            now,
        );
        assert!(updates.due(false));
        let told = updates.take();
        assert_eq!(
            text(&told),
            "- shop.fix: turn:shop.fix/5 failed, asked by you, and 1 earlier since turn:shop.fix/4\n\
             - shop.docs: turn:shop.docs/2 completed, asked by the person"
        );
        let theirs: Vec<bool> = told.items.iter().map(|i| i.theirs).collect();
        assert_eq!(theirs, [false, true]);
        assert_eq!(told.items[0].handle, "turn:shop.fix/5");
        updates.sent(told);
        // The next news is due at once: no gap after a note.
        turn(
            &mut updates,
            "shop.docs",
            3,
            json!({"from": {"bot": "shop.lead", "turn": 10}}),
            "completed",
        );
        assert!(updates.due(false));
    }

    #[test]
    fn a_message_that_started_a_turn_told_as_ended_is_not_told_twice() {
        let mut updates = fleet();
        let from = json!({"from": {"bot": "shop.docs", "turn": 3}});
        updates.said("shop.fix", 6, "shop.docs", None);
        turn(&mut updates, "shop.fix", 6, from.clone(), "completed");
        updates.said("shop.fix", 7, "shop.docs", None);
        updates.said("shop.fix", 8, "the person", Some("and tabs"));
        let now = Instant::now();
        updates.event(&ev("shop.fix", Some(7), "accepted", from), now);
        assert_eq!(
            text(&updates.take()),
            "- shop.fix: turn:shop.fix/8, the person wrote: \"and tabs\"\n\
             - shop.fix: turn:shop.fix/7, shop.docs wrote in it\n\
             - shop.fix: turn:shop.fix/6 completed, asked by shop.docs"
        );
    }

    #[test]
    fn messages_past_one_note_s_bytes_wait_for_the_next() {
        let mut updates = fleet();
        let words = "x".repeat(super::super::QUOTE_CHARS);
        for turn in 1..=50 {
            updates.said(
                "shop.fix",
                turn,
                "the person",
                Some(&format!("{turn} {words}")),
            );
        }
        let first = updates.take();
        let told = first.items.len();
        assert!((20..50).contains(&told), "{told}");
        assert!(first.items.iter().map(|i| i.text.len()).sum::<usize>() <= NOTE_BYTES);
        assert_eq!(first.more, 1);
        updates.sent(first);
        let second = updates.take();
        let handles: Vec<&str> = second.items.iter().map(|i| i.handle.as_str()).collect();
        assert_eq!(handles.len(), 50 - told);
        assert_eq!(handles[0], format!("turn:shop.fix/{}", told + 1));
    }

    #[test]
    fn a_long_backlog_keeps_only_the_newest_words() {
        let mut updates = fleet();
        let long = "y".repeat(10 * super::super::QUOTE_CHARS);
        for turn in 1..=300 {
            updates.said("shop.fix", turn, "the person", Some(&long));
        }
        let said = &updates.tasks[0].1.said;
        assert_eq!(said.len(), 2 * PENDING);
        assert_eq!(said[0].turn, 300 - 2 * PENDING as i64 + 1);
        let quoted = said.iter().filter(|s| s.words.is_some()).count();
        assert_eq!(quoted, PENDING);
        let kept = said.last().and_then(|s| s.words.as_ref()).unwrap();
        assert!(kept.len() < 2 * super::super::QUOTE_CHARS);
    }

    #[test]
    fn a_lead_the_events_left_busy_rests_once_the_daemon_lists_it_idle() {
        let mut updates = fleet();
        let now = Instant::now();
        // Its turn started, and the event that ended it never came.
        updates.event(&ev("shop.lead", Some(11), "accepted", json!({})), now);
        turn(
            &mut updates,
            "shop.fix",
            6,
            json!({"from": {"bot": "shop.lead", "turn": 10}}),
            "completed",
        );
        assert!(!updates.due(false));
        // Not until the news has waited a while.
        assert!(!updates.doubt(now, false, now));
        let later = now + DOUBT;
        assert!(updates.doubt(later, false, now));
        // Still running there: still busy here, and not asked again soon.
        let running =
            json!({"name": "shop.lead", "bot_id": 1, "status": "running", "running_turn": 11});
        assert_eq!(updates.rests(later, &running), None);
        assert!(!updates.doubt(later + Duration::from_secs(30), false, now));
        assert!(!updates.due(false));
        // Another bot under the name says nothing of this one.
        let other =
            json!({"name": "shop.lead", "bot_id": 8, "status": "idle", "running_turn": null});
        assert_eq!(updates.rests(later + DOUBT, &other), None);
        let idle =
            json!({"name": "shop.lead", "bot_id": 1, "status": "idle", "running_turn": null});
        assert_eq!(
            updates.rests(later + DOUBT, &idle).as_deref(),
            Some("running")
        );
        assert!(updates.due(false));
        assert!(!updates.doubt(later + DOUBT * 2, false, now));
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
        assert!(!updates.due(false), "the lead read shop.fix with its wait");
        updates.expire(now + APPROVAL_AFTER);
        let told = updates.take();
        assert_eq!(
            text(&told),
            "- shop.docs: turn:shop.docs/2 waiting for the person's approval"
        );
        // Refused: the news comes back for next time.
        updates.unsent(told);
        let again = updates.take();
        assert!(text(&again).contains("shop.docs/2 waiting for the person's approval"));
    }

    #[test]
    fn a_turn_silent_for_a_long_while_is_news_once_per_silence() {
        let mut updates = fleet();
        let now = Instant::now();
        let silent = |updates: &mut Updates, at: Instant| {
            updates.expire(at);
            let told = updates.take();
            text(&told)
        };
        updates.event(
            &ev(
                "shop.fix",
                Some(3),
                "accepted",
                json!({"from": {"bot": "shop.lead"}}),
            ),
            now,
        );
        assert_eq!(silent(&mut updates, now), "");
        let later = now + QUIET_AFTER;
        assert_eq!(
            silent(&mut updates, later),
            "- shop.fix: turn:shop.fix/3 has printed nothing for 30 minutes"
        );
        assert_eq!(silent(&mut updates, later + DOUBT), "");
        // A wait that returned before the turn ended read no silence.
        updates.event(&ev("shop.fix", Some(3), "text_delta", json!({})), later);
        updates.expire(later + QUIET_AFTER);
        updates.read(&["turn:shop.fix/3".to_owned()].into());
        let told = updates.take();
        assert_eq!(told.items.len(), 1);
        // Printing again starts a new silence, which is news of its own.
        assert_ne!(told.items[0].key, "");
        // A turn waiting on the person or on other turns is not silent,
        // and one that ended is gone.
        let at = later + 2 * QUIET_AFTER;
        updates.event(
            &ev("shop.fix", Some(3), "turn_waiting", json!({"handles": []})),
            at,
        );
        assert_eq!(silent(&mut updates, at + QUIET_AFTER), "");
        updates.event(&ev("shop.fix", Some(3), "turn_resumed", json!({})), at);
        updates.event(
            &ev(
                "shop.fix",
                Some(3),
                "turn_finished",
                json!({"status": "completed"}),
            ),
            at,
        );
        updates.take();
        assert_eq!(silent(&mut updates, at + 4 * QUIET_AFTER), "");
        // A turn already running when its bot came to be watched is
        // tracked from what it does next.
        updates.event(&ev("shop.fix", Some(8), "tool_started", json!({})), at);
        assert!(
            silent(&mut updates, at + 4 * QUIET_AFTER)
                .contains("turn:shop.fix/8 has printed nothing")
        );
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
        assert!(updates.take().items.is_empty());
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
        assert!(updates.due(false));
        updates.read(&["turn:shop.fix/5".to_owned()].into());
        assert!(!updates.due(false));
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
            turn(
                updates,
                "shop.fix",
                4,
                json!({"from": {"bot": "shop.lead", "turn": 9}}),
                "completed",
            );
            let told = updates.take();
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
            assert!(!updates.due(false), "{gone}");
            assert!(updates.take().sent.is_empty());
        }
    }

    #[test]
    fn a_new_task_under_the_name_does_not_inherit_the_last_ones_news() {
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
        assert!(!updates.due(false));
        assert!(updates.take().sent.is_empty());
    }

    #[test]
    fn a_task_deleted_while_the_lookout_was_away_has_no_news() {
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
        assert!(!updates.due(false));
        assert!(updates.take().sent.is_empty());
    }
}
