//! Daemon steps: a script's calls of its bot's own tools, which the daemon
//! decides, runs and records for the client answering the script's call
//! (`run_step`). A step is decided exactly as the same call from the model
//! would be ([`Bot::plan`]), against what its call captured at its first
//! step, and runs in the bot's sandbox and folder with what the turn's
//! commands get in their environment.
//!
//! The coordinator here owns every step from its asking to its reply. It
//! holds no active slot. A step waiting for a place is an entry in a line,
//! with no task and no timer; a task exists only while its tool runs.
//!
//! The order a step runs in, written down: steps start from the line
//! first come, first served, across every call and bot. A bot's steps in
//! flight, across its calls, are at most one round's width, so no bot's
//! steps wait in line behind its own. A command, or a confined change to
//! files whose sandbox helper runs as a process, starts only with a process
//! slot in hand, taken from the registry's semaphore, shared with the
//! turns' own commands, before its task exists; while none is free, one
//! waiter for the whole line stands in the semaphore's fair line beside
//! theirs. No command starts while the load gate holds new commands back.
//! Past the line's bounds a step is refused, and asking again is the only
//! way back in.
use super::{Service, Stepping, turn};
use agent_runtime::{
    Error, Result,
    codec::Part,
    fail_with,
    output::{self, Output},
    store::{ArtifactLines, Capture, Resumed, StepAsk, StepOpened, Stepped, Store},
    tools::{Outcome, Prepared, ReadSource, Slot},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
};
use tokio::{sync::oneshot, task::JoinSet};

/// Steps asked and not yet answered, daemon-wide.
const MAX_STEPS: usize = 65_536;
/// The bytes those steps and their unsent replies hold, daemon-wide.
const MAX_STEP_BYTES: usize = 64 << 20;
/// What a step holds besides its own words: its place in the maps.
const STEP_OVERHEAD: usize = 256;
/// Store jobs for steps (decisions and ends) at once, so a fleet's steps
/// share the store with every other bot's turns rather than fill its
/// queue: a few group commits' worth.
const STORE_WINDOW: usize = 64;
/// How long the line waits for the load gate before it looks again.
const LOAD_RETRY: std::time::Duration = std::time::Duration::from_millis(250);

/// A call, by its turn and id: one step of it at a time.
type Key = (i64, Box<str>);

/// A line of steps, first come first served. A step stopped while in it
/// leaves its entry where it stands, passed over when the line reaches it,
/// so a stop costs nothing however long the line; once the passed-over
/// entries outnumber the live ones, the line is tidied in one sweep.
#[derive(Default)]
struct Queue {
    entries: VecDeque<(Key, u64)>,
    live: usize,
}

impl Queue {
    fn push(&mut self, key: Key, serial: u64) {
        self.entries.push_back((key, serial));
        self.live += 1;
    }

    /// One of its entries was stopped where it stands.
    fn left(&mut self, live: impl Fn(&(Key, u64)) -> bool) {
        self.live -= 1;
        if self.entries.len() > 2 * self.live + 64 {
            self.entries.retain(live);
        }
    }
}

/// The step coordinator's state, kept by the service.
#[derive(Default)]
pub(super) struct Steps {
    /// Every step asked and not yet answered, by its call.
    flights: HashMap<Key, Flight>,
    /// What each call captured at its first step, kept while it may ask
    /// for more: until it ends.
    captured: HashMap<Key, Arc<Capture>>,
    /// Each bot's steps in flight.
    by_bot: HashMap<Box<str>, usize>,
    /// The calls of each turn with a step in flight or a capture, so a
    /// turn's end visits only its own.
    by_turn: HashMap<i64, HashSet<Box<str>>>,
    /// The calls each session has a step in flight for, so its closing
    /// visits only its own.
    by_session: HashMap<u64, HashSet<Key>>,
    /// Steps waiting for their decision's store job.
    asking: Queue,
    /// Steps' store jobs under way, at most [`STORE_WINDOW`] decisions.
    storing: usize,
    /// Allowed steps waiting for a place.
    line: Queue,
    /// Bytes the steps in flight and their unsent replies hold.
    bytes: usize,
    /// Commands started and not ended.
    commands: usize,
    /// A process slot the line's waiter got, for the step at its front.
    spare: Option<Slot>,
    /// The line's waiter, while it stands in the semaphore's line.
    waiter: Option<tokio::task::AbortHandle>,
    /// The semaphore closed: no waiter stands again.
    slots_closed: bool,
    /// Each flight's number, so a job's result finds the flight it was for.
    serial: u64,
    /// The store jobs and tool runs under way.
    pub(super) jobs: JoinSet<(Key, u64, Done)>,
    /// Replies waiting for room in their session's queue.
    pub(super) sending: JoinSet<usize>,
    /// When to look at the line again, the load gate having held it.
    pub(super) retry_at: Option<tokio::time::Instant>,
    /// Steps counted since start, for `stats`.
    pub(super) ran: u64,
    pub(super) refused: u64,
}

/// One step, from its asking to its reply.
struct Flight {
    serial: u64,
    session: u64,
    output: Output,
    request: Value,
    ask: StepAsk,
    /// Its tool and arguments read when it was asked, until it runs.
    prepared: Option<Prepared>,
    bytes: usize,
    state: State,
    /// Why it was stopped, once it was, and whether its asker left.
    stopped: Option<(&'static str, bool)>,
}

enum State {
    /// Waiting for room for its decision's store job.
    Asked,
    /// Its store job decides it.
    Deciding,
    /// On a gate, to run confined when `confined` once allowed.
    Gated { confined: bool },
    /// Allowed and waiting for a place: a command when `command`, and
    /// one that runs with a process slot when `slot`.
    Lined {
        capture: Arc<Capture>,
        confined: bool,
        command: bool,
        slot: bool,
    },
    /// Running; the sender stops it.
    Running {
        stop: Option<oneshot::Sender<&'static str>>,
        command: bool,
    },
    /// Its end is with the store.
    Ending,
}

/// What a job brings back to the run loop.
pub(super) enum Done {
    Opened(Result<StepOpened>),
    Ran(Result<Outcome>),
    /// Its end recorded, with its call's time running again when it was
    /// the last step that stopped it.
    Closed(Result<Value>, Option<Resumed>),
    /// The line's waiter got a process slot, or none, the semaphore closed.
    Slot(Option<Slot>),
}

/// The key the line's waiter brings its slot back under: no call's.
fn waiter_key() -> Key {
    (i64::MIN, "".into())
}

/// What a step's tool runs with: its sandbox, folder and environment.
struct Running {
    capture: Arc<Capture>,
    confined: bool,
    prepared: Prepared,
    slot: Option<Slot>,
    /// The pull cord as it was when the step was let start: a pause
    /// between then and its task's first poll still stops it.
    cord: agent_runtime::tools::Cord,
    turn: i64,
    id: String,
    /// Where a read of the bot's kept output pages it from.
    store: Store,
    /// What starts a background command and records its end, as a turn's.
    lifts: turn::Lifts,
}

impl Steps {
    /// Steps in flight, in line, and commands running, for `stats`.
    pub(super) fn counts(&self) -> Value {
        json!({"in_flight":self.flights.len(),"asked":self.asking.live,"in_line":self.line.live,
            "commands":self.commands,"bytes":self.bytes,"ran":self.ran,"refused":self.refused})
    }

