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
//! steps wait in line behind its own. A command then takes a process slot
//! from the registry's semaphore, which is fair, in the order it started,
//! among the turns' own commands; the coordinator starts no more commands
//! than the process bound has slots, so none waits on a slot holding
//! another, and none starts while the load gate holds new commands back.
//! Past the line's bounds a step is refused, and asking again is the only
//! way back in.
use super::{Service, Stepping, turn};
use agent_runtime::{
    Error, Result, fail_with,
    output::Output,
    store::{Capture, StepAsk, StepOpened, Stepped},
    tools::{Outcome, Prepared},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
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
    /// Steps waiting for their decision's store job, first come first
    /// served.
    asking: VecDeque<Key>,
    /// Steps' store jobs under way, at most [`STORE_WINDOW`] decisions.
    storing: usize,
    /// Allowed steps waiting for a place, first come first served.
    line: VecDeque<Key>,
    /// Bytes the steps in flight and their unsent replies hold.
    bytes: usize,
    /// Commands started and not ended.
    commands: usize,
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
    /// Allowed and waiting for a place: a command when `command`.
    Lined {
        capture: Arc<Capture>,
        confined: bool,
        command: bool,
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
    Closed(Result<Value>),
}

/// What a step's tool runs with: its sandbox, folder and environment.
struct Running {
    capture: Arc<Capture>,
    confined: bool,
    prepared: Prepared,
    turn: i64,
    id: String,
}

impl Steps {
    /// Steps in flight, in line, and commands running, for `stats`.
    pub(super) fn counts(&self) -> Value {
        json!({"in_flight":self.flights.len(),"asked":self.asking.len(),"in_line":self.line.len(),
            "commands":self.commands,"bytes":self.bytes,"ran":self.ran,"refused":self.refused})
    }

    fn take(&mut self, key: &Key) -> Option<Flight> {
        let flight = self.flights.remove(key)?;
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
        let bytes = ask.arguments.len() + ask.call_id.len() + ask.step.len() + STEP_OVERHEAD;
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
        steps.asking.push_back(key);
        self.ask_steps();
        Err(Error::new("deferred"))
    }

    /// Start the decisions of asked steps while the store window has room.
    fn ask_steps(&mut self) {
        let steps = &mut self.steps;
        while steps.storing < STORE_WINDOW {
            let Some(key) = steps.asking.pop_front() else {
                return;
            };
            let Some(flight) = steps.flights.get_mut(&key) else {
                continue;
            };
            if !matches!(flight.state, State::Asked) {
                continue;
            }
            flight.state = State::Deciding;
            steps.storing += 1;
            let (asked, serial) = (flight.ask.clone(), flight.serial);
            let captured = steps.captured.get(&key).cloned();
            let store = self.store.clone();
            steps.jobs.spawn(async move {
                let opened = store
                    .op("step_open", move |db| {
                        db.step_open(&asked, captured, handles_now())
                    })
                    .await;
                (key, serial, Done::Opened(opened))
            });
        }
    }

    /// What a step's job brought back.
    pub(super) fn step_done(&mut self, key: Key, serial: u64, done: Done) {
        if matches!(done, Done::Opened(_) | Done::Closed(_)) {
            self.steps.storing -= 1;
            self.ask_steps();
        }
        if !self.steps.current(&key, serial) {
            return;
        }
        match done {
            Done::Opened(opened) => self.step_opened(key, opened),
            Done::Ran(ran) => self.step_ran(key, ran),
            Done::Closed(closed) => {
                let flight = self.steps.take(&key).expect("a flight");
                let closed = match flight.stopped {
                    Some((why, _)) if closed.is_ok() => Err(stopped(why)),
                    _ => closed,
                };
                self.steps.ran += 1;
                reply(&mut self.steps, &flight, closed);
                self.pump_steps();
            }
        }
    }

    fn step_opened(&mut self, key: Key, opened: Result<StepOpened>) {
        let capture = match &opened {
            Ok(
                StepOpened::Run { capture, .. }
                | StepOpened::Refused { capture, .. }
                | StepOpened::Pending { capture, .. },
            ) => Some(capture.clone()),
            Err(_) => None,
        };
        if let Some(capture) = capture {
            self.steps.captured.entry(key.clone()).or_insert(capture);
        }
        match opened {
            Ok(StepOpened::Run { capture, confined }) => self.line_step(key, capture, confined),
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
                if gone {
                    self.check_steps(vec![approval], true);
                }
            }
            Err(error) => {
                // A call no longer waiting takes no more steps.
                if matches!(
                    error.code.as_str(),
                    "no_pending_call" | "call_already_answered" | "stale_turn" | "turn_not_found"
                ) {
                    self.steps.captured.remove(&key);
                }
                let flight = self.steps.take(&key).expect("a flight");
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
        let held = self.held().map(|_| "the daemon was held before it started");
        let flight = self.steps.flights.get_mut(&key).expect("a flight");
        if let Some(why) = flight.stopped.map(|(why, _)| why).or(held) {
            flight.state = State::Ending;
            self.close_step(key, Err(stopped(why)));
            return;
        }
        let command = matches!(flight.prepared, Some(Prepared::Shell { .. }));
        flight.state = State::Lined {
            capture,
            confined,
            command,
        };
        self.steps.line.push_back(key);
        self.pump_steps();
    }

    /// Start steps from the line while places are free: first come, first
    /// served; a command only while the process bound has a slot for it
    /// and the load gate lets one start.
    pub(super) fn pump_steps(&mut self) {
        if self.held().is_some() {
            return;
        }
        let budget = self.limits.processes.max(1);
        while let Some(key) = self.steps.line.front() {
            let Some(flight) = self.steps.flights.get(key) else {
                self.steps.line.pop_front();
                continue;
            };
            let command = matches!(flight.state, State::Lined { command: true, .. });
            if command {
                if self.steps.commands >= budget {
                    return;
                }
                if !self.registry.admits() {
                    self.steps.retry_at = Some(tokio::time::Instant::now() + LOAD_RETRY);
                    return;
                }
            }
            let key = self.steps.line.pop_front().expect("a key");
            let flight = self.steps.flights.get_mut(&key).expect("a flight");
            let State::Lined {
                capture, confined, ..
            } = std::mem::replace(&mut flight.state, State::Ending)
            else {
                continue;
            };
            let Some(prepared) = flight.prepared.take() else {
                continue;
            };
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
                turn: key.0,
                id: flight.ask.id(),
            };
            let (registry, serial) = (self.registry.clone(), flight.serial);
            self.steps.jobs.spawn(async move {
                let ran = run(registry, running, stopping).await;
                (key, serial, Done::Ran(ran))
            });
        }
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
            let outcome = match outputs {
                Some(outputs) if !outcome.artifacts.is_empty() => {
                    outputs.keep(outcome, turn, &id).await
                }
                _ => outcome,
            };
            let outcome = turn::annotate(outcome, turn, &id);
            let reply = json!({"call_id":id,"output":outcome.output,"failed":outcome.failed});
            let closed = store
                .op("step_close", move |db| {
                    db.step_close(turn, &id, &outcome, error.as_deref(), handles_now())
                })
                .await
                .map(|()| reply);
            (key, serial, Done::Closed(closed))
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
            // Its entry in `asking` finds no flight and is passed over.
            State::Asked => {
                let flight = self.steps.take(key).expect("a flight");
                reply(&mut self.steps, &flight, Err(stopped(why)));
            }
            State::Lined { .. } => {
                flight.state = State::Ending;
                self.steps.line.retain(|lined| lined != key);
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

    /// An answered call ended: its step stopped, and what it captured let go.
    pub(super) fn call_ended(&mut self, turn: i64, call_id: &str, why: &'static str) {
        let key: Key = (turn, call_id.into());
        self.steps.captured.remove(&key);
        self.stop_step(&key, why, false);
    }

    /// A turn ended: the steps of its calls stopped, but those of a call a
    /// message lifted, which waits on past its turn. Which those are, the
    /// store says, read only when the turn has steps.
    pub(super) fn steps_turn_ended(&mut self, turn: i64) {
        let calls: Vec<Box<str>> = (self.steps.captured.keys())
            .chain(self.steps.flights.keys())
            .filter(|(of, _)| *of == turn)
            .map(|(_, call)| call.clone())
            .collect();
        if calls.is_empty() {
            return;
        }
        let (store, handles) = (self.store.clone(), self.handles.clone());
        tokio::spawn(async move {
            let lifted = store
                .op("lifted_calls", move |db| db.lifted_calls(turn))
                .await
                .unwrap_or_default();
            for call in calls {
                if !lifted.iter().any(|lifted| **lifted == *call) {
                    handles.call_ended(turn, call.into());
                }
            }
        });
    }

    /// The daemon is held, paused or stopping: steps in line are stopped,
    /// as a running one is by the pull of the cord.
    pub(super) fn steps_held(&mut self, why: &'static str) {
        let lined: Vec<Key> = (self.steps.asking.iter())
            .chain(self.steps.line.iter())
            .cloned()
            .collect();
        for key in lined {
            self.stop_step(&key, why, false);
        }
    }

    /// A session closed: its steps dropped from the line or stopped, and
    /// told nothing.
    pub(super) fn steps_session_closed(&mut self, session: u64) {
        let keys: Vec<Key> = (self.steps.flights.iter())
            .filter(|(_, flight)| flight.session == session)
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.stop_step(&key, "its asker left", true);
        }
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
        turn,
        id,
    } = running;
    let cord = registry.cord();
    let bot = &capture.bot;
    let workspace = PathBuf::from(capture.workspace.as_deref().unwrap_or_default());
    let root = (bot.workspace.as_ref()).map_or_else(|| workspace.clone(), PathBuf::from);
    // A change to files runs to its end once begun, a stop or not: cut
    // off, it could be half made. Its end is recorded as it happened.
    let lasting = matches!(
        prepared,
        Prepared::Write { .. } | Prepared::Edit { .. } | Prepared::ApplyPatch(_)
    );
    let work = async {
        let sandbox = match confined {
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
        };
        let environment = turn::commands_environment(
            bot,
            &capture.model,
            (&bot.name, bot.bot_id),
            turn,
            capture.effort.as_deref(),
            bot.created_by.as_deref().zip(bot.created_by_id),
        );
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
                    .foreground(
                        command,
                        Prepared::folder(&workspace, workdir.as_deref()),
                        timeout_ms,
                        environment,
                        sandbox,
                        registry.place(),
                        started,
                        spill,
                    )
                    .await
            }
            prepared @ (Prepared::Echo(_)
            | Prepared::Read { .. }
            | Prepared::Write { .. }
            | Prepared::Edit { .. }
            | Prepared::ApplyPatch(_)
            | Prepared::WebFetch(_)) => {
                registry
                    .execute(prepared, &workspace, &environment, sandbox)
                    .await
            }
            _ => Err(Error::with(
                "invalid_step",
                "not one of the tools a daemon step runs",
            )),
        }
    };
    if lasting {
        drop(stopping);
        return work.await;
    }
    tokio::select! {
        biased;
        Ok(why) = stopping => Err(stopped(why)),
        ran = cord.until_pulled(work) => ran,
    }
}
