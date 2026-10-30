//! `/sky`: every agent the daemon keeps, as a sky of stars, lit up live as
//! they talk. Leads sit on the inner ring and the agents each one made fan
//! out behind it, so a project reads as one arm of the sky; the person is
//! the middle. A message travels as a spark along its curve (the person's
//! from the middle, another agent's across, a lookout's or automation's in
//! from the rim), a commons post ripples out, a tool call twinkles, and
//! whoever is lit fades like a firefly. Each agent's state is the state
//! table's, mark and colour: working cyan `●`, needs you yellow `?`,
//! failed red `✗`. Where the sky has no room, under about 60 columns or
//! 24 rows, the same agents are a list, each with a firefly of its own.
//!
//! It reads only what the daemon already tells any client: one page of
//! `bots`, then `follow` of `*`. The follow starts from the first cursor
//! for the senders of past turns alone (who talked to whom, small events),
//! then once live asks again for what animates the sky. Its connection is
//! its own and closes with the view.
use crate::project;
use agent_client::Client;
use ratatui::{
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::f32::consts::{PI, TAU};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

/// Past turns' events read for who talked to whom.
const PAST: &[&str] = &["accepted", "steered", "turn_finished"];
/// Live events: who is made, who talks, works, calls and posts.
const LIVE: &[&str] = &[
    "created",
    "forked",
    "deleted",
    "queued",
    "accepted",
    "steered",
    "tool_started",
    "tool_completed",
    "approval_requested",
    "approval_asked",
    "step_decided",
    "turn_resumed",
    "turn_finished",
];
/// How long a spark takes to cross.
const FLIGHT: Duration = Duration::from_millis(1100);
/// How long a commons post's ripple spreads.
const RIPPLE: Duration = Duration::from_millis(1400);
/// How long a tool call twinkles.
const TWINKLE: Duration = Duration::from_millis(350);
/// How fast a lit agent fades: its glow halves this often.
const HALF_LIFE: f32 = 0.9;
/// Pairs that talked, the newest kept as faint curves.
const TALKS: usize = 96;
/// Queued prompts remembered so their start does not spark again.
const SENT: usize = 1024;
/// Lines of what happened under the sky.
const TICKER: usize = 5;
/// The least room the sky is drawn in; with less, a list.
const SKY_COLS: usize = 60;
const SKY_ROWS: usize = 24;

/// Agent to agent.
const TALK: Color = Color::Magenta;
/// A tool call: code's colour.
const TOOL: Color = Color::Cyan;
/// The state table's: working, needs you.
const WORKING: Color = Color::Cyan;
const NEEDS: Color = Color::Yellow;
/// A commons post.
const BOARD: Color = Color::Green;
/// A failed call.
const FAILED: Color = Color::Red;

/// What the follow brings the view.
enum Feed {
    /// Every agent there is now: who is listed is here, who isn't is gone.
    Roll(Vec<Value>),
    Past(Value),
    /// The calls waiting for the person when the past was read, each with
    /// its agent: what is answered after, live events let go.
    Held(Vec<(String, String)>),
    Live(Value),
    /// Caught up: from here on, what happens.
    Ready,
    Down(String),
}

/// Where a spark starts or lands.
#[derive(Clone, Copy, PartialEq, Debug)]
enum End {
    Person,
    Agent(usize),
    /// The rim, at this agent's angle: a lookout, an automation.
    Rim(usize),
}

/// What a tool call does, which its twinkle's shape tells: a shell's
/// streak across, a file's up and down, the web's corners, any other
/// tool's star.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Work {
    Shell,
    Files,
    Web,
    Other,
}

impl Work {
    fn of(tool: &str) -> Work {
        match tool {
            "shell" | "exec" | "write_stdin" => Work::Shell,
            "read" | "write" | "edit" | "apply_patch" => Work::Files,
            "web_search" | "web_fetch" | "fetch" => Work::Web,
            _ => Work::Other,
        }
    }

    /// Dots around the star, in dot steps.
    fn shape(self) -> &'static [(f32, f32)] {
        match self {
            Work::Shell => &[(-3.0, 0.0), (-2.0, 0.0), (2.0, 0.0), (3.0, 0.0)],
            Work::Files => &[(0.0, -3.0), (0.0, -2.0), (0.0, 2.0), (0.0, 3.0)],
            Work::Web => &[(-2.0, -2.0), (2.0, -2.0), (-2.0, 2.0), (2.0, 2.0)],
            Work::Other => &[
                (0.0, -3.0),
                (0.0, 3.0),
                (-3.0, 0.0),
                (3.0, 0.0),
                (-2.0, -2.0),
                (2.0, -2.0),
                (-2.0, 2.0),
                (2.0, 2.0),
            ],
        }
    }
}

/// Where an agent is, in the state table's words.
#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    NeedsYou,
    Working,
    Failed,
}

impl State {
    fn mark(self) -> (&'static str, Color, &'static str) {
        match self {
            State::NeedsYou => ("?", NEEDS, "needs you"),
            State::Working => ("●", WORKING, "working"),
            State::Failed => ("✗", FAILED, "failed"),
        }
    }
}

struct Spark {
    from: End,
    to: End,
    born: Instant,
    color: Option<Color>,
}

#[derive(Default)]
struct Agent {
    name: String,
    /// The daemon's id for it, so a name used again is someone else.
    id: Option<i64>,
    made_by: Option<String>,
    made_by_id: Option<i64>,
    /// The turn it runs, when it runs one.
    running: Option<i64>,
    /// When it was last lit, and in what colour (none: the person's).
    lit: Option<(Instant, Option<Color>)>,
    twinkle: Option<(Instant, Color, Work)>,
    ripple: Option<Instant>,
    /// It asked the person something, not yet answered.
    asking: bool,
    /// Its calls that wait for the person's approval.
    held: HashSet<String>,
    /// Its last turn failed.
    failed: bool,
    /// The last it said or was told, for the list.
    last: Option<(Instant, String)>,
    /// Who sent its running turn's prompt.
    sender: Option<End>,
    gone: bool,
    // Laid out:
    at: (f32, f32),
    angle: f32,
    root: bool,
}

impl Agent {
    fn state(&self) -> Option<State> {
        if self.asking || !self.held.is_empty() {
            Some(State::NeedsYou)
        } else if self.running.is_some() {
            Some(State::Working)
        } else if self.failed {
            Some(State::Failed)
        } else {
            None
        }
    }

    /// How lit it is, 1 just lit to 0.
    fn glow(&self, now: Instant) -> f32 {
        self.lit.map_or(0.0, |(at, _)| {
            0.5f32.powf(now.duration_since(at).as_secs_f32() / HALF_LIFE)
        })
    }
}

pub struct View {
    /// The daemon's socket, to follow it again after a pause.
    socket: PathBuf,
    feed: mpsc::UnboundedReceiver<Feed>,
    task: tokio::task::JoinHandle<()>,
    agents: Vec<Agent>,
    index: HashMap<String, usize>,
    sparks: Vec<Spark>,
    /// Pairs that talked, newest last.
    talks: VecDeque<(usize, usize)>,
    /// Turns already sparked, so a queued prompt's start is not a second.
    sent: HashSet<(String, i64)>,
    ticker: VecDeque<(Instant, String)>,
    /// When messages were sent in the last minute.
    recent: VecDeque<Instant>,
    laid: bool,
    /// The agents in the tree's order, each with its depth, for the list.
    order: Vec<(usize, usize)>,
    live: bool,
    paused: bool,
    down: Option<String>,
}

impl Drop for View {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl View {
    pub fn open(socket: PathBuf) -> Self {
        let (send, feed) = mpsc::unbounded_channel();
        let task = tokio::spawn(follow(socket.clone(), send));
        Self::with(socket, feed, task)
    }

    fn with(
        socket: PathBuf,
        feed: mpsc::UnboundedReceiver<Feed>,
        task: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            socket,
            feed,
            task,
            agents: Vec::new(),
            index: HashMap::new(),
            sparks: Vec::new(),
            talks: VecDeque::new(),
            sent: HashSet::new(),
            ticker: VecDeque::new(),
            recent: VecDeque::new(),
            laid: false,
            order: Vec::new(),
            live: false,
            paused: false,
            down: None,
        }
    }

    /// Stop following while the view is put aside, so nothing piles up
    /// unread.
    pub fn pause(&mut self) {
        self.paused = true;
        self.task.abort();
    }

    /// Follow again after a pause, from a fresh listing.
    pub fn resume(&mut self) {
        if !self.task.is_finished() && !self.paused {
            return;
        }
        self.paused = false;
        let (send, feed) = mpsc::unbounded_channel();
        self.feed = feed;
        self.task = tokio::spawn(follow(self.socket.clone(), send));
        // Read afresh: agents removed meanwhile go, and who talked to whom
        // is read again from the turns kept.
        self.agents.clear();
        self.index.clear();
        self.talks.clear();
        self.sent.clear();
        self.laid = false;
        self.sparks.clear();
        self.live = false;
        self.down = None;
    }

    /// Whether the follow ended, as when the daemon went: `resume`
    /// follows again.
    pub fn down(&self) -> bool {
        self.down.is_some()
    }