    /// Call `key` has a flight or a capture now.
    fn index(&mut self, (turn, call): &Key) {
        self.by_turn.entry(*turn).or_default().insert(call.clone());
    }

    /// Call `key` may have neither a flight nor a capture now.
    fn unindex(&mut self, key: &Key) {
        if self.flights.contains_key(key) || self.captured.contains_key(key) {
            return;
        }
        if let Some(calls) = self.by_turn.get_mut(&key.0) {
            calls.remove(&key.1);
            if calls.is_empty() {
                self.by_turn.remove(&key.0);
            }
        }
    }

    /// Let call `key`'s capture go.
    fn forget(&mut self, key: &Key) {
        self.captured.remove(key);
        self.unindex(key);
    }

    fn take(&mut self, key: &Key) -> Option<Flight> {
        let flight = self.flights.remove(key)?;
        self.unindex(key);
        if let Some(keys) = self.by_session.get_mut(&flight.session) {
            keys.remove(key);
            if keys.is_empty() {
                self.by_session.remove(&flight.session);
            }
        }
        self.bytes -= flight.bytes;
        if let Some(count) = self.by_bot.get_mut(flight.ask.bot.as_str()) {
            *count -= 1;
            if *count == 0 {
                self.by_bot.remove(flight.ask.bot.as_str());
            }
        }
        Some(flight)
    }

    /// Whether `key`'s job result is for the flight there now.
    fn current(&self, key: &Key, serial: u64) -> bool {
        self.flights.get(key).is_some_and(|f| f.serial == serial)
    }

    /// Whether a line's entry is the flight there now, still `Asked`.
    fn asked(flights: &HashMap<Key, Flight>, (key, serial): &(Key, u64)) -> bool {
        flights
            .get(key)
            .is_some_and(|f| f.serial == *serial && matches!(f.state, State::Asked))
    }

    /// Whether a line's entry is the flight there now, still `Lined`.
    fn lined(flights: &HashMap<Key, Flight>, (key, serial): &(Key, u64)) -> bool {
        flights
            .get(key)
            .is_some_and(|f| f.serial == *serial && matches!(f.state, State::Lined { .. }))
    }
}

/// A step's reply, sent now when its session has room, else from a task
/// that waits for room, its bytes counted until it is sent.
fn reply(steps: &mut Steps, flight: &Flight, result: Result<Value>) {
    if flight.stopped.is_some_and(|(_, gone)| gone) {
        return;
    }
    let (output, request) = (flight.output.clone(), flight.request.clone());
    match output.try_respond(request.clone(), result.clone()) {
        Err(error) if error.code == "output_lagged" => {
            let bytes = match &result {
                Ok(value) => agent_runtime::output::encoded_len(value).unwrap_or(0),
                Err(_) => 0,
            } + STEP_OVERHEAD;
            // Past the daemon's bound, a session that stopped reading is
            // closed, as a lagging follower is: EOF is the overload signal,
            // and each step's end is in the store already.
            if steps.bytes + bytes > MAX_STEP_BYTES {
                output.close();
                return;
            }
            steps.bytes += bytes;
            steps.sending.spawn(async move {
                let _ = output.respond(request, result).await;
                bytes
            });
        }
        _ => {}
    }
}

impl Service {
    /// Take a `run_step`: refused at once while the daemon is held, while
    /// the call has a step in flight, while its bot has a round's width in
    /// flight, or while the line is full; otherwise decided in a store job
    /// the run loop does not wait on, and answered once it ends.
    pub(super) fn run_step(
        &mut self,
        session: u64,
        output: &Output,
        request: Value,
        ask: StepAsk,
        prepared: Prepared,
    ) -> Result<Value> {
        if let Some(held) = self.held() {
            return fail_with(
                held,
                "no step runs while the daemon is held; ask again after resume",
            );
        }
        let key: Key = (ask.turn, ask.call_id.as_str().into());
        let steps = &mut self.steps;
        if let Some(flight) = steps.flights.get(&key) {
            steps.refused += 1;
            return Err(Error::with(
                "step_in_flight",
                format!(
                    "step {} of {} has not ended: one daemon step at a time a call",
                    flight.ask.step, ask.call_id
                ),
            ));
        }
        let wide = self.registry.wide(turn::PARALLEL_CALLS);
        if steps.by_bot.get(ask.bot.as_str()).copied().unwrap_or(0) >= wide {
            steps.refused += 1;
            return Err(Error::with(
                "steps_full",
                format!(
                    "{} has {wide} daemon steps in flight, the most it may",
                    ask.bot
                ),
            )
            .facts(json!({"in_flight":wide})));
        }
        let bytes = ask.arguments.len()
            + ask.call_id.len()
            + ask.step.len()
            + ask.bot.len()
            + STEP_OVERHEAD;
        if steps.flights.len() >= MAX_STEPS || steps.bytes + bytes > MAX_STEP_BYTES {
            steps.refused += 1;
            return Err(Error::with(
                "step_line_full",
                "the daemon holds as many daemon steps as it may; ask again once some end",
            )
            .facts(json!({"in_flight":steps.flights.len(),"bytes":steps.bytes})));
        }
        steps.serial += 1;
        let serial = steps.serial;
        steps.bytes += bytes;
        *steps.by_bot.entry(ask.bot.as_str().into()).or_default() += 1;
        steps.flights.insert(
            key.clone(),
            Flight {
                serial,
                session,
                output: output.clone(),
                request,
                ask,
                prepared: Some(prepared),
                bytes,
                state: State::Asked,
                stopped: None,
            },
        );
        steps.index(&key);
        steps
            .by_session
            .entry(session)
            .or_default()
            .insert(key.clone());
        steps.asking.push(key, serial);
        self.ask_steps();
        Err(Error::new("deferred"))
    }

