//! `/sky`: every agent the daemon keeps, as a sky of stars, lit up live as
//! they talk. Leads sit on the inner ring and the agents each one made fan
//! out behind it, so a project reads as one arm of the sky; the person is
//! the middle. A message travels as a spark along its curve (the person's
//! from the middle, another agent's across, a lookout's or automation's in
//! from the rim), a commons post ripples out, a tool call twinkles, and
//! whoever is lit fades like a firefly. It is a calm window, kept open to
//! watch: every pair that has talked keeps a faint curve, as in 0.7.0, an
//! agent at rest is a dot, a working star breathes, and only what a glance
//! can act on has a hue, yellow `?` for what needs you and red `✗` for
//! what failed, with cyan for a tool call's moment. Beside it,
//! where there is room, the chat log of who talked to whom, newest at the
//! bottom, fading as it rises. Where the sky has no room, under about 60
//! columns or 24 rows, the same agents are a list, each with a firefly.
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
    "configured",
];
/// How long a spark takes to cross.
const FLIGHT: Duration = Duration::from_millis(1100);
/// More leaves than this under one agent spread into a cloud.
const CROWD: usize = 12;
/// The most sparks drawn at once.
const SPARKS: usize = 12;
/// How long stars take to glide to new places, when the centre
/// changes.
const GLIDE: Duration = Duration::from_millis(330);
/// How long a commons post's ripple spreads.
const RIPPLE: Duration = Duration::from_millis(1400);
/// How long a tool call twinkles.
const TWINKLE: Duration = Duration::from_millis(350);
/// How fast a lit agent fades: its glow halves this often.
const HALF_LIFE: f32 = 0.9;
/// A landing flash's radius in braille dots, whatever the pane, as the
/// star it rings is: one message bursts as far as it did in 0.7.0, then
/// shrinks into its star.
const FLASH: f32 = 5.0;
/// The ring's area grows as the heat does, as a map's circles grow with
/// what they count: each doubling of messages √2 wider (5, 7, 10, 14 dots
/// for 1, 2, 4, 8 at once).
const FLASH_GROWTH: f32 = std::f32::consts::SQRT_2;
/// The biggest, about two dozen at once, as wide as 0.7.0's commons
/// ripple spread.
const FLASH_MAX: f32 = 24.0;
/// The pairs that talked, newest kept, each a curve: the web of who
/// works with whom, as in 0.7.0.
const TALKS: usize = 96;
/// What was said and called, kept an hour for the counts.
const HOUR: Duration = Duration::from_secs(3600);
/// A working star's breath, in seconds: about thirteen a minute, the
/// pace of Apple's sleep light, slow enough to stay in the corner of the
/// eye.
const BREATH: f32 = 4.6;
/// Queued prompts remembered so their start does not spark again.
const SENT: usize = 1024;
/// Lines of what happened under the sky.
const TICKER: usize = 5;
/// Lines the chat log keeps.
const LOG: usize = 64;
/// The chat log's column beside the sky, and the gap before it.
const LOG_COLS: usize = 36;
/// The most lines the chat log beside the sky shows: three plain, six
/// muted, three faint, so it ends where its fade does.
const LOG_LINES: usize = 12;
const GAP: usize = 3;
/// How fast a replay plays at first, and the fastest it goes: eight
/// minutes a minute, as fast as the chat log can still be read; an hour
/// in under eight seconds at most.
const REPLAY_SPEED: f32 = 8.0;
const REPLAY_FASTEST: f32 = 512.0;
/// A step of a replay skips what this much watching would show.
const REPLAY_STEP: f64 = 10_000.0;
/// What `r` replays: the last hour, as the talk lens draws.
const REPLAY_BACK: i64 = 3_600_000;
/// The least room the sky is drawn in; with less, a list.
const SKY_COLS: usize = 60;
const SKY_ROWS: usize = 24;
/// How much longer one way than the other the sky may be.
const STRETCH: f32 = 2.0;

/// An agent as its making tells of it, as the listing would.
fn made(name: &str, data: &Value) -> Value {
    json!({"name": name, "bot_id": data["bot_id"],
        "created_by": data["created_by"], "created_by_id": data["created_by_id"],
        "running_turn": data["running_turn"], "model": data["model"],
        "expire_ms": data["expire_ms"]})
}

/// The most events a replay keeps to step back through.
const TAPE: usize = 100_000;

/// A tool call: code's colour, for its moment.
const TOOL: Color = Color::Cyan;
/// Working: the text's own, as a working star breathes instead. Needs
/// you: the state table's yellow.
const WORKING: Color = Color::Reset;
const NEEDS: Color = Color::Yellow;
/// A failed call.
const FAILED: Color = Color::Red;
/// A message, coloured where 0.7.0 coloured it, in the visual system's
/// hues: blue between agents, magenta to or from the person.
const AGENTS: Color = Color::Blue;
const YOU: Color = Color::Magenta;
/// The models' colours on `m`, an exception to one meaning per hue as the
/// usage chart's are: telling models apart needs a fill each. Hues the
/// sky's messages and tools don't take first, then the bright twins.
const MODEL_HUES: [Color; 8] = [
    Color::Green,
    Color::Yellow,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::LightRed,
];

/// What the follow brings the view.
enum Feed {
    /// Every agent there is now: who is listed is here, who isn't is gone.
    Roll(Vec<Value>),
    Past(Value),
    /// The calls waiting for the person when the past was read, each with
    /// its agent: what is answered after, live events let go.
    Held(Vec<(String, String)>),
    Live(Value),
    /// An agent's model, read again after it was configured.
    Model(String, String),
    /// An event a replay plays when its time comes, with when it was
    /// written.
    Tape(Value),
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
    /// Blue between agents, magenta with the person; from outside, the
    /// text's.
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
    /// When it was last lit.
    lit: Option<Instant>,
    twinkle: Option<(Instant, Color, Work)>,
    /// A commons post's ripple: when, and how far it spreads.
    ripple: Option<(Instant, f32)>,
    /// How much it has been messaged lately: each message that lands adds
    /// one, and it halves as a glow does.
    heat: f32,
    heated: Option<Instant>,
    /// The burst's colour: the message's.
    flash: Option<Color>,
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
    /// The model it runs.
    model: String,
    gone: bool,
    // Laid out: where it is drawn now, gliding from where it was to where
    // the layout puts it.
    at: (f32, f32),
    from: (f32, f32),
    goal: Option<(f32, f32)>,
    angle: f32,
    root: bool,
    /// The root whose arm it is on, in the tree.
    arm: usize,
}

impl Agent {
    /// Its model, as `m` colours it.
    fn model_or_unknown(&self) -> &str {
        match self.model.as_str() {
            "" => "unknown",
            model => model,
        }
    }

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

    /// Its heat now.
    fn warmth(&self, now: Instant) -> f32 {
        self.heated.map_or(0.0, |at| {
            self.heat * 0.5f32.powf(now.duration_since(at).as_secs_f32() / HALF_LIFE)
        })
    }

    /// How lit it is, 1 just lit to 0.
    fn glow(&self, now: Instant) -> f32 {
        self.lit.map_or(0.0, |at| {
            0.5f32.powf(now.duration_since(at).as_secs_f32() / HALF_LIFE)
        })
    }

    /// Where its breath is, 0 to 1: each agent on a phase of its own, so
    /// many working shimmer as stars do rather than pulse together.
    fn breath(&self, now: Instant, start: Instant) -> f32 {
        let phase =
            (self.name.bytes()).fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
        let phase = (phase % 1000) as f32 / 1000.0 * TAU;
        let t = now.duration_since(start).as_secs_f32();
        0.5 - 0.5 * (TAU * t / BREATH + phase).cos()
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
    /// Who told whom, and when, agent to agent: an hour of it.
    said: VecDeque<(Instant, usize, usize)>,
    /// Who has told whom, past turns and live, the last told at the back.
    talks: VecDeque<(usize, usize)>,
    /// When tools were called: an hour of it.
    calls: VecDeque<Instant>,
    /// Turns already sparked, so a queued prompt's start is not a second.
    sent: HashSet<(String, i64)>,
    /// The chat log.
    ticker: VecDeque<Note>,
    /// When the event a replay plays now was written.
    written: Option<i64>,
    /// While a replay plays an event: now, and how long before the
    /// replay's now it was written, not sped up, which the hour's counts
    /// and weights go by while sparks and bursts play faster.
    history: Option<(Instant, Duration)>,
    /// When messages were sent: an hour of it.
    recent: VecDeque<Instant>,
    /// When the view opened: breaths count from it, and the hour's counts
    /// say "so far" until it is an hour old.
    opened: Instant,
    laid: bool,
    /// The agents in the tree's order, each with its depth, for the list.
    order: Vec<(usize, usize)>,
    live: bool,
    paused: bool,
    /// What each mark, colour and shape means, and the counts, in the
    /// chat log's place.
    pub legend: bool,
    /// Only the sky: no chat log, no counts, no names but what needs the
    /// person or failed.
    pub zen: bool,
    /// Each agent in its model's colour, on `m`.
    pub models: bool,
    /// The agent in the middle in the person's place, by name: its own,
    /// or the label or project it was asked for by.
    centre: Option<String>,
    /// The star picked with the arrows, to centre on with Enter.
    pick: Option<String>,
    /// Where the person is: the middle, or a ring of a centred sky.
    person: Glide,
    /// When the stars last set off for new places.
    moved: Option<Instant>,
    /// The pane is too small for the sky, so it shows the list.
    listing: bool,
    down: Option<String>,
    /// Playing the past back instead of watching live.
    replay: Option<Replay>,
    /// Throwaway copies, by name, and the agent each copies (its name,
    /// and its id when known): what one
    /// does is drawn on its agent, as the approver's judgments light it.
    copies: HashMap<String, (String, Option<i64>)>,
}

/// A replay: the daemon's events from a time on, played as they happened,
/// faster. The sky starts from who is here now, less those made since,
/// all of them idle, and what was said and done then plays over it.
struct Replay {
    /// When it starts, in Unix ms.
    from: i64,
    /// When it has played to, in Unix ms, never past now.
    at: f64,
    /// Story ms a real one plays.
    speed: f32,
    playing: bool,
    /// When `at` last moved.
    tick: Instant,
    /// Who was here when it started, as the listing says.
    cast: Vec<Value>,
    /// Every event from `from` on, in the store's order, and how many
    /// have played.
    tape: Vec<Value>,
    played: usize,
    /// The tape holds all the past; what comes after is live.
    ready: bool,
}

impl Replay {
    /// Puts an event on the tape. Past [`TAPE`] events, the oldest tenth
    /// that has played goes and the replay starts where the rest does, as
    /// a ring buffer keeps a recorder's last stretch: a replay left open
    /// on a busy daemon stays bounded, and a step back reaches that far.
    /// Who was made or removed in what goes is folded into the cast, as a
    /// keyframe takes in the frames before it.
    fn keep(&mut self, event: Value) {
        self.tape.push(event);
        let gone = TAPE / 10;
        if self.tape.len() <= TAPE || self.played < gone {
            return;
        }
        for event in self.tape.drain(..gone) {
            let Some(name) = event["bot"].as_str() else {
                continue;
            };
            match event["event"].as_str() {
                Some("created" | "forked") => {
                    self.cast.retain(|bot| bot["name"] != name);
                    self.cast.push(made(name, &event["data"]));
                }
                Some("deleted") => self.cast.retain(|bot| bot["name"] != name),
                _ => {}
            }
        }
        self.played -= gone;
        if let Some(first) = self.tape.first().and_then(|e| e["written_ms"].as_i64()) {
            self.from = self.from.max(first);
        }
    }

    /// Gives agents their new models, by name, wherever a step back would
    /// seat them from: one pass over the cast and the tape, however many.
    fn remodel(&mut self, models: &HashMap<&str, &str>) {
        let new = |name: &Value| name.as_str().and_then(|name| models.get(name).copied());
        for bot in &mut self.cast {
            if let Some(model) = new(&bot["name"]) {
                bot["model"] = json!(model);
            }
        }
        for event in &mut self.tape {
            if matches!(event["event"].as_str(), Some("created" | "forked"))
                && let Some(model) = new(&event["bot"])
            {
                event["data"]["model"] = json!(model);
            }
        }
    }