    /// Take what the follow brought, and let what landed light up.
    pub fn drain(&mut self) {
        let now = Instant::now();
        while let Ok(feed) = self.feed.try_recv() {
            match feed {
                Feed::Roll(bots) => {
                    let listed: HashSet<&str> =
                        bots.iter().filter_map(|bot| bot["name"].as_str()).collect();
                    for agent in &mut self.agents {
                        agent.gone = !listed.contains(agent.name.as_str());
                    }
                    for bot in &bots {
                        self.seat(bot);
                    }
                }
                Feed::Past(event) => self.event(&event, now, false),
                Feed::Held(calls) => {
                    for (bot, call) in calls {
                        if let Some(at) = self.agent(&bot) {
                            self.agents[at].held.insert(call);
                        }
                    }
                }
                Feed::Live(event) => {
                    self.live = true;
                    self.event(&event, now, true);
                }
                Feed::Ready => self.live = true,
                Feed::Down(why) => self.down = Some(why),
            }
        }
        self.sweep();
        self.land(now);
    }

    /// Let removed agents go for good, and whatever pointed at them.
    fn sweep(&mut self) {
        if !self.agents.iter().any(|agent| agent.gone) {
            return;
        }
        let mut moved = vec![None; self.agents.len()];
        let mut kept = 0;
        for (at, agent) in self.agents.iter().enumerate() {
            if !agent.gone {
                moved[at] = Some(kept);
                kept += 1;
            }
        }
        let end = |end: End| match end {
            End::Person => Some(End::Person),
            End::Agent(at) => moved[at].map(End::Agent),
            End::Rim(at) => moved[at].map(End::Rim),
        };
        self.agents.retain(|agent| !agent.gone);
        for agent in &mut self.agents {
            agent.sender = agent.sender.and_then(end);
        }
        self.sparks
            .retain_mut(|spark| match (end(spark.from), end(spark.to)) {
                (Some(from), Some(to)) => {
                    (spark.from, spark.to) = (from, to);
                    true
                }
                _ => false,
            });
        self.talks = (self.talks.iter())
            .filter_map(|&(a, b)| Some((moved[a]?, moved[b]?)))
            .collect();
        self.index = (self.agents.iter().enumerate())
            .map(|(at, agent)| (agent.name.clone(), at))
            .collect();
        self.laid = false;
    }

    fn land(&mut self, now: Instant) {
        let mut landed = Vec::new();
        self.sparks.retain(|spark| {
            let done = now.duration_since(spark.born) >= FLIGHT;
            if done {
                landed.push((spark.to, spark.color));
            }
            !done
        });
        for (to, color) in landed {
            if let End::Agent(at) = to {
                self.agents[at].lit = Some((now, color));
            }
        }
        while self
            .recent
            .front()
            .is_some_and(|at| now.duration_since(*at) > Duration::from_secs(60))
        {
            self.recent.pop_front();
        }
    }

    fn seat(&mut self, bot: &Value) -> Option<usize> {
        let name = bot["name"].as_str()?;
        // A removed agent's name, or one now someone else's: let the old
        // one go with what pointed at it, and seat this one afresh.
        if let Some(&at) = self.index.get(name) {
            let (was, now) = (self.agents[at].id, bot["bot_id"].as_i64());
            if self.agents[at].gone || was.is_some() && now.is_some() && was != now {
                self.agents[at].gone = true;
                self.sweep();
            }
        }
        let at = match self.index.get(name) {
            Some(&at) => at,
            None => {
                self.agents.push(Agent {
                    name: name.to_owned(),
                    ..Agent::default()
                });
                self.index.insert(name.to_owned(), self.agents.len() - 1);
                self.laid = false;
                self.agents.len() - 1
            }
        };
        let agent = &mut self.agents[at];
        agent.gone = false;
        agent.id = bot["bot_id"].as_i64();
        agent.made_by = (bot["created_by"].as_str())
            .filter(|by| !by.is_empty())
            .map(str::to_owned);
        agent.made_by_id = bot["created_by_id"].as_i64();
        agent.running = bot["running_turn"].as_i64();
        Some(at)
    }

    /// Who made the agent at `at`, when that one is still here: by name,
    /// and by id when both are known, as a name used again is a stranger.
    fn maker(&self, at: usize) -> Option<usize> {
        let agent = &self.agents[at];
        let by = self.agent(agent.made_by.as_deref()?)?;
        match (agent.made_by_id, self.agents[by].id) {
            (Some(made_by), Some(id)) if made_by != id => None,
            _ => Some(by),
        }
    }

    fn agent(&self, name: &str) -> Option<usize> {
        self.index
            .get(name)
            .copied()
            .filter(|&at| !self.agents[at].gone)
    }

    /// One of the daemon's events: `{cursor, bot, turn, event, data}`,
    /// what is particular to its kind in `data`.
    fn event(&mut self, event: &Value, now: Instant, live: bool) {
        let Some(name) = event["bot"].as_str() else {
            return;
        };
        let data = &event["data"];
        match event["event"].as_str().unwrap_or("") {
            "created" | "forked" => {
                let record = json!({"name": name, "bot_id": data["bot_id"],
                    "created_by": data["created_by"], "created_by_id": data["created_by_id"],
                    "running_turn": data["running_turn"]});
                if let Some(at) = self.seat(&record) {
                    self.agents[at].lit = Some((now, None));
                }
                return;
            }
            "deleted" => {
                if let Some(at) = self.agent(name) {
                    self.agents[at].gone = true;
                    self.laid = false;
                }
                return;
            }
            _ => {}
        }
        let Some(at) = self.agent(name) else {
            return;
        };
        let turn = event["turn"].as_i64().unwrap_or(-1);
        match event["event"].as_str().unwrap_or("") {
            kind @ ("queued" | "accepted" | "steered") => {
                let sent_turn = match kind {
                    "steered" => data["steer"].as_i64().unwrap_or(turn),
                    _ => turn,
                };
                let from = self.sender(data, at);
                // Past turns say who talked to whom; whether an agent works
                // now is the listing's, and live events' after it.
                if kind == "accepted" && live {
                    let agent = &mut self.agents[at];
                    agent.running = Some(turn);
                    agent.sender = Some(from);
                    // A new turn tries again what failed.
                    agent.failed = false;
                }
                // What it asked is answered by the one it asked: the person,
                // or the agent that made it.
                if kind != "queued" && live {
                    let maker = self.agents[at]
                        .made_by
                        .as_deref()
                        .and_then(|by| self.agent(by));
                    if from == End::Person || matches!(from, End::Agent(by) if Some(by) == maker) {
                        self.agents[at].asking = false;
                    }
                }
                // A queued prompt sparks when sent; its start, or the steer
                // taking it, is the same message and ends the watch for it.
                let key = (name.to_owned(), sent_turn);
                if live {
                    let again = match kind {
                        "queued" => !self.sent.insert(key),
                        _ => self.sent.remove(&key),
                    };
                    if again {
                        return;
                    }
                    // Prompts queued and dropped unstarted are let go.
                    if self.sent.len() > SENT {
                        self.sent.clear();
                    }
                }
                if let End::Agent(by) = from
                    && by != at
                {
                    self.talks.retain(|pair| *pair != (by, at));
                    self.talks.push_back((by, at));
                    if self.talks.len() > TALKS {
                        self.talks.pop_front();
                    }
                }
                if live {
                    self.send(from, End::Agent(at), now);
                    let by = match from {
                        End::Person => "you".to_owned(),
                        End::Agent(by) => self.short(by),
                        End::Rim(_) => {
                            crate::transcript::from_outside(data["origin"].as_str()).to_owned()
                        }
                    };
                    let to = self.short(at);
                    self.agents[at].last = Some((now, format!("from {by}")));
                    if let End::Agent(by) = from {
                        self.agents[by].last = Some((now, format!("to {to}")));
                    }
                    self.say(now, format!("{by} → {to}"));
                }
            }
            "tool_started" if live => {
                let tool = data["name"].as_str().unwrap_or("");
                // A call approved starts.
                if let Some(call) = data["call_id"].as_str() {
                    self.agents[at].held.remove(call);
                }
                self.agents[at].twinkle = Some((now, TOOL, Work::of(tool)));
                if tool == crate::asks::TOOL {
                    self.agents[at].asking = true;
                }
                if tool == "commons" && posted(&data["arguments"]) {
                    self.agents[at].ripple = Some(now);
                    self.agents[at].last = Some((now, "posted to the commons".into()));
                    let said = format!("{} posted to the commons", self.short(at));
                    self.say(now, said);
                }
            }
            // A call waits for the person: one gated for them, or one the
            // reviewer handed them.
            "approval_requested" if live => {
                for call in data["calls"].as_array().into_iter().flatten() {
                    let manual = (call["gates"].as_array())
                        .is_some_and(|gates| gates.iter().any(|g| g == "manual"));
                    if let (true, Some(id)) = (manual, call["call_id"].as_str()) {
                        self.agents[at].held.insert(id.to_owned());
                    }
                }
            }
            "approval_asked" if live => {
                if let Some(id) = data["call_id"].as_str() {
                    self.agents[at].held.insert(id.to_owned());
                }
            }
            // Each answer lets go of its call alone.
            "step_decided" | "turn_resumed" if live => {
                if let Some(id) = data["call_id"].as_str() {
                    self.agents[at].held.remove(id);
                }
            }
            "tool_completed" if live => {
                if let Some(id) = data["call_id"].as_str() {
                    self.agents[at].held.remove(id);
                }
                if data["failed"] == true || data["denied"] == true {
                    let work = Work::of(data["name"].as_str().unwrap_or(""));
                    self.agents[at].twinkle = Some((now, FAILED, work));
                }
            }
            // A steer the running turn absorbed finishes as a turn of its
            // own: only the running turn's end means the agent stopped.
            "turn_finished"
                if self.agents[at]
                    .running
                    .is_none_or(|running| running == turn) =>
            {
                let agent = &mut self.agents[at];
                agent.running = None;
                agent.held.clear();
                // Its last turn's end, past or live, says whether it failed;
                // one that ended with nothing to say ended as it should.
                agent.failed = data["status"] == "failed" && data["error"] != "empty_completion";
                if live && agent.failed {
                    agent.last = Some((now, "failed".into()));
                }
                // The answer to the person flies back to the middle.
                if live && agent.sender == Some(End::Person) && data["status"] == "completed" {
                    agent.lit = Some((now, None));
                    self.sparks.push(Spark {
                        from: End::Agent(at),
                        to: End::Person,
                        born: now,
                        color: None,
                    });
                }
                self.agents[at].sender = None;
            }
            _ => {}
        }
    }