    /// Start the decisions of asked steps while the store window has room.
    fn ask_steps(&mut self) {
        let steps = &mut self.steps;
        while steps.storing < STORE_WINDOW {
            let Some(entry) = steps.asking.entries.pop_front() else {
                return;
            };
            if !Steps::asked(&steps.flights, &entry) {
                continue;
            }
            steps.asking.live -= 1;
            let key = entry.0;
            let flight = steps.flights.get_mut(&key).expect("a flight");
            flight.state = State::Deciding;
            steps.storing += 1;
            let (asked, serial) = (flight.ask.clone(), flight.serial);
            let captured = steps.captured.get(&key).cloned();
            let store = self.store.clone();
            steps.jobs.spawn(async move {
                let mut asked = asked;
                // A later step of a call reads its links before it is
                // asked; the first, once its call is captured.
                if let Some(capture) = &captured {
                    learn_links(capture, &mut asked).await;
                }
                let open = |asked: StepAsk, captured| {
                    store.op("step_open", move |db| {
                        db.step_open(&asked, captured, handles_now())
                    })
                };
                let opened = match open(asked.clone(), captured).await {
                    Ok(StepOpened::Learn { capture }) => {
                        learn_links(&capture, &mut asked).await;
                        match open(asked, Some(capture)).await {
                            Ok(StepOpened::Learn { .. }) => Err(Error::with(
                                "invalid_step",
                                "the step's links could not be read",
                            )),
                            opened => opened,
                        }
                    }
                    opened => opened,
                };
                (key, serial, Done::Opened(opened))
            });
        }
    }

    /// What a step's job brought back.
    pub(super) fn step_done(&mut self, key: Key, serial: u64, done: Done) {
        if let Done::Slot(slot) = done {
            self.steps.waiter = None;
            // A closed semaphore starts nothing more: no waiter again.
            self.steps.slots_closed = slot.is_none();
            self.steps.spare = slot;
            self.pump_steps();
            return;
        }
        if matches!(done, Done::Opened(_) | Done::Closed(..)) {
            self.steps.storing -= 1;
            self.ask_steps();
        }
        if !self.steps.current(&key, serial) {
            return;
        }
        match done {
            Done::Opened(opened) => self.step_opened(key, opened),
            Done::Ran(ran) => self.step_ran(key, ran),
            Done::Closed(closed, resumed) => {
                if let Some(resumed) = resumed {
                    self.call_resumed(key.0, resumed);
                }
                let flight = self.steps.take(&key).expect("a flight");
                let closed = match flight.stopped {
                    Some((why, _)) if closed.is_ok() => Err(stopped(why)),
                    _ => closed,
                };
                self.steps.ran += 1;
                reply(&mut self.steps, &flight, closed);
                self.pump_steps();
            }
            // Taken up above, under no call's key.
            Done::Slot(_) => {}
        }
    }

    /// Call `turn`'s time runs again, its last running step ended: whoever
    /// waits on it is told, so its deadline is armed again.
    fn call_resumed(&mut self, turn: i64, resumed: Resumed) {
        if let Some(notify) = resumed.notify {
            notify.notify_one();
        }
        if let Some(bot) = resumed.wake {
            self.handles.wake(bot, turn);
        }
        if let Some(process) = resumed.lifted {
            self.check_lifted(vec![process]);
        }
    }

    fn step_opened(&mut self, key: Key, opened: Result<StepOpened>) {
        let capture = match &opened {
            Ok(
                StepOpened::Run { capture, .. }
                | StepOpened::Refused { capture, .. }
                | StepOpened::Pending { capture, .. },
            ) => Some(capture.clone()),
            // Taken up in the job that asked, never brought back here.
            Ok(StepOpened::Learn { .. }) | Err(_) => None,
        };
        // Kept for the call's next steps, unless this one was stopped
        // while it was decided: a call that ended meanwhile asks no more,
        // and nothing would let its capture go.
        let stopped = (self.steps.flights.get(&key)).is_some_and(|flight| flight.stopped.is_some());
        if let Some(capture) = capture
            && !stopped
        {
            self.steps.captured.entry(key.clone()).or_insert(capture);
            self.steps.index(&key);
        }
        match opened {
            Ok(StepOpened::Run { capture, confined }) => self.line_step(key, capture, confined),
            Ok(StepOpened::Learn { .. }) => unreachable!("links are read in the job that asked"),
            Ok(StepOpened::Refused { reply: refused, .. }) => {
                let flight = self.steps.take(&key).expect("a flight");
                self.steps.ran += 1;
                reply(&mut self.steps, &flight, Ok(refused));
                self.pump_steps();
            }
            Ok(StepOpened::Pending {
                confined, stepped, ..
            }) => {
                let Stepped {
                    step:
                        agent_runtime::store::Step::Pending {
                            approval,
                            expires_ms,
                        },
                    notify,
                    wake,
                    ..
                } = stepped
                else {
                    unreachable!("a step put to a gate waits")
                };
                let flight = self.steps.flights.get_mut(&key).expect("a flight");
                flight.state = State::Gated { confined };
                self.handles.step_lapses(approval, expires_ms);
                if let Some(notify) = notify {
                    notify.notify_one();
                }
                if wake {
                    self.handles.wake(flight.ask.bot.clone(), key.0);
                }
                let gone = flight.stopped.is_some_and(|(_, gone)| gone);
                self.stepping.insert(
                    approval,
                    Stepping {
                        session: flight.session,
                        output: flight.output.clone(),
                        request: flight.request.clone(),
                        bot: flight.ask.bot.clone(),
                        turn: key.0,
                        id: flight.ask.id(),
                        gone,
                        checking: false,
                        again: false,
                        daemon: Some(key),
                    },
                );
                // Its verdict may have come before it was registered here,
                // its notice then passed over: one look at what is stored.
                self.check_steps(vec![approval], gone);
            }
            Err(error) => {
                // A call no longer waiting takes no more steps.
                // Another bot's asking of it, refused, leaves it be.
                let flight = self.steps.take(&key).expect("a flight");
                let owner = (self.steps.captured.get(&key))
                    .is_none_or(|capture| capture.bot.name == flight.ask.bot);
                if owner
                    && matches!(
                        error.code.as_str(),
                        "no_pending_call"
                            | "call_already_answered"
                            | "stale_turn"
                            | "turn_not_found"
                    )
                {
                    self.steps.forget(&key);
                }
                self.steps.refused += 1;
                reply(&mut self.steps, &flight, Err(error));
                self.pump_steps();
            }
        }
    }

    /// A gated daemon step decided: run once allowed, else answered with
    /// what its gates or its call's end made of it.
    pub(super) fn step_gated(&mut self, key: Key, decided: Result<Value>) {
        let Some(flight) = self.steps.flights.get_mut(&key) else {
            return;
        };
        let State::Gated { confined, .. } = flight.state else {
            return;
        };
        let allowed = decided
            .as_ref()
            .is_ok_and(|decided| decided["decision"] == "allow");
        if allowed {
            let capture = self.steps.captured.get(&key).cloned();
            match capture {
                Some(capture) => self.line_step(key, capture, confined),
                None => {
                    flight.state = State::Ending;
                    self.close_step(key, Err(stopped("its call ended")));
                }
            }
            return;
        }
        let flight = self.steps.take(&key).expect("a flight");
        let result = decided.map(|mut decided| {
            let reason = decided["reason"].clone();
            decided["output"] =
                json!(json!({"error":"approval_denied","detail":reason}).to_string());
            decided["failed"] = json!(true);
            decided
        });
        let result = match flight.stopped {
            Some((why, _)) => Err(stopped(why)),
            None => result,
        };
        self.steps.ran += 1;
        reply(&mut self.steps, &flight, result);
        self.pump_steps();
    }