    fn new(from: i64) -> Self {
        Replay {
            from,
            at: from as f64,
            speed: REPLAY_SPEED,
            playing: true,
            tick: Instant::now(),
            cast: Vec::new(),
            tape: Vec::new(),
            played: 0,
            ready: false,
        }
    }
}

/// A line of the chat log.
struct Note {
    at: Instant,
    text: String,
    /// The hue of what needs you or failed.
    hue: Option<Color>,
    /// The agents it is about, so a centred sky keeps only its agent's.
    about: Vec<String>,
    /// In a replay, when it happened, in Unix ms, which it says for its
    /// time.
    written: Option<i64>,
}

/// A place drawn on the way from one to another.
#[derive(Clone, Copy, Default)]
struct Glide {
    at: (f32, f32),
    from: (f32, f32),
    goal: (f32, f32),
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
            said: VecDeque::new(),
            talks: VecDeque::new(),
            calls: VecDeque::new(),
            sent: HashSet::new(),
            ticker: VecDeque::new(),
            written: None,
            history: None,
            recent: VecDeque::new(),
            opened: Instant::now(),
            laid: false,
            order: Vec::new(),
            live: false,
            paused: false,
            legend: false,
            zen: false,
            models: false,
            centre: None,
            pick: None,
            person: Glide::default(),
            moved: None,
            listing: false,
            down: None,
            replay: None,
            copies: HashMap::new(),
        }
    }

    /// Stop following while the view is put aside, so nothing piles up
    /// unread.
    pub fn pause(&mut self) {
        self.paused = true;
        self.task.abort();
    }

    /// Follow again after a pause, from a fresh listing; a replay reads
    /// its tape again and goes on from where it was.
    pub fn resume(&mut self) {
        if !self.task.is_finished() && !self.paused {
            return;
        }
        self.paused = false;
        self.restart();
    }

    fn restart(&mut self) {
        self.task.abort();
        let (send, feed) = mpsc::unbounded_channel();
        self.feed = feed;
        self.task = match &mut self.replay {
            Some(replay) => {
                replay.cast.clear();
                replay.tape.clear();
                replay.played = 0;
                replay.ready = false;
                tokio::spawn(tape(self.socket.clone(), replay.from, send))
            }
            None => tokio::spawn(follow(self.socket.clone(), send)),
        };
        // Read afresh: agents removed meanwhile go, and who talked to whom
        // is read again from the turns kept.
        self.forget();
        self.live = false;
        self.down = None;
    }

    /// Let go of every agent and what they did, to draw them again.
    fn forget(&mut self) {
        self.agents.clear();
        self.index.clear();
        self.said.clear();
        self.talks.clear();
        self.sent.clear();
        self.laid = false;
        self.sparks.clear();
    }

    /// Play the past back from `from` (Unix ms), or, with none, watch live
    /// again. Either way the sky is read anew, and the chat log with it.
    pub fn replay(&mut self, from: Option<i64>) {
        self.replay = from.map(Replay::new);
        self.ticker.clear();
        self.recent.clear();
        self.calls.clear();
        self.restart();
    }

    /// Whether the past is playing back.
    pub fn replaying(&self) -> bool {
        self.replay.is_some()
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
                // A replay seats who was here once its tape is in.
                Feed::Roll(bots) if let Some(replay) = &mut self.replay => {
                    // The listing's model is each agent's model now, which
                    // no event says again: an agent made in the replay
                    // takes it too, from the roll read once the tape is in.
                    let models: HashMap<&str, &str> = (bots.iter())
                        .filter_map(|bot| Some((bot["name"].as_str()?, bot["model"].as_str()?)))
                        .collect();
                    replay.remodel(&models);
                    replay.cast = bots;
                }
                Feed::Tape(event) => {
                    if let Some(replay) = &mut self.replay {
                        replay.keep(event);
                    }
                }
                Feed::Ready if self.replay.as_ref().is_some_and(|r| !r.ready) => {
                    self.live = true;
                    let at = self.replay.as_ref().map_or(0.0, |r| r.at);
                    if let Some(replay) = &mut self.replay {
                        replay.ready = true;
                        replay.tick = now;
                    }
                    self.seek(at, now);
                }
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
                Feed::Model(name, model) => {
                    // A replay seats agents again on a step back, from its
                    // cast and its tape: they take the new model too, as
                    // the listing gives every agent its model of now.
                    if let Some(replay) = &mut self.replay {
                        replay.remodel(&HashMap::from([(name.as_str(), model.as_str())]));
                    }
                    if let Some(at) = self.agent(&name)
                        && self.agents[at].model != model
                    {
                        self.agents[at].model = model;
                    }
                }
                Feed::Ready => self.live = true,
                Feed::Down(why) => self.down = Some(why),
            }
        }
        self.play(now);
        self.sweep();
        self.land(now);
    }

    /// Move a replay on by the time since the last frame, at its speed,
    /// and play what was written up to there.
    fn play(&mut self, now: Instant) {
        let Some(replay) = &mut self.replay else {
            return;
        };
        if !replay.ready {
            return;
        }
        if replay.playing {
            let ran = now.duration_since(replay.tick).as_secs_f64() * 1000.0;
            replay.at = (replay.at + ran * f64::from(replay.speed)).min(wall_ms() as f64);
        }
        replay.tick = now;
        self.played_to(now);
    }

    /// Play every event written by the replay's `at`, each stamped as
    /// long ago as it would have been, at this speed, had it played: so
    /// a jump leaves trails and counts as playing would, its sparks long
    /// landed.
    fn played_to(&mut self, now: Instant) {
        loop {
            let Some(replay) = &mut self.replay else {
                return;
            };
            let Some(event) = replay.tape.get(replay.played) else {
                return;
            };
            let written = event["written_ms"].as_f64().unwrap_or(replay.from as f64);
            if written > replay.at {
                return;
            }
            let ago = (replay.at - written) / f64::from(replay.speed) / 1000.0;
            let stamp = now
                .checked_sub(Duration::from_secs_f64(ago))
                .unwrap_or(self.opened.min(now));
            let age = Duration::from_secs_f64((replay.at - written).max(0.0) / 1000.0);
            let event = event.clone();
            replay.played += 1;
            self.written = Some(written as i64);
            self.history = Some((now, age));
            self.event(&event, stamp, true);
            (self.written, self.history) = (None, None);
        }
    }

    /// Go to `at` (Unix ms) in a replay: back means from the start again,
    /// who was here seated anew.
    fn seek(&mut self, at: f64, now: Instant) {
        let Some(replay) = &mut self.replay else {
            return;
        };
        let at = at.clamp(replay.from as f64, wall_ms() as f64);
        if !replay.ready {
            replay.at = at;
            return;
        }
        if at < replay.at || replay.played == 0 {
            replay.played = 0;
            // Who the tape makes waits for its making; a name used again
            // is someone else, told apart by id as `seat` tells them.
            let mut made: HashMap<&str, Vec<Option<i64>>> = HashMap::new();
            for event in &replay.tape {
                if matches!(event["event"].as_str(), Some("created" | "forked"))
                    && let Some(name) = event["bot"].as_str()
                {
                    made.entry(name)
                        .or_default()
                        .push(event["data"]["bot_id"].as_i64());
                }
            }
            let cast: Vec<Value> = (replay.cast.iter())
                .filter(|bot| {
                    let Some(name) = bot["name"].as_str() else {
                        return false;
                    };
                    let id = bot["bot_id"].as_i64();
                    !made.get(name).is_some_and(|ids| {
                        id.is_none() || ids.iter().any(|made| made.is_none() || *made == id)
                    })
                })
                .cloned()
                .collect();
            replay.at = at;
            self.forget();
            self.ticker.clear();
            self.recent.clear();
            self.calls.clear();
            for bot in &cast {
                // All idle: what they did then plays from the tape.
                if let Some(at) = self.seat(bot) {
                    self.agents[at].running = None;
                }
            }
        }
        if let Some(replay) = &mut self.replay {
            replay.at = at;
        }
        self.played_to(now);
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
        self.said = (self.said.iter())
            .filter_map(|&(when, a, b)| Some((when, moved[a]?, moved[b]?)))
            .collect();
        self.talks = (self.talks.iter())
            .filter_map(|&(a, b)| Some((moved[a]?, moved[b]?)))
            .collect();
        self.index = (self.agents.iter().enumerate())
            .map(|(at, agent)| (agent.name.clone(), at))
            .collect();
        self.laid = false;
    }

    fn land(&mut self, now: Instant) {
        // Each lands when its flight ends, in that order: a replay's jump
        // lands many at once, long ago.
        let mut landed = Vec::new();
        self.sparks.retain(|spark| {
            let done = now.duration_since(spark.born) >= FLIGHT;
            if done {
                landed.push((spark.born + FLIGHT, spark.to, spark.color));
            }
            !done
        });
        landed.sort_by_key(|&(at, ..)| at);
        for (when, to, color) in landed {
            if let End::Agent(at) = to {
                let agent = &mut self.agents[at];
                // A replay's jump can land an older message after the agent
                // sent a newer one: it adds its heat, decayed to the newer
                // time, and leaves that glow as it is.
                if let Some(later) = agent.heated.filter(|&heated| heated > when) {
                    let ago = later.duration_since(when).as_secs_f32();
                    agent.heat += 0.5f32.powf(ago / HALF_LIFE);
                    continue;
                }
                agent.lit = Some(when);
                agent.heat = agent.warmth(when) + 1.0;
                agent.heated = Some(when);
                agent.flash = color;
            }
        }
        let old = |at: &Instant| now.duration_since(*at) > HOUR;
        while self.recent.front().is_some_and(old) {
            self.recent.pop_front();
        }
        while self.calls.front().is_some_and(old) {
            self.calls.pop_front();
        }
        while self.said.front().is_some_and(|(at, ..)| old(at)) {
            if let Some((_, a, b)) = self.said.pop_front()
                && self.centre.is_some()
                && !(self.said.iter()).any(|&(_, x, y)| (x, y) == (a, b) || (x, y) == (b, a))
            {
                // A pair gone quiet moves the rings of a centred sky.
                self.laid = false;
            }
        }
    }

    fn seat(&mut self, bot: &Value) -> Option<usize> {
        let name = bot["name"].as_str()?;
        // A copy the daemon throws away once idle is no star of its own:
        // what one an agent made does is drawn on that agent, as the
        // approver's judgments are; Sonny's own, a guess at the next
        // message or a haiku, made by no agent, are not drawn at all.
        if !bot["expire_ms"].is_null() {
            if let Some(by) = bot["created_by"].as_str().filter(|by| !by.is_empty()) {
                let id = bot["created_by_id"].as_i64();
                self.copies.insert(name.to_owned(), (by.to_owned(), id));
            }
            return None;
        }
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
        let model = bot["model"].as_str().unwrap_or_default();
        if agent.model != model {
            agent.model = model.to_owned();
        }
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

    /// The agent a throwaway copy is drawn on: the one that made it, or
    /// that one's, should it be a copy too; by name, and by id when both
    /// are known, as a name used again is a stranger.
    fn copied(&self, name: &str) -> Option<usize> {
        let maker = |name: &str| {
            let (by, id) = self.copies.get(name)?;
            Some((by.as_str(), *id))
        };
        let same = |at: usize, id: Option<i64>| match (id, self.agents[at].id) {
            (Some(made_by), Some(id)) => made_by == id,
            _ => true,
        };
        let (by, id) = maker(name)?;
        match self.agent(by) {
            Some(at) => same(at, id).then_some(at),
            None => {
                let (by, id) = maker(by)?;
                self.agent(by).filter(|&at| same(at, id))
            }
        }
    }

    fn agent_or_copied(&self, name: &str) -> Option<usize> {
        self.agent(name).or_else(|| self.copied(name))
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
                if let Some(at) = self.seat(&made(name, data)) {
                    self.agents[at].lit = Some(now);
                }
                return;
            }
            "deleted" => {
                self.copies.remove(name);
                if let Some(at) = self.agent(name) {
                    self.agents[at].gone = true;
                    self.laid = false;
                }
                return;
            }
            _ => {}
        }
        let Some(at) = self.agent_or_copied(name) else {
            return;
        };
        // A copy lends its agent what it is told and the tools it calls,
        // not its turns' states: its agent works, waits or fails on its
        // own.
        let kind = event["event"].as_str().unwrap_or("");
        let lent = matches!(kind, "queued" | "accepted" | "steered" | "tool_started");
        let own = self.agent(name).is_some();
        if !own && !lent {
            return;
        }
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
                // A copy's turn is its own: its agent's state stays its.
                if kind == "accepted" && live && own {
                    let agent = &mut self.agents[at];
                    agent.running = Some(turn);
                    agent.sender = Some(from);
                    // A new turn tries again what failed.
                    agent.failed = false;
                }
                // What it asked is answered by the one it asked: the person,
                // or the agent that made it. A copy's prompt answers none of
                // its agent's questions.
                if kind != "queued" && live && own {
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
                    // A past turn of the last hour counts toward its pair's
                    // weight, at the time the daemon wrote it.
                    if !live
                        && let Some(written) = event["written_ms"].as_i64()
                        && let Some(ago) = wall_ms().checked_sub(written)
                        && let Some(when) =
                            now.checked_sub(Duration::from_millis(ago.max(0) as u64))
                        && now.duration_since(when) <= HOUR
                    {
                        self.hear(when, by, at);
                    }
                }
                if live {
                    if let End::Agent(by) = from
                        && by != at
                        && let Some(then) = self.then(now)
                    {
                        self.hear(then, by, at);
                    }
                    self.send(from, End::Agent(at), now);
                    let by = match from {
                        End::Person => "you".to_owned(),
                        End::Agent(by) => self.short(by),
                        End::Rim(_) => match data["person"].as_str() {
                            Some(person) => person.to_owned(),
                            None => {
                                crate::transcript::from_outside(data["origin"].as_str()).to_owned()
                            }
                        },
                    };
                    let to = self.short(at);
                    self.agents[at].last = Some((now, format!("from {by}")));
                    if let End::Agent(by) = from {
                        self.agents[by].last = Some((now, format!("to {to}")));
                    }
                    let about = match from {
                        End::Agent(by) => vec![by, at],
                        _ => vec![at],
                    };
                    self.say(now, format!("{by} → {to}"), None, &about);
                }
            }
            "tool_started" if live => {
                let tool = data["name"].as_str().unwrap_or("");
                // A call approved starts; a copy's calls are lent as a
                // twinkle, its asks and approvals staying its own.
                if let Some(call) = data["call_id"].as_str()
                    && own
                {
                    self.agents[at].held.remove(call);
                }
                self.agents[at].twinkle = Some((now, TOOL, Work::of(tool)));
                if let Some(then) = self.then(now) {
                    self.calls.push_back(then);
                }
                if tool == crate::asks::TOOL && own {
                    if self.agents[at].state() != Some(State::NeedsYou) {
                        let said = format!("? {} asks you", self.short(at));
                        self.say(now, said, Some(NEEDS), &[at]);
                    }
                    self.agents[at].asking = true;
                }
                if tool == "commons" && posted(&data["arguments"]) {
                    // As far as the post reaches: who has read the thread
                    // or posted in it. A post to an empty board stays small.
                    let audience = (thread(&data["arguments"]))
                        .zip(std::env::var_os("HOME"))
                        .map_or(0, |(thread, home)| {
                            crate::commons::audience(
                                std::path::Path::new(&home),
                                &thread,
                                &self.agents[at].name,
                            )
                        });
                    let reach = (0.08 + 0.04 * (1.0 + audience as f32).ln()).min(0.22);
                    self.agents[at].ripple = Some((now, reach));
                    self.agents[at].last = Some((now, "posted to the commons".into()));
                    let said = format!("{} posted to the commons", self.short(at));
                    self.say(now, said, None, &[at]);
                }
            }
            // A call waits for the person: one gated for them, or one the
            // reviewer handed them.
            "approval_requested" if live => {
                let waiting = self.agents[at].state() == Some(State::NeedsYou);
                for call in data["calls"].as_array().into_iter().flatten() {
                    let manual = (call["gates"].as_array())
                        .is_some_and(|gates| gates.iter().any(|g| g == "manual"));
                    if let (true, Some(id)) = (manual, call["call_id"].as_str()) {
                        self.agents[at].held.insert(id.to_owned());
                    }
                }
                self.waits(at, waiting, now);
            }
            "approval_asked" if live => {
                let waiting = self.agents[at].state() == Some(State::NeedsYou);
                if let Some(id) = data["call_id"].as_str() {
                    self.agents[at].held.insert(id.to_owned());
                }
                self.waits(at, waiting, now);
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
                    let said = format!("✗ {} failed", self.short(at));
                    self.say(now, said, Some(FAILED), &[at]);
                }
                // The answer to the person flies back to the middle.
                let agent = &mut self.agents[at];
                if live && agent.sender == Some(End::Person) && data["status"] == "completed" {
                    agent.lit = Some(now);
                    self.sparks.push(Spark {
                        from: End::Agent(at),
                        to: End::Person,
                        born: now,
                        color: Some(YOU),
                    });
                }
                self.agents[at].sender = None;
            }
            _ => {}
        }
    }

    /// Who sent a prompt: another agent, a client named by its origin or
    /// another person a client named (in from the rim), or the person.
    fn sender(&self, data: &Value, at: usize) -> End {
        if let Some(by) = data["from"]["bot"].as_str() {
            // A name used again is someone else: from outside, then.
            let id = data["from"]["bot_id"].as_i64();
            if let Some(by) = self.copied(by) {
                return End::Agent(by);
            }
            return (self.agent(by))
                .filter(|&by| {
                    id.is_none() || self.agents[by].id.is_none_or(|known| Some(known) == id)
                })
                .map_or(End::Rim(at), End::Agent);
        }
        match data["origin"].is_string() || data["person"].is_string() {
            true => End::Rim(at),
            false => End::Person,
        }
    }

    /// Remembers that `by` talked to `at` for the hour's weights. A pair new
    /// to the hour moves the rings of a centred sky, past turns as live ones.
    fn hear(&mut self, when: Instant, by: usize, at: usize) {
        let new = !(self.said.iter()).any(|&(_, a, b)| (a, b) == (by, at) || (a, b) == (at, by));
        self.laid = self.laid && !(new && self.centre.is_some());
        self.said.push_back((when, by, at));
    }

    /// A spark takes the hue of who is talking (see [`hue`]); where it starts
    /// and lands says who.
    fn send(&mut self, from: End, to: End, now: Instant) {
        // The sender bursts as the receiver will, in the message's colour.
        let color = hue(from, to);
        if let End::Agent(at) = from {
            let agent = &mut self.agents[at];
            agent.lit = Some(now);
            agent.heat = agent.warmth(now) + 1.0;
            agent.heated = Some(now);
            agent.flash = color;
        }
        self.sparks.push(Spark {
            from,
            to,
            born: now,
            color,
        });
        if let Some(then) = self.then(now) {
            self.recent.push_back(then);
        }
    }

    /// When what happens at `now` happened, for the hour's counts: `now`
    /// live; in a replay, as long before now as it was before the
    /// replay's now, or nothing once more than the hour back.
    fn then(&self, now: Instant) -> Option<Instant> {
        match self.history {
            None => Some(now),
            Some((real, age)) => (age <= HOUR).then(|| real.checked_sub(age)).flatten(),
        }
    }

    /// Told once when an agent comes to wait on the person's approval.
    fn waits(&mut self, at: usize, waiting: bool, now: Instant) {
        if !waiting && self.agents[at].state() == Some(State::NeedsYou) {
            let said = format!("? {} waits for you", self.short(at));
            self.say(now, said, Some(NEEDS), &[at]);
        }
    }

    fn say(&mut self, now: Instant, text: String, hue: Option<Color>, about: &[usize]) {
        let about = about
            .iter()
            .map(|&at| self.agents[at].name.clone())
            .collect();
        self.ticker.push_back(Note {
            at: now,
            text,
            hue,
            about,
            written: self.written,
        });
        if self.ticker.len() > LOG {
            self.ticker.pop_front();
        }
    }

    /// Messages sent in the last minute.
    fn minute(&self, now: Instant) -> usize {
        let minute = Duration::from_secs(60);
        (self.recent.iter().rev())
            .take_while(|at| now.duration_since(**at) <= minute)
            .count()
    }

    /// An agent's name as the sky labels it: a lead by its project, an
    /// agent of a project without the project's prefix.
    fn short(&self, at: usize) -> String {
        short(&self.agents[at].name)
    }

    /// Lay the agents out. The list always follows the tree of who made
    /// whom; the sky follows it too, or rings by hops round a centred
    /// agent. Stars already placed glide to their new places, so the eye
    /// keeps its bearings (Yee et al. 2001).
    fn lay(&mut self, now: Instant) {
        if self.laid {
            return;
        }
        self.laid = true;
        let count = self.agents.len();
        let (roots, children) = self.tree();
        let (mut tree, order) = radial(&roots, &children, count);
        // A crowd of leaves under one agent spreads inward, each by its
        // own name, so it reads as a cloud whose density says its size
        // rather than a hard ring; the spread is the same every time.
        for kids in children.iter().filter(|kids| kids.len() > CROWD) {
            for &kid in kids.iter().filter(|&&kid| children[kid].is_empty()) {
                if let Some(((x, y), _)) = &mut tree[kid] {
                    let pull = 1.0 - 0.32 * hashed(&self.agents[kid].name);
                    (*x, *y) = (*x * pull, *y * pull);
                }
            }
        }
        let mut arm = 0;
        for &(at, depth) in &order {
            if depth == 0 {
                arm = at;
            }
            self.agents[at].arm = arm;
        }
        self.order = order;
        for agent in &mut self.agents {
            agent.root = false;
        }
        for &root in &roots {
            self.agents[root].root = true;
        }
        let mut person = (0.0, 0.0);
        let places = match self.centred() {
            Some(centre) => {
                // The person is a node too, next to the leads they made.
                let you = count;
                let mut near: Vec<Vec<usize>> = vec![Vec::new(); count + 1];
                let mut link = |a: usize, b: usize| {
                    if a != b && !near[a].contains(&b) {
                        near[a].push(b);
                        near[b].push(a);
                    }
                };
                for (at, kids) in children.iter().enumerate() {
                    for &kid in kids {
                        link(at, kid);
                    }
                }
                for &root in &roots {
                    link(you, root);
                }
                for &(_, a, b) in &self.said {
                    link(a, b);
                }
                let name = |at: &usize| match self.agents.get(*at) {
                    Some(agent) => (agent.name.clone(), 0),
                    None => (String::new(), 1),
                };
                for list in &mut near {
                    list.sort_by_key(name);
                }
                // Breadth first from the centre: each ring one hop further.
                let mut kids: Vec<Vec<usize>> = vec![Vec::new(); count + 1];
                let mut seen = vec![false; count + 1];
                seen[centre] = true;
                let mut queue = VecDeque::from([centre]);
                while let Some(at) = queue.pop_front() {
                    for &next in &near[at] {
                        if !seen[next] && next >= count || !seen[next] && !self.agents[next].gone {
                            seen[next] = true;
                            kids[at].push(next);
                            queue.push_back(next);
                        }
                    }
                }
                let (mut places, _) = radial(&kids[centre], &kids, count + 1);
                places[centre] = Some(((0.0, 0.0), 0.0));
                if let Some((at, _)) = places[you] {
                    person = at;
                }
                places
            }
            None => tree,
        };
        let first = self.moved.is_none() && self.agents.iter().all(|a| a.goal.is_none());
        for (at, agent) in self.agents.iter_mut().enumerate() {
            let Some(Some((goal, angle))) = places.get(at).copied() else {
                continue;
            };
            agent.from = match agent.goal {
                Some(_) => agent.at,
                None => goal,
            };
            agent.goal = Some(goal);
            agent.angle = angle;
            if agent.from == goal {
                agent.at = goal;
            }
        }
        self.person.from = self.person.at;
        self.person.goal = person;
        if !first {
            self.moved = Some(now);
        } else {
            self.person = Glide {
                at: person,
                from: person,
                goal: person,
            };
        }
        self.glide(now);
    }

    /// Who made whom: the roots (leads first, then by name) and each
    /// agent's children. A cycle of makers has no root: its agents are
    /// roots then.
    fn tree(&self) -> (Vec<usize>, Vec<Vec<usize>>) {
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
        // In the order they started, by the daemon's ids, so a new agent
        // takes the next place rather than reshuffling the rest (Misue et
        // al. 1995: keep the mental map). Roots go by when their project
        // started, its lead first.
        let project = |at: usize| {
            let name = &self.agents[at].name;
            name.split('.').next().unwrap_or(name).to_owned()
        };
        let started = |at: usize| {
            (
                self.agents[at].id.unwrap_or(i64::MAX),
                self.agents[at].name.clone(),
            )
        };
        let mut first: HashMap<String, (i64, String)> = HashMap::new();
        for at in (0..count).filter(|&at| !self.agents[at].gone) {
            let when = started(at);
            first
                .entry(project(at))
                .and_modify(|was| *was = (*was).clone().min(when.clone()))
                .or_insert(when);
        }
        roots.sort_by_key(|&at| {
            (
                first[&project(at)].clone(),
                !project::is_lead(&self.agents[at].name),
                started(at),
            )
        });
        for kids in &mut children {
            kids.sort_by_key(|&at| started(at));
        }
        let mut seen = vec![false; count];
        let mut stack = roots.clone();
        while let Some(at) = stack.pop() {
            if !std::mem::replace(&mut seen[at], true) {
                stack.extend(&children[at]);
            }
        }
        for (at, agent) in self.agents.iter().enumerate() {
            if !seen[at] && !agent.gone {
                roots.push(at);
                let mut stack = vec![at];
                while let Some(at) = stack.pop() {
                    if !std::mem::replace(&mut seen[at], true) {
                        stack.extend(&children[at]);
                    }
                }
            }
        }
        (roots, children)
    }

    /// Move each star along its way to its new place, easing in and out.
    fn glide(&mut self, now: Instant) {
        let t = self.moved.map_or(1.0, |at| {
            (now.duration_since(at).as_secs_f32() / GLIDE.as_secs_f32()).min(1.0)
        });
        let e = ease(t);
        let mix = |from: (f32, f32), goal: (f32, f32)| {
            (
                from.0 + (goal.0 - from.0) * e,
                from.1 + (goal.1 - from.1) * e,
            )
        };
        for agent in &mut self.agents {
            if let Some(goal) = agent.goal {
                agent.at = mix(agent.from, goal);
            }
        }
        self.person.at = mix(self.person.from, self.person.goal);
    }

    /// The agent in the middle: the one asked for by its name, else by
    /// its label, else the lead of the project named.
    fn centred(&self) -> Option<usize> {
        let want = self.centre.as_deref()?;
        let lead = format!("{want}.lead");
        self.agent(want)
            .or_else(|| {
                (0..self.agents.len())
                    .find(|&at| !self.agents[at].gone && short(&self.agents[at].name) == want)
            })
            .or_else(|| self.agent(&lead))
    }

    /// Which agents draw as stars: all when they fit in `room`, else the
    /// most interesting first. Interest is what a glance can act on, then
    /// what happens, then place: needs you 1000, failed 500, working 40,
    /// 30 for a glow just lit, 60 for a lead. What needs the person or
    /// failed, and `always`, draw whatever the room.
    fn shown(&self, now: Instant, room: usize, always: &[Option<usize>]) -> Vec<bool> {
        let count = self.agents.len();
        let live = self.agents.iter().filter(|a| !a.gone).count();
        if live <= room {
            return vec![true; count];
        }
        let interest = |at: usize| {
            let agent = &self.agents[at];
            let state = match agent.state() {
                Some(State::NeedsYou) => 1000.0,
                Some(State::Failed) => 500.0,
                Some(State::Working) => 40.0,
                None => 0.0,
            };
            state
                + 30.0 * agent.glow(now).max(agent.warmth(now).min(1.0))
                + if agent.root { 60.0 } else { 0.0 }
        };
        let mut ranked: Vec<(f32, usize)> = (0..count)
            .filter(|&at| !self.agents[at].gone)
            .map(|at| (interest(at), at))
            .collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut shown = vec![false; count];
        for (n, (score, at)) in ranked.into_iter().enumerate() {
            shown[at] = n < room || score >= 500.0;
        }
        for at in always.iter().flatten() {
            shown[*at] = true;
        }
        shown
    }

    /// Put the agent asked for in the middle, or the person again.
    pub fn centre_on(&mut self, name: Option<String>) {
        if self.centre != name {
            self.centre = name;
            self.laid = false;
        }
    }

    /// A key the sky takes: the arrows pick a star, Enter centres on it,
    /// `?` the legend, `z` zen, `m` the models' colours. Esc lets go of the
    /// pick, then of the centre; false when there was nothing to let go
    /// of, or the key isn't the sky's.
    pub fn press(&mut self, code: ratatui::crossterm::event::KeyCode) -> bool {
        use ratatui::crossterm::event::KeyCode;
        let now = Instant::now();
        if let Some(replay) = &mut self.replay {
            match code {
                KeyCode::Char(' ') => {
                    replay.playing = !replay.playing;
                    replay.tick = now;
                    return true;
                }
                KeyCode::Char('[') => {
                    replay.speed = (replay.speed / 2.0).max(1.0);
                    return true;
                }
                KeyCode::Char(']') => {
                    replay.speed = (replay.speed * 2.0).min(REPLAY_FASTEST);
                    return true;
                }
                KeyCode::Left | KeyCode::Right => {
                    let step = REPLAY_STEP * f64::from(replay.speed);
                    let at = match code {
                        KeyCode::Left => replay.at - step,
                        _ => replay.at + step,
                    };
                    self.seek(at, now);
                    return true;
                }
                KeyCode::Esc if self.pick.is_none() && self.centre.is_none() => {
                    self.replay(None);
                    return true;
                }
                _ => {}
            }
        }
        match code {
            KeyCode::Char('r') if self.replay.is_none() => {
                self.replay(Some(wall_ms() - REPLAY_BACK))
            }
            KeyCode::Char('?') => self.legend = !self.legend,
            KeyCode::Char('z') => self.zen = !self.zen,
            KeyCode::Char('m') => self.models = !self.models,
            KeyCode::Up if self.listing => self.step_list(false),
            KeyCode::Down if self.listing => self.step_list(true),
            KeyCode::Left | KeyCode::Right if self.listing => {}
            KeyCode::Up => self.step((0.0, -1.0)),
            KeyCode::Down => self.step((0.0, 1.0)),
            KeyCode::Left => self.step((-1.0, 0.0)),
            KeyCode::Right => self.step((1.0, 0.0)),
            KeyCode::Enter => match self.pick.take() {
                Some(pick) => self.centre_on(Some(pick)),
                None => return false,
            },
            KeyCode::Esc if self.pick.is_some() => self.pick = None,
            KeyCode::Esc if self.centre.is_some() => self.centre_on(None),
            _ => return false,
        }
        true
    }

    /// Pick the next agent down the list, or up it.
    fn step_list(&mut self, down: bool) {
        let listed: Vec<usize> = (self.order.iter())
            .map(|&(at, _)| at)
            .filter(|&at| !self.agents[at].gone)
            .collect();
        let here = (self.pick.as_deref())
            .and_then(|name| self.agent(name))
            .and_then(|at| listed.iter().position(|&listed| listed == at));
        let next = match (here, down) {
            (None, true) => 0,
            (None, false) => listed.len().saturating_sub(1),
            (Some(here), true) => (here + 1).min(listed.len().saturating_sub(1)),
            (Some(here), false) => here.saturating_sub(1),
        };
        if let Some(&at) = listed.get(next) {
            self.pick = Some(self.agents[at].name.clone());
        }
    }

    /// Pick the star nearest the picked one, or the middle, that way.
    fn step(&mut self, way: (f32, f32)) {
        let from = (self.pick.as_deref())
            .and_then(|name| self.agent(name))
            .map_or((0.0, 0.0), |at| self.agents[at].at);
        let picked = self.pick.as_deref().and_then(|name| self.agent(name));
        let best = (0..self.agents.len())
            .filter(|&at| !self.agents[at].gone && Some(at) != picked)
            .filter_map(|at| {
                let to = self.agents[at].at;
                let (dx, dy) = (to.0 - from.0, to.1 - from.1);
                let along = dx * way.0 + dy * way.1;
                let across = (dx * way.1 - dy * way.0).abs();
                (along > 0.01).then_some((along + 2.0 * across, at))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((_, at)) = best {
            self.pick = Some(self.agents[at].name.clone());
        }
    }

    /// The sky `cols` wide and `rows` high, what happened under it.
    pub fn card(&mut self, cols: usize, rows: usize) -> Vec<Line<'static>> {
        let now = Instant::now();
        self.lay(now);
        self.glide(now);
        let cols = cols.max(20);
        let mut lines = Vec::new();
        let shown = self.agents.iter().filter(|a| !a.gone).count();
        if shown == 0 {
            lines.push(Line::from(match (&self.down, self.live) {
                (Some(why), _) => format!("Couldn't follow the agents: {why}"),
                (None, false) if self.replay.is_some() => "Reading what happened…".into(),
                _ => "No agents yet.".into(),
            }));
            return lines;
        }
        // The list's rows say what the ticker would, agent by agent.
        self.listing = cols < SKY_COLS || rows < SKY_ROWS;
        if self.listing {
            let under = match (self.zen, self.legend) {
                (true, _) => 0,
                (false, true) => LEGEND,
                (false, false) => 2,
            };
            lines.extend(self.list(now, cols, rows.saturating_sub(under).max(3)));
            // The list takes the rows; the log, its newest line, the one
            // kept for it under the list's blank.
            if !self.zen {
                lines.extend(match self.legend {
                    true => legend(true),
                    false => self.ticked(now, cols, under - 1, false),
                });
            }
            return lines;
        }
        // The chat log beside the sky where the sky has width to spare,
        // else under it; in zen, neither.
        let beside =
            !self.zen && cols >= SKY_COLS + LOG_COLS + GAP && cols >= 2 * rows + LOG_COLS + GAP;
        let (sky_cols, sky_rows) = match (self.zen, beside) {
            (true, _) => (cols, rows),
            (false, true) => (cols - LOG_COLS - GAP, rows),
            (false, false) => (cols, rows.saturating_sub(TICKER + 3).clamp(8, cols / 2 + 1)),
        };
        let mut sky = Sky::new(sky_cols, sky_rows);
        let centre = self.centred();
        let picked = self.pick.as_deref().and_then(|name| self.agent(name));
        // At scale, the sky draws its most interesting agents as stars,
        // as many as the pane has room for, and folds the rest into a
        // faint cloud on their arm (Furnas' degree of interest). What
        // needs the person or failed always draws.
        let room = (sky_cols * sky_rows / 24).max(8);
        let shown = self.shown(now, room, &[picked, centre]);
        // The person: the place and the word say who. In the middle, or
        // on a ring when an agent is centred.
        sky.text_at(self.person.at, "you", Style::default().bold());
        // Who made whom: dotted lines, dim, as all that says how things
        // stand rather than what happens.
        for (at, agent) in self.agents.iter().enumerate().filter(|(_, a)| !a.gone) {
            if let Some(by) = self.maker(at)
                && !agent.root
                && shown[at]
            {
                sky.line(self.agents[by].at, agent.at, Ink::Muted, 3);
            }
        }
        // Who talked: a curve for each pair that has, past turns and live,
        // as in 0.7.0; told both ways, two, as each bends its own way. The
        // more a pair talked this hour the heavier its curve: denser, and
        // plain for the busiest, so where the work is shows.
        let mut hour: HashMap<(usize, usize), usize> = HashMap::new();
        for &(when, by, to) in self.said.iter().rev() {
            if now.duration_since(when) > HOUR {
                break;
            }
            *hour.entry((by.min(to), by.max(to))).or_default() += 1;
        }
        let busiest = hour.values().copied().max().unwrap_or(1);
        for &(by, to) in &self.talks {
            if !(shown[by] && shown[to]) || self.agents[by].gone || self.agents[to].gone {
                continue;
            }
            let n = hour.get(&(by.min(to), by.max(to))).copied().unwrap_or(0);
            let share = n as f32 / busiest as f32;
            // Every 4th dot for a pair quiet this hour, down to every 2nd.
            let gap = 4 - (2.0 * share.sqrt()).round() as usize;
            let ink = if share > 0.6 && n >= 4 {
                Ink::Plain
            } else {
                Ink::Muted
            };
            let (a, b) = (self.agents[by].at, self.agents[to].at);
            sky.curve(a, bend(a, b), b, ink, gap);
        }
        // Ripples and flashes, then sparks, then the stars over them.
        for (_, agent) in (self.agents.iter().enumerate()).filter(|(at, a)| !a.gone && shown[*at]) {
            if let Some((at, reach)) = agent.ripple {
                let t = now.duration_since(at).as_secs_f32() / RIPPLE.as_secs_f32();
                if t < 1.0 {
                    // A commons post is the quietest event: nobody need act.
                    let ink = if t < 0.5 { Ink::Muted } else { Ink::Faint };
                    sky.ring(agent.at, 0.03 + (reach - 0.03) * t, ink);
                }
            }
            // A message landing bursts round its star, then shrinks into it
            // as it fades, as in 0.7.0; how big it bursts is how hot the
            // agent is.
            let glow = agent.heated.map_or(0.0, |at| {
                0.5f32.powf(now.duration_since(at).as_secs_f32() / HALF_LIFE)
            });
            if glow > 0.08 {
                let ink = match agent.flash {
                    Some(color) => Ink::Color(color, glow > 0.4),
                    None if glow > 0.4 => Ink::Bright,
                    None => Ink::Plain,
                };
                let doublings = agent.heat.max(1.0).log2();
                let burst = (FLASH * FLASH_GROWTH.powf(doublings)).min(FLASH_MAX);
                sky.circle(agent.at, burst * (0.25 + 0.75 * glow), ink);
            }
        }
        // The newest dozen in flight: more is a blur, not news.
        for spark in self.sparks.iter().rev().take(SPARKS) {
            let t = (now.duration_since(spark.born).as_secs_f32() / FLIGHT.as_secs_f32()).min(1.0);
            let (a, b) = (self.end(spark.from), self.end(spark.to));
            let control = match (spark.from, spark.to) {
                (End::Agent(_), End::Agent(_)) => bend(a, b),
                _ => ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0),
            };
            for step in 0..6 {
                let back = t - step as f32 * 0.035;
                if back < 0.0 {
                    break;
                }
                let at = bezier(a, control, b, ease(back));
                let ink = match spark.color {
                    Some(color) => Ink::Color(color, step < 3),
                    None if step < 3 => Ink::Bright,
                    None => Ink::Plain,
                };
                match step {
                    0 => sky.blob(at, ink),
                    _ => sky.dot(at, ink),
                }
            }
        }
        let shade = Shade::now();
        let hues: HashMap<String, Color> = self.hues().into_iter().collect();
        // The folded: a faint dot each, so a cloud's density says its size,
        // and a count on each arm.
        let mut folded: HashMap<usize, (usize, (f32, f32))> = HashMap::new();
        for (at, agent) in self.agents.iter().enumerate() {
            if agent.gone || shown[at] {
                continue;
            }
            sky.dot(agent.at, Ink::Faint);
            let cloud = folded.entry(agent.arm).or_insert((0, (0.0, 0.0)));
            cloud.0 += 1;
            cloud.1 = (cloud.1.0 + agent.at.0, cloud.1.1 + agent.at.1);
        }
        for (_, agent) in (self.agents.iter().enumerate()).filter(|(at, a)| !a.gone && shown[*at]) {
            if let Some((at, color, work)) = agent.twinkle
                && now.duration_since(at) < TWINKLE
            {
                sky.twinkle(agent.at, work, Ink::Color(color, true));
            }
            // What needs the person or failed, still in its colour; a
            // working star breathes. As in 0.7.0, those are blobs, and so
            // is one a message lights; one at rest is a single dot.
            // On `m`, each in its model's colour instead, bold while it
            // works.
            let hue = (self.models)
                .then(|| hues.get(agent.model_or_unknown()).copied())
                .flatten();
            let (ink, big) = match (hue, agent.state()) {
                (Some(hue), state) => {
                    let big = state.is_some() || agent.glow(now) > 0.4;
                    (Ink::Color(hue, big), big)
                }
                (None, Some(State::Working)) => {
                    (shade.breathe(agent.breath(now, self.opened)), true)
                }
                (None, _) => (Ink::Plain, agent.glow(now) > 0.4),
            };
            sky.star(agent.at, ink, big);
        }
        // Names: every star's that has room, leads and what works or was
        // just lit before the quiet, nearest the middle first, never over
        // each other, so no line runs to a star no one can name. In zen,
        // only what needs the person or failed.
        let active = |agent: &Agent| {
            agent.root
                || agent.state().is_some()
                || (agent.lit).is_some_and(|lit| now.duration_since(lit) < Duration::from_secs(4))
        };
        let mut named: Vec<usize> = (0..self.agents.len())
            .filter(|&at| {
                let agent = &self.agents[at];
                if agent.gone || !shown[at] {
                    return false;
                }
                if self.zen {
                    return matches!(agent.state(), Some(State::NeedsYou | State::Failed));
                }
                true
            })
            .collect();
        for at in picked.into_iter().chain(centre) {
            if !named.contains(&at) {
                named.push(at);
            }
        }
        // The pick, the centre, then what needs the person, so none is
        // ever crowded out; leads of the biggest projects before the rest.
        let mut sizes: HashMap<usize, usize> = HashMap::new();
        for agent in self.agents.iter().filter(|agent| !agent.gone) {
            *sizes.entry(agent.arm).or_default() += 1;
        }
        named.sort_by_key(|&at| {
            let agent = &self.agents[at];
            (
                Some(at) != picked,
                Some(at) != centre,
                agent.state() != Some(State::NeedsYou),
                !active(agent),
                !agent.root,
                std::cmp::Reverse(agent.root.then(|| sizes.get(&agent.arm).copied())),
                (agent.at.0.hypot(agent.at.1) * 100.0) as i32,
            )
        });
        // Every star named that has room, in this order, a row clear of
        // the next name: what needs the person and finds none is counted
        // on its arm instead.
        let mut unnamed: BTreeMap<usize, usize> = BTreeMap::new();
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
            let style = match (Some(at) == picked, Some(at) == centre) {
                // The pick reversed, as a picker's selection is.
                (true, _) => Style::default().reversed(),
                (_, true) => Style::default().bold(),
                _ => name_style(agent.state(), agent.glow(now)),
            };
            if !sky.label(agent.at, mark, &short(&agent.name), style)
                && agent.state() == Some(State::NeedsYou)
            {
                *unnamed.entry(agent.arm).or_default() += 1;
            }
        }
        for (arm, n) in unnamed {
            let mark = Some(("?", Style::default().fg(NEEDS).bold()));
            sky.label(
                self.agents[arm].at,
                mark,
                &n.to_string(),
                Style::default().bold(),
            );
        }
        // The biggest clouds are counted, as many as a sixth of the rows.
        if !self.zen {
            let mut clouds: Vec<_> = folded.into_iter().collect();
            clouds.sort_by_key(|&(arm, (n, _))| (std::cmp::Reverse(n), arm));
            for (_, (n, (x, y))) in clouds.into_iter().take((sky_rows / 6).max(2)) {
                let middle = (x / n as f32, y / n as f32);
                sky.label(middle, None, &format!("+{n}"), Ink::Muted.style());
            }
        }
        let drawn = sky.lines();
        if beside {
            let column = match self.legend {
                true => self.key(now, rows),
                false => self.ticked(now, LOG_COLS, rows, true),
            };
            for (row, mut line) in drawn.into_iter().enumerate() {
                line.spans.push(Span::raw(" ".repeat(GAP)));
                if let Some(side) = column.get(row) {
                    line.spans.extend(side.spans.iter().cloned());
                }
                lines.push(line);
            }
            return lines;
        }
        lines.extend(drawn);
        if !self.zen {
            lines.push(Line::default());
            lines.extend(match self.legend {
                true => legend(false),
                false => self.ticked(now, cols, TICKER, false),
            });
        }
        lines
    }

    /// The chat log's lines, `room` of them at most and `cols` wide: who
    /// told whom and how long ago, what needs the person or failed in its
    /// hue. In a column of its own, `tall`, it fills the column with the
    /// newest at the bottom, the newest few plain, older ones muted, the
    /// oldest faint, and a line just come in fades up from faint. Or that
    /// the follow is lost, or nothing has happened yet.
    fn ticked(&self, now: Instant, cols: usize, room: usize, tall: bool) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        // Centred, only what the agent in the middle said or was told.
        let centre = self.centred().map(|at| &self.agents[at].name);
        let kept: Vec<_> = (self.ticker.iter())
            .filter(|note| centre.is_none_or(|name| note.about.contains(name)))
            .collect();
        // Beside the sky, as many lines as its fade carries, at the foot.
        let lines_kept = if tall { room.min(LOG_LINES) } else { room };
        let shown = kept
            .len()
            .min(lines_kept.saturating_sub(usize::from(self.down.is_some())));
        let entries = &kept[kept.len() - shown..];
        for (
            n,
            Note {
                at,
                text,
                hue,
                written,
                ..
            },
        ) in entries.iter().copied().enumerate()
        {
            let old = shown - 1 - n;
            let fresh = now.duration_since(*at).as_secs_f32();
            let weight = match (tall, old) {
                _ if fresh < 0.3 => Ink::Faint,
                _ if fresh < 0.6 => Ink::Muted,
                (false, _) | (true, 0..=2) => Ink::Plain,
                (true, 3..=8) => Ink::Muted,
                (true, _) => Ink::Faint,
            };
            let words = match (hue, weight) {
                (Some(hue), Ink::Plain | Ink::Muted) => Style::default().fg(*hue),
                _ => weight.style(),
            };
            let words = if tall && old == 0 && fresh >= 0.6 {
                words.bold()
            } else {
                words
            };
            let age = match weight {
                Ink::Faint => Ink::Faint.style(),
                _ => Ink::Muted.style(),
            };
            // A replay says when it happened, as a chat log's time does.
            let when = match written {
                Some(written) => format!("{} ", clock(*written)),
                None => format!("{:>4}  ", ago(now, *at)),
            };
            lines.push(Line::from(vec![
                Span::styled(when, age),
                Span::styled(cut(text, cols.saturating_sub(6)), words),
            ]));
        }
        if let Some(why) = &self.down {
            lines.push(Line::from(cut(
                &format!("Couldn't follow the agents: {why}"),
                cols,
            )));
        } else if kept.is_empty() {
            let quiet = if self.replay.is_some() && self.live {
                "Quiet then. What was said plays here."
            } else if self.live {
                "Quiet. Messages light up here as they go."
            } else {
                "Catching up with who is here…"
            };
            // A narrow column wraps the words, as a hint read whole; dim on
            // the words, as the column keeps a span's style, not a line's.
            lines.extend(
                wrapped(quiet, cols)
                    .into_iter()
                    .map(|line| Line::from(Span::from(line).dim())),
            );
        }
        if tall {
            let blank = room.saturating_sub(lines.len());
            lines.splice(0..0, std::iter::repeat_n(Line::default(), blank));
        }
        lines
    }

    /// The legend and the counts, in the chat log's column: what each mark
    /// means, drawn as the sky draws it, then how many projects, leads and
    /// agents there are, how much was said and called, and the busiest
    /// pair.
    fn key(&self, now: Instant, rows: usize) -> Vec<Line<'static>> {
        let says = |text: &str| Span::raw(text.to_owned()).dim();
        let mut lines = vec![Line::from(says("legend")), Line::default()];
        let row = |mark: Span<'static>, words: Vec<Span<'static>>| {
            let mut spans = vec![Span::raw(" "), mark];
            spans.extend(words);
            Line::from(spans)
        };
        lines.extend([
            row(Span::raw("⠂"), vec![says("   an agent, at rest")]),
            row(Span::raw("⠛").bold(), vec![says("   working: it breathes")]),
            row(
                Span::raw("?").fg(NEEDS),
                vec![says("   by a name: it needs you")],
            ),
            row(
                Span::raw("✗").fg(FAILED),
                vec![says("   by a name: it failed")],
            ),
            row(
                Span::raw("⠶").fg(AGENTS).bold(),
                vec![says("   a message between agents")],
            ),
            row(
                Span::raw("⠶").fg(YOU).bold(),
                vec![says("   to or from you")],
            ),
            row(
                Span::raw("○").fg(AGENTS),
                vec![says("   a burst: the busier, the bigger")],
            ),
            row(
                Span::raw("⠤⠤").fg(TOOL),
                vec![
                    says("  shell  "),
                    Span::raw("⡇").fg(TOOL),
                    says(" files  "),
                    Span::raw("⡁⢈").fg(TOOL),
                    says(" web"),
                ],
            ),
            row(
                Span::raw("✳").fg(TOOL),
                vec![
                    says("   another tool  "),
                    Span::raw("✳").fg(FAILED),
                    says(" it failed"),
                ],
            ),
            row(
                Span::styled("◯", Ink::Muted.style()),
                vec![says("   a commons post, by its reach")],
            ),
            row(
                Span::styled("⠁⠈", Ink::Muted.style()),
                vec![says("  who made whom, who talked")],
            ),
            Line::default(),
            Line::from(says("now")),
            Line::default(),
        ]);
        let shown: Vec<&Agent> = self.agents.iter().filter(|a| !a.gone).collect();
        let leads = shown.iter().filter(|a| project::is_lead(&a.name)).count();
        let projects: HashSet<&str> = (shown.iter())
            .filter_map(|a| a.name.split_once('.').map(|(project, _)| project))
            .collect();
        let hour = if now.duration_since(self.opened) >= HOUR {
            "this hour"
        } else {
            "so far"
        };
        let minute = Duration::from_secs(60);
        let calls = (self.calls.iter())
            .filter(|at| now.duration_since(**at) <= minute)
            .count();
        let mut pairs: HashMap<(usize, usize), usize> = HashMap::new();
        for &(_, by, to) in &self.said {
            *pairs.entry((by.min(to), by.max(to))).or_default() += 1;
        }
        let busiest = (pairs.into_iter())
            .filter(|((a, b), _)| !self.agents[*a].gone && !self.agents[*b].gone)
            .max_by_key(|&((a, b), n)| (n, std::cmp::Reverse((a, b))))
            .map(|((a, b), _)| format!("{} ⇄ {}", self.short(a), self.short(b)));
        let mut stat = |what: &str, value: String| {
            lines.push(Line::from(vec![
                Span::raw(" "),
                says(&format!("{what:<14}")),
                Span::raw(cut(&value, LOG_COLS - 15)),
            ]));
        };
        stat("projects", projects.len().to_string());
        stat("leads", leads.to_string());
        stat("agents", shown.len().to_string());
        stat("messages", format!("{} a minute", self.minute(now)));
        stat("", format!("{} {hour}", self.recent.len()));
        stat("tool calls", format!("{calls} a minute"));
        stat("", format!("{} {hour}", self.calls.len()));
        if let Some(busiest) = busiest {
            stat("busiest pair", busiest);
        }
        lines.truncate(rows);
        lines
    }

    /// Each model's colour on `m`: the most agents' first, then by name,
    /// a hue each while they last; the rest share the text's.
    fn hues(&self) -> Vec<(String, Color)> {
        let mut models: HashMap<&str, usize> = HashMap::new();
        for agent in self.agents.iter().filter(|a| !a.gone) {
            *models.entry(agent.model_or_unknown()).or_default() += 1;
        }
        let mut models: Vec<(&str, usize)> = models.into_iter().collect();
        models.sort_by_key(|&(model, n)| (std::cmp::Reverse(n), model));
        (models.into_iter())
            .map(|(model, _)| model.to_owned())
            .zip(
                MODEL_HUES
                    .iter()
                    .copied()
                    .chain(std::iter::repeat(Color::Reset)),
            )
            .collect()
    }

    fn end(&self, end: End) -> (f32, f32) {
        match end {
            End::Person => self.person.at,
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
        let picked = self.pick.as_deref().and_then(|name| self.agent(name));
        let rank = |at: usize| {
            let agent = &self.agents[at];
            let lit = agent.glow(now) > 0.08;
            if Some(at) == picked {
                return -1;
            }
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
            let firefly = match glow {
                g if g > 0.4 => Span::raw("✦").bold(),
                g if g > 0.08 => Span::raw("•"),
                _ => Span::raw("·").dim(),
            };
            let mark = match agent.state() {
                Some(state) => {
                    let (mark, color, _) = state.mark();
                    Span::raw(mark).fg(color)
                }
                None => Span::raw(" "),
            };
            let style = match Some(at) == picked {
                // The pick reversed, as in the sky.
                true => Style::default().reversed(),
                false => name_style(agent.state(), glow),
            };
            let name = Span::styled(short(&agent.name), style);
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
        if let Some(replay) = &self.replay {
            return replay_line(replay, width);
        }
        let shown: Vec<&Agent> = self.agents.iter().filter(|a| !a.gone).collect();
        let count = shown.len();
        let mut spans = vec![
            Span::raw(format!(
                "  {count} {}",
                if count == 1 { "agent" } else { "agents" }
            ))
            .dim(),
        ];
        // On `m`, which colour is which model, as many as fit.
        if self.models {
            for (model, hue) in self.hues() {
                let key = format!(" ⠛ {model}");
                let used: usize = spans.iter().map(|span| span.content.width()).sum();
                if used + 3 + key.width() > width {
                    break;
                }
                spans.push(Span::raw(" ·").dim());
                spans.push(Span::raw(" ⠛").fg(hue));
                spans.push(Span::raw(format!(" {model}")).dim());
            }
        }
        let states = match self.models {
            true => &[][..],
            false => &[State::NeedsYou, State::Working, State::Failed][..],
        };
        for &state in states {
            let n = shown.iter().filter(|a| a.state() == Some(state)).count();
            if n > 0 || state == State::Working {
                let (mark, color, word) = state.mark();
                let word = if state == State::NeedsYou && n != 1 {
                    "need you"
                } else {
                    word
                };
                spans.push(Span::raw(" · ").dim());
                spans.push(Span::raw(mark).fg(color));
                spans.push(Span::raw(format!(" {n} {word}")).dim());
            }
        }
        // Who is in the middle.
        let mut seen = String::new();
        if let Some(at) = self.centred() {
            seen.push_str(&format!(" · around {}", self.short(at)));
        }
        let used: usize = spans.iter().map(|span| span.content.width()).sum();
        if !seen.is_empty() && used + seen.width() <= width {
            spans.push(Span::raw(seen).dim());
        }
        let sent = self.minute(Instant::now());
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

/// A replay's footer: playing or paused, its speed, the time it has
/// reached, and a bar from where it started to now.
fn replay_line(replay: &Replay, width: usize) -> Line<'static> {
    let mut spans = vec![Span::raw("  replay").dim()];
    if !replay.ready {
        spans.push(Span::raw(format!(" · reading from {}…", clock(replay.from))).dim());
        return Line::from(spans);
    }
    let mark = if replay.playing { "▶" } else { "‖" };
    spans.push(Span::raw(format!("  {mark} {}×  ", replay.speed)));
    let now = wall_ms() as f64;
    let caught_up = replay.at >= now - 1000.0;
    spans.push(Span::raw(if caught_up {
        "now".to_owned()
    } else {
        clock(replay.at as i64)
    }));
    let used: usize = spans.iter().map(|span| span.content.width()).sum();
    let room = width.saturating_sub(used + 2 + 5);
    if room >= 8 {
        let span = (now - replay.from as f64).max(1.0);
        let done = (((replay.at - replay.from as f64) / span) * (room - 1) as f64).round() as usize;
        let done = done.min(room - 1);
        spans.push(Span::raw("  "));
        spans.push(Span::styled("━".repeat(done), Ink::Muted.style()));
        spans.push(Span::styled("●", Ink::Plain.style()));
        spans.push(Span::styled(
            "─".repeat(room - 1 - done),
            Ink::Faint.style(),
        ));
        spans.push(Span::raw("  now").dim());
    }
    Line::from(spans)
}

/// The legend's rows, the ticker's room.
const LEGEND: usize = TICKER;

/// What the sky's marks mean, each drawn as the sky draws it: the states,
/// the sparks, a tool's twinkle by kind, a commons post and the lines; in
/// the list, the firefly in the sparks' place.
fn legend(list: bool) -> Vec<Line<'static>> {
    let mark = |text: &str, color: Color| Span::raw(text.to_owned()).fg(color);
    let says = |text: &str| Span::raw(text.to_owned()).dim();
    let mut lines = vec![Line::from(vec![
        Span::raw("⠛").bold(),
        says(" working breathes  "),
        mark("?", NEEDS),
        says(" needs you  "),
        mark("✗", FAILED),
        says(" failed  "),
        Span::raw("name").bold(),
        says(" works, "),
        says("name"),
        says(" idle"),
    ])];
    if list {
        lines.push(Line::from(vec![
            Span::raw("✦").bold(),
            says(" just talked  "),
            Span::raw("•"),
            says(" a while ago  "),
            says("· quiet"),
        ]));
        return lines;
    }
    lines.extend([
        Line::from(vec![
            mark("⠶", AGENTS),
            says(" between agents  "),
            mark("⠶", YOU),
            says(" with you  "),
            Span::styled("◯", Ink::Muted.style()),
            says(" a commons post  "),
            Span::styled("⠁⠈⠁", Ink::Muted.style()),
            says(" who made whom, who talked"),
        ]),
        Line::from(vec![
            mark("⠤⠤", TOOL),
            says(" shell  "),
            mark("⡇", TOOL),
            says(" files  "),
            mark("⡁⢈", TOOL),
            says(" web  "),
            mark("✳", TOOL),
            says(" another tool  "),
            mark("✳", FAILED),
            says(" a call failed"),
        ]),
        Line::from(says("A star glows as its agent talks, then fades.")),
    ]);
    lines
}

/// A name: bold while it works or needs the person, muted while it is
/// quiet. In the text's own colour: here every label is an agent's name,
/// so agents' blue would say nothing a name doesn't, and the sky's hues
/// are kept for what needs the person or failed.
fn name_style(state: Option<State>, glow: f32) -> Style {
    match state {
        Some(State::Working | State::NeedsYou) => Style::default().bold(),
        Some(State::Failed) => Style::default(),
        None if glow > 0.08 => Style::default(),
        None => Style::default().add_modifier(Modifier::DIM),
    }
}

/// A message's colour: magenta to or from the person, blue between agents,
/// none from outside (a lookout's or an automation's).
fn hue(from: End, to: End) -> Option<Color> {
    match (from, to) {
        (End::Person, _) | (_, End::Person) => Some(YOU),
        (End::Agent(_), End::Agent(_)) => Some(AGENTS),
        _ => None,
    }
}

/// A time of day, as a replay's chat log and footer say it: `02:10`.
fn clock(ms: i64) -> String {
    let local = crate::trigger::local(ms.div_euclid(1000));
    format!("{:02}:{:02}", local.hour, local.minute)
}

/// Now, in Unix ms.
fn wall_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// When `--replay` starts, in Unix ms: `2h`, `30m` or `1d` ago, or the
/// last time it was `02:10`.
pub fn replay_from(when: &str, now: i64) -> Option<i64> {
    if let Some((hour, minute)) = when.split_once(':') {
        let (hour, minute): (u8, u8) = (hour.parse().ok()?, minute.parse().ok()?);
        if hour > 23 || minute > 59 {
            return None;
        }
        let today = crate::trigger::local(now.div_euclid(1000));
        let on = |day: crate::trigger::Local| {
            crate::trigger::epoch_of(crate::trigger::Local {
                hour,
                minute,
                ..day
            })
        };
        let at = on(today)? * 1000;
        if at <= now {
            return Some(at);
        }
        // Later today is yesterday's, by the calendar: a day back from
        // noon is yesterday whatever the clocks did overnight.
        let noon = crate::trigger::epoch_of(crate::trigger::Local {
            hour: 12,
            minute: 0,
            ..today
        })?;
        return Some(on(crate::trigger::local(noon - 86_400))? * 1000);
    }
    let unit = when.find(|c: char| !c.is_ascii_digit())?;
    let scale = match &when[unit..] {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    let n: i64 = when[..unit].parse().ok()?;
    Some(now - n.checked_mul(scale)?)
}

/// How long ago, as the ticker says it.
fn ago(now: Instant, at: Instant) -> String {
    let s = now.duration_since(at).as_secs();
    match s {
        0..=59 => format!("{s}s"),
        _ => format!("{}m", s / 60),
    }
}

/// Words wrapped to lines of `room` columns, a word too long for one cut.
fn wrapped(text: &str, room: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.width() + 1 + word.width() <= room => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(cut(word, room)),
        }
    }
    lines
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

/// A radial tree of `nodes` nodes: each root's arm as wide as the leaves
/// under it, the roots on the inner ring, each generation a ring further
/// out, the outermost reaching the rim. Each node placed gets its place
/// and its angle; the order is the tree's, each node with its depth.
#[allow(clippy::type_complexity)]
fn radial(
    roots: &[usize],
    children: &[Vec<usize>],
    nodes: usize,
) -> (Vec<Option<((f32, f32), f32)>>, Vec<(usize, usize)>) {
    let mut places = vec![None; nodes];
    let mut order = Vec::new();
    // How many generations below the leads the deepest line goes.
    fn height(at: usize, children: &[Vec<usize>], depth: usize) -> usize {
        match depth > children.len() {
            true => depth,
            false => (children[at].iter())
                .map(|&kid| height(kid, children, depth + 1))
                .fold(depth, usize::max),
        }
    }
    let deepest = (roots.iter())
        .map(|&root| height(root, children, 0))
        .max()
        .unwrap_or(0);
    #[allow(clippy::too_many_arguments)]
    fn place(
        at: usize,
        from: f32,
        span: f32,
        depth: usize,
        deepest: usize,
        children: &[Vec<usize>],
        places: &mut Vec<Option<((f32, f32), f32)>>,
        order: &mut Vec<(usize, usize)>,
    ) {
        if places[at].is_some() {
            return;
        }
        // Leads on the inner ring and every agent that made none on the
        // outer, as a cluster dendrogram puts its leaves (d3.cluster), so
        // the stars fill the disc rather than huddle where the deepest
        // line leaves them; the agents between by their generation.
        let radius = match (deepest, depth) {
            (0, _) => 0.62,
            (_, 0) => 0.45,
            _ if children[at].is_empty() => 1.0,
            (deepest, depth) => 0.45 + 0.55 * depth as f32 / deepest as f32,
        };
        let angle = from + span / 2.0;
        places[at] = Some(((radius * angle.cos(), radius * angle.sin()), angle));
        order.push((at, depth));
        let kids = &children[at];
        let weights: Vec<f32> = kids
            .iter()
            .map(|&kid| weight(kid, children) as f32)
            .collect();
        let total: f32 = weights.iter().sum();
        let mut start = from;
        for (kid, w) in kids.iter().zip(weights) {
            let part = span * w / total;
            place(
                *kid,
                start,
                part,
                depth + 1,
                deepest,
                children,
                places,
                order,
            );
            start += part;
        }
    }
    // Each arm as wide as the square root of what is under it, so one
    // big project can't squeeze the rest to nothing.
    let weights: Vec<f32> = (roots.iter())
        .map(|&root| (weight(root, children) as f32).sqrt())
        .collect();
    let total: f32 = weights.iter().sum::<f32>().max(1.0);
    let mut from = -PI / 2.0;
    for (&root, weight) in roots.iter().zip(weights) {
        let span = TAU * weight / total;
        place(
            root,
            from,
            span,
            0,
            deepest,
            children,
            &mut places,
            &mut order,
        );
        from += span;
    }
    let far = (places.iter().flatten())
        .map(|((x, y), _)| x.hypot(*y))
        .fold(0.0f32, f32::max);
    if far > 0.0 {
        let grow = 0.9 / far;
        for ((x, y), _) in places.iter_mut().flatten() {
            (*x, *y) = (*x * grow, *y * grow);
        }
    }
    (places, order)
}

/// A name's own number in 0..1, the same every time.
fn hashed(name: &str) -> f32 {
    let hash = (name.bytes()).fold(2166136261u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16777619)
    });
    (hash % 10_000) as f32 / 10_000.0
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