    /// Who sent a prompt: another agent, a client named by its origin
    /// (in from the rim), or the person.
    fn sender(&self, data: &Value, at: usize) -> End {
        if let Some(by) = data["from"]["bot"].as_str() {
            // A name used again is someone else: from outside, then.
            let id = data["from"]["bot_id"].as_i64();
            return (self.agent(by))
                .filter(|&by| {
                    id.is_none() || self.agents[by].id.is_none_or(|known| Some(known) == id)
                })
                .map_or(End::Rim(at), End::Agent);
        }
        match data["origin"].as_str() {
            Some(_) => End::Rim(at),
            None => End::Person,
        }
    }

    fn send(&mut self, from: End, to: End, now: Instant) {
        let color = match from {
            End::Agent(_) => Some(TALK),
            End::Person => None,
            End::Rim(_) => Some(Color::Reset),
        };
        if let End::Agent(at) = from {
            self.agents[at].lit = Some((now, color));
        }
        self.sparks.push(Spark {
            from,
            to,
            born: now,
            color,
        });
        self.recent.push_back(now);
    }

    fn say(&mut self, now: Instant, text: String) {
        self.ticker.push_back((now, text));
        if self.ticker.len() > TICKER {
            self.ticker.pop_front();
        }
    }

    /// An agent's name as the sky labels it: a lead by its project, an
    /// agent of a project without the project's prefix.
    fn short(&self, at: usize) -> String {
        short(&self.agents[at].name)
    }

    /// Lay the agents out as a radial tree: each root's arm as wide as the
    /// agents under it, leads first, the made agents a ring further out
    /// for each generation.
    fn lay(&mut self) {
        if self.laid {
            return;
        }
        self.laid = true;
        self.order.clear();
        let count = self.agents.len();
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
        let mut roots = Vec::new();
        for at in 0..count {
            if self.agents[at].gone {
                continue;
            }
            let parent =
                (self.maker(at)).filter(|&by| by != at && !project::is_lead(&self.agents[at].name));
            match parent {
                Some(by) => children[by].push(at),
                None => roots.push(at),
            }
        }
        // A cycle of makers has no root: its agents are roots then.
        let mut placed = vec![false; count];
        let name = |at: &usize| self.agents[*at].name.clone();
        roots.sort_by_key(|at| (!project::is_lead(&self.agents[*at].name), name(at)));
        for kids in &mut children {
            kids.sort_by_key(name);
        }
        fn leaves(at: usize, children: &[Vec<usize>], seen: &mut Vec<bool>) -> usize {
            if seen[at] {
                return 0;
            }
            seen[at] = true;
            let below: usize = (children[at].iter())
                .map(|&kid| leaves(kid, children, seen))
                .sum();
            below.max(1)
        }
        let mut seen = vec![false; count];
        let weights: Vec<usize> = (roots.iter())
            .map(|&at| leaves(at, &children, &mut seen))
            .collect();
        for (at, agent) in self.agents.iter().enumerate() {
            if !seen[at] && !agent.gone {
                roots.push(at);
            }
        }
        let mut weights = weights;
        weights.resize(roots.len(), 1);
        let total: usize = weights.iter().sum::<usize>().max(1);
        let deep = !children.iter().all(Vec::is_empty);
        let mut from = -PI / 2.0;
        for (root, weight) in roots.iter().zip(weights) {
            let span = TAU * weight as f32 / total as f32;
            self.place(*root, from, span, 0, deep, &children, &mut placed);
            self.agents[*root].root = true;
            from += span;
        }
        // The outermost ring reaches the rim, whatever the generations.
        let far = (self.agents.iter())
            .filter(|a| !a.gone)
            .map(|a| a.at.0.hypot(a.at.1))
            .fold(0.0f32, f32::max);
        if far > 0.0 {
            let grow = 0.9 / far;
            for agent in &mut self.agents {
                agent.at = (agent.at.0 * grow, agent.at.1 * grow);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn place(
        &mut self,
        at: usize,
        from: f32,
        span: f32,
        depth: usize,
        deep: bool,
        children: &[Vec<usize>],
        placed: &mut Vec<bool>,
    ) {
        if placed[at] {
            return;
        }
        placed[at] = true;
        self.order.push((at, depth));
        let radius = match (deep, depth) {
            (false, _) => 0.62,
            (true, 0) => 0.4,
            (true, depth) => (0.66 + 0.14 * (depth - 1) as f32).min(0.95),
        };
        let angle = from + span / 2.0;
        let agent = &mut self.agents[at];
        agent.angle = angle;
        agent.at = (radius * angle.cos(), radius * angle.sin());
        agent.root = false;
        let kids = &children[at];
        if kids.is_empty() {
            return;
        }
        let weights: Vec<f32> = kids
            .iter()
            .map(|&kid| weight(kid, children) as f32)
            .collect();
        let total: f32 = weights.iter().sum();
        let mut start = from;
        for (kid, w) in kids.iter().zip(weights) {
            let part = span * w / total;
            self.place(*kid, start, part, depth + 1, deep, children, placed);
            start += part;
        }
    }

    /// The sky `cols` wide and `rows` high, what happened under it.
    pub fn card(&mut self, cols: usize, rows: usize) -> Vec<Line<'static>> {
        self.lay();
        let now = Instant::now();
        let cols = cols.max(20);
        let mut lines = Vec::new();
        let shown = self.agents.iter().filter(|a| !a.gone).count();
        if shown == 0 {
            lines.push(Line::from(match (&self.down, self.live) {
                (Some(why), _) => format!("Couldn't follow the agents: {why}"),
                _ => "No agents yet.".into(),
            }));
            return lines;
        }
        // The list's rows say what the ticker would, agent by agent.
        if cols < SKY_COLS || rows < SKY_ROWS {
            lines.extend(self.list(now, cols, rows.saturating_sub(2).max(3)));
            lines.extend(self.ticked(now, cols, false));
            return lines;
        }
        let sky_rows = rows.saturating_sub(TICKER + 3).clamp(8, cols / 2 + 1);
        let mut sky = Sky::new(cols, sky_rows);
        // The person in the middle.
        sky.text_at((0.0, 0.0), "you", Style::default().bold());
        // Who made whom: dotted lines; who talked: dotted curves. In the
        // text's own colour at full weight, as dim braille all but
        // vanishes in some themes; hues are kept for what happens now.
        for (at, agent) in self.agents.iter().enumerate().filter(|(_, a)| !a.gone) {
            if let Some(by) = self.maker(at)
                && !agent.root
            {
                sky.line(self.agents[by].at, agent.at, Ink::Plain, 3);
            }
        }
        for &(by, to) in &self.talks {
            if self.agents[by].gone || self.agents[to].gone {
                continue;
            }
            let (a, b) = (self.agents[by].at, self.agents[to].at);
            sky.curve(a, bend(a, b), b, Ink::Plain, 4);
        }
        // Ripples, then sparks, then the stars over them.
        for agent in self.agents.iter().filter(|a| !a.gone) {
            if let Some(at) = agent.ripple {
                let t = now.duration_since(at).as_secs_f32() / RIPPLE.as_secs_f32();
                if t < 1.0 {
                    sky.ring(agent.at, 0.04 + 0.26 * t, Ink::Color(BOARD, t < 0.6));
                }
            }
        }
        for spark in &self.sparks {
            let t = (now.duration_since(spark.born).as_secs_f32() / FLIGHT.as_secs_f32()).min(1.0);
            let (a, b) = (self.end(spark.from), self.end(spark.to));
            let control = match (spark.from, spark.to) {
                (End::Agent(_), End::Agent(_)) => bend(a, b),
                _ => ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0),
            };
            let ink = match spark.color {
                None | Some(Color::Reset) => Ink::Bright,
                Some(color) => Ink::Color(color, true),
            };
            for step in 0..6 {
                let back = t - step as f32 * 0.035;
                if back < 0.0 {
                    break;
                }
                let at = bezier(a, control, b, ease(back));
                match step {
                    0 => sky.blob(at, ink),
                    _ => sky.dot(at, if step < 3 { ink } else { ink.fade() }),
                }
            }
        }
        for agent in self.agents.iter().filter(|a| !a.gone) {
            let glow = agent.glow(now);
            let color = agent
                .lit
                .and_then(|(_, color)| color)
                .filter(|c| *c != Color::Reset);
            if glow > 0.08 {
                let ink = match color {
                    Some(color) => Ink::Color(color, glow > 0.4),
                    None if glow > 0.4 => Ink::Bright,
                    None => Ink::Plain,
                };
                sky.ring(agent.at, 0.015 + 0.045 * glow, ink);
            }
            if let Some((at, color, work)) = agent.twinkle
                && now.duration_since(at) < TWINKLE
            {
                sky.twinkle(agent.at, work, Ink::Color(color, true));
            }
            // The star in its state's colour, else the text's.
            match agent.state() {
                Some(state) => sky.blob(agent.at, Ink::Color(state.mark().1, true)),
                None => sky.blob(agent.at, Ink::Plain),
            }
        }
        // Names: leads always, others while working or lit, nearest the
        // middle first, never over each other.
        let mut named: Vec<usize> = (0..self.agents.len())
            .filter(|&at| {
                let agent = &self.agents[at];
                !agent.gone
                    && (agent.root
                        || agent.state().is_some()
                        || agent.lit.is_some_and(|(lit, _)| {
                            now.duration_since(lit) < Duration::from_secs(4)
                        }))
            })
            .collect();
        // What needs the person is named first, so it is never crowded out.
        named.sort_by_key(|&at| {
            let agent = &self.agents[at];
            (
                agent.state() != Some(State::NeedsYou),
                !agent.root,
                (agent.at.0.hypot(agent.at.1) * 100.0) as i32,
            )
        });
        for at in named {
            let agent = &self.agents[at];
            // The mark says what the colour does: `? ` for an agent that
            // needs you, `✗ ` for one that failed. Bold while it works.
            let mark = match agent.state() {
                Some(state @ (State::NeedsYou | State::Failed)) => {
                    let (mark, color, _) = state.mark();
                    Some((mark, Style::default().fg(color).bold()))
                }
                _ => None,
            };
            let style = match agent.state() {
                Some(State::Working | State::NeedsYou) => Style::default().bold(),
                Some(State::Failed) => Style::default(),
                None => Style::default().add_modifier(Modifier::DIM),
            };
            sky.label(agent.at, mark, &short(&agent.name), style);
        }
        lines.extend(sky.lines());
        lines.push(Line::default());
        lines.extend(self.ticked(now, cols, true));
        lines
    }

    /// The ticker's lines: what happened last, how long ago; or that the
    /// follow is lost, or nothing has happened yet.
    fn ticked(&self, now: Instant, cols: usize, ticker: bool) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for (at, text) in self.ticker.iter().filter(|_| ticker) {
            lines.push(Line::from(vec![
                Span::raw(format!("{:>4}  ", ago(now, *at))).dim(),
                Span::raw(cut(text, cols.saturating_sub(6))),
            ]));
        }
        if let Some(why) = &self.down {
            lines.push(Line::from(format!("Couldn't follow the agents: {why}")));
        } else if self.ticker.is_empty() {
            lines.push(if self.live {
                Line::from("Quiet. Messages light up here as they go.").dim()
            } else {
                Line::from("Catching up with who talked to whom…").dim()
            });
        }
        lines
    }