    /// An allowed step, marked started, goes in line; one stopped while
    /// it was decided is closed so.
    fn line_step(&mut self, key: Key, capture: Arc<Capture>, confined: bool) {
        self.calls_ended();
        let held = self.held().map(|_| "the daemon was held before it started");
        let flight = self.steps.flights.get_mut(&key).expect("a flight");
        // Held, it is stopped as any stop does, and answers so.
        if flight.stopped.is_none() {
            flight.stopped = held.map(|why| (why, false));
        }
        if let Some((why, _)) = flight.stopped {
            flight.state = State::Ending;
            self.close_step(key, Err(stopped(why)));
            return;
        }
        // A background command takes its place in the registry's own line
        // as it starts, as a turn's does, and holds nothing of the step's.
        let command = matches!(
            flight.prepared,
            Some(Prepared::Shell {
                background: false,
                ..
            })
        );
        // A confined change to files runs its sandbox's helper as a process.
        let writes = matches!(
            flight.prepared,
            Some(Prepared::Write { .. } | Prepared::Edit { .. } | Prepared::ApplyPatch(_))
        );
        let slot = command || (confined && writes);
        flight.state = State::Lined {
            capture,
            confined,
            command,
            slot,
        };
        // One that needs no process slot needs no place: it starts now,
        // never behind a command waiting for one.
        if !slot {
            self.start_step(key, None);
            return;
        }
        let serial = flight.serial;
        self.steps.line.push(key, serial);
        self.pump_steps();
    }

    /// Start steps from the line while places are free: first come, first
    /// served; one that runs with a process slot only once it has one in
    /// hand, and a command only while the load gate lets one start.
    pub(super) fn pump_steps(&mut self) {
        // A slot the waiter brought back and nothing takes goes back.
        let mut spare = self.steps.spare.take();
        if self.held().is_some() {
            return;
        }
        loop {
            // A call's end told by now stops its step before it can start.
            self.calls_ended();
            let Some(entry) = self.steps.line.entries.front() else {
                return;
            };
            if !Steps::lined(&self.steps.flights, entry) {
                self.steps.line.entries.pop_front();
                continue;
            }
            let flight = &self.steps.flights[&entry.0];
            let State::Lined { command, slot, .. } = flight.state else {
                unreachable!("a lined flight");
            };
            let slot = match slot {
                false => None,
                true => {
                    if command && !self.registry.admits() {
                        self.steps.retry_at = Some(tokio::time::Instant::now() + LOAD_RETRY);
                        return;
                    }
                    match spare.take().or_else(|| self.registry.try_slot()) {
                        Some(slot) => Some(slot),
                        None => {
                            self.await_slot();
                            return;
                        }
                    }
                }
            };
            let (key, _) = self.steps.line.entries.pop_front().expect("an entry");
            self.steps.line.live -= 1;
            self.start_step(key, slot);
        }
    }

    /// Start a lined step, with its process slot when it takes one.
    fn start_step(&mut self, key: Key, slot: Option<Slot>) {
        let flight = self.steps.flights.get_mut(&key).expect("a flight");
        let State::Lined {
            capture,
            confined,
            command,
            ..
        } = std::mem::replace(&mut flight.state, State::Ending)
        else {
            unreachable!("a lined flight");
        };
        let prepared = flight.prepared.take().expect("its tool");
        let (stop, stopping) = oneshot::channel();
        flight.state = State::Running {
            stop: Some(stop),
            command,
        };
        if command {
            self.steps.commands += 1;
        }
        let running = Running {
            capture,
            confined,
            prepared,
            slot,
            cord: self.registry.cord(),
            turn: key.0,
            id: flight.ask.id(),
            store: self.store.clone(),
            lifts: turn::Lifts {
                store: self.store.clone(),
                registry: self.registry.clone(),
                handles: self.handles.clone(),
                failures: self.background_failures.clone(),
            },
        };
        let (registry, serial) = (self.registry.clone(), flight.serial);
        self.steps.jobs.spawn(async move {
            let ran = run(registry, running, stopping).await;
            (key, serial, Done::Ran(ran))
        });
    }

    /// Stand one waiter for the whole line in the semaphore's fair line,
    /// beside the turns' own commands, unless one stands there already.
    fn await_slot(&mut self) {
        if self.steps.waiter.is_some() || self.steps.slots_closed {
            return;
        }
        let slot = self.registry.slot();
        self.steps.waiter =
            Some((self.steps.jobs).spawn(async move { (waiter_key(), 0, Done::Slot(slot.await)) }));
    }

    fn step_ran(&mut self, key: Key, ran: Result<Outcome>) {
        let flight = self.steps.flights.get_mut(&key).expect("a flight");
        if let State::Running { command: true, .. } = flight.state {
            self.steps.commands -= 1;
        }
        flight.state = State::Ending;
        self.close_step(key, ran);
        self.pump_steps();
    }