/// The thread a `commons` call posts to or replies in, from its
/// arguments, a JSON preview that may be cut short.
fn thread(arguments: &Value) -> Option<String> {
    let text = arguments.as_str().unwrap_or("");
    if let Ok(call) = serde_json::from_str::<Value>(text) {
        let name = call["reply"]["thread"]
            .as_str()
            .or(call["post"]["name"].as_str());
        return name.map(str::to_owned);
    }
    let key = ["\"thread\":\"", "\"name\":\""]
        .iter()
        .find_map(|key| text.find(key).map(|at| at + key.len()))?;
    let rest = &text[key..];
    rest.find('"').map(|end| rest[..end].to_owned())
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
    /// Structure: the system's faint weight.
    Faint,
    /// At rest: muted, as quiet text.
    Muted,
    Plain,
    /// A working star's breath: a shade of the text's colour.
    Shade(Color),
    Bright,
    Color(Color, bool),
}

impl Ink {
    fn rank(self) -> u8 {
        match self {
            Ink::Faint => 0,
            Ink::Muted => 1,
            Ink::Plain => 2,
            Ink::Color(_, false) => 3,
            Ink::Shade(_) | Ink::Bright => 4,
            Ink::Color(_, true) => 5,
        }
    }
    fn style(self) -> Style {
        match self {
            Ink::Faint => match crate::look::faint() {
                Some(color) => Style::default().fg(color),
                None => Style::default().add_modifier(Modifier::DIM),
            },
            Ink::Muted => Style::default().add_modifier(Modifier::DIM),
            Ink::Plain => Style::default(),
            Ink::Shade(color) => Style::default().fg(color).bold(),
            Ink::Bright => Style::default().bold(),
            Ink::Color(color, true) => Style::default().fg(color).bold(),
            Ink::Color(color, false) => Style::default().fg(color),
        }
    }
}