    fn end(&self, end: End) -> (f32, f32) {
        match end {
            End::Person => (0.0, 0.0),
            End::Agent(at) => self.agents[at].at,
            End::Rim(at) => {
                let angle = self.agents[at].angle;
                (1.05 * angle.cos(), 1.05 * angle.sin())
            }
        }
    }

    /// The agents as a list where the sky has no room: the tree's order,
    /// each row a firefly that lights as its agent talks and fades, its
    /// state's mark, and the last it said or was told. When they don't all
    /// fit, what needs the person, what failed, works or is lit come
    /// first, the leads after them.
    fn list(&self, now: Instant, cols: usize, rows: usize) -> Vec<Line<'static>> {
        let rank = |at: usize| {
            let agent = &self.agents[at];
            let lit = agent.glow(now) > 0.08;
            match agent.state() {
                Some(State::NeedsYou) => 0,
                Some(State::Failed) => 1,
                Some(State::Working) => 2,
                None if lit => 3,
                None if agent.root => 4,
                None => 5,
            }
        };
        let mut kept: Vec<usize> = self.order.iter().map(|&(at, _)| at).collect();
        let more = kept.len().saturating_sub(rows);
        if more > 0 {
            kept.sort_by_key(|&at| rank(at));
            kept.truncate(rows.saturating_sub(1));
        }
        let mut lines = Vec::new();
        for &(at, depth) in self.order.iter().filter(|(at, _)| kept.contains(at)) {
            let agent = &self.agents[at];
            let glow = agent.glow(now);
            let color = (agent.lit)
                .and_then(|(_, color)| color)
                .filter(|c| *c != Color::Reset);
            let firefly = match (glow, color) {
                (g, Some(color)) if g > 0.4 => Span::raw("✦").fg(color).bold(),
                (g, None) if g > 0.4 => Span::raw("✦").bold(),
                (g, Some(color)) if g > 0.08 => Span::raw("•").fg(color),
                (g, None) if g > 0.08 => Span::raw("•"),
                _ => Span::raw("·").dim(),
            };
            let mark = match agent.state() {
                Some(state) => {
                    let (mark, color, _) = state.mark();
                    Span::raw(mark).fg(color)
                }
                None => Span::raw(" "),
            };
            let name = short(&agent.name);
            let name = match agent.state() {
                Some(State::Working | State::NeedsYou) => Span::raw(name).bold(),
                Some(State::Failed) => Span::raw(name),
                None if glow > 0.08 => Span::raw(name),
                None => Span::raw(name).dim(),
            };
            let indent = "  ".repeat(depth.min(4));
            let used = 4 + indent.width() + name.content.width();
            let mut spans = vec![
                mark,
                Span::raw(" "),
                Span::raw(indent),
                firefly,
                Span::raw(" "),
                name,
            ];
            if let Some((when, said)) = &agent.last
                && cols > used + 12
            {
                let said = format!("{said} · {}", ago(now, *when));
                spans.push(Span::raw(format!("  {}", cut(&said, cols - used - 2))).dim());
            }
            lines.push(Line::from(spans));
        }
        if more > 0 {
            let hidden = self.order.len() - kept.len();
            lines.push(Line::from(format!("  … {hidden} more, quiet")).dim());
        }
        lines.push(Line::default());
        lines
    }

    /// Counts as the footer gives them, each mark with its word: everyone,
    /// what needs the person, who works, what failed, and how many
    /// messages a minute.
    pub fn footer(&self, width: usize) -> Line<'static> {
        let shown: Vec<&Agent> = self.agents.iter().filter(|a| !a.gone).collect();
        let count = shown.len();
        let mut spans = vec![
            Span::raw(format!(
                "  {count} {}",
                if count == 1 { "agent" } else { "agents" }
            ))
            .dim(),
        ];
        for state in [State::NeedsYou, State::Working, State::Failed] {
            let n = shown.iter().filter(|a| a.state() == Some(state)).count();
            if n > 0 || state == State::Working {
                let (mark, color, word) = state.mark();
                spans.push(Span::raw(" · ").dim());
                spans.push(Span::raw(mark).fg(color));
                spans.push(Span::raw(format!(" {n} {word}")).dim());
            }
        }
        let sent = self.recent.len();
        let sent = format!(
            " · {sent} {} in the last minute",
            if sent == 1 { "message" } else { "messages" }
        );
        // The states first: the messages' count goes where it won't fit.
        let used: usize = spans.iter().map(|span| span.content.width()).sum();
        if used + sent.width() <= width {
            spans.push(Span::raw(sent).dim());
        }
        Line::from(spans)
    }
}

/// How long ago, as the ticker says it.
fn ago(now: Instant, at: Instant) -> String {
    let s = now.duration_since(at).as_secs();
    match s {
        0..=59 => format!("{s}s"),
        _ => format!("{}m", s / 60),
    }
}

/// Words cut to `room` columns, an ellipsis saying so.
fn cut(text: &str, room: usize) -> String {
    if text.width() <= room {
        return text.to_owned();
    }
    let mut out = String::new();
    for ch in text.chars() {
        if out.width() + ch.to_string().width() + 1 > room {
            break;
        }
        out.push(ch);
    }
    out + "…"
}

fn weight(at: usize, children: &[Vec<usize>]) -> usize {
    fn go(at: usize, children: &[Vec<usize>], depth: usize) -> usize {
        if depth > children.len() {
            return 1;
        }
        let below: usize = (children[at].iter())
            .map(|&kid| go(kid, children, depth + 1))
            .sum();
        below.max(1)
    }
    go(at, children, 0)
}

/// Whether a `commons` call's arguments, a JSON preview that may be cut
/// short, post or reply.
fn posted(arguments: &Value) -> bool {
    let text = arguments.as_str().unwrap_or("");
    match serde_json::from_str::<Value>(text) {
        Ok(call) => call.get("post").is_some() || call.get("reply").is_some(),
        Err(_) => ["\"post\":", "\"reply\":"]
            .iter()
            .any(|key| text.contains(key)),
    }
}

/// An agent's label: `shop lead`, `shop lead billing`, or the name after
/// its project's prefix.
pub fn short(name: &str) -> String {
    if let Some(project) = project::lead_project(name) {
        return match name.strip_suffix(".lead") {
            Some(_) => format!("{project} lead"),
            None => format!("{project} lead {}", name.rsplit('.').next().unwrap_or(name)),
        };
    }
    match name.split_once('.') {
        Some((_, rest)) if !rest.is_empty() => rest.to_owned(),
        _ => name.to_owned(),
    }
}