    /// End a step that was marked started: its outputs kept, its end
    /// committed, then its reply.
    fn close_step(&mut self, key: Key, ran: Result<Outcome>) {
        let flight = self.steps.flights.get(&key).expect("a flight");
        let (turn, id, serial) = (key.0, flight.ask.id(), flight.serial);
        // The reply's id, as encoded, is part of what its pictures share.
        let envelope = ENVELOPE
            + agent_runtime::output::encoded_len(&flight.request).unwrap_or(usize::MAX / 2);
        let capture = self.steps.captured.get(&key).cloned();
        let outputs = capture.as_ref().and_then(|capture| {
            let root = PathBuf::from(
                (capture.bot.workspace.as_deref())
                    .or(capture.workspace.as_deref())
                    .unwrap_or_default(),
            );
            turn::Outputs::of(&capture.bot, root, self.registry.daemon())
        });
        let store = self.store.clone();
        self.steps.storing += 1;
        self.steps.jobs.spawn(async move {
            let (outcome, error) = match ran {
                Ok(outcome) => (outcome, None),
                Err(error) => {
                    let code = error.code.clone();
                    (failure(error), Some(code))
                }
            };
            // A picture's base64 is sized, moved and copied off the
            // runtime thread, as a lifted call's pictures are.
            let pictured = outcome.parts.iter().any(|part| part.image().is_some());
            let outcome = if pictured {
                let id = id.clone();
                tokio::task::spawn_blocking(move || parted(outcome, turn, &id, envelope))
                    .await
                    .map_err(|_| Error::new("step_task_failed"))
            } else {
                Ok(outcome)
            };
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => return (key, serial, Done::Closed(Err(error), None)),
            };
            let outcome = match outputs {
                Some(outputs) if !outcome.artifacts.is_empty() => {
                    outputs.keep(outcome, turn, &id).await
                }
                _ => outcome,
            };
            let outcome = turn::annotate(outcome, turn, &id);
            let built = |outcome: Outcome, id: String| {
                let mut reply =
                    json!({"call_id":id,"output":outcome.output,"failed":outcome.failed});
                // A picture read goes back as one, in the parts `tool_result`
                // takes, so the script can hand it on to the model.
                if !outcome.parts.is_empty() {
                    reply["parts"] = json!(outcome.parts);
                }
                (outcome, id, reply)
            };
            let built = if pictured {
                tokio::task::spawn_blocking(move || built(outcome, id))
                    .await
                    .map_err(|_| Error::new("step_task_failed"))
            } else {
                Ok(built(outcome, id))
            };
            let (outcome, id, reply) = match built {
                Ok(built) => built,
                Err(error) => return (key, serial, Done::Closed(Err(error), None)),
            };
            let closed = store
                .op("step_close", move |db| {
                    db.step_close(turn, &id, &outcome, error.as_deref(), handles_now())
                })
                .await;
            let (closed, resumed) = match closed {
                Ok(resumed) => (Ok(reply), resumed),
                Err(error) => (Err(error), None),
            };
            (key, serial, Done::Closed(closed, resumed))
        });
    }

    /// Stop the step of call `key`, for `why`: dropped from the line or
    /// stopped as it runs, its end recorded so. `gone` when its asker left,
    /// which is then told nothing.
    pub(super) fn stop_step(&mut self, key: &Key, why: &'static str, gone: bool) {
        let Some(flight) = self.steps.flights.get_mut(key) else {
            return;
        };
        if flight.stopped.is_none() {
            flight.stopped = Some((why, gone));
        } else if gone {
            flight.stopped = flight.stopped.map(|(why, _)| (why, true));
        }
        match &mut flight.state {
            // Nothing of it was recorded: dropped, and answered so.
            // Its entry in `asking` is passed over where it stands.
            State::Asked => {
                let flight = self.steps.take(key).expect("a flight");
                let flights = &self.steps.flights;
                (self.steps.asking).left(|entry| Steps::asked(flights, entry));
                reply(&mut self.steps, &flight, Err(stopped(why)));
            }
            State::Lined { .. } => {
                flight.state = State::Ending;
                let flights = &self.steps.flights;
                (self.steps.line).left(|entry| Steps::lined(flights, entry));
                self.close_step(key.clone(), Err(stopped(why)));
            }
            State::Running { stop, .. } => {
                if let Some(stop) = stop.take() {
                    let _ = stop.send(why);
                }
            }
            // Its decision, or its end, comes back and finds it stopped;
            // a gate's step its call's end or its asker's leaving ends.
            State::Deciding | State::Gated { .. } | State::Ending => {}
        }
    }

    /// Bot `bot` answers call `key`: its step stopped first, as
    /// [`call_ended`](Self::call_ended), only when the call is that bot's;
    /// another's answer, which the store refuses, stops nothing.
    pub(super) fn call_answered(&mut self, bot: &str, turn: i64, call_id: &str) {
        let key: Key = (turn, call_id.into());
        let owner = match self.steps.flights.get(&key) {
            Some(flight) => Some(flight.ask.bot.as_str()),
            None => self.steps.captured.get(&key).map(|c| c.bot.name.as_str()),
        };
        if owner.is_none_or(|owner| owner == bot) {
            self.call_ended(turn, call_id, "the call was answered");
        }
    }

    /// An answered call ended: its step stopped, and what it captured let go.
    pub(super) fn call_ended(&mut self, turn: i64, call_id: &str, why: &'static str) {
        let key: Key = (turn, call_id.into());
        self.steps.forget(&key);
        self.stop_step(&key, why, false);
    }

    /// The answered calls the store ended since the last look: each one's
    /// step stopped, and what it captured let go. Looked at as a step is
    /// let start too, so none starts once its call's end was told.
    pub(super) fn calls_ended(&mut self) {
        for (turn, call_id) in self.handles.take_ended() {
            self.call_ended(turn, &call_id, "its call ended");
        }
    }

    /// The daemon is held, paused or stopping: steps asking, in line or
    /// running are stopped, and answer `step_stopped` for `why` as every
    /// stop does, a running one's tool ending as the pull of the cord ends
    /// it. One decided or gated meanwhile is stopped before it can start.
    pub(super) fn steps_held(&mut self, why: &'static str) {
        let flights = &self.steps.flights;
        let running = flights
            .iter()
            .filter(|(_, flight)| matches!(flight.state, State::Running { .. }))
            .map(|(key, _)| key.clone());
        let lined: Vec<Key> = (self.steps.asking.entries.iter())
            .filter(|entry| Steps::asked(flights, entry))
            .chain((self.steps.line.entries.iter()).filter(|entry| Steps::lined(flights, entry)))
            .map(|(key, _)| key.clone())
            .chain(running)
            .collect();
        for key in lined {
            self.stop_step(&key, why, false);
        }
    }

    /// A session closed: its steps dropped from the line or stopped, and
    /// told nothing.
    pub(super) fn steps_session_closed(&mut self, session: u64) {
        let Some(keys) = self.steps.by_session.get(&session) else {
            return;
        };
        let keys: Vec<Key> = keys.iter().cloned().collect();
        for key in keys {
            self.stop_step(&key, "its asker left", true);
        }
    }

    /// The daemon stops: the line's waiter goes, and the steps still running
    /// are seen to their ends and recorded, a change to files already begun
    /// finishing, the rest ended by the stop the registry says. Bounded, as
    /// the daemon's other drains are.
    pub(super) async fn steps_stopping(&mut self) {
        self.steps_held("the daemon is stopping");
        if let Some(waiter) = self.steps.waiter.take() {
            waiter.abort();
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(done) = self.steps.jobs.join_next().await {
                if let Ok((key, serial, done)) = done {
                    self.step_done(key, serial, done);
                }
            }
        })
        .await;
    }

    /// A reply that waited for room was sent, or its session closed.
    pub(super) fn step_sent(&mut self, bytes: usize) {
        self.steps.bytes -= bytes;
    }
}

/// How a step the daemon stopped answers.
fn stopped(why: &'static str) -> Error {
    Error::with("step_stopped", why)
}