/// The two ends a working star breathes between: the theme's text and the
/// text taken further from the background, toward white on a dark theme
/// and black on a light one. Where the theme's colours are not known, or
/// with `NO_COLOR`, the star holds still in bold: a breath stepped through
/// bold and plain would blink, the most distracting motion there is.
struct Shade(Option<(crate::term::Rgb, crate::term::Rgb)>);

impl Shade {
    fn now() -> Shade {
        let theme = crate::term::theme();
        Shade(
            match (crate::look::no_color(), theme.foreground, theme.background) {
                (false, Some(foreground), Some(_)) => {
                    let far = if crate::term::light() {
                        (0, 0, 0)
                    } else {
                        (255, 255, 255)
                    };
                    Some((foreground, crate::look::blend(far, foreground, 0.6)))
                }
                _ => None,
            },
        )
    }

    fn breathe(&self, level: f32) -> Ink {
        match self.0 {
            Some((rest, full)) => {
                let (r, g, b) = crate::look::blend(full, rest, level);
                Ink::Shade(Color::Rgb(r, g, b))
            }
            None => Ink::Bright,
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
    /// The cells an agent's star covers, which no name runs into.
    stars: Vec<bool>,
}

impl Sky {
    fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols,
            rows,
            dots: vec![0; cols * rows],
            inks: vec![None; cols * rows],
            text: vec![None; cols * rows],
            stars: vec![false; cols * rows],
        }
    }

