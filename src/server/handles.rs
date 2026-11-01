//! Things a waiter can wait on without holding execution capacity: peer
//! turns, resolved from the daemon's own completions, and background commands
//! recorded in the store. A waiter is either a parked turn (a store row plus a
//! registry entry, no task) or a client request awaiting its response.
//!
//! What waits here has no task or timer of its own: every deadline, a
//! wait's and a waiting step's, is one entry in this registry's schedule,
//! which the service's run loop sleeps on.
use agent_runtime::{
    Result, fail_with,
    output::Output,
    store::{Reader, Store},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handle {
    Turn { bot: String, turn: i64 },
    Process(i64),
}

impl Handle {
    pub fn parse(text: &str) -> Result<Handle> {
        if let Some(rest) = text.strip_prefix("turn:")
            && let Some((bot, turn)) = rest.rsplit_once('/')
            && let Ok(turn) = turn.parse::<i64>()
            && !bot.is_empty()
            && turn > 0
            && text == format!("turn:{bot}/{turn}")
        {
            return Ok(Handle::Turn {
                bot: bot.into(),
                turn,
            });
        }
        if let Some(rest) = text.strip_prefix("proc:")
            && let Ok(id) = rest.parse::<i64>()
            && id > 0
            && rest == id.to_string()
        {
            return Ok(Handle::Process(id));
        }
        fail_with("invalid_handle", text)
    }
}
impl std::fmt::Display for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Handle::Turn { bot, turn } => write!(f, "turn:{bot}/{turn}"),
            Handle::Process(id) => write!(f, "proc:{id}"),
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What to do once every handle has resolved.
#[derive(Clone)]
pub enum Completion {
    /// Spawn a task that continues the parked turn.
    Resume { bot: String, turn: i64 },
    /// Answer a client's `wait` request on its session. A turn still
    /// pending is shown as it stands, read through `reader`, which keeps
    /// no writer open past shutdown.
    Respond {
        session: u64,
        output: Output,
        request: Value,
        reader: Reader,
    },
}

/// Who is waiting: parked turns are keyed by turn id, requests by a counter.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Waiter {
    Turn(i64),
    Request(u64),
}

struct Waiting {
    generation: u64,
    /// Complete on the first resolved handle; the rest are reported pending
    /// and stay valid for a later wait.
    any: bool,
    remaining: BTreeSet<String>,
    results: BTreeMap<String, Arc<Value>>,
    completion: Completion,
    /// When the wait gives up on what is still pending, as scheduled.
    deadline: Option<u64>,
}
/// What comes due on the schedule: a wait's deadline, by its generation, or
/// a waiting step's lapse, by its approval.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Due {
    Wait(Waiter, u64),
    Step(i64),
}
struct Retained {
    value: Arc<Value>,
    waiters: usize,
}
#[derive(Default)]
struct Inner {
    next_generation: u64,
    next_request: u64,
    waiters: HashMap<Waiter, Waiting>,
    by_turn: HashMap<(String, i64), Vec<Waiter>>,
    by_process: HashMap<i64, Vec<Waiter>>,
    // Only immutable, durable outcomes, retained while a waiter needs them.
    retained: HashMap<String, Retained>,
    // Answers still reading their pending turns, each holding its session's
    // output; shutdown ends them before the outputs close.
    reads: tokio::task::JoinSet<()>,
    schedule: BTreeSet<(u64, Due)>,
    /// Each waiting step's lapse on the schedule.
    lapses: HashMap<i64, u64>,
    /// Steps an answer or their turn's end may have decided, by approval,
    /// for the run loop to check.
    stepped: Vec<i64>,
}

#[derive(Clone)]
pub struct Handles {
    inner: Arc<Mutex<Inner>>,
    resume: mpsc::UnboundedSender<(String, i64)>,
    /// Told when the schedule's first entry comes sooner, or a step is
    /// stepped: the run loop's sleep is out of date.
    sooner: Arc<tokio::sync::Notify>,
    /// Told when a turn became ready away from the run loop, as one that
    /// tells a bot its background command ended does.
    ready: Arc<tokio::sync::Notify>,
}