fn failure(error: Error) -> Outcome {
    Outcome {
        output: json!({"error":error.code,"detail":error.detail}).to_string(),
        artifacts: Vec::new(),
        files: Default::default(),
        parts: Vec::new(),
        failed: true,
    }
}

/// Read off the runtime thread the links a sandboxed bot's change to files
/// passes, as a direct call's are read before it is judged.
async fn learn_links(capture: &Capture, ask: &mut StepAsk) {
    let folder = PathBuf::from(capture.workspace.as_deref().unwrap_or_default());
    let calls = vec![ask.call()];
    let Some(learn) = capture.bot.links_to_learn(&calls, &folder, false) else {
        return;
    };
    let learned = tokio::task::spawn_blocking(move || {
        let mut calls = calls;
        learn(&mut calls);
        calls
    })
    .await;
    if let Ok(mut calls) = learned
        && let Some(call) = calls.pop()
    {
        ask.links = call.links;
    }
}

fn handles_now() -> u64 {
    super::handles::now_ms()
}

/// Run a step's tool: in the bot's sandbox when confined, in its turn's
/// folder, with what the turn's commands get in their environment, until
/// it ends, is stopped, or the pause or the daemon's stop pulls the cord.
async fn run(
    registry: agent_runtime::tools::Registry,
    running: Running,
    stopping: oneshot::Receiver<&'static str>,
) -> Result<Outcome> {
    let Running {
        capture,
        confined,
        prepared,
        slot,
        cord,
        turn,
        id,
        store,
        lifts,
    } = running;
    let mut stopping = futures_util::FutureExt::fuse(stopping);
    let bot = &capture.bot;
    let workspace = PathBuf::from(capture.workspace.as_deref().unwrap_or_default());
    let root = (bot.workspace.as_ref()).map_or_else(|| workspace.clone(), PathBuf::from);
    // A change to files runs to its end once begun, a stop or not: cut
    // off, it could be half made. Its end is recorded as it happened.
    // Until its sandbox is ready, nothing of it has begun.
    let lasting = matches!(
        prepared,
        Prepared::Write { .. } | Prepared::Edit { .. } | Prepared::ApplyPatch(_)
    );
    let ready = async {
        Ok::<_, Error>(match confined {
            true => {
                let (settings, daemon, id, root) = (
                    bot.settings.clone(),
                    registry.daemon().clone(),
                    bot.bot_id,
                    root.clone(),
                );
                tokio::task::spawn_blocking(move || {
                    turn::bot_sandbox(&settings, &root, id, &daemon)
                })
                .await
                .map_err(|_| Error::new("sandbox_unavailable"))
                .flatten()?
                .map(Arc::new)
            }
            // A sandboxed bot's read, which the daemon does outside any
            // sandbox, keeps out of the folders no sandboxed bot reads.
            false if bot.sandboxed() && matches!(prepared, Prepared::Read { .. }) => Some(
                Arc::new(agent_runtime::sandbox::Policy::reads(registry.daemon())),
            ),
            false => None,
        })
    };
    let sandbox = tokio::select! {
        biased;
        Ok(why) = &mut stopping => return Err(stopped(why)),
        ready = cord.clone().until_pulled(ready) => ready?,
    };
    let environment = turn::commands_environment(
        bot,
        &capture.model,
        (&bot.name, bot.bot_id),
        turn,
        capture.effort.as_deref(),
        bot.created_by.as_deref().zip(bot.created_by_id),
    );
    let held = cord.clone();
    let work = async {
        match prepared {
            Prepared::Shell {
                command,
                timeout_ms,
                background: false,
                workdir,
            } => {
                let spill = turn::Outputs::of(bot, root.clone(), registry.daemon())
                    .map(|outputs| outputs.spill(turn, &id));
                let (started, _running) = oneshot::channel();
                registry
                    .foreground_holding(
                        command,
                        Prepared::folder(&workspace, workdir.as_deref()),
                        timeout_ms,
                        environment,
                        sandbox,
                        registry.place(),
                        started,
                        spill,
                        slot,
                    )
                    .await
            }
            // As a turn's background command: the bot's, not the call's, so
            // its handle outlives the script, which hands it on for the
            // model to wait on, and its end is told as any one's. It runs
            // under its own name beside the step's (`CALL:STEP`), so what
            // it keeps outlives the call's answer, which lets the step's go.
            Prepared::Shell {
                command,
                timeout_ms,
                background: true,
                workdir,
            } => {
                let queued = registry.queue()?;
                let named = match id.split_once('/') {
                    Some((call, step)) => format!("{call}:{step}"),
                    None => id.clone(),
                };
                let listed = command.clone();
                let (owner, started) = (bot.name.clone(), named.clone());
                let outputs = turn::Outputs::of(bot, root.clone(), registry.daemon());
                let folder = Prepared::folder(&workspace, workdir.as_deref());
                let (store, registry, lifts, owner_id, cord) = (
                    store.clone(),
                    registry.clone(),
                    lifts.clone(),
                    bot.bot_id,
                    held.clone(),
                );
                // Its row, its command and its collector are made whole on
                // their own task: a step dropped meanwhile (its session
                // gone, its call answered, the daemon pausing) leaves no
                // row running that nothing collects or stops. The step's
                // cord goes with it, so a pause meanwhile keeps the command
                // from starting and its row ends stopped by it.
                let registered = tokio::spawn(async move {
                    let process = store
                        .op("process_start", move |db| {
                            db.process_start(
                                turn,
                                &started,
                                agent_runtime::store::ProcessKind::Background,
                                &listed,
                            )
                        })
                        .await?;
                    let spill = (outputs.clone()).map(|outputs| outputs.spill(turn, &named));
                    let (sender, receiver) = oneshot::channel();
                    registry.background(
                        cord,
                        registry.stopper(process),
                        owner_id,
                        command,
                        folder,
                        timeout_ms,
                        environment,
                        sandbox,
                        queued,
                        sender,
                        spill,
                    );
                    lifts.collect(
                        &owner, turn, process, &named, receiver, outputs, None, false,
                    );
                    Ok::<_, Error>(process)
                });
                let process = registered
                    .await
                    .map_err(|_| Error::new("storage_error"))??;
                Ok(Outcome::text(
                    json!({"handle":format!("proc:{process}"),"background":true}).to_string(),
                ))
            }
            // A command's output kept past its result, paged as the
            // turn's own read pages it, from any of the bot's turns.
            Prepared::Read {
                source:
                    ReadSource::Artifact {
                        turn: owner,
                        call_id,
                        stream,
                    },
                offset,
                limit,
            } => {
                let name = bot.name.clone();
                let page = match (store.op("artifact_lines", move |db| {
                    db.artifact_lines(&name, owner, &call_id, &stream, offset, limit)
                }))
                .await?
                {
                    ArtifactLines::Page(page) => page,
                    ArtifactLines::File(file) => tokio::task::spawn_blocking(move || {
                        agent_runtime::tools::page_file(&file, offset, limit)
                    })
                    .await
                    .map_err(|_| Error::new("storage_error"))??
                    .ok_or(Error::new("artifact_not_found"))?,
                };
                Ok(Outcome::text(registry.redact_text(page)))
            }
            prepared @ (Prepared::Echo(_)
            | Prepared::Read { .. }
            | Prepared::Write { .. }
            | Prepared::Edit { .. }
            | Prepared::ApplyPatch(_)
            | Prepared::WebFetch(_)) => {
                registry
                    .execute_holding(prepared, &workspace, &environment, sandbox, slot)
                    .await
            }
            _ => Err(Error::with(
                "invalid_step",
                "not one of the tools a daemon step runs",
            )),
        }
    };
    if lasting {
        return work.await;
    }
    tokio::select! {
        biased;
        Ok(why) = &mut stopping => Err(stopped(why)),
        ran = cord.until_pulled(work) => ran,
    }
}