/// The control point of a curve between two agents: pulled toward the
/// middle, as hierarchical edge bundling bends links through the tree.
fn bend(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    ((a.0 + b.0) * 0.2, (a.1 + b.1) * 0.2)
}

fn bezier(a: (f32, f32), c: (f32, f32), b: (f32, f32), t: f32) -> (f32, f32) {
    let u = 1.0 - t;
    (
        u * u * a.0 + 2.0 * u * t * c.0 + t * t * b.0,
        u * u * a.1 + 2.0 * u * t * c.1 + t * t * b.1,
    )
}

fn ease(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// What a dot is drawn with, weakest first: a later dot in the same cell
/// takes the cell's colour only when it is at least as strong.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Ink {
    Plain,
    Bright,
    Color(Color, bool),
}

impl Ink {
    fn rank(self) -> u8 {
        match self {
            Ink::Plain => 0,
            Ink::Color(_, false) => 1,
            Ink::Bright => 2,
            Ink::Color(_, true) => 3,
        }
    }
    fn fade(self) -> Ink {
        match self {
            Ink::Bright => Ink::Plain,
            Ink::Color(color, _) => Ink::Color(color, false),
            ink => ink,
        }
    }
    fn style(self) -> Style {
        match self {
            Ink::Plain => Style::default(),
            Ink::Bright => Style::default().bold(),
            Ink::Color(color, true) => Style::default().fg(color).bold(),
            Ink::Color(color, false) => Style::default().fg(color),
        }
    }
}

/// A braille canvas: each cell two dots wide and four high, so its dots
/// are square in a cell twice as high as wide.
struct Sky {
    cols: usize,
    rows: usize,
    dots: Vec<u8>,
    inks: Vec<Option<Ink>>,
    text: Vec<Option<(char, Style)>>,
}

impl Sky {
    fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols,
            rows,
            dots: vec![0; cols * rows],
            inks: vec![None; cols * rows],
            text: vec![None; cols * rows],
        }
    }

    /// The dot a point of the unit disc falls on.
    fn spot(&self, (x, y): (f32, f32)) -> (f32, f32) {
        let (w, h) = (self.cols as f32 * 2.0, self.rows as f32 * 4.0);
        let scale = (w.min(h) / 2.0 - 2.0).max(1.0);
        (w / 2.0 + x * scale, h / 2.0 + y * scale)
    }

    fn plot(&mut self, x: f32, y: f32, ink: Ink) {
        if x < 0.0 || y < 0.0 {
            return;
        }
        let (x, y) = (x as usize, y as usize);
        let (col, row) = (x / 2, y / 4);
        if col >= self.cols || row >= self.rows {
            return;
        }
        let bit = match (x % 2, y % 4) {
            (0, 0) => 0x01,
            (0, 1) => 0x02,
            (0, 2) => 0x04,
            (1, 0) => 0x08,
            (1, 1) => 0x10,
            (1, 2) => 0x20,
            (0, _) => 0x40,
            _ => 0x80,
        };
        let cell = row * self.cols + col;
        self.dots[cell] |= bit;
        if self.inks[cell].is_none_or(|was| ink.rank() >= was.rank()) {
            self.inks[cell] = Some(ink);
        }
    }

    fn dot(&mut self, at: (f32, f32), ink: Ink) {
        let (x, y) = self.spot(at);
        self.plot(x, y, ink);
    }

    fn blob(&mut self, at: (f32, f32), ink: Ink) {
        let (x, y) = self.spot(at);
        for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
            self.plot(x + dx - 0.5, y + dy - 0.5, ink);
        }
    }

    fn twinkle(&mut self, at: (f32, f32), work: Work, ink: Ink) {
        let (x, y) = self.spot(at);
        for (dx, dy) in work.shape() {
            self.plot(x + dx, y + dy, ink);
        }
    }

    fn ring(&mut self, at: (f32, f32), radius: f32, ink: Ink) {
        let (x, y) = self.spot(at);
        let (r, _) = self.spot((radius, 0.0));
        let r = r - self.spot((0.0, 0.0)).0;
        let steps = (r * 6.0).clamp(8.0, 160.0) as usize;
        for step in 0..steps {
            let a = TAU * step as f32 / steps as f32;
            self.plot(x + r * a.cos(), y + r * a.sin(), ink);
        }
    }

    /// Every `gap`th dot of a straight line.
    fn line(&mut self, a: (f32, f32), b: (f32, f32), ink: Ink, gap: usize) {
        let (ax, ay) = self.spot(a);
        let (bx, by) = self.spot(b);
        let steps = (bx - ax).abs().max((by - ay).abs()).ceil() as usize;
        for step in (0..=steps).step_by(gap.max(1)) {
            let t = step as f32 / steps.max(1) as f32;
            self.plot(ax + (bx - ax) * t, ay + (by - ay) * t, ink);
        }
    }

    fn curve(&mut self, a: (f32, f32), c: (f32, f32), b: (f32, f32), ink: Ink, gap: usize) {
        let (ax, ay) = self.spot(a);
        let (bx, by) = self.spot(b);
        let steps = ((bx - ax).abs() + (by - ay).abs()).ceil() as usize;
        for step in (0..=steps).step_by(gap.max(1)) {
            let t = step as f32 / steps.max(1) as f32;
            let p = self.spot(bezier(a, c, b, t));
            self.plot(p.0, p.1, ink);
        }
    }

    /// Words beside a point, a mark before them, right of it where they
    /// fit, else left; never over other words.
    fn label(&mut self, at: (f32, f32), mark: Option<(&str, Style)>, words: &str, style: Style) {
        let (x, y) = self.spot(at);
        let (col, row) = ((x / 2.0) as usize, (y / 4.0) as usize);
        let marked = match mark {
            Some((mark, _)) => format!("{mark} {words}"),
            None => words.to_owned(),
        };
        let words = marked.as_str();
        let width = words.width();
        if row >= self.rows || width + 2 > self.cols {
            return;
        }
        let right = (col + 2 + width <= self.cols).then_some(col + 2);
        let left = col.checked_sub(width + 1);
        let start = match at.0 >= 0.0 {
            true => right.or(left),
            false => left.or(right),
        };
        let Some(start) = start else {
            return;
        };
        if self.put(row, start, words, style)
            && let Some((mark, mark_style)) = mark
        {
            for (cell, ch) in (row * self.cols + start..).zip(mark.chars()) {
                self.text[cell] = Some((ch, mark_style));
            }
        }
    }

    /// Words centred on a point.
    fn text_at(&mut self, at: (f32, f32), words: &str, style: Style) {
        let (x, y) = self.spot(at);
        let (col, row) = ((x / 2.0) as usize, (y / 4.0) as usize);
        let start = col.saturating_sub(words.width() / 2);
        self.put(row, start, words, style);
    }

    /// Words put at a place in a row; false when they don't fit there.
    fn put(&mut self, row: usize, start: usize, words: &str, style: Style) -> bool {
        let cells = row * self.cols + start..row * self.cols + start + words.chars().count();
        if start + words.chars().count() > self.cols
            || self.text[cells.clone()].iter().any(Option::is_some)
        {
            return false;
        }
        for (cell, ch) in cells.zip(words.chars()) {
            self.text[cell] = Some((ch, style));
        }
        true
    }

    fn lines(&self) -> Vec<Line<'static>> {
        (0..self.rows)
            .map(|row| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                let mut run = String::new();
                let mut style = Style::default();
                for col in 0..self.cols {
                    let cell = row * self.cols + col;
                    let (ch, cell_style) = match self.text[cell] {
                        Some((ch, style)) => (ch, style),
                        None if self.dots[cell] == 0 => (' ', Style::default()),
                        None => (
                            char::from_u32(0x2800 + self.dots[cell] as u32).unwrap_or(' '),
                            self.inks[cell].map_or(Style::default(), Ink::style),
                        ),
                    };
                    if cell_style != style && !run.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut run), style));
                    }
                    style = cell_style;
                    run.push(ch);
                }
                if !run.is_empty() {
                    spans.push(Span::styled(run, style));
                }
                Line::from(spans)
            })
            .collect()
    }
}

/// How often `sonny sky` draws: smooth enough for a spark's flight.
const FRAME: Duration = Duration::from_millis(60);
/// How long `sonny sky` waits before following a daemon that went again.
const RETRY: Duration = Duration::from_secs(3);
/// What the keys do in `sonny sky`.
const KEYS: &str = "q quit";