    /// The dot a point of the unit disc falls on.
    fn spot(&self, (x, y): (f32, f32)) -> (f32, f32) {
        let (w, h) = (self.cols as f32 * 2.0, self.rows as f32 * 4.0);
        let (across, down) = self.scale();
        (w / 2.0 + x * across, h / 2.0 + y * down)
    }

    /// How many dots the unit disc's radius takes across and down, two
    /// dots in from the canvas's edges: round, as in 0.7.0, stretched
    /// toward the pane's longer side by at most a quarter, so a wide pane
    /// uses its width and the sky still reads as round.
    fn scale(&self) -> (f32, f32) {
        let (w, h) = (self.cols as f32 * 2.0, self.rows as f32 * 4.0);
        let (across, down) = ((w / 2.0 - 2.0).max(1.0), (h / 2.0 - 2.0).max(1.0));
        (across.min(down * STRETCH), down.min(across * STRETCH))
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

    /// An agent: a blob, or at rest a dot; names keep off either.
    fn star(&mut self, at: (f32, f32), ink: Ink, big: bool) {
        match big {
            true => self.blob(at, ink),
            false => self.dot(at, ink),
        }
        let (row, cols) = self.covers(at);
        for col in cols.filter(|_| row < self.rows) {
            self.stars[row * self.cols + col] = true;
        }
    }

    /// The row and columns a star at a point covers.
    fn covers(&self, at: (f32, f32)) -> (usize, std::ops::RangeInclusive<usize>) {
        let (x, y) = self.spot(at);
        let col = |x: f32| ((x.max(0.0) / 2.0) as usize).min(self.cols.saturating_sub(1));
        ((y / 4.0) as usize, col(x - 0.5)..=col(x + 0.5))
    }

    fn twinkle(&mut self, at: (f32, f32), work: Work, ink: Ink) {
        let (x, y) = self.spot(at);
        for (dx, dy) in work.shape() {
            self.plot(x + dx, y + dy, ink);
        }
    }

    fn ring(&mut self, at: (f32, f32), radius: f32, ink: Ink) {
        // Round, whatever the ellipse: a ring is a mark, not a place.
        let (across, down) = self.scale();
        self.circle(at, radius * across.min(down), ink);
    }

    /// A ring of `r` braille dots round a point.
    fn circle(&mut self, at: (f32, f32), r: f32, ink: Ink) {
        let (x, y) = self.spot(at);
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
    /// fit, else left; never over or against other words. False when
    /// there is no room for them.
    fn label(
        &mut self,
        at: (f32, f32),
        mark: Option<(&str, Style)>,
        words: &str,
        style: Style,
    ) -> bool {
        let (_, y) = self.spot(at);
        let row = (y / 4.0) as usize;
        let marked = match mark {
            Some((mark, _)) => format!("{mark} {words}"),
            None => words.to_owned(),
        };
        let words = marked.as_str();
        let width = words.width();
        if row >= self.rows || width + 2 > self.cols {
            return false;
        }
        // A blank cell between the words and their star, whichever cells
        // the star covers.
        let (_, own) = self.covers(at);
        let right = (own.end() + 2 + width <= self.cols).then_some(own.end() + 2);
        let left = own.start().checked_sub(width + 1);
        let sides = match at.0 >= 0.0 {
            true => [right, left],
            false => [left, right],
        };
        // Beside the star on its own row, else centred over it, else under
        // it, as a map places a name where the dots leave room.
        let centred = (own.start() + 1).saturating_sub(width / 2);
        let spots = (sides
            .into_iter()
            .flatten()
            .map(|start| (row, start, Some(own.clone()))))
        .chain((row > 0).then(|| (row - 1, centred, None)))
        .chain((row + 1 < self.rows).then(|| (row + 1, centred, None)));
        let Some((row, start)) = spots.into_iter().find_map(|(row, start, own)| {
            (start + width <= self.cols
                && self.clear_of_stars(row, start, width, own.as_ref())
                && self.put(row, start, words, style))
            .then_some((row, start))
        }) else {
            return false;
        };
        if let Some((mark, mark_style)) = mark {
            for (cell, ch) in (row * self.cols + start..).zip(mark.chars()) {
                self.text[cell] = Some((ch, mark_style));
            }
        }
        true
    }

    /// No other agent's star in the cells words would take or the blank
    /// either side of them; `own`, the cells of the star they name.
    fn clear_of_stars(
        &self,
        row: usize,
        start: usize,
        width: usize,
        own: Option<&std::ops::RangeInclusive<usize>>,
    ) -> bool {
        (start.saturating_sub(1)..(start + width + 1).min(self.cols)).all(|col| {
            own.is_some_and(|own| own.contains(&col)) || !self.stars[row * self.cols + col]
        })
    }

    /// Words centred on a point.
    fn text_at(&mut self, at: (f32, f32), words: &str, style: Style) {
        let (x, y) = self.spot(at);
        let (col, row) = ((x / 2.0) as usize, (y / 4.0) as usize);
        let start = col.saturating_sub(words.width() / 2);
        self.put(row, start, words, style);
    }

    /// Words put at a place in a row, a blank cell kept either side and
    /// a blank row above and below, so names never run together or
    /// stack into a column; false when they don't fit there.
    fn put(&mut self, row: usize, start: usize, words: &str, style: Style) -> bool {
        let count = words.chars().count();
        if start + count > self.cols {
            return false;
        }
        let cells = row * self.cols + start..row * self.cols + start + count;
        // A row clear above and below, and two cells beside on its own row,
        // so neighbouring names never read as one.
        let crowded = (row.saturating_sub(1)..(row + 2).min(self.rows)).any(|near| {
            let gap = if near == row { 2 } else { 1 };
            let span = start.saturating_sub(gap)..(start + count + gap).min(self.cols);
            self.text[near * self.cols + span.start..near * self.cols + span.end]
                .iter()
                .any(Option::is_some)
        });
        if crowded {
            return false;
        }
        for (cell, ch) in cells.zip(words.chars()) {
            self.text[cell] = Some((ch, style));
        }
        // A cell of clear sky either side, as a map's names mask the lines
        // under their edges, so a dense web never runs into the words.
        let sides = [
            start.checked_sub(1),
            Some(start + count).filter(|&c| c < self.cols),
        ];
        for col in sides.into_iter().flatten() {
            self.dots[row * self.cols + col] = 0;
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
const KEYS: &str = "↑↓←→ pick · m models · r replay · z zen · ? legend · q quit";
/// A replay's keys, in the title's place.
pub const REPLAY_KEYS: &str = "space pause · ←→ step · [ ] speed · esc live";

/// `sonny sky`: the same sky, the whole terminal, on its own, to keep
/// beside the work as btop is kept: its title and the keys over it, the
/// counts under it. A daemon that goes is followed again once it is back.
/// Its follow runs on `runtime`; the drawing and the keys on this thread.
pub fn watch(
    runtime: &tokio::runtime::Runtime,
    socket: PathBuf,
    centre: Option<String>,
    replay: Option<i64>,
) -> Result<(), String> {
    use ratatui::crossterm::{
        cursor, event, execute,
        terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
    };
    let _inside = runtime.enter();
    let mut view = View::open(socket);
    view.centre_on(centre);
    if replay.is_some() {
        view.replay(replay);
    }
    terminal::enable_raw_mode().map_err(|e| e.to_string())?;
    // The theme's colours, so muted reads as the measured gray it is in
    // the conversation, and `NO_COLOR` is honoured here too.
    crate::term::detect();
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
                    .draw(|frame| {
                        frame.render_widget(&mut view, frame.area());
                        crate::look::Look::now().finish(frame.buffer_mut());
                    })
                    .map_err(|e| e.to_string())?;
                if !event::poll(FRAME).map_err(|e| e.to_string())? {
                    continue;
                }
                if let event::Event::Key(key) = event::read().map_err(|e| e.to_string())?
                    && key.kind != event::KeyEventKind::Release
                {
                    // Esc lets go of the pick and the centre before it leaves.
                    if !view.press(key.code) && quits(&key) {
                        return Ok(());
                    }
                }
            }
        });
    let _ = execute!(std::io::stdout(), cursor::Show, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    drawn
}

/// q, Esc and Ctrl+C leave `sonny sky`, Esc once nothing is picked or
/// centred.
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
        let mut keys = match self.replay {
            Some(_) => format!("{REPLAY_KEYS} · q quit"),
            None => KEYS.to_owned(),
        };
        // A narrow terminal keeps the last keys, quitting among them.
        while 4 + keys.width() + 1 > width
            && let Some((_, rest)) = keys.split_once(" · ")
        {
            keys = rest.to_owned();
        }
        let used = 4 + keys.width() + 1;
        let keys = crate::hint::spans(&format!("{keys} "));
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
        // Zen keeps the sky alone.
        if self.zen {
            return;
        }
        // The footer's own margin, one column less, as the title's.
        let mut foot = self.footer(width + 1);
        if let Some(first) = foot.spans.first_mut() {
            first.content = first.content.replacen(' ', "", 1).into();
        }
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
        // A configured agent is read again: the event says only what
        // changed, and `m` colours by its model.
        if event["event"] == "configured" {
            if let Some(model) = configured(&client, &event).await
                && send.send(model).is_err()
            {
                return;
            }
            continue;
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

/// A configured agent's model, read again when it changed: the event says
/// only what changed, and `m` colours by its model.
async fn configured(client: &Client, event: &Value) -> Option<Feed> {
    let changed = event["changed"].as_array()?;
    if !changed.iter().any(|c| c == "model") {
        return None;
    }
    let name = event["bot"].as_str()?;
    let bot = client.request("inspect", json!({"bot": name})).await.ok()?;
    Some(Feed::Model(
        name.to_owned(),
        bot["model"].as_str()?.to_owned(),
    ))
}

/// An event as a replay's tape keeps it: a tool's arguments only for a
/// commons post, which is all the sky reads of them.
fn slim(mut event: Value) -> Value {
    // What the daemon sends unwritten, such as a deletion, happened when
    // it came: dated so, it plays at its time and not at the start.
    if event["written_ms"].is_null()
        && let Some(fields) = event.as_object_mut()
    {
        fields.insert("written_ms".into(), json!(wall_ms()));
    }
    if event["event"] == "tool_started"
        && event["data"]["name"] != "commons"
        && let Some(data) = event["data"].as_object_mut()
    {
        data.remove("arguments");
    }
    event
}

/// An event of a replay's connection onto its tape; false once the sky
/// or the daemon has gone.
async fn taped(client: &Client, event: Value, send: &mpsc::UnboundedSender<Feed>) -> bool {
    let sent = match event["event"].as_str() {
        Some("pruned") => return true,
        // As live: a new model is read again, for `m`.
        Some("configured") => match configured(client, &event).await {
            Some(model) => send.send(model),
            None => return true,
        },
        Some("lagged" | "follow_lagged") => {
            let _ = send.send(Feed::Down("fell behind the daemon".into()));
            return false;
        }
        // What the sky reads of a tool's arguments is a commons post; the
        // rest stays with the daemon, not in the tape. An unwritten event
        // is dated as it comes.
        _ => send.send(Feed::Tape(slim(event))),
    };
    sent.is_ok()
}

/// A replay's connection: every agent here now, then every event written
/// since `from` (Unix ms), the past as one tape, then what follows live.
async fn tape(socket: PathBuf, from: i64, send: mpsc::UnboundedSender<Feed>) {
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
        .request(
            "follow",
            json!({"bot": "*", "since_ms": from, "kinds": LIVE}),
        )
        .await
    {
        let _ = send.send(Feed::Down(error.to_string()));
        return;
    }
    while let Some(event) = events.recv().await {
        if event["event"] != "follow_live" {
            if !taped(&client, event, &send).await {
                return;
            }
            continue;
        }
        // Who is here may have changed while the past was read, and a
        // removal then is in neither the tape nor what follows: the cast is
        // read again, as `follow` reads the roll again. What came while it
        // was read goes on the tape before the replay starts, so an agent
        // made then, listed already, waits for its making.
        if !roll(&client, &send).await {
            return;
        }
        while let Ok(event) = events.try_recv() {
            if !taped(&client, event, &send).await {
                return;
            }
        }
        if send.send(Feed::Ready).is_err() {
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
            " 1 agent · ● 1 working · 0 messages in the last minute"
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
    async fn a_full_small_pane_keeps_the_newest_log_line_in_its_rows() {
        let (mut view, send) = view();
        let bots: Vec<Value> = (0..31)
            .map(|n| json!({"name": format!("a{n:02}")}))
            .collect();
        send.send(Feed::Roll(bots)).unwrap();
        send.send(Feed::Ready).unwrap();
        for n in 0..5 {
            let event = json!({"cursor": n + 1, "event": "accepted", "bot": format!("a{n:02}"),
                "turn": 1, "data": {}});
            send.send(Feed::Live(event)).unwrap();
        }
        view.drain();
        let card = text(&view.card(50, 20));
        let lines: Vec<&str> = card.lines().collect();
        assert!(lines.len() <= 20, "{card}");
        assert!(
            lines.last().unwrap().trim_end().ends_with("you → a04"),
            "{card}"
        );
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
        view.lay(Instant::now());
        let radius = |name: &str| {
            let at = view.agents[view.index[name]].at;
            at.0.hypot(at.1)
        };
        assert!(radius("shop.lead") < radius("shop.login"));
        assert!(view.agents[view.index["shop.lead"]].root);
        assert!(view.agents[view.index["lone"]].root);
        assert!(!view.agents[view.index["shop.cart"]].root);
    }

    #[test]
    fn a_commons_call_names_its_thread_even_cut_short() {
        let thread = |text: &str| super::thread(&json!(text));
        assert_eq!(
            thread(r#"{"reply":{"thread":"t1","text":"x"}}"#).as_deref(),
            Some("t1")
        );
        assert_eq!(
            thread(r#"{"post":{"name":"n1","title":"x"}}"#).as_deref(),
            Some("n1")
        );
        assert_eq!(
            thread(r#"{"reply":{"thread":"t2","text":"a long te"#).as_deref(),
            Some("t2")
        );
        assert_eq!(thread(r#"{"read":"x"}"#), None);
    }

    #[tokio::test]
    async fn messages_landing_heat_an_agent_and_it_cools() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "a"})])).unwrap();
        view.drain();
        let now = Instant::now();
        let land = |view: &mut View, at: Instant| {
            view.sparks.push(Spark {
                from: End::Person,
                to: End::Agent(0),
                born: at - FLIGHT,
                color: None,
            });
            view.land(at);
        };
        land(&mut view, now);
        let one = view.agents[0].warmth(now);
        land(&mut view, now);
        land(&mut view, now);
        // Three at once: hotter than one, so the flash is bigger.
        assert!((one - 1.0).abs() < 0.01);
        assert!(view.agents[0].warmth(now) > 2.9);
        let later = now + Duration::from_secs_f32(HALF_LIFE);
        assert!((view.agents[0].warmth(later) - 1.5).abs() < 0.05);
    }

    #[tokio::test]
    async fn a_new_project_takes_the_next_place_and_the_rest_stay() {
        let (mut view, send) = view();
        let roll = |names: &[(&str, i64)]| {
            let bots = names
                .iter()
                .map(|(name, id)| json!({"name": name, "bot_id": id}))
                .collect();
            Feed::Roll(bots)
        };
        send.send(roll(&[("zoo.lead", 1), ("apps.lead", 2)]))
            .unwrap();
        view.drain();
        view.card(120, 40);
        let angle = |view: &View, name: &str| view.agents[view.index[name]].angle;
        // Started first, so first round from the top, though named later.
        assert!(angle(&view, "zoo.lead") < angle(&view, "apps.lead"));
        let (zoo, apps) = (angle(&view, "zoo.lead"), angle(&view, "apps.lead"));
        send.send(roll(&[("zoo.lead", 1), ("apps.lead", 2), ("mid.lead", 3)]))
            .unwrap();
        view.drain();
        view.card(120, 40);
        // The newcomer goes last; the others keep their order.
        assert!(angle(&view, "apps.lead") < angle(&view, "mid.lead"));
        assert!(angle(&view, "zoo.lead") <= zoo && angle(&view, "apps.lead") <= apps);
    }

    #[tokio::test]
    async fn a_centred_agent_sits_in_the_middle_with_rings_by_hops() {
        use ratatui::crossterm::event::KeyCode;
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![
            json!({"name": "shop.lead", "model": "m1"}),
            json!({"name": "shop.login", "created_by": "shop.lead", "model": "m2"}),
            json!({"name": "shop.cart", "created_by": "shop.login", "model": "m2"}),
            json!({"name": "notes.lead", "model": "m1"}),
        ]))
        .unwrap();
        view.drain();
        view.card(120, 40);
        // Asked for by its project, the lead goes to the middle.
        view.centre_on(Some("shop".into()));
        view.card(120, 40);
        view.moved = None;
        view.glide(Instant::now());
        let radius = |view: &View, name: &str| {
            let at = view.agents[view.index[name]].at;
            at.0.hypot(at.1)
        };
        assert!(radius(&view, "shop.lead") < 0.001);
        // Its maker (the person) and what it made one ring out; two hops,
        // the next ring.
        let you = view.person.at.0.hypot(view.person.at.1);
        assert!((you - radius(&view, "shop.login")).abs() < 0.01);
        assert!(radius(&view, "shop.cart") > radius(&view, "shop.login") + 0.1);
        assert!(radius(&view, "notes.lead") > you + 0.1);
        assert!(view.footer(120).to_string().contains("around shop lead"));
        // Esc lets go of the centre, then has nothing left to let go of.
        assert!(view.press(KeyCode::Esc));
        assert!(view.centred().is_none());
        assert!(!view.press(KeyCode::Esc));
        // An arrow picks a star; Enter centres on it.
        view.card(120, 40);
        assert!(view.press(KeyCode::Right));
        let picked = view.pick.clone().expect("a star picked");
        assert!(view.press(KeyCode::Enter));
        assert_eq!(view.centre.as_deref(), Some(picked.as_str()));
        assert!(view.pick.is_none());
        // `m` colours each agent by its model where it stands, the
        // footer saying which colour is which.
        view.centre_on(None);
        view.card(120, 40);
        let goal = view.agents[view.index["shop.login"]].goal;
        assert!(view.press(KeyCode::Char('m')));
        assert!(view.models);
        let hues = view.hues();
        assert_eq!(hues.len(), 2);
        let footer = view.footer(120).to_string();
        assert!(
            footer.contains("⠛ m1") && footer.contains("⠛ m2"),
            "{footer}"
        );
        view.card(120, 40);
        assert_eq!(view.agents[view.index["shop.login"]].goal, goal);
        assert!(view.press(KeyCode::Char('m')));
        assert!(!view.models);
    }

    #[tokio::test]
    async fn a_replay_plays_the_past_in_its_time_and_steps_back_to_its_start() {
        use ratatui::crossterm::event::KeyCode;
        let (mut view, send) = view();
        let from = wall_ms() - 3_600_000;
        view.replay = Some(Replay::new(from));
        send.send(Feed::Roll(vec![
            json!({"name": "shop.lead", "running_turn": 4}),
            json!({"name": "shop.login", "created_by": "shop.lead"}),
            json!({"name": "shop.cart", "created_by": "shop.lead"}),
        ]))
        .unwrap();
        let said = |cursor: i64, ms: i64, to: &str| {
            json!({"cursor": cursor, "event": "accepted", "bot": to, "turn": cursor,
                "written_ms": from + ms, "data": {"from": {"bot": "shop.lead", "turn": 1}}})
        };
        send.send(Feed::Tape(said(1, 1_000, "shop.login"))).unwrap();
        // Made during the replay: not there until it is.
        send.send(Feed::Tape(
            json!({"cursor": 2, "event": "created", "bot": "shop.cart",
            "written_ms": from + 60_000, "data": {"bot_id": 3, "created_by": "shop.lead"}}),
        ))
        .unwrap();
        send.send(Feed::Tape(said(3, 90_000, "shop.cart"))).unwrap();
        // Before it's all in, only the footer says so.
        view.drain();
        assert!(view.agents.is_empty());
        assert!(text(&[view.footer(80)]).contains("reading from"));
        send.send(Feed::Ready).unwrap();
        view.drain();
        // Who was here, all idle, and nothing yet said.
        assert!(view.agent("shop.lead").is_some() && view.agent("shop.cart").is_none());
        assert_eq!(view.agents[view.index["shop.lead"]].running, None);
        assert!(view.ticker.is_empty());
        // Eight times as fast: a second and a half plays twelve.
        let replay = view.replay.as_mut().unwrap();
        replay.tick -= Duration::from_millis(1500);
        view.drain();
        let logged: Vec<_> = view
            .ticker
            .iter()
            .map(|t| (t.text.clone(), t.written))
            .collect();
        assert_eq!(
            logged,
            [("shop lead → login".to_owned(), Some(from + 1_000))]
        );
        // The hour's weights go by when it was said, not sped up: eleven
        // seconds before the replay's now, not a second and a half.
        let ago = view.said[0].0.elapsed().as_secs_f32();
        assert!((10.0..12.5).contains(&ago), "{ago}");
        assert!(view.agent("shop.cart").is_none());
        // A step on, in the list a small pane shows too: a minute and
        // twenty seconds at 8×, the cart made and messaged, its time in
        // the log.
        view.listing = true;
        assert!(view.press(KeyCode::Right));
        view.listing = false;
        assert!(view.agent("shop.cart").is_some());
        assert_eq!(view.ticker.len(), 2);
        let log = text(&view.ticked(Instant::now(), 40, 12, true));
        assert!(log.contains(&clock(from + 90_000)), "{log}");
        // Paused, nothing moves; slower and faster halve and double.
        assert!(view.press(KeyCode::Char(' ')));
        let at = view.replay.as_ref().unwrap().at;
        view.replay.as_mut().unwrap().tick -= Duration::from_secs(5);
        view.drain();
        assert_eq!(view.replay.as_ref().unwrap().at, at);
        assert!(text(&[view.footer(80)]).contains("‖ 8×"));
        view.press(KeyCode::Char(']'));
        view.press(KeyCode::Char('['));
        view.press(KeyCode::Char('['));
        assert_eq!(view.replay.as_ref().unwrap().speed, 4.0);
        // Back past the start: from the start again, who came later gone.
        for _ in 0..8 {
            view.press(KeyCode::Left);
        }
        assert_eq!(view.replay.as_ref().unwrap().at, from as f64);
        assert!(view.agent("shop.cart").is_none());
        assert!(view.ticker.is_empty());
        // Esc watches live again.
        assert!(view.press(KeyCode::Esc));
        assert!(!view.replaying());
    }

    #[test]
    fn a_replay_s_tape_keeps_tool_arguments_only_for_a_commons_post_and_dates_the_unwritten() {
        let call = |name: &str| json!({"event": "tool_started", "bot": "a", "data": {"name": name, "arguments": "{}"}});
        assert!(slim(call("shell"))["data"]["arguments"].is_null());
        assert_eq!(slim(call("commons"))["data"]["arguments"], "{}");
        assert_eq!(slim(call("shell"))["data"]["name"], "shell");
        // A deletion comes unwritten: it is dated as it comes, and what is
        // written keeps its date.
        let before = wall_ms();
        let gone = slim(json!({"event": "deleted", "bot": "a"}))["written_ms"].as_i64();
        assert!(gone.is_some_and(|at| at >= before));
        let said = json!({"event": "queued", "bot": "a", "written_ms": 5});
        assert_eq!(slim(said)["written_ms"], 5);
    }

    #[test]
    fn a_replay_s_tape_lets_its_oldest_played_tenth_go_past_its_bound() {
        let mut replay = Replay::new(0);
        for at in 0..TAPE as i64 {
            replay.keep(json!({"event": "queued", "written_ms": at}));
        }
        // Nothing played yet: nothing goes.
        replay.keep(json!({"event": "queued", "written_ms": TAPE}));
        assert_eq!(replay.tape.len(), TAPE + 1);
        replay.played = TAPE;
        replay.keep(json!({"event": "queued", "written_ms": TAPE + 1}));
        assert_eq!(replay.tape.len(), TAPE + 2 - TAPE / 10);
        assert_eq!(replay.played, TAPE - TAPE / 10);
        assert_eq!(replay.from, (TAPE / 10) as i64);
        // Who was made, or removed, in what went is in the cast now.
        let mut replay = Replay::new(0);
        replay.cast = vec![json!({"name": "old"})];
        replay.keep(json!({"event": "created", "bot": "new", "data": {"bot_id": 9}}));
        replay.keep(json!({"event": "deleted", "bot": "old"}));
        for at in 0..TAPE as i64 {
            replay.keep(json!({"event": "queued", "written_ms": at}));
        }
        replay.played = TAPE;
        replay.keep(json!({"event": "queued", "written_ms": TAPE}));
        let cast: Vec<_> = replay
            .cast
            .iter()
            .map(|bot| (bot["name"].clone(), bot["bot_id"].clone()))
            .collect();
        assert_eq!(cast, [(json!("new"), json!(9))]);
    }

    #[test]
    fn a_replay_seats_an_agent_with_its_new_model_after_a_step_back() {
        let mut replay = Replay::new(0);
        replay.cast = vec![json!({"name": "a", "model": "m1"})];
        replay.keep(json!({"event": "created", "bot": "b", "data": {"model": "m1"}}));
        replay.remodel(&HashMap::from([("a", "m2"), ("b", "m3")]));
        assert_eq!(replay.cast[0]["model"], "m2");
        assert_eq!(replay.tape[0]["data"]["model"], "m3");
    }

    #[tokio::test]
    async fn a_replay_keeps_who_had_a_name_before_it_was_used_again() {
        let (mut view, send) = view();
        let from = wall_ms() - 3_600_000;
        view.replay = Some(Replay::new(from));
        send.send(Feed::Roll(vec![json!({"name": "shop.a", "bot_id": 1})]))
            .unwrap();
        send.send(Feed::Tape(json!({"event": "deleted", "bot": "shop.a",
            "written_ms": from + 60_000})))
            .unwrap();
        send.send(Feed::Tape(json!({"event": "created", "bot": "shop.a",
            "written_ms": from + 90_000, "data": {"bot_id": 2}})))
            .unwrap();
        send.send(Feed::Ready).unwrap();
        view.drain();
        // At the start, the first one, not waiting on the second's making.
        assert_eq!(view.agents[view.index["shop.a"]].id, Some(1));
    }

    #[tokio::test]
    async fn a_replay_takes_the_listing_s_model_for_an_agent_it_saw_made() {
        let (mut view, send) = view();
        view.replay = Some(Replay::new(0));
        send.send(Feed::Tape(
            json!({"event": "created", "bot": "b", "written_ms": 1, "data": {"model": "m1"}}),
        ))
        .unwrap();
        send.send(Feed::Roll(vec![json!({"name": "b", "model": "m2"})]))
            .unwrap();
        view.drain();
        let replay = view.replay.as_ref().unwrap();
        assert_eq!(replay.tape[0]["data"]["model"], "m2");
    }

    #[tokio::test]
    async fn an_older_spark_landing_after_a_newer_send_keeps_the_newer_glow() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "a"}), json!({"name": "b"})]))
            .unwrap();
        view.drain();
        let (a, b) = (view.index["a"], view.index["b"]);
        let now = Instant::now();
        // A jump plays a message to `a` a moment back, then `a` sending now.
        view.sparks.push(Spark {
            from: End::Agent(b),
            to: End::Agent(a),
            born: now - FLIGHT - Duration::from_millis(200),
            color: Some(Color::Blue),
        });
        view.send(End::Agent(a), End::Agent(b), now);
        let heat = view.agents[a].heat;
        view.land(now);
        let agent = &view.agents[a];
        assert_eq!((agent.lit, agent.heated), (Some(now), Some(now)));
        assert!(agent.heat > heat);
    }

    #[test]
    fn a_replay_starts_some_time_ago_or_at_a_time_of_day() {
        let now = wall_ms();
        assert_eq!(replay_from("2h", now), Some(now - 7_200_000));
        assert_eq!(replay_from("30m", now), Some(now - 1_800_000));
        assert_eq!(replay_from("1d", now), Some(now - 86_400_000));
        let at = replay_from("02:10", now).unwrap();
        assert!(at <= now && now - at < 86_400_000 + 3_600_000);
        assert_eq!(clock(at), "02:10");
        for wrong in ["2", "2x", "25:00", "1:61", "", "h"] {
            assert_eq!(replay_from(wrong, now), None, "{wrong}");
        }
    }

    #[tokio::test]
    async fn a_centred_sky_follows_who_talks_and_a_small_pane_picks_in_its_list() {
        use ratatui::crossterm::event::KeyCode;
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![
            json!({"name": "shop.lead"}),
            json!({"name": "shop.login", "created_by": "shop.lead"}),
            json!({"name": "shop.cart", "created_by": "shop.login"}),
        ]))
        .unwrap();
        view.drain();
        view.centre_on(Some("shop.lead".into()));
        view.card(120, 40);
        let radius = |view: &View, name: &str| {
            let at = view.agents[view.index[name]].at;
            at.0.hypot(at.1)
        };
        view.moved = None;
        view.glide(Instant::now());
        assert!(radius(&view, "shop.cart") > radius(&view, "shop.login") + 0.1);
        // Two hops out until it talks to the centre: then one.
        let from = json!({"bot": "shop.cart", "turn": 1});
        send.send(Feed::Live(
            json!({"cursor": 1, "event": "accepted", "bot": "shop.lead", "turn": 2, "data": {"from": from}}),
        ))
        .unwrap();
        view.drain();
        view.card(120, 40);
        view.moved = None;
        view.glide(Instant::now());
        assert!((radius(&view, "shop.cart") - radius(&view, "shop.login")).abs() < 0.01);
        // One made while the sky is open comes with its model.
        send.send(Feed::Live(
            json!({"cursor": 2, "event": "created", "bot": "shop.new",
            "data": {"bot_id": 9, "created_by": "shop.lead", "model": "m3"}}),
        ))
        .unwrap();
        view.drain();
        assert_eq!(view.agents[view.index["shop.new"]].model, "m3");
        // One configured to another model moves to its group.
        send.send(Feed::Model("shop.new".into(), "m4".into()))
            .unwrap();
        view.drain();
        assert_eq!(view.agents[view.index["shop.new"]].model, "m4");
        // Too small for the sky, the arrows walk the list, which keeps
        // the pick in view.
        view.centre_on(None);
        view.card(40, 12);
        assert!(view.press(KeyCode::Down));
        let first = view.pick.clone().expect("a row picked");
        assert!(view.press(KeyCode::Down));
        assert_ne!(view.pick.as_ref(), Some(&first));
        assert!(view.press(KeyCode::Up));
        assert_eq!(view.pick.as_ref(), Some(&first));
        let listed = text(&view.card(40, 12));
        assert!(listed.contains(&short(&first)), "{listed}");
    }

    #[tokio::test]
    async fn a_crowd_folds_to_clouds_and_keeps_who_needs_you() {
        let (mut view, send) = view();
        let mut bots = vec![json!({"name": "shop.lead"}), json!({"name": "notes.lead"})];
        for n in 0..2000 {
            let lead = if n % 4 == 0 {
                "notes.lead"
            } else {
                "shop.lead"
            };
            bots.push(
                json!({"name": format!("{}.t{n}", &lead[..lead.len() - 5]), "created_by": lead}),
            );
        }
        send.send(Feed::Roll(bots)).unwrap();
        send.send(Feed::Live(
            json!({"cursor": 1, "event": "tool_started", "bot": "shop.t7",
            "turn": 1, "data": {"name": "ask"}}),
        ))
        .unwrap();
        view.drain();
        let (cols, rows) = (150 - LOG_COLS - GAP, 46);
        let sky = text(&view.card(150, 46));
        // Who needs the person is always a star and named; the leads too.
        let room = (cols * (rows - 3) / 24).max(8);
        let shown = view.shown(Instant::now(), room, &[]);
        assert!(shown[view.index["shop.t7"]]);
        assert!(shown.iter().filter(|&&shown| shown).count() <= room + 3);
        assert!(sky.contains("? t7"), "{sky}");
        assert!(
            sky.contains("shop lead") && sky.contains("notes lead"),
            "{sky}"
        );
        // The rest are counted as clouds, the biggest first, never more
        // counts than half the names.
        let counts = sky
            .split_whitespace()
            .filter(|word| word.starts_with('+'))
            .count();
        assert!((1..=rows / 3 / 2).contains(&counts), "{sky}");
        // No two names on rows next to each other overlap.
        let lines: Vec<&str> = sky.lines().collect();
        for pair in lines.windows(2) {
            let words = |line: &str| -> Vec<(usize, usize)> {
                let line: Vec<char> = line.chars().take(cols).collect();
                let mut spans = Vec::new();
                let mut at = 0;
                while at < line.len() {
                    if line[at].is_alphanumeric() {
                        let start = at;
                        while at < line.len()
                            && line[at] != ' '
                            && !('\u{2800}'..='\u{28ff}').contains(&line[at])
                        {
                            at += 1;
                        }
                        spans.push((start, at));
                    }
                    at += 1;
                }
                spans
            };
            for (a, b) in words(pair[0]) {
                for (c, d) in words(pair[1]) {
                    assert!(b <= c || d <= a, "names stacked:\n{}\n{}", pair[0], pair[1]);
                }
            }
        }
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
        // Only agent to agent is kept for the trails and the counts.
        assert_eq!(view.said.len(), 1);
        let shown = text(&view.card(60, 40));
        assert!(shown.contains("shop lead → login"), "{shown}");
        assert!(shown.contains("you → shop lead"), "{shown}");
        assert_eq!(
            text(&[view.footer(100)]),
            "  2 agents · ● 2 working · 2 messages in the last minute"
        );
    }

    #[tokio::test]
    async fn past_turns_spark_nothing_but_keep_who_talked() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "a"}), json!({"name": "b"})]))
            .unwrap();
        send.send(Feed::Past(
            json!({"event": "accepted", "bot": "b", "turn": 1, "data": {"from": {"bot": "a"}}}),
        ))
        .unwrap();
        view.drain();
        // The past sparks nothing and counts nothing, but who talked to
        // whom keeps its curve, as in 0.7.0.
        assert!(view.sparks.is_empty());
        assert!(view.said.is_empty());
        assert_eq!(view.talks, [(view.index["a"], view.index["b"])]);
        assert!(view.ticker.is_empty());
        assert!(view.agents.iter().all(|a| a.running.is_none()));
    }

    #[tokio::test]
    async fn the_legend_beside_the_sky_fits_its_column() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "a"})])).unwrap();
        view.drain();
        for line in view.key(Instant::now(), 40) {
            let width: usize = line.spans.iter().map(|span| span.content.width()).sum();
            assert!(width <= LOG_COLS, "{line}");
        }
    }

    #[tokio::test]
    async fn a_past_turn_of_the_last_hour_weighs_on_its_pair() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![
            json!({"name": "a"}),
            json!({"name": "b"}),
            json!({"name": "c"}),
        ]))
        .unwrap();
        let minutes = |n: i64| wall_ms() - n * 60_000;
        send.send(Feed::Past(
            json!({"event": "accepted", "bot": "b", "turn": 1,
            "written_ms": minutes(10), "data": {"from": {"bot": "a"}}}),
        ))
        .unwrap();
        send.send(Feed::Past(
            json!({"event": "accepted", "bot": "c", "turn": 1,
            "written_ms": minutes(90), "data": {"from": {"bot": "a"}}}),
        ))
        .unwrap();
        view.drain();
        // Both keep their curve; only the one this hour counts.
        let (a, b, c) = (view.index["a"], view.index["b"], view.index["c"]);
        assert_eq!(view.talks, [(a, b), (a, c)]);
        assert_eq!(
            view.said
                .iter()
                .map(|&(_, by, to)| (by, to))
                .collect::<Vec<_>>(),
            [(a, b)]
        );
        let ago = view.said[0].0.elapsed().as_secs();
        assert!((595..=605).contains(&ago), "{ago}");
        // On a centred sky, a past pair new to the hour lays the rings again.
        view.centre = Some("a".to_owned());
        view.laid = true;
        send.send(Feed::Past(
            json!({"event": "accepted", "bot": "c", "turn": 2,
            "written_ms": minutes(5), "data": {"from": {"bot": "a"}}}),
        ))
        .unwrap();
        view.drain();
        assert!(!view.laid);
    }

    #[tokio::test]
    async fn a_throwaway_copy_lends_its_agent_what_it_is_told_and_no_star_of_its_own() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![
            json!({"name": "approver.auto", "bot_id": 1}),
            json!({"name": "shop.lead", "bot_id": 2}),
            json!({"name": "shop.lead.suggest-1", "bot_id": 3, "expire_ms": 60_000}),
        ]))
        .unwrap();
        send.send(Feed::Ready).unwrap();
        send.send(Feed::Live(
            json!({"event": "forked", "bot": "approver.auto.1.9.0",
            "data": {"bot_id": 4, "created_by": "approver.auto", "expire_ms": 120_000}}),
        ))
        .unwrap();
        view.drain();
        assert!(view.agent("approver.auto.1.9.0").is_none());
        assert!(view.agent("shop.lead.suggest-1").is_none());
        let auto = view.index["approver.auto"];
        // A judgment: from the judge, at the edge, onto the approver.
        send.send(Feed::Live(
            json!({"event": "accepted", "bot": "approver.auto.1.9.0",
            "turn": 1, "data": {"origin": "judge"}}),
        ))
        .unwrap();
        // Its turn's end is the copy's, not the approver's.
        send.send(Feed::Live(
            json!({"event": "turn_finished", "bot": "approver.auto.1.9.0",
            "turn": 1, "data": {"status": "failed", "error": "x"}}),
        ))
        .unwrap();
        // Sonny's own guess, made by no agent, is not drawn at all.
        send.send(Feed::Live(
            json!({"event": "accepted", "bot": "shop.lead.suggest-1",
            "turn": 1, "data": {}}),
        ))
        .unwrap();
        view.drain();
        assert_eq!(view.sparks.len(), 1);
        assert!(matches!(view.sparks[0].from, End::Rim(_)));
        assert!(matches!(view.sparks[0].to, End::Agent(at) if at == auto));
        assert_eq!(view.agents[auto].state(), None);
        assert!(text(&view.ticked(Instant::now(), 40, 12, true)).contains("a judge → auto"));
        send.send(Feed::Live(
            json!({"event": "deleted", "bot": "approver.auto.1.9.0"}),
        ))
        .unwrap();
        view.drain();
        assert!(view.copies.is_empty());
    }

    #[tokio::test]
    async fn a_copy_leaves_its_agents_asks_alone_and_no_stranger_takes_its_work() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "shop.lead", "bot_id": 2})]))
            .unwrap();
        send.send(Feed::Ready).unwrap();
        send.send(Feed::Live(
            json!({"event": "forked", "bot": "shop.lead.side",
            "data": {"bot_id": 3, "created_by": "shop.lead", "created_by_id": 2, "expire_ms": 60_000}}),
        ))
        .unwrap();
        // The lead asks; its copy is told something and asks too.
        send.send(Feed::Live(
            json!({"event": "tool_started", "bot": "shop.lead",
            "turn": 1, "data": {"name": crate::asks::TOOL, "call_id": "c1"}}),
        ))
        .unwrap();
        view.drain();
        let lead = view.index["shop.lead"];
        assert!(view.agents[lead].asking);
        send.send(Feed::Live(
            json!({"event": "accepted", "bot": "shop.lead.side",
            "turn": 1, "data": {}}),
        ))
        .unwrap();
        view.drain();
        assert!(view.agents[lead].asking, "a copy's prompt answers nothing");
        view.agents[lead].asking = false;
        send.send(Feed::Live(
            json!({"event": "tool_started", "bot": "shop.lead.side",
            "turn": 1, "data": {"name": crate::asks::TOOL, "call_id": "c2"}}),
        ))
        .unwrap();
        view.drain();
        assert!(!view.agents[lead].asking, "a copy's ask is its own");
        assert!(view.agents[lead].twinkle.is_some());
        // The lead is removed and its name taken by someone else.
        send.send(Feed::Live(json!({"event": "deleted", "bot": "shop.lead"})))
            .unwrap();
        send.send(Feed::Live(json!({"event": "created", "bot": "shop.lead",
            "data": {"bot_id": 9}})))
            .unwrap();
        view.drain();
        assert!(view.copied("shop.lead.side").is_none());
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
        assert_eq!(view.said.back().map(|&(_, by, to)| (by, to)), Some((b, c)));
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
                Feed::Past(_) | Feed::Held(_) | Feed::Down(_) | Feed::Model(..) | Feed::Tape(_) => {
                    continue;
                }
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
        assert!(view.said.is_empty());
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
            "  3 agents · ? 2 need you · ● 0 working · ✗ 1 failed · 3 messages in the last minute"
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
    async fn the_legend_takes_the_tickers_place() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "shop.lead"})]))
            .unwrap();
        view.drain();
        view.legend = true;
        let shown = text(&view.card(80, 30));
        assert!(
            shown.contains("⠛ working breathes  ? needs you  ✗ failed"),
            "{shown}"
        );
        assert!(shown.contains("⠤⠤ shell  ⡇ files  ⡁⢈ web"), "{shown}");
        assert!(!shown.contains("Catching up"), "{shown}");
        let listed = text(&view.card(40, 12));
        assert!(listed.contains("✦ just talked"), "{listed}");
        // Beside a wide sky, the legend and the counts take the chat log's
        // column.
        let wide = text(&view.card(140, 40));
        assert!(wide.contains("   working: it breathes"), "{wide}");
        assert!(wide.contains(" leads         1"), "{wide}");
        view.legend = false;
        assert!(text(&view.card(80, 30)).contains("Catching up"));
    }

    #[tokio::test]
    async fn a_wide_sky_keeps_its_chat_log_beside_it_newest_last() {
        let (mut view, send) = view();
        send.send(Feed::Roll(vec![json!({"name": "a"}), json!({"name": "b"})]))
            .unwrap();
        for turn in 1..=3 {
            send.send(Feed::Live(
                json!({"event": "accepted", "bot": "b", "turn": turn,
                "data": {"from": {"bot": "a"}}}),
            ))
            .unwrap();
            send.send(Feed::Live(
                json!({"event": "turn_finished", "bot": "b", "turn": turn,
                "data": {"status": "completed"}}),
            ))
            .unwrap();
        }
        send.send(Feed::Live(
            json!({"event": "accepted", "bot": "a", "turn": 9, "data": {}}),
        ))
        .unwrap();
        send.send(Feed::Live(
            json!({"event": "tool_started", "bot": "a", "turn": 9,
            "data": {"name": "ask"}}),
        ))
        .unwrap();
        view.drain();
        let rows: Vec<String> = view
            .card(140, 40)
            .iter()
            .map(|line| text(std::slice::from_ref(line)))
            .collect();
        assert_eq!(rows.len(), 40);
        // The newest at the bottom of the column, what needs you in its hue.
        assert!(rows[39].ends_with("? a asks you"), "{:?}", rows[39]);
        assert!(rows[38].ends_with("you → a"), "{:?}", rows[38]);
        assert!(rows[37].ends_with("a → b"), "{:?}", rows[37]);
        assert!(rows[0].trim().is_empty() || !rows[0].contains('→'));
        // Zen: the sky alone, and no names but what needs the person.
        view.zen = true;
        let zen = text(&view.card(140, 40));
        assert!(!zen.contains('→'), "{zen}");
        assert!(zen.contains("? a"), "{zen}");
        assert!(
            !zen.lines().any(|row| row.trim_end().ends_with(" b")),
            "{zen}"
        );
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
    fn a_hint_wraps_in_a_narrow_column() {
        let lines = wrapped("Quiet. Messages light up here as they go.", 20);
        assert_eq!(lines, ["Quiet. Messages", "light up here as", "they go."]);
        assert_eq!(wrapped("Catching", 5), ["Catc…"]);
    }

    #[test]
    fn braille_dots_land_in_their_cells() {
        let mut sky = Sky::new(2, 1);
        sky.plot(0.0, 0.0, Ink::Plain);
        sky.plot(3.0, 3.0, Ink::Color(NEEDS, true));
        let lines = sky.lines();
        assert_eq!(text(&lines), "⠁⢀");
        assert_eq!(lines[0].spans[1].style.fg, Some(NEEDS));
    }
}