/// What a reply carries beside its result and its encoded id: the object
/// around them.
const ENVELOPE: usize = 256;

/// The streams a step's pictures too large for its reply are kept under.
const PICTURES: [&str; 20] = [
    "picture-1",
    "picture-2",
    "picture-3",
    "picture-4",
    "picture-5",
    "picture-6",
    "picture-7",
    "picture-8",
    "picture-9",
    "picture-10",
    "picture-11",
    "picture-12",
    "picture-13",
    "picture-14",
    "picture-15",
    "picture-16",
    "picture-17",
    "picture-18",
    "picture-19",
    "picture-20",
];

/// A step's outcome with what of its pictures fits one reply, which the
/// transport bounds, in the reply, in order; each one past that is kept
/// as its own output, its base64 under `CALL/STEP` as `picture-N`, read
/// with `artifact` as any kept output is, and its output says so. Nothing
/// is decoded or encoded again: a step reads what the direct call does.
fn parted(mut outcome: Outcome, turn: i64, id: &str, envelope: usize) -> Outcome {
    if !outcome.parts.iter().any(|part| part.image().is_some()) {
        return outcome;
    }
    let size = |value: &Value| serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len());
    let bare = size(&json!({"call_id":id,"output":outcome.output,"failed":outcome.failed}));
    let mut room = output::MAX_EVENT.saturating_sub(bare.saturating_add(envelope));
    let (mut kept, mut lines) = (0, Vec::new());
    outcome.parts = (std::mem::take(&mut outcome.parts).into_iter())
        .filter_map(|part| {
            // A picture's base64 is sent as it is, in its own object.
            let bytes = match &part {
                Part::Image(image) => image.data.len() + image.media_type.len() + 64,
                Part::Text { .. } => size(&json!(part)),
            };
            if bytes <= room {
                room -= bytes;
                return Some(part);
            }
            let Part::Image(image) = part else {
                lines.push("words past the reply's room left out".to_owned());
                return None;
            };
            let stream = PICTURES[kept.min(PICTURES.len() - 1)];
            kept += 1;
            lines.push(format!(
                "a {} picture too large for the reply kept as {turn}/{id}/{stream}; read it with artifact",
                image.media_type
            ));
            outcome.artifacts.push((stream, image.data.into_bytes()));
            None
        })
        .collect();
    for line in lines {
        outcome.output.push_str("; ");
        outcome.output.push_str(&line);
    }
    outcome
}

#[cfg(test)]
mod tests {
    //! The coordinator over every state a step can be in against every
    //! cause that stops one: what it does at once, and how the step ends.
    use super::super::tests::{bare_service, running};
    use super::*;
    use agent_runtime::store::Store;
    use std::time::Duration;

    #[derive(Clone, Copy, Debug)]
    enum Cause {
        /// Its own bot answers the call.
        Answered,
        /// Another bot's answer, which the store refuses.
        Others,
        /// The store told of the call's end: taken, out of time, or gone
        /// with its turn or its lifted command.
        Ended,
        /// The daemon was paused.
        Paused,
        /// The session that asked closed.
        Left,
    }

    #[derive(Clone, Copy, Debug)]
    enum Phase {
        Asked,
        Deciding,
        Gated,
        Lined,
        Running,
        Ending,
    }

    /// How a step came out.
    #[derive(Debug, PartialEq)]
    enum End {
        /// Answered `step_stopped`, for this reason.
        Stopped(&'static str),
        /// Answered as it ran: nothing stopped it.
        Ran,
        /// Let start: nothing stopped it.
        Started,
        /// Still waiting for its decision: nothing stopped it.
        Asking,
        /// Told nothing: its asker left.
        Silent,
    }

    struct Bench {
        service: Service,
        lines: output::Lines,
        key: Key,
        serial: u64,
        capture: Arc<Capture>,
        stop: Option<oneshot::Receiver<&'static str>>,
    }

