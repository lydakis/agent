//! Things a waiter can wait on without holding execution capacity: peer
//! turns, resolved from the daemon's own completions, and background commands
//! recorded in the store. A waiter is either a parked turn (a store row plus a
//! registry entry, no task) or a client request awaiting its response.
use agent_runtime::{Result, fail_with, output::Output, store::Store};
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
    /// Answer a client's `wait` request on its session.
    Respond {
        session: u64,
        output: Output,
        request: Value,
    },
}

/// Who is waiting: parked turns are keyed by turn id, requests by a counter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Waiter {
    Turn(i64),
    Request(u64),
}

struct Waiting {
    generation: u64,
    remaining: BTreeSet<String>,
    results: BTreeMap<String, Arc<Value>>,
    completion: Completion,
    timer: Option<tokio::task::AbortHandle>,
}
impl Drop for Waiting {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.take() {
            timer.abort();
        }
    }
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
}

#[derive(Clone)]
pub struct Handles {
    inner: Arc<Mutex<Inner>>,
    resume: mpsc::UnboundedSender<(String, i64)>,
}

impl Handles {
    pub fn new(resume: mpsc::UnboundedSender<(String, i64)>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            resume,
        }
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
                output, request, ..
            } => {
                // A request waiter is consumed by its response.
                Self::detach_locked(inner, waiter);
                let results = inner
                    .waiters
                    .remove(&waiter)
                    .map(|mut w| std::mem::take(&mut w.results));
                if output
                    .try_respond(request, Ok(wait_result(results.unwrap_or_default())))
                    .is_err()
                {
                    output.close();
                }
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
            inner.waiters.insert(
                waiter,
                Waiting {
                    generation,
                    remaining,
                    results: BTreeMap::new(),
                    completion,
                    timer: None,
                },
            );
            generation
        };
        for (text, parsed) in handles.iter().zip(parsed) {
            let cached = self
                .inner
                .lock()
                .unwrap()
                .retained
                .get(text)
                .map(|r| r.value.clone());
            let result = if let Some(cached) = cached {
                Some((cached, true))
            } else {
                let result = match parsed {
                    Err(error) => Some((json!({"error":error.code,"detail":error.detail}), false)),
                    Ok(Handle::Turn { bot, turn }) => {
                        match store.call(move |db| db.turn_outcome(&bot, turn)).await {
                            Ok(Some(outcome)) => Some((outcome, true)),
                            Ok(None) => None,
                            Err(error) => {
                                Some((json!({"error":error.code,"detail":error.detail}), false))
                            }
                        }
                    }
                    Ok(Handle::Process(id)) => {
                        match store.call(move |db| db.process_result(id)).await {
                            Ok(None) => Some((json!({"error":"unknown_handle"}), false)),
                            Ok(Some((_, Some(result)))) => Some((result, true)),
                            Ok(Some((_, None))) => None,
                            Err(error) => {
                                Some((json!({"error":error.code,"detail":error.detail}), false))
                            }
                        }
                    }
                };
                result.map(|(value, durable)| (Arc::new(value), durable))
            };
            if let Some((result, durable)) = result {
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
            let delay = Duration::from_millis(deadline.saturating_sub(now_ms()));
            let handles = self.clone();
            waiting.timer = Some(
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    handles.expire(waiter, generation);
                })
                .abort_handle(),
            );
        }
    }

    /// A request waiter for a client's `wait` op.
    pub fn request_waiter(&self) -> Waiter {
        let mut inner = self.inner.lock().unwrap();
        inner.next_request += 1;
        Waiter::Request(inner.next_request)
    }

    /// Report unresolved handles as pending and complete the waiter.
    fn expire(&self, waiter: Waiter, generation: u64) {
        let mut inner = self.inner.lock().unwrap();
        let Some(waiting) = inner.waiters.get_mut(&waiter) else {
            return;
        };
        if waiting.generation != generation || waiting.remaining.is_empty() {
            return;
        }
        for handle in std::mem::take(&mut waiting.remaining) {
            waiting
                .results
                .insert(handle, Arc::new(json!({"pending":true})));
        }
        Self::complete_locked(&mut inner, &self.resume, waiter);
    }

    /// Consume a parked turn's results when it resumes.
    pub fn take(&self, turn: i64) -> BTreeMap<String, Arc<Value>> {
        let mut inner = self.inner.lock().unwrap();
        Self::detach_locked(&mut inner, Waiter::Turn(turn));
        inner
            .waiters
            .remove(&Waiter::Turn(turn))
            .map(|mut w| std::mem::take(&mut w.results))
            .unwrap_or_default()
    }

    /// Drop a waiter's registration: interrupts, or a session closing.
    pub fn forget(&self, waiter: Waiter) {
        let mut inner = self.inner.lock().unwrap();
        Self::detach_locked(&mut inner, waiter);
        inner.waiters.remove(&waiter);
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
            inner.waiters.remove(&waiter);
        }
    }

    /// Release session outputs and stop deadlines before the stdout worker
    /// is joined. Background completions may retain this registry afterward.
    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.waiters.clear();
        inner.by_turn.clear();
        inner.by_process.clear();
        inner.retained.clear();
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
                        remaining: ["proc:1".into(), "proc:2".into()].into(),
                        results: BTreeMap::new(),
                        completion: Completion::Resume {
                            bot: turn.to_string(),
                            turn,
                        },
                        timer: None,
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