#[cfg(test)]
mod scale {
    use super::*;

    /// Frame times at scale: `cargo test -p sonny --release scale -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn frame_times_at_scale() {
        for count in [30usize, 300, 3_000, 10_000] {
            let (send, feed) = mpsc::unbounded_channel();
            let mut view = View::with(PathBuf::new(), feed, tokio::spawn(async {}));
            let projects = (count / 30).max(1);
            let mut bots = Vec::new();
            for p in 0..projects {
                bots.push(json!({"name": format!("p{p}.lead"), "bot_id": p * 1000 + 1}));
            }
            for n in 0..count - projects {
                let p = n % projects;
                bots.push(
                    json!({"name": format!("p{p}.a{n}"), "created_by": format!("p{p}.lead"),
                    "bot_id": p * 1000 + 2 + n, "running_turn": (n % 4 == 0).then_some(1)}),
                );
            }
            send.send(Feed::Roll(bots)).unwrap();
            send.send(Feed::Ready).unwrap();
            for n in 0..200 {
                let (p, q) = (n % projects, (n * 7) % (count - projects));
                let event = json!({"cursor": n + 1, "event": "accepted", "bot": format!("p{}.a{q}", q % projects),
                    "turn": 2, "data": {"from": {"bot": format!("p{p}.lead")}}});
                send.send(Feed::Live(event)).unwrap();
            }
            view.drain();
            let start = Instant::now();
            view.card(200, 56);
            let first = start.elapsed();
            let start = Instant::now();
            for _ in 0..20 {
                view.drain();
                view.card(200, 56);
            }
            let frame = start.elapsed() / 20;
            let start = Instant::now();
            view.card(50, 20);
            let list = start.elapsed();
            eprintln!(
                "{count:>6} agents: first frame {first:?}, then {frame:?} a frame; list {list:?}"
            );
        }
    }
}
