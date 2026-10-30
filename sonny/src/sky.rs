//! `/sky`: every agent the daemon keeps, as a sky of stars, lit up live as
//! they talk. Leads sit on the inner ring and the agents each one made fan
//! out behind it, so a project reads as one arm of the sky; the person is
//! the middle. A message travels as a spark along its curve (the person's
//! from the middle, another agent's across, a lookout's or automation's in
//! from the rim), a commons post ripples out, a tool call twinkles, and
//! whoever is lit fades like a firefly.
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
use std::collections::{HashMap, HashSet, VecDeque};
use std::f32::consts::{PI, TAU};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

/// Past turns' events read for who talked to whom.
const PAST: &[&str] = &["accepted", "steered"];
/// Live events: who is made, who talks, works, calls and posts.
const LIVE: &[&str] = &[
    "created",
    "forked",
    "deleted",
    "queued",
    "accepted",
    "steered",
    "message",
    "tool_started",
    "tool_completed",
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
/// Lines of what happened under the sky.
const TICKER: usize = 5;

/// Agent to agent.
const TALK: Color = Color::Magenta;
/// A tool call: code's colour.
const TOOL: Color = Color::Cyan;
/// A commons post.
const BOARD: Color = Color::Green;
/// A failed call.
const FAILED: Color = Color::Red;

/// What the follow brings the view.
enum Feed {
    Bots(Vec<Value>),
    Past(Value),
    Live(Value),
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

struct Spark {
    from: End,
    to: End,
    born: Instant,
    color: Option<Color>,
}

#[derive(Default)]
struct Agent {
    name: String,
    made_by: Option<String>,
    working: bool,
    /// When it was last lit, and in what colour (none: the person's).
    lit: Option<(Instant, Option<Color>)>,
    twinkle: Option<(Instant, Color)>,
    ripple: Option<Instant>,
    /// Who sent its running turn's prompt.
    sender: Option<End>,
    gone: bool,
    // Laid out:
    at: (f32, f32),
    angle: f32,
    root: bool,
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

    /// Follow again after a pause: the agents read afresh, who talked
    /// to whom kept.
    pub fn resume(&mut self) {
        if !self.task.is_finished() && !self.paused {
            return;
        }
        self.paused = false;
        let (send, feed) = mpsc::unbounded_channel();
        self.feed = feed;
        self.task = tokio::spawn(follow(self.socket.clone(), send));
        self.sparks.clear();
        self.live = false;
        self.down = None;
    }

    /// Take what the follow brought, and let what landed light up.
    pub fn drain(&mut self) {
        let now = Instant::now();
        while let Ok(feed) = self.feed.try_recv() {
            match feed {
                Feed::Bots(bots) => {
                    for bot in &bots {
                        self.seat(bot);
                    }
                }
                Feed::Past(event) => self.event(&event, now, false),
                Feed::Live(event) => {
                    self.live = true;
                    self.event(&event, now, true);
                }
                Feed::Down(why) => self.down = Some(why),
            }
        }
        self.land(now);
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
        agent.made_by = (bot["created_by"].as_str())
            .filter(|by| !by.is_empty())
            .map(str::to_owned);
        agent.working = bot["running_turn"].as_i64().is_some();
        Some(at)
    }

    fn agent(&self, name: &str) -> Option<usize> {
        self.index
            .get(name)
            .copied()
            .filter(|&at| !self.agents[at].gone)
    }

    fn event(&mut self, event: &Value, now: Instant, live: bool) {
        let Some(name) = event["bot"].as_str() else {
            return;
        };
        match event["event"].as_str().unwrap_or("") {
            "created" | "forked" => {
                if let Some(at) = self.seat(event) {
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
                    "steered" => event["steer"].as_i64().unwrap_or(turn),
                    _ => turn,
                };
                let from = self.sender(event, at);
                if kind == "accepted" {
                    self.agents[at].working = true;
                    self.agents[at].sender = Some(from);
                }
                if !self.sent.insert((name.to_owned(), sent_turn)) {
                    return;
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
                    let said = match from {
                        End::Person => format!("you → {}", self.short(at)),
                        End::Agent(by) => format!("{} → {}", self.short(by), self.short(at)),
                        End::Rim(_) => format!(
                            "{} → {}",
                            event["origin"].as_str().unwrap_or("outside"),
                            self.short(at)
                        ),
                    };
                    self.say(now, said);
                }
            }
            "message" if live => {
                let agent = &mut self.agents[at];
                agent.lit = Some((now, agent.lit.and_then(|(_, color)| color)));
                // Words for the person fly back to the middle.
                if agent.sender == Some(End::Person) && event["role"] != "user" {
                    self.sparks.push(Spark {
                        from: End::Agent(at),
                        to: End::Person,
                        born: now,
                        color: None,
                    });
                }
            }
            "tool_started" if live => {
                self.agents[at].twinkle = Some((now, TOOL));
                if event["name"] == "commons"
                    && matches!(
                        event["arguments"]["action"].as_str(),
                        Some("post" | "reply")
                    )
                {
                    self.agents[at].ripple = Some(now);
                    let said = format!("{} posted to the commons", self.short(at));
                    self.say(now, said);
                }
            }
            "tool_completed" if live => {
                if event["failed"] == true || event["denied"] == true {
                    self.agents[at].twinkle = Some((now, FAILED));
                }
            }
            "turn_finished" => {
                self.agents[at].working = false;
                self.agents[at].sender = None;
            }
            _ => {}
        }
    }

    /// Who sent a prompt: another agent, a client named by its origin
    /// (in from the rim), or the person.
    fn sender(&self, event: &Value, at: usize) -> End {
        if let Some(by) = event["from"]["bot"].as_str() {
            return self.agent(by).map_or(End::Rim(at), End::Agent);
        }
        match event["origin"].as_str() {
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
        let count = self.agents.len();
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
        let mut roots = Vec::new();
        for at in 0..count {
            if self.agents[at].gone {
                continue;
            }
            let parent = (self.agents[at].made_by.as_deref())
                .and_then(|by| self.agent(by))
                .filter(|&by| by != at && !project::is_lead(&self.agents[at].name));
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
        let sky_rows = rows.saturating_sub(TICKER + 3).clamp(8, cols / 2 + 1);
        let mut sky = Sky::new(cols, sky_rows);
        // The person in the middle.
        sky.text_at((0.0, 0.0), "you", Style::default().bold());
        // Who made whom: faint lines; who talked: faint curves.
        for agent in self.agents.iter().filter(|a| !a.gone) {
            if let Some(by) = agent.made_by.as_deref().and_then(|by| self.agent(by))
                && !agent.root
            {
                sky.line(self.agents[by].at, agent.at, Ink::Faint, 3);
            }
        }
        for &(by, to) in &self.talks {
            if self.agents[by].gone || self.agents[to].gone {
                continue;
            }
            let (a, b) = (self.agents[by].at, self.agents[to].at);
            sky.curve(a, bend(a, b), b, Ink::Faint, 4);
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
            let glow = agent.lit.map_or(0.0, |(at, _)| {
                0.5f32.powf(now.duration_since(at).as_secs_f32() / HALF_LIFE)
            });
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
            if let Some((at, color)) = agent.twinkle
                && now.duration_since(at) < TWINKLE
            {
                sky.twinkle(agent.at, Ink::Color(color, true));
            }
            match agent.working || glow > 0.4 {
                true => sky.blob(
                    agent.at,
                    if agent.working {
                        Ink::Bright
                    } else {
                        Ink::Plain
                    },
                ),
                false => sky.dot(agent.at, Ink::Plain),
            }
        }
        // Names: leads always, others while working or lit, nearest the
        // middle first, never over each other.
        let mut named: Vec<usize> = (0..self.agents.len())
            .filter(|&at| {
                let agent = &self.agents[at];
                !agent.gone
                    && (agent.root
                        || agent.working
                        || agent.lit.is_some_and(|(lit, _)| {
                            now.duration_since(lit) < Duration::from_secs(4)
                        }))
            })
            .collect();
        named.sort_by_key(|&at| {
            (
                !self.agents[at].root,
                (self.agents[at].at.0.hypot(self.agents[at].at.1) * 100.0) as i32,
            )
        });
        for at in named {
            let agent = &self.agents[at];
            let style = match agent.working {
                true => Style::default().bold(),
                false => Style::default().add_modifier(Modifier::DIM),
            };
            sky.label(agent.at, &short(&agent.name), style);
        }
        lines.extend(sky.lines());
        lines.push(Line::default());
        let ago = |at: Instant| {
            let s = now.duration_since(at).as_secs();
            match s {
                0..=59 => format!("{s}s"),
                _ => format!("{}m", s / 60),
            }
        };
        for (at, text) in &self.ticker {
            let mut text = text.clone();
            let room = cols.saturating_sub(6);
            if text.width() > room {
                text = text
                    .chars()
                    .take(room.saturating_sub(1))
                    .collect::<String>()
                    + "…";
            }
            lines.push(Line::from(vec![
                Span::raw(format!("{:>4}  ", ago(*at))).dim(),
                Span::raw(text),
            ]));
        }
        if self.ticker.is_empty() {
            lines.push(match (&self.down, self.live) {
                (Some(why), _) => Line::from(format!("Couldn't follow the agents: {why}")),
                (None, true) => Line::from("Quiet. Messages light up here as they go.").dim(),
                (None, false) => Line::from("Catching up with who talked to whom…").dim(),
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

    /// Counts in words: everyone, who works, how many messages a minute.
    pub fn footer(&self) -> String {
        let shown = self.agents.iter().filter(|a| !a.gone);
        let working = shown.clone().filter(|a| a.working).count();
        let count = shown.count();
        format!(
            "{count} {} · {working} working · {} {} in the last minute",
            if count == 1 { "agent" } else { "agents" },
            self.recent.len(),
            if self.recent.len() == 1 {
                "message"
            } else {
                "messages"
            },
        )
    }
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
    Faint,
    Plain,
    Bright,
    Color(Color, bool),
}

impl Ink {
    fn rank(self) -> u8 {
        match self {
            Ink::Faint => 0,
            Ink::Plain => 1,
            Ink::Color(_, false) => 2,
            Ink::Bright => 3,
            Ink::Color(_, true) => 4,
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
            Ink::Faint => Style::default().add_modifier(Modifier::DIM),
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

    fn twinkle(&mut self, at: (f32, f32), ink: Ink) {
        let (x, y) = self.spot(at);
        for (dx, dy) in [(-2.0, -2.0), (2.0, -2.0), (-2.0, 2.0), (2.0, 2.0)] {
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

    /// Words beside a point, right of it where they fit, else left; never
    /// over other words.
    fn label(&mut self, at: (f32, f32), words: &str, style: Style) {
        let (x, y) = self.spot(at);
        let (col, row) = ((x / 2.0) as usize, (y / 4.0) as usize);
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
        self.put(row, start, words, style);
    }

    /// Words centred on a point.
    fn text_at(&mut self, at: (f32, f32), words: &str, style: Style) {
        let (x, y) = self.spot(at);
        let (col, row) = ((x / 2.0) as usize, (y / 4.0) as usize);
        let start = col.saturating_sub(words.width() / 2);
        self.put(row, start, words, style);
    }

    fn put(&mut self, row: usize, start: usize, words: &str, style: Style) {
        let cells = row * self.cols + start..row * self.cols + start + words.chars().count();
        if start + words.chars().count() > self.cols
            || self.text[cells.clone()].iter().any(Option::is_some)
        {
            return;
        }
        for (cell, ch) in cells.zip(words.chars()) {
            self.text[cell] = Some((ch, style));
        }
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
    let mut after: Option<String> = None;
    loop {
        let page = match client
            .request("bots", json!({"after": after, "limit": 256}))
            .await
        {
            Ok(page) => page,
            Err(error) => {
                let _ = send.send(Feed::Down(error.to_string()));
                return;
            }
        };
        let bots = page["bots"].as_array().cloned().unwrap_or_default();
        if send.send(Feed::Bots(bots)).is_err() {
            return;
        }
        match page["next_after"].as_str() {
            Some(next) => after = Some(next.to_owned()),
            None => break,
        }
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
    while let Some(event) = events.recv().await {
        match event["event"].as_str() {
            Some("follow_live" | "pruned") => continue,
            Some("lagged" | "follow_lagged") => {
                let _ = send.send(Feed::Down("fell behind the daemon".into()));
                return;
            }
            _ => {}
        }
        if send.send(Feed::Live(event)).is_err() {
            return;
        }
    }
    let _ = send.send(Feed::Down("the daemon closed the connection".into()));
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
    async fn a_lead_sits_inside_the_agents_it_made() {
        let (mut view, send) = view();
        send.send(Feed::Bots(vec![
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
        send.send(Feed::Bots(vec![
            json!({"name": "shop.lead"}),
            json!({"name": "shop.login", "created_by": "shop.lead"}),
        ]))
        .unwrap();
        let from = json!({"bot": "shop.lead", "turn": 3});
        for kind in ["queued", "accepted"] {
            send.send(Feed::Live(
                json!({"event": kind, "bot": "shop.login", "turn": 2, "from": from}),
            ))
            .unwrap();
        }
        send.send(Feed::Live(
            json!({"event": "accepted", "bot": "shop.lead", "turn": 4}),
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
        assert!(
            view.footer()
                .starts_with("2 agents · 2 working · 2 messages")
        );
    }

    #[tokio::test]
    async fn past_turns_draw_who_talked_without_sparks() {
        let (mut view, send) = view();
        send.send(Feed::Bots(vec![json!({"name": "a"}), json!({"name": "b"})]))
            .unwrap();
        send.send(Feed::Past(
            json!({"event": "accepted", "bot": "b", "turn": 1, "from": {"bot": "a"}}),
        ))
        .unwrap();
        view.drain();
        assert!(view.sparks.is_empty());
        assert_eq!(view.talks.len(), 1);
        assert!(view.ticker.is_empty());
    }

    #[tokio::test]
    async fn a_commons_post_ripples_and_an_origin_comes_from_the_rim() {
        let (mut view, send) = view();
        send.send(Feed::Bots(vec![json!({"name": "a"})])).unwrap();
        send.send(Feed::Live(
            json!({"event": "tool_started", "bot": "a", "turn": 1,
            "name": "commons", "arguments": {"action": "post"}}),
        ))
        .unwrap();
        send.send(Feed::Live(
            json!({"event": "accepted", "bot": "a", "turn": 2, "origin": "lookout.sources"}),
        ))
        .unwrap();
        view.drain();
        assert!(view.agents[0].ripple.is_some());
        assert_eq!(view.sparks[0].from, End::Rim(0));
        let shown = text(&view.card(50, 30));
        assert!(shown.contains("a posted to the commons"), "{shown}");
        assert!(shown.contains("lookout.sources → a"), "{shown}");
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