impl Handles {
    pub fn new(resume: mpsc::UnboundedSender<(String, i64)>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            resume,
            sooner: Default::default(),
            ready: Default::default(),
        }
    }

    /// A turn is ready for a slot: the run loop looks for it.
    pub fn turn_ready(&self) {
        self.ready.notify_one();
    }

    /// Until a turn became ready away from the run loop.
    pub async fn readied(&self) {
        self.ready.notified().await;
    }

    fn schedule_locked(inner: &mut Inner, sooner: &tokio::sync::Notify, at: u64, due: Due) {
        if inner.schedule.first().is_none_or(|&(first, _)| at < first) {
            sooner.notify_one();
        }
        inner.schedule.insert((at, due));
    }

    /// Remove a waiter, and its deadline from the schedule.
    fn remove_locked(inner: &mut Inner, waiter: Waiter) -> Option<Waiting> {
        let waiting = inner.waiters.remove(&waiter)?;
        if let Some(at) = waiting.deadline {
            inner
                .schedule
                .remove(&(at, Due::Wait(waiter, waiting.generation)));
        }
        Some(waiting)
    }

    /// Put a waiting step's lapse on the schedule, in place of any it had:
    /// none when no gate of it lapses, or once it is decided.
    pub fn step_lapses(&self, approval: i64, at: Option<u64>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(old) = inner.lapses.remove(&approval) {
            inner.schedule.remove(&(old, Due::Step(approval)));
        }
        if let Some(at) = at {
            inner.lapses.insert(approval, at);
            Self::schedule_locked(&mut inner, &self.sooner, at, Due::Step(approval));
        }
    }

    /// A waiting step was answered, or its turn or call ended: the run
    /// loop checks it. Told after the store committed.
    pub fn stepped(&self, approval: i64) {
        self.inner.lock().unwrap().stepped.push(approval);
        self.sooner.notify_one();
    }

    /// Until something on the schedule comes due or a step was stepped:
    /// the run loop's one sleep for every deadline here.
    pub async fn due(&self) {
        loop {
            let next = {
                let inner = self.inner.lock().unwrap();
                if !inner.stepped.is_empty() {
                    return;
                }
                inner.schedule.first().map(|&(at, _)| at)
            };
            let sooner = self.sooner.notified();
            match next {
                Some(at) if at <= now_ms() => return,
                Some(at) => {
                    let delay = Duration::from_millis(at - now_ms().min(at));
                    tokio::select! {
                        () = tokio::time::sleep(delay) => {}
                        () = sooner => {}
                    }
                }
                None => sooner.await,
            }
        }
    }

    /// Give up the waits whose deadlines came by `now`, and hand back the
    /// steps to check: those stepped, and those whose lapse came.
    pub fn take_due(&self, now: u64) -> Vec<i64> {
        let mut inner = self.inner.lock().unwrap();
        let mut steps = std::mem::take(&mut inner.stepped);
        while let Some(&(at, due)) = inner.schedule.first()
            && at <= now
        {
            inner.schedule.pop_first();
            match due {
                Due::Wait(waiter, generation) => {
                    if let Some(waiting) = inner.waiters.get_mut(&waiter)
                        && waiting.generation == generation
                    {
                        waiting.deadline = None;
                        Self::expire_locked(&mut inner, &self.resume, waiter);
                    }
                }
                Due::Step(approval) => {
                    inner.lapses.remove(&approval);
                    steps.push(approval);
                }
            }
        }
        steps.sort_unstable();
        steps.dedup();
        steps
    }

    /// Wake anyone parked on a background command whose result is recorded.
    pub fn process_finished(&self, id: i64, result: Value) {
        let mut inner = self.inner.lock().unwrap();
        let handle = Handle::Process(id).to_string();
        let result = Arc::new(result);
        for waiter in inner.by_process.remove(&id).unwrap_or_default() {
            Self::resolve_locked(
                &mut inner,
                &self.resume,
                waiter,
                &handle,
                result.clone(),
                true,
            );
        }
    }

    /// Wake every waiter parked on a finished peer turn.
    pub fn turn_finished(&self, bot: &str, turn: i64, outcome: Value) {
        let mut inner = self.inner.lock().unwrap();
        let handle = Handle::Turn {
            bot: bot.into(),
            turn,
        }
        .to_string();
        let outcome = Arc::new(outcome);
        for waiter in inner
            .by_turn
            .remove(&(bot.to_owned(), turn))
            .unwrap_or_default()
        {
            Self::resolve_locked(
                &mut inner,
                &self.resume,
                waiter,
                &handle,
                outcome.clone(),
                true,
            );
        }
    }

    fn resolve_locked(
        inner: &mut Inner,
        resume: &mpsc::UnboundedSender<(String, i64)>,
        waiter: Waiter,
        handle: &str,
        result: Arc<Value>,
        durable: bool,
    ) {
        let Some(waiting) = inner.waiters.get_mut(&waiter) else {
            return;
        };
        if !waiting.remaining.remove(handle) {
            return;
        }
        let result = if durable {
            let retained = inner.retained.entry(handle.to_owned()).or_insert(Retained {
                value: result,
                waiters: 0,
            });
            retained.waiters += 1;
            retained.value.clone()
        } else {
            result
        };
        waiting.results.insert(handle.to_owned(), result);
        if waiting.remaining.is_empty() {
            Self::complete_locked(inner, resume, waiter);
        } else if waiting.any {
            // First one wins; the others are reported pending, exactly as a
            // timeout would report them, and stay valid for a later wait.
            Self::pend_remaining(waiting);
            Self::complete_locked(inner, resume, waiter);
        }
    }

    fn pend_remaining(waiting: &mut Waiting) {
        for handle in std::mem::take(&mut waiting.remaining) {
            waiting
                .results
                .insert(handle, Arc::new(json!({"pending":true})));
        }
    }

    fn complete_locked(
        inner: &mut Inner,
        resume: &mpsc::UnboundedSender<(String, i64)>,
        waiter: Waiter,
    ) {
        let completion = inner.waiters[&waiter].completion.clone();
        match completion {
            Completion::Resume { bot, turn } => {
                let _ = resume.send((bot, turn));
            }
            Completion::Respond {
                output,
                request,
                reader,
                ..
            } => {
                // A request waiter is consumed by its response.
                Self::detach_locked(inner, waiter);
                let results = Self::remove_locked(inner, waiter)
                    .map(|mut w| std::mem::take(&mut w.results))
                    .unwrap_or_default();
                let pending: Vec<(String, String, i64)> = results
                    .iter()
                    .filter(|(_, result)| result["pending"] == true)
                    .filter_map(|(text, _)| match Handle::parse(text) {
                        Ok(Handle::Turn { bot, turn }) => Some((text.clone(), bot, turn)),
                        _ => None,
                    })
                    .collect();
                if pending.is_empty() {
                    respond(&output, request, results);
                    return;
                }
                // Pending turns are read in one job on the reader, off the
                // registry lock, as `wait` reports a turn: one that finished
                // meanwhile answers with its outcome, one that vanished stays
                // pending, and a failed read says so for each.
                while inner.reads.try_join_next().is_some() {}
                inner.reads.spawn(async move {
                    let mut results = results;
                    let texts: Vec<String> =
                        pending.iter().map(|(text, ..)| text.clone()).collect();
                    let answers = reader
                        .read("turn_answers", move |db| {
                            Ok(pending
                                .into_iter()
                                .filter_map(|(text, bot, turn)| match db.turn_answer(&bot, turn) {
                                    Ok(answer) => Some((text, answer)),
                                    Err(error) if error.code == "turn_not_found" => None,
                                    Err(error) => Some((text, failed(&error))),
                                })
                                .collect::<Vec<_>>())
                        })
                        .await
                        .unwrap_or_else(|error| {
                            texts
                                .into_iter()
                                .map(|text| (text, failed(&error)))
                                .collect()
                        });
                    for (text, answer) in answers {
                        results.insert(text, Arc::new(answer));
                    }
                    respond(&output, request, results);
                });
            }
        }
    }

    /// Register a waiter, then settle handles that already resolved.
    /// Notifications arriving after registration reach the resolvers; those
    /// before it are found in the store here. Parked turns register only
    /// after their `waiting` row is durable.
    pub async fn attach(
        &self,
        store: &Store,
        waiter: Waiter,
        handles: &[String],
        deadline_ms: Option<u64>,
        any: bool,
        completion: Completion,
    ) {
        let parsed: Vec<Result<Handle>> = handles.iter().map(|h| Handle::parse(h)).collect();
        let generation = {
            let mut inner = self.inner.lock().unwrap();
            inner.next_generation += 1;
            let generation = inner.next_generation;
            // Track every input, including an invalid handle restored from
            // storage, so an error resolves it instead of stranding the wait.
            let remaining = handles.iter().cloned().collect();
            for handle in parsed.iter().flatten() {
                match handle {
                    Handle::Turn { bot, turn } => inner
                        .by_turn
                        .entry((bot.clone(), *turn))
                        .or_default()
                        .push(waiter),
                    Handle::Process(id) => inner.by_process.entry(*id).or_default().push(waiter),
                }
            }
            Self::remove_locked(&mut inner, waiter);
            inner.waiters.insert(
                waiter,
                Waiting {
                    generation,
                    any,
                    remaining,
                    results: BTreeMap::new(),
                    completion,
                    deadline: None,
                },
            );
            generation
        };
        for (text, parsed) in handles.iter().zip(parsed) {
            if let Some((result, durable)) = self.settled(store, text, parsed).await {
                let mut inner = self.inner.lock().unwrap();
                if inner
                    .waiters
                    .get(&waiter)
                    .is_some_and(|w| w.generation == generation)
                {
                    Self::resolve_locked(&mut inner, &self.resume, waiter, text, result, durable);
                }
            }
        }
        if handles.is_empty() {
            let mut inner = self.inner.lock().unwrap();
            if inner.waiters.contains_key(&waiter) {
                Self::complete_locked(&mut inner, &self.resume, waiter);
            }
        }
        if let Some(deadline) = deadline_ms {
            let mut inner = self.inner.lock().unwrap();
            let Some(waiting) = inner.waiters.get_mut(&waiter) else {
                return;
            };
            if waiting.generation != generation || waiting.remaining.is_empty() {
                return;
            }
            if deadline <= now_ms() {
                // Polls inspect every handle above, then return pending results
                // without waiting for the schedule.
                Self::expire_locked(&mut inner, &self.resume, waiter);
                return;
            }
            waiting.deadline = Some(deadline);
            Self::schedule_locked(
                &mut inner,
                &self.sooner,
                deadline,
                Due::Wait(waiter, generation),
            );
        }
    }

    /// A handle's result if it has one: retained here, or found in the
    /// store, where a result is durable before it reaches this registry.
    /// The flag says whether it is durable rather than an error reading it.
    async fn settled(
        &self,
        store: &Store,
        text: &str,
        parsed: Result<Handle>,
    ) -> Option<(Arc<Value>, bool)> {
        let cached = self
            .inner
            .lock()
            .unwrap()
            .retained
            .get(text)
            .map(|r| r.value.clone());
        if let Some(cached) = cached {
            return Some((cached, true));
        }
        let result = match parsed {
            Err(error) => Some((failed(&error), false)),
            Ok(Handle::Turn { bot, turn }) => {
                match store
                    .op("turn_answer", move |db| db.turn_answer(&bot, turn))
                    .await
                {
                    Ok(answer) if answer["pending"] == true => None,
                    Ok(answer) => {
                        let durable = answer["error"] != "turn_result_pruned";
                        Some((answer, durable))
                    }
                    Err(error) => Some((failed(&error), false)),
                }
            }
            Ok(Handle::Process(id)) => {
                match store
                    .op("process_result", move |db| db.process_result(id))
                    .await
                {
                    Ok(None) => Some((
                        json!({"error":"unknown_handle","detail":format!("no command {id}")}),
                        false,
                    )),
                    Ok(Some((_, Some(result)))) => Some((result, true)),
                    Ok(Some((_, None))) => None,
                    Err(error) => Some((failed(&error), false)),
                }
            }
        };
        result.map(|(value, durable)| (Arc::new(value), durable))
    }

    /// Consume a parked turn's results when it resumes early, before every
    /// handle resolved: those the registry has not seen yet are read from
    /// the store, and the rest are reported pending.
    pub async fn take_settled(
        &self,
        store: &Store,
        turn: i64,
        handles: &[String],
    ) -> BTreeMap<String, Arc<Value>> {
        let mut results = self.take(turn);
        for handle in handles {
            if !results.contains_key(handle) {
                let found = self.settled(store, handle, Handle::parse(handle)).await;
                let result = found.map_or_else(|| Arc::new(json!({"pending":true})), |(r, _)| r);
                results.insert(handle.clone(), result);
            }
        }
        results
    }

    /// A request waiter for a client's `wait` op.
    pub fn request_waiter(&self) -> Waiter {
        let mut inner = self.inner.lock().unwrap();
        inner.next_request += 1;
        Waiter::Request(inner.next_request)
    }

    /// Report unresolved handles as pending and complete the waiter.
    fn expire_locked(
        inner: &mut Inner,
        resume: &mpsc::UnboundedSender<(String, i64)>,
        waiter: Waiter,
    ) {
        let Some(waiting) = inner.waiters.get_mut(&waiter) else {
            return;
        };
        if waiting.remaining.is_empty() {
            return;
        }
        Self::pend_remaining(waiting);
        Self::complete_locked(inner, resume, waiter);
    }

    /// Registry size for `stats`: live waiters and retained outcomes.
    pub fn stats(&self) -> (usize, usize) {
        let inner = self.inner.lock().unwrap();
        (inner.waiters.len(), inner.retained.len())
    }

    /// Resume a parked turn whose wake-up is not a handle: a verdict for
    /// the call it parked on. The service checks it is still parked there.
    pub fn wake(&self, bot: String, turn: i64) {
        let _ = self.resume.send((bot, turn));
    }
    /// Consume a parked turn's results when it resumes.
    pub fn take(&self, turn: i64) -> BTreeMap<String, Arc<Value>> {
        let mut inner = self.inner.lock().unwrap();
        Self::detach_locked(&mut inner, Waiter::Turn(turn));
        Self::remove_locked(&mut inner, Waiter::Turn(turn))
            .map(|mut w| std::mem::take(&mut w.results))
            .unwrap_or_default()
    }

    /// Drop a waiter's registration: interrupts, or a session closing.
    pub fn forget(&self, waiter: Waiter) {
        let mut inner = self.inner.lock().unwrap();
        Self::detach_locked(&mut inner, waiter);
        Self::remove_locked(&mut inner, waiter);
    }

    /// Sessions own only their still-pending requests, not a history of waits.
    pub fn close_session(&self, session: u64) {
        let mut inner = self.inner.lock().unwrap();
        let waiters: Vec<Waiter> = inner.waiters.iter().filter_map(|(id, waiting)| {
            matches!(waiting.completion, Completion::Respond { session: owner, .. } if owner == session)
                .then_some(*id)
        }).collect();
        for waiter in waiters {
            Self::detach_locked(&mut inner, waiter);
            Self::remove_locked(&mut inner, waiter);
        }
    }

    /// Release session outputs and stop deadlines before the stdout worker
    /// is joined. Background completions may retain this registry afterward.
    /// Drop every waiter, and hand back the answers still reading, for the
    /// caller to let finish or end before closing their outputs.
    /// Answer every client's `wait` now, as its deadline would: what is
    /// still pending is reported as it stands, a turn the daemon parked as
    /// it stops included, which goes on only at the next start.
    pub fn expire_requests(&self) {
        let mut inner = self.inner.lock().unwrap();
        let requests: Vec<Waiter> = (inner.waiters.keys())
            .filter(|waiter| matches!(waiter, Waiter::Request(_)))
            .copied()
            .collect();
        for waiter in requests {
            Self::expire_locked(&mut inner, &self.resume, waiter);
        }
    }

    pub fn shutdown(&self) -> tokio::task::JoinSet<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.waiters.clear();
        inner.schedule.clear();
        inner.lapses.clear();
        inner.stepped.clear();
        inner.by_turn.clear();
        inner.by_process.clear();
        inner.retained.clear();
        std::mem::take(&mut inner.reads)
    }

    fn detach_locked(inner: &mut Inner, waiter: Waiter) {
        if let Some(waiting) = inner.waiters.get(&waiter) {
            for (handle, value) in &waiting.results {
                if let Some(retained) = inner.retained.get_mut(handle)
                    && Arc::ptr_eq(&retained.value, value)
                {
                    retained.waiters -= 1;
                    if retained.waiters == 0 {
                        inner.retained.remove(handle);
                    }
                }
            }
        }
        inner
            .by_turn
            .values_mut()
            .for_each(|v| v.retain(|w| *w != waiter));
        inner.by_turn.retain(|_, v| !v.is_empty());
        inner
            .by_process
            .values_mut()
            .for_each(|v| v.retain(|w| *w != waiter));
        inner.by_process.retain(|_, v| !v.is_empty());
    }
}