/// `sonny sky`: the same sky, the whole terminal, on its own, to keep
/// beside the work as btop is kept: its title and the keys over it, the
/// counts under it. A daemon that goes is followed again once it is back.
/// Its follow runs on `runtime`; the drawing and the keys on this thread.
pub fn watch(runtime: &tokio::runtime::Runtime, socket: PathBuf) -> Result<(), String> {
    use ratatui::crossterm::{
        cursor, event, execute,
        terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
    };
    let _inside = runtime.enter();
    let mut view = View::open(socket);
    terminal::enable_raw_mode().map_err(|e| e.to_string())?;
    let _ = execute!(std::io::stdout(), EnterAlternateScreen, cursor::Hide);
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let drawn = ratatui::Terminal::new(backend)
        .map_err(|e| e.to_string())
        .and_then(|mut terminal| {
            let mut down_since: Option<Instant> = None;
            loop {
                view.drain();
                // Gone: tried again a little later, the sky drawn anew.
                match (view.down(), down_since) {
                    (false, _) => down_since = None,
                    (true, None) => down_since = Some(Instant::now()),
                    (true, Some(at)) if at.elapsed() >= RETRY => {
                        view.resume();
                        down_since = None;
                    }
                    (true, Some(_)) => {}
                }
                terminal
                    .draw(|frame| frame.render_widget(&mut view, frame.area()))
                    .map_err(|e| e.to_string())?;
                if !event::poll(FRAME).map_err(|e| e.to_string())? {
                    continue;
                }
                if let event::Event::Key(key) = event::read().map_err(|e| e.to_string())?
                    && key.kind != event::KeyEventKind::Release
                    && quits(&key)
                {
                    return Ok(());
                }
            }
        });
    let _ = execute!(std::io::stdout(), cursor::Show, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    drawn
}

/// q, Esc and Ctrl+C leave `sonny sky`.
fn quits(key: &ratatui::crossterm::event::KeyEvent) -> bool {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => true,
        KeyCode::Char('c') => key.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

/// The whole of `sonny sky`: a title row as the pane's, the sky a column
/// in from the edge, and the counts on the last row.
impl ratatui::widgets::Widget for &mut View {
    fn render(self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        if area.height < 3 || area.width < 4 {
            return;
        }
        let width = area.width as usize;
        let mut head = vec![Span::raw(" "), Span::raw("sky").bold()];
        let keys = crate::hint::spans(&format!("{KEYS} "));
        let used = 4 + KEYS.width() + 1;
        head.push(Span::raw(" ".repeat(width.saturating_sub(used))));
        head.extend(keys);
        buf.set_line(area.x, area.y, &Line::from(head), area.width);
        // The rows between the title's and the counts', a row apart from each.
        let rows = area.height.saturating_sub(4) as usize;
        for (at, line) in self
            .card(width.saturating_sub(2), rows)
            .into_iter()
            .take(rows)
            .enumerate()
        {
            buf.set_line(area.x + 1, area.y + 2 + at as u16, &line, area.width - 1);
        }
        let foot = Line::from(format!(" {}", self.footer())).dim();
        buf.set_line(area.x, area.bottom() - 1, &foot, area.width);
    }
}

/// The view's own connection: every agent, then who talked to whom in the
/// turns kept, then live. A daemon gone ends it, saying so.
async fn follow(socket: PathBuf, send: mpsc::UnboundedSender<Feed>) {
    let (client, mut events) = match Client::connect(&socket).await {
        Ok(connected) => connected,
        Err(error) => {
            let _ = send.send(Feed::Down(error.to_string()));
            return;
        }
    };
    if !roll(&client, &send).await {
        return;
    }
    if let Err(error) = client
        .request("follow", json!({"bot": "*", "after": 0, "kinds": PAST}))
        .await
    {
        let _ = send.send(Feed::Down(error.to_string()));
        return;
    }
    let mut cursor = 0;
    while let Some(event) = events.recv().await {
        if event["event"] == "follow_live" {
            cursor = event["cursor"].as_i64().unwrap_or(0);
            break;
        }
        if send.send(Feed::Past(event)).is_err() {
            return;
        }
    }
    // What waits for the person now; the live replay from the past's end
    // brings what changed since.
    match held(&client).await {
        Ok(calls) => {
            if send.send(Feed::Held(calls)).is_err() {
                return;
            }
        }
        Err(error) => {
            let _ = send.send(Feed::Down(error.to_string()));
            return;
        }
    }
    // Asking again replaces the follow: from where the past ended, what
    // animates the sky. Its own replay repeats nothing already sent.
    if let Err(error) = client
        .request(
            "follow",
            json!({"bot": "*", "after": cursor, "kinds": LIVE}),
        )
        .await
    {
        let _ = send.send(Feed::Down(error.to_string()));
        return;
    }
    // Until the new follow is live, the old one's last events may still be
    // arriving, ahead of the replay that brings them again in order: wait
    // for it, and pass each cursor once, in the store's order.
    let mut handoff = Some(BTreeMap::new());
    while let Some(event) = events.recv().await {
        match event["event"].as_str() {
            Some("follow_live") => {
                for (_, event) in handoff.take().into_iter().flatten() {
                    if send.send(Feed::Live(event)).is_err() {
                        return;
                    }
                }
                // Who is here and who works may have changed while the past
                // was read, removals among it, which no replay brings: the
                // roll again says so, now that what follows it is kept.
                if !roll(&client, &send).await || send.send(Feed::Ready).is_err() {
                    return;
                }
                continue;
            }
            Some("pruned") => continue,
            Some("lagged" | "follow_lagged") => {
                let _ = send.send(Feed::Down("fell behind the daemon".into()));
                return;
            }
            _ => {}
        }
        if let (Some(waiting), Some(at)) = (&mut handoff, event["cursor"].as_i64()) {
            waiting.insert(at, event);
            continue;
        }
        if send.send(Feed::Live(event)).is_err() {
            return;
        }
    }
    let _ = send.send(Feed::Down("the daemon closed the connection".into()));
}

/// Every agent the daemon keeps, page after page, as one roll; false when
/// the view is gone or the daemon couldn't say.
async fn roll(client: &Client, send: &mpsc::UnboundedSender<Feed>) -> bool {
    let mut bots = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = match client
            .request("bots", json!({"after": after, "limit": 256}))
            .await
        {
            Ok(page) => page,
            Err(error) => {
                let _ = send.send(Feed::Down(error.to_string()));
                return false;
            }
        };
        bots.extend(page["bots"].as_array().cloned().unwrap_or_default());
        match page["next_after"].as_str() {
            Some(next) => after = Some(next.to_owned()),
            None => return send.send(Feed::Roll(bots)).is_ok(),
        }
    }
}

/// The calls a person must answer, as the conversation reads them: every
/// one gated `manual`, and those the approver serving `auto` handed over.
/// Each tag's listing reads only the calls still waiting on it.
async fn held(client: &Client) -> agent_client::Result<Vec<(String, String)>> {
    let mut calls = Vec::new();
    for tag in ["manual", "auto"] {
        let mut after = 0;
        loop {
            let page = client
                .request(
                    "approvals",
                    json!({"tag": tag, "after": after, "limit": 256}),
                )
                .await?;
            for call in page["approvals"].as_array().into_iter().flatten() {
                if tag == "auto" && call["asked"].is_null() {
                    continue;
                }
                if let (Some(bot), Some(id)) = (call["bot"].as_str(), call["call_id"].as_str()) {
                    calls.push((bot.to_owned(), id.to_owned()));
                }
            }
            match page["next_after"].as_i64() {
                Some(next) => after = next,
                None => break,
            }
        }
    }
    Ok(calls)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> (View, mpsc::UnboundedSender<Feed>) {
        let (send, feed) = mpsc::unbounded_channel();
        let task = tokio::spawn(async {});
        (View::with(PathBuf::new(), feed, task), send)
    }

    fn text(lines: &[Line]) -> String {
        (lines.iter())
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn sonny_sky_draws_its_title_the_sky_and_the_counts() {
        use ratatui::widgets::Widget;
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![
            json!({"name": "shop.lead", "running_turn": 3}),
        ]))
        .unwrap();
        view.drain();
        let area = ratatui::layout::Rect::new(0, 0, 60, 30);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        (&mut view).render(area, &mut buf);
        let row = |y: u16| -> String {
            (0..area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        };
        assert!(row(0).starts_with(" sky") && row(0).ends_with("q quit"));
        assert!((2..29).any(|y| row(y).contains("shop lead")));
        assert_eq!(
            row(29),
            " 1 agent · 1 working · 0 messages in the last minute"
        );
        // q, Esc and Ctrl+C leave it; other keys do not.
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |code, mods| quits(&KeyEvent::new(code, mods));
        assert!(key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!key(KeyCode::Char('c'), KeyModifiers::NONE));
    }

    #[tokio::test]
    async fn a_lead_sits_inside_the_agents_it_made() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![
            json!({"name": "shop.lead"}),
            json!({"name": "shop.login", "created_by": "shop.lead"}),
            json!({"name": "shop.cart", "created_by": "shop.lead"}),
            json!({"name": "lone"}),
        ]))
        .unwrap();
        view.drain();
        view.lay();
        let radius = |name: &str| {
            let at = view.agents[view.index[name]].at;
            at.0.hypot(at.1)
        };
        assert!(radius("shop.lead") < radius("shop.login"));
        assert!(view.agents[view.index["shop.lead"]].root);
        assert!(view.agents[view.index["lone"]].root);
        assert!(!view.agents[view.index["shop.cart"]].root);
    }

    #[tokio::test]
    async fn a_message_between_agents_sparks_once_and_is_told() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![
            json!({"name": "shop.lead"}),
            json!({"name": "shop.login", "created_by": "shop.lead"}),
        ]))
        .unwrap();
        let from = json!({"bot": "shop.lead", "turn": 3});
        for kind in ["queued", "accepted"] {
            send.send(Feed::Live(
                json!({"cursor": 1, "event": kind, "bot": "shop.login", "turn": 2, "data": {"from": from}}),
            ))
            .unwrap();
        }
        send.send(Feed::Live(
            json!({"cursor": 3, "event": "accepted", "bot": "shop.lead", "turn": 4, "data": {}}),
        ))
        .unwrap();
        view.drain();
        assert_eq!(view.sparks.len(), 2);
        assert_eq!(view.sparks[0].from, End::Agent(view.index["shop.lead"]));
        assert_eq!(view.sparks[1].from, End::Person);
        assert_eq!(view.talks.len(), 1);
        let shown = text(&view.card(60, 40));
        assert!(shown.contains("shop lead → login"), "{shown}");
        assert!(shown.contains("you → shop lead"), "{shown}");
        assert_eq!(
            text(&[view.footer(100)]),
            "  2 agents · ● 2 working · 2 messages in the last minute"
        );
    }

    #[tokio::test]
    async fn past_turns_draw_who_talked_without_sparks() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "a"}), json!({"name": "b"})]))
            .unwrap();
        send.send(Feed::Past(
            json!({"event": "accepted", "bot": "b", "turn": 1, "data": {"from": {"bot": "a"}}}),
        ))
        .unwrap();
        view.drain();
        assert!(view.sparks.is_empty());
        assert_eq!(view.talks.len(), 1);
        assert!(view.ticker.is_empty());
        assert!(view.agents.iter().all(|a| a.running.is_none()));
    }

    #[tokio::test]
    async fn a_commons_post_ripples_and_an_origin_comes_from_the_rim() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "a"})])).unwrap();
        send.send(Feed::Live(
            json!({"event": "tool_started", "bot": "a", "turn": 1, "data": {
            "name": "commons", "arguments": r#"{"post":{"name":"flaky-tests","title":"Flaky tests","text":"Seen twice"}}"#}}),
        ))
        .unwrap();
        send.send(Feed::Live(
            json!({"event": "accepted", "bot": "a", "turn": 2, "data": {"origin": "lookout.sources"}}),
        ))
        .unwrap();
        view.drain();
        assert!(view.agents[0].ripple.is_some());
        assert_eq!(view.sparks[0].from, End::Rim(0));
        let shown = text(&view.card(60, 30));
        assert!(shown.contains("a posted to the commons"), "{shown}");
        assert!(shown.contains("a follow → a"), "{shown}");
    }

    #[tokio::test]
    async fn an_absorbed_steer_finishing_leaves_the_agent_working() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "a", "running_turn": 5})]))
            .unwrap();
        // The daemon finishes the steer's own turn, then reports it steered.
        send.send(Feed::Live(
            json!({"event": "turn_finished", "bot": "a", "turn": 6,
            "data": {"status": "steered", "into": 5}}),
        ))
        .unwrap();
        view.drain();
        assert_eq!(view.agents[0].running, Some(5));
        send.send(Feed::Live(
            json!({"event": "turn_finished", "bot": "a", "turn": 5,
            "data": {"status": "completed"}}),
        ))
        .unwrap();
        view.drain();
        assert_eq!(view.agents[0].running, None);
    }

    #[tokio::test]
    async fn a_made_agent_is_seated_from_its_event_and_answers_fly_home() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "shop.lead"})]))
            .unwrap();
        send.send(Feed::Live(
            json!({"event": "created", "bot": "shop.cart", "turn": null,
            "data": {"id": 9, "created_by": "shop.lead", "running_turn": null}}),
        ))
        .unwrap();
        send.send(Feed::Live(
            json!({"event": "accepted", "bot": "shop.lead", "turn": 1, "data": {}}),
        ))
        .unwrap();
        send.send(Feed::Live(
            json!({"event": "turn_finished", "bot": "shop.lead", "turn": 1,
            "data": {"status": "completed"}}),
        ))
        .unwrap();
        view.drain();
        let cart = view.index["shop.cart"];
        assert_eq!(view.agents[cart].made_by.as_deref(), Some("shop.lead"));
        assert_eq!(view.sparks.last().map(|s| s.to), Some(End::Person));
    }

    #[tokio::test]
    async fn a_view_put_aside_reads_its_agents_afresh_when_back() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "gone"})]))
            .unwrap();
        send.send(Feed::Past(
            json!({"event": "accepted", "bot": "gone", "turn": 1, "data": {}}),
        ))
        .unwrap();
        view.drain();
        view.pause();
        view.resume();
        view.drain();
        assert!(view.agents.is_empty() && view.sent.is_empty());
    }

    #[test]
    fn a_cut_commons_preview_still_reads_as_a_post() {
        assert!(posted(&json!(r#"{"reply":{"thread":"x","text":"lo"#)));
        assert!(!posted(&json!(r#"{"read":"x","from":40}"#)));
        assert!(!posted(&json!("{}")));
    }

    #[tokio::test]
    async fn a_second_roll_settles_who_is_here_and_who_works() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "a"}), json!({"name": "b"})]))
            .unwrap();
        send.send(Feed::Roll(vec![
            json!({"name": "a", "running_turn": 3}),
            json!({"name": "c"}),
        ]))
        .unwrap();
        view.drain();
        let at = |name: &str| &view.agents[view.index[name]];
        assert_eq!(at("a").running, Some(3));
        assert!(!view.index.contains_key("b") && !at("c").gone);
    }

    #[tokio::test]
    async fn a_removed_agent_leaves_nothing_behind() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![
            json!({"name": "a"}),
            json!({"name": "b"}),
            json!({"name": "c"}),
        ]))
        .unwrap();
        send.send(Feed::Live(
            json!({"event": "accepted", "bot": "c", "turn": 1,
            "data": {"from": {"bot": "b", "turn": 1}}}),
        ))
        .unwrap();
        send.send(Feed::Live(
            json!({"event": "deleted", "bot": "a", "data": {}}),
        ))
        .unwrap();
        view.drain();
        assert_eq!(view.agents.len(), 2);
        let (b, c) = (view.index["b"], view.index["c"]);
        assert_eq!(view.talks.back(), Some(&(b, c)));
        assert_eq!(view.agents[c].sender, Some(End::Agent(b)));
        assert!(view.sparks.iter().all(|spark| spark.to == End::Agent(c)));
    }

    #[tokio::test]
    async fn events_across_the_handoff_come_once_and_in_order() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = std::env::temp_dir().join(format!("sky-handoff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut out = vec![json!({"event": "ready", "protocol": agent_client::PROTOCOL})];
            let mut lines = BufReader::new(read).lines();
            let (mut rolls, mut follows) = (0, 0);
            let made = json!({"cursor": 11, "bot": "c", "event": "created",
                "data": {"created_by": "a"}});
            let started = json!({"cursor": 12, "bot": "c", "turn": 2, "event": "accepted",
                "data": {"from": {"bot": "a", "turn": 1}}});
            loop {
                for line in out.drain(..) {
                    write
                        .write_all(format!("{line}\n").as_bytes())
                        .await
                        .unwrap();
                }
                let Ok(Some(line)) = lines.next_line().await else {
                    return;
                };
                let asked: Value = serde_json::from_str(&line).unwrap();
                let id = asked["id"].clone();
                if asked["op"] == "bots" {
                    rolls += 1;
                    out.push(json!({"id": id, "result": {"bots": [{"name": "a"},
                        {"name": "b"}, {"name": "c"}]}}));
                    if rolls == 2 {
                        for line in out.drain(..) {
                            write
                                .write_all(format!("{line}\n").as_bytes())
                                .await
                                .unwrap();
                        }
                        return;
                    }
                } else if asked["op"] == "approvals" {
                    out.push(json!({"id": id, "result": {"approvals": []}}));
                } else {
                    follows += 1;
                    if follows == 2 {
                        // The old follow, still live until it is replaced.
                        out.push(started.clone());
                    }
                    out.push(json!({"id": id, "result": {}}));
                    if follows == 1 {
                        out.push(json!({"event": "follow_live", "cursor": 10}));
                    } else {
                        out.push(made.clone());
                        out.push(started.clone());
                        out.push(json!({"event": "follow_live", "cursor": 12}));
                    }
                }
            }
        });
        let (send, mut feed) = mpsc::unbounded_channel();
        follow(socket, send).await;
        let mut seen = Vec::new();
        while let Ok(item) = feed.try_recv() {
            seen.push(match item {
                Feed::Live(event) => event["cursor"].to_string(),
                Feed::Roll(_) => "roll".into(),
                Feed::Ready => "ready".into(),
                Feed::Past(_) | Feed::Held(_) | Feed::Down(_) => continue,
            });
        }
        // Made, then started, once each in the store's order; then the roll
        // again, then caught up.
        assert_eq!(seen, ["roll", "11", "12", "roll", "ready"]);
    }

    #[tokio::test]
    async fn a_name_used_again_is_not_the_maker() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![
            json!({"name": "a", "bot_id": 7}),
            json!({"name": "b", "bot_id": 2, "created_by": "a", "created_by_id": 1}),
            json!({"name": "c", "bot_id": 3, "created_by": "a", "created_by_id": 7}),
        ]))
        .unwrap();
        view.drain();
        assert_eq!(view.maker(view.index["b"]), None);
        assert_eq!(view.maker(view.index["c"]), Some(view.index["a"]));
    }

    #[tokio::test]
    async fn a_sender_whose_name_was_used_again_comes_from_outside() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![
            json!({"name": "a", "bot_id": 7}),
            json!({"name": "b", "bot_id": 2}),
        ]))
        .unwrap();
        view.drain();
        let (a, b) = (view.index["a"], view.index["b"]);
        let from = |id: i64| json!({"from": {"bot": "a", "bot_id": id, "turn": 1}});
        assert_eq!(view.sender(&from(1), b), End::Rim(b));
        assert_eq!(view.sender(&from(7), b), End::Agent(a));
    }

    #[tokio::test]
    async fn a_name_taken_again_starts_afresh() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![
            json!({"name": "a", "bot_id": 1}),
            json!({"name": "b", "bot_id": 2}),
        ]))
        .unwrap();
        send.send(Feed::Live(
            json!({"event": "accepted", "bot": "b", "turn": 1,
            "data": {"from": {"bot": "a", "bot_id": 1, "turn": 1}}}),
        ))
        .unwrap();
        send.send(Feed::Roll(vec![
            json!({"name": "a", "bot_id": 9}),
            json!({"name": "b", "bot_id": 2}),
        ]))
        .unwrap();
        view.drain();
        let b = view.index["b"];
        assert!(view.talks.is_empty());
        assert_eq!(view.agents[b].sender, None);
        assert!(
            view.sparks
                .iter()
                .all(|spark| spark.from != End::Agent(view.index["a"]))
        );
    }

    #[tokio::test]
    async fn a_quiet_fleet_once_caught_up_says_so() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "a"})])).unwrap();
        send.send(Feed::Ready).unwrap();
        view.drain();
        let text: Vec<String> = (view.card(60, 30).iter())
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text.iter().any(|line| line.starts_with("Quiet.")));
    }

    #[tokio::test]
    async fn a_lost_follow_is_told_under_the_messages() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "a"})])).unwrap();
        send.send(Feed::Live(
            json!({"event": "accepted", "bot": "a", "turn": 1,
            "data": {}}),
        ))
        .unwrap();
        send.send(Feed::Down("fell behind the daemon".into()))
            .unwrap();
        view.drain();
        let text: Vec<String> = (view.card(60, 30).iter())
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text.iter().any(|line| line.contains("you → a")));
        assert!(
            text.iter()
                .any(|line| line.contains("fell behind the daemon"))
        );
    }

    #[tokio::test]
    async fn an_agent_shows_the_state_tables_mark_for_what_it_is_doing() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![
            json!({"name": "shop.lead"}),
            json!({"name": "shop.login", "created_by": "shop.lead"}),
            json!({"name": "shop.cart", "created_by": "shop.lead"}),
        ]))
        .unwrap();
        let live = |bot: &str, kind: &str, turn: i64, data: Value| {
            Feed::Live(json!({"event": kind, "bot": bot, "turn": turn, "data": data}))
        };
        for feed in [
            live("shop.lead", "accepted", 1, json!({})),
            live("shop.lead", "tool_started", 1, json!({"name": "ask"})),
            live(
                "shop.lead",
                "turn_finished",
                1,
                json!({"status": "completed"}),
            ),
            live(
                "shop.login",
                "accepted",
                2,
                json!({"from": {"bot": "shop.lead"}}),
            ),
            live(
                "shop.login",
                "approval_requested",
                2,
                json!({"calls": [{"call_id": "c1", "gates": ["manual"]}]}),
            ),
            live(
                "shop.cart",
                "accepted",
                3,
                json!({"from": {"bot": "shop.lead"}}),
            ),
            live(
                "shop.cart",
                "turn_finished",
                3,
                json!({"status": "failed", "error": "provider_error"}),
            ),
        ] {
            send.send(feed).unwrap();
        }
        view.drain();
        let state = |view: &View, name: &str| view.agents[view.index[name]].state();
        assert_eq!(state(&view, "shop.lead"), Some(State::NeedsYou));
        assert_eq!(state(&view, "shop.login"), Some(State::NeedsYou));
        assert_eq!(state(&view, "shop.cart"), Some(State::Failed));
        let shown = text(&view.card(100, 40));
        assert!(shown.contains("? shop lead"), "{shown}");
        assert!(shown.contains("✗ cart"), "{shown}");
        assert_eq!(
            text(&[view.footer(100)]),
            "  3 agents · ? 2 needs you · ● 0 working · ✗ 1 failed · 3 messages in the last minute"
        );

        // The call allowed, the agent works again; a new turn for what
        // failed and what asked starts afresh.
        for feed in [
            live("shop.login", "tool_completed", 2, json!({"call_id": "c1"})),
            live("shop.cart", "accepted", 4, json!({})),
            live("shop.lead", "accepted", 5, json!({})),
            live(
                "shop.lead",
                "turn_finished",
                5,
                json!({"status": "failed", "error": "empty_completion"}),
            ),
        ] {
            send.send(feed).unwrap();
        }
        view.drain();
        assert_eq!(state(&view, "shop.login"), Some(State::Working));
        assert_eq!(state(&view, "shop.cart"), Some(State::Working));
        assert_eq!(state(&view, "shop.lead"), None);
    }

    #[tokio::test]
    async fn what_needs_the_person_stays_until_they_answer_it() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![
            json!({"name": "shop.lead"}),
            json!({"name": "shop.login", "created_by": "shop.lead"}),
            json!({"name": "shop.cart", "created_by": "shop.lead", "running_turn": 7}),
            json!({"name": "shop.ops"}),
        ]))
        .unwrap();
        // Read when the view opened: a failed last turn, and a call that
        // already waited.
        send.send(Feed::Past(
            json!({"event": "turn_finished", "bot": "shop.ops",
            "turn": 1, "data": {"status": "failed", "error": "provider_error"}}),
        ))
        .unwrap();
        send.send(Feed::Held(vec![("shop.cart".into(), "c0".into())]))
            .unwrap();
        let live = |bot: &str, kind: &str, turn: i64, data: Value| {
            Feed::Live(json!({"event": kind, "bot": bot, "turn": turn, "data": data}))
        };
        for feed in [
            live(
                "shop.login",
                "accepted",
                2,
                json!({"from": {"bot": "shop.lead"}}),
            ),
            live("shop.login", "tool_started", 2, json!({"name": "ask"})),
            live(
                "shop.login",
                "turn_finished",
                2,
                json!({"status": "completed"}),
            ),
            live(
                "shop.lead",
                "accepted",
                3,
                json!({"from": {"bot": "shop.ops"}}),
            ),
            live(
                "shop.lead",
                "approval_requested",
                3,
                json!({"calls": [{"call_id": "a", "gates": ["manual"]},
                    {"call_id": "b", "gates": ["manual"]}]}),
            ),
            live("shop.lead", "tool_completed", 3, json!({"call_id": "a"})),
            // A message from someone else is no answer to what it asked.
            live(
                "shop.login",
                "accepted",
                4,
                json!({"from": {"bot": "shop.ops"}}),
            ),
        ] {
            send.send(feed).unwrap();
        }
        view.drain();
        let state = |view: &View, name: &str| view.agents[view.index[name]].state();
        assert_eq!(state(&view, "shop.ops"), Some(State::Failed));
        assert_eq!(state(&view, "shop.cart"), Some(State::NeedsYou));
        assert_eq!(state(&view, "shop.lead"), Some(State::NeedsYou));
        assert_eq!(state(&view, "shop.login"), Some(State::NeedsYou));

        // The last call answered, the waiting one started, and the lead
        // that made it answers what it asked.
        for feed in [
            live("shop.lead", "step_decided", 3, json!({"call_id": "b"})),
            live(
                "shop.cart",
                "tool_started",
                7,
                json!({"call_id": "c0", "name": "shell"}),
            ),
            live(
                "shop.login",
                "steered",
                4,
                json!({"from": {"bot": "shop.lead"}}),
            ),
        ] {
            send.send(feed).unwrap();
        }
        view.drain();
        assert_eq!(state(&view, "shop.lead"), Some(State::Working));
        assert_eq!(state(&view, "shop.cart"), Some(State::Working));
        assert_eq!(state(&view, "shop.login"), Some(State::Working));
    }

    #[tokio::test]
    async fn a_small_pane_lists_the_agents_with_a_firefly_each() {
        let (mut view, send) = view();
        let mut bots = vec![json!({"name": "shop.lead"})];
        for n in 0..30 {
            bots.push(json!({"name": format!("shop.t{n}"), "created_by": "shop.lead"}));
        }
        send.send(Feed::Roll(bots)).unwrap();
        send.send(Feed::Live(
            json!({"event": "accepted", "bot": "shop.t29", "turn": 1,
            "data": {"from": {"bot": "shop.lead"}}}),
        ))
        .unwrap();
        view.drain();
        let shown = text(&view.card(50, 20));
        let rows: Vec<&str> = shown.lines().collect();
        // No braille: a list, the lead first, its agents under it.
        assert!(
            !shown
                .chars()
                .any(|c| ('\u{2801}'..='\u{28ff}').contains(&c)),
            "{shown}"
        );
        assert!(rows[0].contains("shop lead"), "{shown}");
        // The one at work is kept though it is last, and says who told it.
        assert!(
            rows.iter().any(|row| row.starts_with("●")
                && row.contains("t29")
                && row.contains("from shop lead")),
            "{shown}"
        );
        assert!(shown.contains("more, quiet"), "{shown}");
        assert!(rows.len() <= 20, "{shown}");
    }

    #[test]
    fn labels_name_leads_by_their_project() {
        assert_eq!(short("shop.lead"), "shop lead");
        assert_eq!(short("shop.lead.billing"), "shop lead billing");
        assert_eq!(short("shop.fix-login"), "fix-login");
        assert_eq!(short("solo"), "solo");
    }

    #[test]
    fn braille_dots_land_in_their_cells() {
        let mut sky = Sky::new(2, 1);
        sky.plot(0.0, 0.0, Ink::Plain);
        sky.plot(3.0, 3.0, Ink::Color(TALK, true));
        let lines = sky.lines();
        assert_eq!(text(&lines), "⠁⢀");
        assert_eq!(lines[0].spans[1].style.fg, Some(TALK));
    }
}