    async fn bench(dir: &std::path::Path, phase: Phase) -> Bench {
        let (store, _publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let turn = running(&store, &["Bob".into()]).await[0].1;
        let bot = store.call(|db| db.inspect("Bob")).await.unwrap();
        let capture = Arc::new(Capture {
            bot,
            rules: Default::default(),
            workspace: None,
            model: "synthetic".into(),
            effort: None,
        });
        let mut service = bare_service(&store);
        let (output, lines) = Output::in_process();
        let key: Key = (turn, "c".into());
        let (state, prepared, stop) = match phase {
            Phase::Asked => (State::Asked, Prepared::Echo("hi".into()), None),
            Phase::Deciding => (State::Deciding, Prepared::Echo("hi".into()), None),
            Phase::Gated => (
                State::Gated { confined: false },
                Prepared::Echo("hi".into()),
                None,
            ),
            // A command waits in line for a process slot.
            Phase::Lined => (
                State::Lined {
                    capture: capture.clone(),
                    confined: false,
                    command: true,
                    slot: true,
                },
                Prepared::Shell {
                    command: "true".into(),
                    timeout_ms: None,
                    background: false,
                    workdir: None,
                },
                None,
            ),
            Phase::Running => {
                let (stop, stopping) = oneshot::channel();
                (
                    State::Running {
                        stop: Some(stop),
                        command: false,
                    },
                    Prepared::Echo("hi".into()),
                    Some(stopping),
                )
            }
            Phase::Ending => (State::Ending, Prepared::Echo("hi".into()), None),
        };
        let steps = &mut service.steps;
        steps.serial += 1;
        let serial = steps.serial;
        steps.flights.insert(
            key.clone(),
            Flight {
                serial,
                session: 1,
                output,
                request: json!(7),
                ask: StepAsk {
                    bot: "Bob".into(),
                    turn,
                    call_id: "c".into(),
                    step: "s".into(),
                    tool: "echo".into(),
                    arguments: "{}".into(),
                    links: Default::default(),
                },
                prepared: Some(prepared),
                bytes: 0,
                state,
                stopped: None,
            },
        );
        steps.index(&key);
        (steps.by_session.entry(1).or_default()).insert(key.clone());
        *steps.by_bot.entry("Bob".into()).or_default() += 1;
        match phase {
            Phase::Asked => steps.asking.push(key.clone(), serial),
            Phase::Lined => steps.line.push(key.clone(), serial),
            // Its decision's store job, or its end's, is under way.
            Phase::Deciding | Phase::Ending => steps.storing += 1,
            Phase::Gated | Phase::Running => {}
        }
        // Decided, it has captured its call.
        if !matches!(phase, Phase::Asked | Phase::Deciding) {
            steps.captured.insert(key.clone(), capture.clone());
        }
        Bench {
            service,
            lines,
            key,
            serial,
            capture,
            stop,
        }
    }

    fn stop(bench: &mut Bench, cause: Cause) {
        let (turn, call) = (bench.key.0, bench.key.1.to_string());
        let service = &mut bench.service;
        match cause {
            Cause::Answered => service.call_answered("Bob", turn, &call),
            Cause::Others => service.call_answered("Eve", turn, &call),
            Cause::Ended => {
                service.handles.call_ended(turn, call);
                service.calls_ended();
            }
            Cause::Paused => {
                service.paused = Some(json!({}));
                service.steps_held("the pause stopped it");
            }
            Cause::Left => service.steps_session_closed(1),
        }
    }

    /// What its decision's job, its gate, its run or its end's job brings
    /// back next, as the store and its tool would.
    fn go_on(bench: &mut Bench, phase: Phase) {
        let (key, serial) = (bench.key.clone(), bench.serial);
        let service = &mut bench.service;
        if !service.steps.current(&key, serial) {
            return;
        }
        let ran = || Outcome {
            output: "hi".into(),
            artifacts: Vec::new(),
            files: Default::default(),
            parts: Vec::new(),
            failed: false,
        };
        match phase {
            Phase::Asked | Phase::Lined => service.pump_steps(),
            Phase::Deciding => {
                let opened = StepOpened::Run {
                    capture: bench.capture.clone(),
                    confined: false,
                };
                service.step_done(key, serial, Done::Opened(Ok(opened)));
            }
            Phase::Gated => service.step_gated(key, Ok(json!({"decision":"allow"}))),
            Phase::Running => service.step_done(key, serial, Done::Ran(Ok(ran()))),
            Phase::Ending => {
                let reply = json!({"call_id":"c/s","output":"hi","failed":false});
                service.step_done(key, serial, Done::Closed(Ok(reply), None));
            }
        }
    }

    async fn end(bench: &mut Bench) -> End {
        let key = bench.key.clone();
        // Its end's store job, when one was started, comes back.
        while let Some(flight) = bench.service.steps.flights.get(&key) {
            match flight.state {
                State::Asked => return End::Asking,
                State::Running { .. } => return End::Started,
                State::Ending => {
                    let done = tokio::time::timeout(
                        Duration::from_secs(10),
                        bench.service.steps.jobs.join_next(),
                    )
                    .await
                    .expect("its end's job")
                    .expect("a job")
                    .expect("not panicked");
                    let (key, serial, done) = done;
                    bench.service.step_done(key, serial, done);
                }
                _ => panic!("left mid-way"),
            }
        }
        let line = tokio::time::timeout(Duration::from_millis(100), bench.lines.next()).await;
        let Ok(Some(line)) = line else {
            return End::Silent;
        };
        let reply: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(reply["id"], 7, "{reply}");
        match reply["error"].as_str() {
            Some("step_stopped") => End::Stopped(match reply["detail"].as_str().unwrap() {
                "the call was answered" => "the call was answered",
                "its call ended" => "its call ended",
                "the pause stopped it" => "the pause stopped it",
                "the daemon was held before it started" => "the daemon was held before it started",
                other => panic!("stopped for {other}"),
            }),
            None => End::Ran,
            Some(other) => panic!("answered {other}: {reply}"),
        }
    }

    #[tokio::test]
    async fn every_state_against_every_cause_that_stops_a_step() {
        use Cause::*;
        use End::*;
        const ANSWERED: End = Stopped("the call was answered");
        const ENDED: End = Stopped("its call ended");
        const PAUSED: End = Stopped("the pause stopped it");
        const HELD: End = Stopped("the daemon was held before it started");
        // Each state's row: Answered, Others, Ended, Paused, Left. One
        // paused while decided or gated is stopped before it can start; one
        // ending has its end with the store already.
        let table: [(Phase, [End; 5]); 6] = [
            (Phase::Asked, [ANSWERED, Asking, ENDED, PAUSED, Silent]),
            (Phase::Deciding, [ANSWERED, Started, ENDED, HELD, Silent]),
            (Phase::Gated, [ANSWERED, Started, ENDED, HELD, Silent]),
            (Phase::Lined, [ANSWERED, Started, ENDED, PAUSED, Silent]),
            (Phase::Running, [ANSWERED, Ran, ENDED, PAUSED, Silent]),
            (Phase::Ending, [ANSWERED, Ran, ENDED, Ran, Silent]),
        ];
        let root = std::env::temp_dir().join(format!("agent-step-stops-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (phase, row) in table {
            for (cause, want) in [Answered, Others, Ended, Paused, Left].into_iter().zip(row) {
                let dir = root.join(format!("{phase:?}-{cause:?}"));
                std::fs::create_dir_all(&dir).unwrap();
                let mut bench = bench(&dir, phase).await;
                stop(&mut bench, cause);
                // A running step is sent its stop at once, when stopped.
                if let Some(stopping) = &mut bench.stop {
                    let sent = stopping.try_recv().ok();
                    let stopped = !matches!(cause, Others);
                    assert_eq!(sent.is_some(), stopped, "{phase:?} {cause:?}: {sent:?}");
                }
                go_on(&mut bench, phase);
                let got = end(&mut bench).await;
                assert_eq!(got, want, "{phase:?} stopped by {cause:?}");
                // Stopped by its call's end, its capture went with it.
                if matches!(cause, Answered | Ended) {
                    assert!(!bench.service.steps.captured.contains_key(&bench.key));
                }
                // An ended step holds nothing.
                if !matches!(got, Asking | Started) {
                    let steps = &bench.service.steps;
                    assert!(steps.flights.is_empty(), "{phase:?} {cause:?}");
                    assert!(steps.by_bot.is_empty() && steps.by_session.is_empty());
                    assert_eq!((steps.asking.live, steps.line.live), (0, 0));
                }
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