/// A handle's result when reading it failed.
fn failed(error: &agent_runtime::Error) -> Value {
    json!({"error":error.code,"detail":error.detail})
}

fn respond(output: &Output, request: Value, results: BTreeMap<String, Arc<Value>>) {
    if output
        .try_respond(request, Ok(wait_result(results)))
        .is_err()
    {
        output.close();
    }
}

/// The shape a wait produces, for tool results and `wait` responses alike.
pub fn wait_result(results: BTreeMap<String, Arc<Value>>) -> Value {
    let results: BTreeMap<String, Value> = results
        .into_iter()
        .map(|(handle, value)| {
            (
                handle,
                Arc::try_unwrap(value).unwrap_or_else(|v| (*v).clone()),
            )
        })
        .collect();
    let pending: Vec<&String> = results
        .iter()
        .filter(|(_, v)| v["pending"] == true)
        .map(|(h, _)| h)
        .collect();
    json!({"results":results,"pending":pending})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn any_mode_completes_on_the_first_result_and_pends_the_rest() {
        let dir = std::env::temp_dir().join(format!("agent-any-wait-test-{}", std::process::id()));
        let (store, _publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let handles = Handles::new(mpsc::unbounded_channel().0);
        let (output, _worker) = Output::stdout();
        {
            let mut inner = handles.inner.lock().unwrap();
            inner.waiters.insert(
                Waiter::Request(1),
                Waiting {
                    generation: 1,
                    any: true,
                    remaining: ["proc:1".into(), "proc:2".into()].into(),
                    results: BTreeMap::new(),
                    completion: Completion::Respond {
                        session: 7,
                        output,
                        request: json!(1),
                        reader: store.reader(),
                    },
                    deadline: None,
                },
            );
            inner.by_process.insert(2, vec![Waiter::Request(1)]);
            inner.by_process.insert(1, vec![Waiter::Request(1)]);
        }
        handles.process_finished(2, json!({"exit_code":0}));
        {
            let inner = handles.inner.lock().unwrap();
            // Consumed by its response, with nothing left registered.
            assert!(inner.waiters.is_empty());
            assert!(inner.by_process.is_empty());
        }
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn shutdown_ends_an_answer_still_reading() {
        use tokio::io::AsyncReadExt;
        let dir = std::env::temp_dir().join(format!("agent-wait-read-test-{}", std::process::id()));
        let (store, _publications) = Store::open(&dir.join("state.sqlite")).await.unwrap();
        let handles = Handles::new(mpsc::unbounded_channel().0);
        let (writer, mut written) = tokio::io::duplex(65536);
        {
            let mut inner = handles.inner.lock().unwrap();
            inner.waiters.insert(
                Waiter::Request(1),
                Waiting {
                    generation: 1,
                    any: true,
                    remaining: ["proc:2".into()].into(),
                    results: BTreeMap::from([(
                        "turn:Bob/1".to_string(),
                        Arc::new(json!({"pending":true})),
                    )]),
                    completion: Completion::Respond {
                        session: 7,
                        output: Output::writer(writer),
                        request: json!(1),
                        reader: store.reader(),
                    },
                    deadline: None,
                },
            );
            inner.by_process.insert(2, vec![Waiter::Request(1)]);
        }
        // The reader is busy, so the answer's read of its pending turn waits.
        let (started, busy) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let reader = store.reader();
        let holder = tokio::spawn(async move {
            reader
                .read("hold", move |_| {
                    started.send(()).unwrap();
                    let _ = held.recv();
                    Ok(())
                })
                .await
        });
        while busy.try_recv().is_err() {
            tokio::task::yield_now().await;
        }
        handles.process_finished(2, json!({"exit_code":0}));
        let mut reads = handles.shutdown();
        assert_eq!(reads.len(), 1);
        // Ended, it drops the session's output, so the output closes.
        reads.shutdown().await;
        let mut bytes = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            written.read_to_end(&mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(bytes.is_empty());
        release.send(()).unwrap();
        holder.await.unwrap().unwrap();
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn every_deadline_is_one_entry_on_the_schedule_and_none_outlives_its_waiter() {
        let (resume, mut resumed) = mpsc::unbounded_channel();
        let handles = Handles::new(resume);
        let waiting = |turn: i64, at: u64| Waiting {
            generation: turn as u64,
            any: false,
            remaining: ["proc:1".into()].into(),
            results: BTreeMap::new(),
            completion: Completion::Resume {
                bot: "Bob".into(),
                turn,
            },
            deadline: Some(at),
        };
        {
            let mut inner = handles.inner.lock().unwrap();
            for turn in [1, 2] {
                inner.waiters.insert(Waiter::Turn(turn), waiting(turn, 100));
                let due = Due::Wait(Waiter::Turn(turn), turn as u64);
                Handles::schedule_locked(&mut inner, &handles.sooner, 100, due);
            }
        }
        // A waiter that leaves takes its deadline with it.
        handles.forget(Waiter::Turn(2));
        assert_eq!(handles.inner.lock().unwrap().schedule.len(), 1);
        // A step's lapse moves rather than adds, and goes once it is due.
        handles.step_lapses(5, Some(50));
        handles.step_lapses(5, Some(90));
        handles.step_lapses(6, Some(70));
        handles.step_lapses(6, None);
        handles.stepped(7);
        // Something stepped: the run loop's sleep ends at once.
        handles.due().await;
        assert_eq!(handles.take_due(99), [5, 7]);
        assert_eq!(handles.inner.lock().unwrap().schedule.len(), 1);
        // The wait's deadline gives up on what is pending, and resumes it.
        assert!(handles.take_due(100).is_empty());
        assert_eq!(resumed.try_recv().unwrap(), ("Bob".into(), 1));
        let inner = handles.inner.lock().unwrap();
        assert!(inner.schedule.is_empty() && inner.lapses.is_empty());
    }

    #[test]
    fn shared_results_are_released_when_waiters_leave() {
        let handles = Handles::new(mpsc::unbounded_channel().0);
        {
            let mut inner = handles.inner.lock().unwrap();
            for turn in [1, 2] {
                let waiter = Waiter::Turn(turn);
                inner.waiters.insert(
                    waiter,
                    Waiting {
                        generation: turn as u64,
                        any: false,
                        remaining: ["proc:1".into(), "proc:2".into()].into(),
                        results: BTreeMap::new(),
                        completion: Completion::Resume {
                            bot: turn.to_string(),
                            turn,
                        },
                        deadline: None,
                    },
                );
                for process in [1, 2] {
                    inner.by_process.entry(process).or_default().push(waiter);
                }
            }
        }
        handles.process_finished(1, json!({"stdout":"x".repeat(128 * 1024)}));
        let weak = {
            let inner = handles.inner.lock().unwrap();
            let first = &inner.waiters[&Waiter::Turn(1)].results["proc:1"];
            let second = &inner.waiters[&Waiter::Turn(2)].results["proc:1"];
            assert!(Arc::ptr_eq(first, second));
            Arc::downgrade(first)
        };
        handles.forget(Waiter::Turn(1));
        assert!(weak.upgrade().is_some());
        handles.process_finished(2, json!({"stdout":"done"}));
        let result = wait_result(handles.take(2));
        assert_eq!(
            result["results"]["proc:1"]["stdout"]
                .as_str()
                .unwrap()
                .len(),
            128 * 1024
        );
        assert_eq!(result["pending"], json!([]));
        assert!(weak.upgrade().is_none());
        assert!(handles.inner.lock().unwrap().retained.is_empty());
    }

    #[test]
    fn handles_parse_and_print_round_trip() {
        assert_eq!(
            Handle::parse("turn:Bob/3").unwrap(),
            Handle::Turn {
                bot: "Bob".into(),
                turn: 3
            }
        );
        assert_eq!(Handle::parse("proc:7").unwrap().to_string(), "proc:7");
        for bad in [
            "turn:Bob",
            "turn:/3",
            "turn:Bob/0",
            "proc:x",
            "proc:0",
            "job:1",
        ] {
            assert_eq!(Handle::parse(bad).unwrap_err().code, "invalid_handle");
        }
    }
}
