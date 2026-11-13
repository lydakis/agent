//! Per-provider pacing. A fleet must use a provider's allowance fully and
//! fairly without ever overshooting it, so every model call passes through
//! one gate per provider and pool: a fair FIFO lock holding buckets for
//! requests and for each token dimension the provider limits (total, and
//! input and output where it publishes them), learned from the provider's
//! own rate-limit headers and corrected by every response. Unknown limits
//! impose no admission cap. A refusal blocks the
//! pool until the time the provider names; callers queue in arrival order
//! and are released one at a time, so nothing retries in lockstep. The gate
//! is an uncontended lock and a few arithmetic operations per call.
use super::Usage;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
// Tokio's clock, so the pool pauses and advances with the runtime under test.
use std::sync::Weak;
use tokio::time::Instant;

/// Rate-limit waits below this threshold stay in the gate.
pub const MIN_PARK: Duration = Duration::from_millis(250);

/// What one call reports back besides its completion: provider usage when
/// the stream carried it, and how long pacing held the call.
#[derive(Debug, Default)]
pub struct Report {
    /// Admission deferred before dispatch; no attempt or reservation was spent.
    pub park_for: Option<Duration>,
    pub usage: Option<Usage>,
    pub paced_ms: u64,
    /// The HTTP send future was entered, even if the attempt was interrupted.
    pub dispatched: bool,
}

/// What a pool asks of the daemon's run loop, which owns every wake-up.
pub enum Woken {
    /// Resume this parked turn now: a place on the ramp is kept for it.
    Turn(String, i64),
    /// Call `Pace::lapse` on this pool then, to pass on a kept place its
    /// turn never came back for.
    Lapse(Instant, Weak<Pace>),
}
pub type Wake = Arc<dyn Fn(Woken) + Send + Sync>;

/// A turn asking for a call, and how to wake it once parked: a caller
/// that finds a ramp's window full parks, and a settle wakes it.
#[derive(Clone)]
pub struct Waiter {
    pub bot: String,
    pub turn: i64,
    pub wake: Wake,
}

impl Waiter {
    fn is(&self, other: &Waiter) -> bool {
        self.bot == other.bot && self.turn == other.turn
    }
}

/// An in-process caller's place in a ramp's queue. A caller dropped while
/// it waits gives its place up, and any place kept for it goes on.
struct Queued<'a> {
    pace: &'a Pace,
    ticket: Option<(Waiter, Arc<tokio::sync::Notify>)>,
}
impl Drop for Queued<'_> {
    fn drop(&mut self) {
        let Some((waiter, _)) = self.ticket.take() else {
            return;
        };
        let mut state = self.pace.state.lock().unwrap();
        if let Some(ramp) = state.ramp.as_mut() {
            ramp.forget(&waiter.bot, waiter.turn);
            ramp.wake(Instant::now(), &self.pace.this);
        }
        drop(state);
        self.pace.changed.notify_one();
    }
}

/// Pools keyed by model within one provider.
#[derive(Default)]
pub struct Pools(Mutex<HashMap<String, Arc<Pace>>>);
impl Pools {
    /// Every pool's learned allowance and level, for `stats`.
    pub fn status(&self) -> serde_json::Value {
        let pools = self.0.lock().unwrap();
        let mut out = serde_json::Map::new();
        for (model, pace) in pools.iter() {
            out.insert(model.clone(), pace.status());
        }
        serde_json::Value::Object(out)
    }
    pub fn get(&self, model: &str) -> Arc<Pace> {
        let mut pools = self.0.lock().unwrap();
        if let Some(pace) = pools.get(model) {
            return pace.clone();
        }
        let pace = Arc::new_cyclic(|this| Pace {
            this: this.clone(),
            ..Default::default()
        });
        pools.insert(model.to_owned(), pace.clone());
        pace
    }
}

/// Which rate-limit headers a pool learns from: its family's, or those
/// Anthropic publishes for fast mode, whose limits are its own
/// (`anthropic-fast-input-tokens-*` and `anthropic-fast-output-tokens-*`,
/// no request count; its fast-mode page, read 2026-10-30).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limits {
    Responses,
    Anthropic,
    AnthropicFast,
}
impl From<super::Family> for Limits {
    fn from(family: super::Family) -> Limits {
        match family {
            super::Family::Responses => Limits::Responses,
            super::Family::Anthropic => Limits::Anthropic,
        }
    }
}

/// What one call costs the pool, in the dimensions a provider may limit
/// separately: total tokens, and the input and output shares of that total.
/// Responses providers publish only the total; Anthropic publishes all three.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    pub input: u64,
    pub output: u64,
}
impl Cost {
    fn dims(self) -> [u64; DIMS] {
        [self.input + self.output, self.input, self.output]
    }
}
const DIMS: usize = 3;
const TOTAL: usize = 0;
const INPUT: usize = 1;
const OUTPUT: usize = 2;

#[derive(Default)]
pub struct Pace {
    /// Fair: tokio's mutex hands the lock to waiters in arrival order.
    gate: tokio::sync::Mutex<()>,
    state: Mutex<State>,
    changed: tokio::sync::Notify,
    /// The pool itself, for the run loop to call back when a kept place
    /// lapses.
    this: Weak<Pace>,
}
/// One limited quantity: its learned allowance and the estimates reserved
/// against it by calls that have not yet been accounted for by headers.
#[derive(Default)]
struct Dim {
    bucket: Bucket,
    reserved: u64,
}
#[derive(Default)]
struct State {
    requests: Bucket,
    tokens: [Dim; DIMS],
    blocked_until: Option<Instant>,
    /// Blocks in a row that ended in another refusal naming no delay.
    unnamed_blocks: u32,
    reserved_requests: u64,
    /// Calls answered and overloads refused since the window began.
    overloads: Overloads,
    /// Output tokens the pool's latest calls reported, oldest first.
    outputs: std::collections::VecDeque<u32>,
    /// Parts per million of each latest call's input that counted against the
    /// limit, oldest first, for a provider that leaves cache reads out.
    counted: std::collections::VecDeque<u32>,
    /// Allowance set aside for calls parked until it refills.
    booked: Booked,
    /// What the pool did to the calls it saw, since the daemon started.
    counts: Counts,
    /// How many calls the pool lets fly at once after a refusal it could
    /// not foresee, while it learns again what the provider takes.
    ramp: Option<Ramp>,
    /// Ramps begun, so a call admitted under an ended ramp never settles
    /// into a later one.
    ramps: u64,
    /// In-process callers queued on a full ramp, for their places in it.
    tickets: i64,
}

/// After a refusal from a pool that knows no allowance (a provider that
/// publishes none, as Bedrock), every call parked on it would otherwise go
/// at once when it reopens, be refused together, and park again. Instead
/// the pool lets one call fly, and one more for each that is answered,
/// doubling each round, as a TCP sender's slow start does; a refusal while
/// it ramps halves the window and growth goes on one call a round. The
/// window is learned from the provider's own answers and grows without
/// bound while they succeed: past `RAMP_DONE` the pool lets everything go
/// again. A refusal that names a ramp limit (Anthropic's
/// `ramp_limit_exceeded`) ramps a pool whatever it knows.
struct Ramp {
    /// Which ramp this is (`State::ramps` when it began).
    generation: u64,
    window: u32,
    /// Where growth turns from doubling to one call a round, once a
    /// refusal has come while ramping.
    threshold: Option<u32>,
    /// Calls this ramp admitted that have not settled.
    flying: u32,
    /// Answers counted toward the next call of growth past the threshold.
    credit: u32,
    /// Turns parked on the full window, in the order they came.
    queued: std::collections::VecDeque<Waiter>,
    /// Turns woken for a place, which the window keeps for each until it
    /// comes back or `PROMISE` passes.
    promised: Vec<(String, i64, Instant)>,
}
impl Ramp {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            window: 1,
            threshold: None,
            flying: 0,
            credit: 0,
            queued: Default::default(),
            promised: Vec::new(),
        }
    }
    fn full(&self) -> bool {
        self.flying + self.promised.len() as u32 >= self.window
    }
    fn promised_to(&self, waiter: Option<&Waiter>) -> bool {
        waiter.is_some_and(|w| {
            self.promised
                .iter()
                .any(|(bot, turn, _)| *bot == w.bot && *turn == w.turn)
        })
    }
    /// Drop a caller's place in the queue and any place kept for it.
    fn forget(&mut self, bot: &str, turn: i64) {
        self.queued.retain(|w| !(w.bot == bot && w.turn == turn));
        self.promised.retain(|(b, t, _)| !(b == bot && *t == turn));
    }
    /// Wake parked turns, oldest first, for the places the window has free,
    /// and have the run loop look again when the places kept for them
    /// lapse, so one that never comes back does not hold its place.
    fn wake(&mut self, now: Instant, pace: &Weak<Pace>) {
        self.promised.retain(|(_, _, until)| *until > now);
        let mut last = None;
        while !self.full() {
            let Some(waiter) = self.queued.pop_front() else {
                break;
            };
            self.promised
                .push((waiter.bot.clone(), waiter.turn, now + PROMISE));
            (waiter.wake)(Woken::Turn(waiter.bot.clone(), waiter.turn));
            last = Some(waiter.wake);
        }
        if let Some(wake) = last {
            wake(Woken::Lapse(now + PROMISE, pace.clone()));
        }
    }
}

/// How long a woken turn's place is kept for it to come back.
const PROMISE: Duration = Duration::from_secs(5);
/// How long a turn parked on a full window waits if no settle wakes it.
const RAMP_PARK: Duration = Duration::from_secs(30);

/// A window this wide lets the pool's calls go as they come.
const RAMP_DONE: u32 = 1024;

/// What a pool did to its calls, for `stats`: how many it admitted, how
/// many of those waited in the gate and for how long in all, how many it
/// parked, how often a refusal closed it, and the overloads refused.
#[derive(Default)]
struct Counts {
    admitted: u64,
    waited: u64,
    waited_ms: u64,
    parked: u64,
    closed: u64,
    overloads: u64,
}

/// The scale of a counted-input share: parts per million.
const SHARE: u64 = 1_000_000;

/// A parked call's share of the allowance, taken when it parked so each
/// later call parks behind it, and spent by the first call to arrive once it
/// is due. One no call spends returns to the allowance after `BOOKING_GRACE`.
const BOOKING_GRACE: Duration = Duration::from_secs(5);

/// Where a booking stands among the others: when it is due, then the order
/// it was made in.
type BookingKey = (Instant, u64);

/// Every parked call's booking, soonest due first, and what they hold
/// together in each dimension, so an admission reads the soonest and the
/// totals without visiting the rest, however many calls are parked.
#[derive(Debug, Default)]
struct Booked {
    by_due: std::collections::BTreeMap<BookingKey, [u64; DIMS]>,
    held: [u64; DIMS],
    next: u64,
}
impl Booked {
    fn len(&self) -> usize {
        self.by_due.len()
    }
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.by_due.is_empty()
    }
    /// What every booking holds in dimension `index`.
    fn held(&self, index: usize) -> u64 {
        self.held[index]
    }
    fn book(&mut self, due: Instant, costs: [u64; DIMS]) {
        self.next += 1;
        self.by_due.insert((due, self.next), costs);
        for (held, cost) in self.held.iter_mut().zip(costs) {
            *held += cost;
        }
    }
    /// The soonest booking, if it is due by `by`.
    fn due_by(&self, by: Instant) -> Option<BookingKey> {
        let (&key, _) = self.by_due.first_key_value()?;
        (key.0 <= by).then_some(key)
    }
    fn costs(&self, key: BookingKey) -> [u64; DIMS] {
        self.by_due[&key]
    }
    /// Set what a booking holds, keeping the totals.
    fn hold(&mut self, key: BookingKey, costs: [u64; DIMS]) {
        let old = std::mem::replace(self.by_due.get_mut(&key).expect("a booking"), costs);
        for ((held, old), new) in self.held.iter_mut().zip(old).zip(costs) {
            *held = *held - old + new;
        }
    }
    /// Move a booking to a later due, keeping its place among those made
    /// at the same time.
    fn defer(&mut self, key: BookingKey, due: Instant) {
        if let Some(costs) = self.by_due.remove(&key) {
            self.by_due.insert((due, key.1), costs);
        }
    }
    fn cancel(&mut self, key: BookingKey) -> Option<[u64; DIMS]> {
        let costs = self.by_due.remove(&key)?;
        for (held, cost) in self.held.iter_mut().zip(costs) {
            *held -= cost;
        }
        Some(costs)
    }
    /// Take off the bookings whose grace ran out by `now`, soonest first.
    fn lapsed(&mut self, now: Instant) -> impl Iterator<Item = [u64; DIMS]> + '_ {
        std::iter::from_fn(move || {
            let key = self.due_by(now.checked_sub(BOOKING_GRACE)?)?;
            self.cancel(key)
        })
    }
}

/// The output and counted-input estimates follow this many of the latest
/// calls, once they have `OUTPUT_KNOWN` of them.
const OUTPUT_SAMPLES: usize = 32;
const OUTPUT_KNOWN: usize = 8;

/// The 90th percentile of a pool's latest samples, once enough are known.
fn p90(samples: &std::collections::VecDeque<u32>) -> Option<u64> {
    if samples.len() < OUTPUT_KNOWN {
        return None;
    }
    let mut seen: Vec<u32> = samples.iter().copied().collect();
    let at = (seen.len() * 9).div_ceil(10) - 1;
    Some(u64::from(*seen.select_nth_unstable(at).1))
}

/// The input a call's usage counts toward its provider's limits: all of
/// it, or for a provider that leaves cache reads out, the rest.
fn counted(usage: &Usage, family: super::Family) -> u64 {
    match family.counts_cache_reads() {
        true => usage.input_tokens,
        false => usage.input_tokens.saturating_sub(usage.cached_input_tokens),
    }
}

/// Keep the latest `OUTPUT_SAMPLES` samples.
fn sample(samples: &mut std::collections::VecDeque<u32>, value: u64) {
    if samples.len() == OUTPUT_SAMPLES {
        samples.pop_front();
    }
    samples.push_back(u32::try_from(value).unwrap_or(u32::MAX));
}

/// Overload refusals that name no delay close the pool only when they
/// cluster: this many within one window, a quarter or more of its answers.
const OVERLOAD_CLUSTER: u32 = 3;
const OVERLOAD_WINDOW: Duration = Duration::from_secs(2);

impl State {
    /// The allowance refilled to `now`, what parked calls booked and none
    /// spent returned to it, and never more than the limit with what they
    /// still hold.
    fn refill(&mut self, now: Instant) {
        self.requests.refill(now);
        for dim in self.tokens.iter_mut() {
            dim.bucket.refill(now);
        }
        self.unbook_expired(now);
        if let Some(limit) = self.requests.per_minute {
            let held = self.booked.len() as f64;
            self.requests.available = self.requests.available.min(limit - held);
        }
        for (index, dim) in self.tokens.iter_mut().enumerate() {
            if let Some(limit) = dim.bucket.per_minute {
                let held = self.booked.held(index) as f64;
                dim.bucket.available = dim.bucket.available.min(limit - held);
            }
        }
    }
    /// Return to the allowance what parked calls booked and none spent.
    fn unbook_expired(&mut self, now: Instant) {
        let (requests, tokens) = (&mut self.requests, &mut self.tokens);
        for costs in self.booked.lapsed(now) {
            requests.give(1.0);
            for (dim, cost) in tokens.iter_mut().zip(costs) {
                dim.bucket.give(cost as f64);
            }
        }
    }
    /// How long a call of `costs` waits: zero once it is admitted, having
    /// taken its estimate, or the booking `due` that pays for it. A wait no
    /// refund from a call in flight could shorten, and of `MIN_PARK` or
    /// more, parks the call instead, booking its estimate so the next call
    /// parks behind it.
    fn admit_or_book(
        &mut self,
        costs: [u64; DIMS],
        due: Option<BookingKey>,
        now: Instant,
        park_for: &mut Option<Duration>,
    ) -> crate::Result<Duration> {
        // What the allowance must still hold for this call: its estimate,
        // or beside a due booking, its estimate less everything booked, as
        // the booking is first in line and every other one is still owed. A
        // booking whose wait counted on a refund that did not come is not
        // yet paid for.
        let mut request = 1.0;
        let mut need = costs.map(|cost| cost as f64);
        if due.is_some() {
            request -= self.booked.len() as f64;
            for (index, need) in need.iter_mut().enumerate() {
                *need -= self.booked.held(index) as f64;
            }
        }
        let mut wait = self.requests.shortfall(request);
        // Calls in flight return what their estimates overstated when
        // headers or usage arrive, and wake this one; only the wait that no
        // refund could shorten parks.
        let mut refill = self
            .requests
            .shortfall(request - self.reserved_requests as f64);
        for (dim, need) in self.tokens.iter().zip(need) {
            wait = wait.max(dim.bucket.shortfall(need));
            refill = refill.max(dim.bucket.shortfall(need - dim.reserved as f64));
        }
        let park = refill >= MIN_PARK;
        if !park && !wait.is_zero() {
            return Ok(wait);
        }
        match due {
            None => {
                self.requests.take(1.0);
                for (dim, cost) in self.tokens.iter_mut().zip(costs) {
                    dim.bucket.take(cost as f64);
                }
                if park {
                    self.booked.book(now + refill, costs);
                }
            }
            Some(at) => {
                let mut booking = self.booked.costs(at);
                for ((dim, cost), held) in self.tokens.iter_mut().zip(costs).zip(&mut booking) {
                    match cost.checked_sub(*held) {
                        Some(grew) => {
                            dim.bucket.take(grew as f64);
                            *held = cost;
                        }
                        // A smaller call returns the rest once it goes.
                        None if !park => dim.bucket.give((*held - cost) as f64),
                        None => {}
                    }
                }
                self.booked.hold(at, booking);
                match park {
                    true => self.booked.defer(at, now + refill),
                    false => drop(self.booked.cancel(at)),
                }
            }
        }
        if park {
            *park_for = Some(refill);
            self.counts.parked += 1;
            return Err(crate::Error::new("provider_paced"));
        }
        Ok(Duration::ZERO)
    }
}

#[derive(Debug, Default)]
struct Overloads {
    since: Option<Instant>,
    answered: u32,
    refused: u32,
}
impl Overloads {
    /// The current window, a fresh one once the last has run out.
    fn at(&mut self, now: Instant) -> &mut Self {
        if self
            .since
            .is_none_or(|since| now - since >= OVERLOAD_WINDOW)
        {
            *self = Overloads {
                since: Some(now),
                ..Overloads::default()
            };
        }
        self
    }
}

/// An unsent reservation is fully refundable. After dispatch, cancellation
/// conservatively charges the estimate until headers or usage account for it.
pub struct Reservation<'a> {
    pace: &'a Pace,
    estimated: [u64; DIMS],
    actual: [u64; DIMS],
    reported: [bool; DIMS],
    dispatched: bool,
    request_reported: bool,
    pub waited: Duration,
    /// Admitted under a ramp (its generation), whose window it holds until
    /// it settles.
    ramp: Option<u64>,
}
impl Reservation<'_> {
    /// Call immediately before polling the HTTP send future.
    pub fn dispatch(&mut self) {
        debug_assert!(!self.dispatched);
        self.dispatched = true;
        self.actual = self.estimated;
    }
    pub fn learn(&mut self, headers: &reqwest::header::HeaderMap, limits: impl Into<Limits>) {
        debug_assert!(self.dispatched && !self.reported[TOTAL]);
        self.reported = self.pace.learn(headers, limits, self.estimated, 1);
        self.request_reported = true;
    }
    /// Final accounting by total tokens; the split is taken as input.
    pub fn settle(self, actual: u64) {
        self.settle_cost(Cost {
            input: actual,
            output: 0,
        });
    }
    /// Final accounting by what the provider reported billing, without the
    /// cache reads a provider leaves out of its limits.
    pub fn settle_usage(self, usage: Option<&Usage>, estimate: Cost, family: super::Family) {
        let Some(usage) = usage else {
            return self.settle_cost(estimate);
        };
        let input = counted(usage, family);
        let mut state = self.pace.state.lock().unwrap();
        sample(&mut state.outputs, usage.output_tokens);
        if !family.counts_cache_reads() && usage.input_tokens > 0 {
            // Rounded up, so a few uncached tokens beside a long cached
            // prompt still reserve something.
            sample(
                &mut state.counted,
                (input * SHARE).div_ceil(usage.input_tokens),
            );
        }
        drop(state);
        self.settle_cost(Cost {
            input,
            output: usage.output_tokens,
        });
    }
    fn settle_cost(mut self, actual: Cost) {
        debug_assert!(self.dispatched);
        self.actual = actual.dims();
    }
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut state = self.pace.state.lock().unwrap();
        if let Some(generation) = self.ramp
            && let Some(ramp) = state.ramp.as_mut()
            && ramp.generation == generation
        {
            ramp.flying = ramp.flying.saturating_sub(1);
            ramp.wake(Instant::now(), &self.pace.this);
        }
        if !self.request_reported {
            state.reserved_requests -= 1;
            if !self.dispatched
                && let Some(limit) = state.requests.per_minute
            {
                state.requests.available = (state.requests.available + 1.0).min(limit);
            }
        }
        // Every dimension the headers did not account for returns its
        // unspent estimate, or charges the actual overrun.
        for index in 0..DIMS {
            if self.reported[index] {
                continue;
            }
            let dim = &mut state.tokens[index];
            dim.reserved -= self.estimated[index];
            if let Some(limit) = dim.bucket.per_minute {
                dim.bucket.available = (dim.bucket.available + self.estimated[index] as f64
                    - self.actual[index] as f64)
                    .min(limit);
            }
        }
        drop(state);
        self.pace.changed.notify_one();
    }
}
/// A per-minute allowance. Unknown until the provider reports it, and
/// unbounded until then: the first response teaches the pool.
#[derive(Default)]
struct Bucket {
    per_minute: Option<f64>,
    available: f64,
    updated: Option<Instant>,
}
impl Bucket {
    fn refill(&mut self, now: Instant) {
        if let (Some(limit), Some(updated)) = (self.per_minute, self.updated) {
            let elapsed = now.saturating_duration_since(updated).as_secs_f64();
            self.available = (self.available + limit * elapsed / 60.0).min(limit);
        }
        self.updated = Some(now);
    }
    /// A reported level only ever lowers ours: with many responses in
    /// flight the reports arrive out of order, and a stale high one would
    /// admit work the provider has already counted. Refill raises it.
    fn learn(&mut self, limit: f64, remaining: f64, now: Instant) {
        let known = self.per_minute.is_some();
        self.refill(now);
        self.per_minute = Some(limit.max(1.0));
        self.available = if known {
            self.available.min(remaining)
        } else {
            remaining
        }
        .min(limit);
        self.updated = Some(now);
    }
    fn shortfall(&self, cost: f64) -> Duration {
        match self.per_minute {
            Some(limit) if self.available < cost => {
                Duration::from_secs_f64(((cost - self.available) / limit * 60.0).max(0.0))
            }
            _ => Duration::ZERO,
        }
    }
    fn take(&mut self, cost: f64) {
        if self.per_minute.is_some() {
            self.available -= cost;
        }
    }
    fn give(&mut self, cost: f64) {
        if let Some(limit) = self.per_minute {
            self.available = (self.available + cost).min(limit);
        }
    }
}

/// Writes elapsed pacing even when cancellation drops the acquire future.
struct WaitTime<'a> {
    elapsed_ms: &'a mut u64,
    started: Instant,
}
impl Drop for WaitTime<'_> {
    fn drop(&mut self) {
        *self.elapsed_ms = self.started.elapsed().as_millis() as u64;
    }
}

impl Pace {
    pub(super) async fn acquire_reported(
        &self,
        cost: Cost,
        report: &mut Report,
    ) -> crate::Result<Reservation<'_>> {
        let _time = WaitTime {
            elapsed_ms: &mut report.paced_ms,
            started: Instant::now(),
        };
        self.acquire_cost(cost, &mut report.park_for, None).await
    }
    /// As `acquire_reported`, for a turn that can park on a full ramp and
    /// be woken when a place frees.
    pub(super) async fn acquire_waking(
        &self,
        cost: Cost,
        report: &mut Report,
        waiter: Option<&Waiter>,
    ) -> crate::Result<Reservation<'_>> {
        let _time = WaitTime {
            elapsed_ms: &mut report.paced_ms,
            started: Instant::now(),
        };
        self.acquire_cost(cost, &mut report.park_for, waiter).await
    }
    /// Hold the caller until the pool can afford one request of `tokens`,
    /// then reserve it until headers, final usage, or cancellation account
    /// for it. The tokens count as input.
    pub async fn acquire<'a>(
        &'a self,
        tokens: u64,
        park_for: &mut Option<Duration>,
    ) -> crate::Result<Reservation<'a>> {
        self.acquire_cost(
            Cost {
                input: tokens,
                output: 0,
            },
            park_for,
            None,
        )
        .await
    }
    pub async fn acquire_cost<'a>(
        &'a self,
        cost: Cost,
        park_for: &mut Option<Duration>,
        waiter: Option<&Waiter>,
    ) -> crate::Result<Reservation<'a>> {
        let started = Instant::now();
        let costs = cost.dims();
        // A caller that cannot park and finds the ramp full queues in it
        // as a turn does, and waits outside the gate for its place, so the
        // turns woken ahead of it can come back through the gate.
        let mut queued = Queued {
            pace: self,
            ticket: None,
        };
        let mut gate = None;
        loop {
            if gate.is_none() {
                gate = Some(self.gate.lock().await);
            }
            let changed = self.changed.notified();
            let mut outside = None;
            let wait = {
                let mut state = self.state.lock().unwrap();
                let own = queued.ticket.as_ref().map(|(w, _)| w.clone());
                let waiter = waiter.or(own.as_ref());
                let now = Instant::now();
                for (dim, cost) in state.tokens.iter().zip(costs) {
                    if let Some(limit) = dim.bucket.per_minute
                        && cost as f64 > limit
                    {
                        return Err(crate::Error::with(
                            "provider_pacing_limit",
                            format!(
                                "estimated {cost} tokens exceeds learned limit {limit} per minute"
                            ),
                        ));
                    }
                }
                match state.blocked_until {
                    Some(until) if until > now => {
                        let left = until - now;
                        if left >= MIN_PARK {
                            *park_for = Some(left);
                            state.counts.parked += 1;
                            return Err(crate::Error::new("provider_paced"));
                        }
                        left
                    }
                    _ => {
                        state.blocked_until = None;
                        state.refill(now);
                        let due = state.booked.due_by(now + MIN_PARK);
                        let left = due.map(|(at, _)| at.saturating_duration_since(now));
                        // A place kept for a woken turn that never came
                        // back goes to the next parked one.
                        if let Some(ramp) = state.ramp.as_mut() {
                            ramp.wake(now, &self.this);
                        }
                        let wait = match left {
                            Some(left) if !left.is_zero() => left,
                            // A full ramp waits for one of its calls to
                            // settle, in its queue, oldest first; the
                            // settle wakes the oldest with a place kept for
                            // it. A turn parks, holding nothing live; any
                            // other caller waits outside the gate.
                            _ if state
                                .ramp
                                .as_ref()
                                .is_some_and(|r| r.full() && !r.promised_to(waiter)) =>
                            {
                                let waiter = match waiter {
                                    Some(waiter) => waiter.clone(),
                                    None => {
                                        state.tickets += 1;
                                        let notify = Arc::new(tokio::sync::Notify::new());
                                        let ticket = notify.clone();
                                        let waiter = Waiter {
                                            bot: String::new(),
                                            turn: -state.tickets,
                                            wake: Arc::new(move |woken: Woken| {
                                                if let Woken::Turn(..) = woken {
                                                    ticket.notify_one();
                                                }
                                            }),
                                        };
                                        queued.ticket = Some((waiter.clone(), notify));
                                        waiter
                                    }
                                };
                                let ramp = state.ramp.as_mut().unwrap();
                                if !ramp.queued.iter().any(|w| w.is(&waiter)) {
                                    ramp.queued.push_back(waiter);
                                }
                                match &queued.ticket {
                                    Some((_, notify)) => {
                                        outside = Some(notify.clone());
                                        RAMP_PARK
                                    }
                                    None => {
                                        *park_for = Some(RAMP_PARK);
                                        state.counts.parked += 1;
                                        return Err(crate::Error::new("provider_paced"));
                                    }
                                }
                            }
                            _ => state.admit_or_book(costs, due, now, park_for)?,
                        };
                        if wait.is_zero() {
                            for (dim, cost) in state.tokens.iter_mut().zip(costs) {
                                dim.reserved += cost;
                            }
                            state.reserved_requests += 1;
                            let ramp = state.ramp.as_mut().map(|r| {
                                if let Some(w) = waiter {
                                    r.forget(&w.bot, w.turn);
                                }
                                r.flying += 1;
                                r.generation
                            });
                            queued.ticket = None;
                            let waited = started.elapsed();
                            state.counts.admitted += 1;
                            if waited >= Duration::from_millis(1) {
                                state.counts.waited += 1;
                                state.counts.waited_ms += waited.as_millis() as u64;
                            }
                            return Ok(Reservation {
                                pace: self,
                                estimated: costs,
                                actual: [0; DIMS],
                                reported: [false; DIMS],
                                dispatched: false,
                                request_reported: false,
                                waited,
                                ramp,
                            });
                        }
                        wait
                    }
                }
            };
            if let Some(notify) = outside {
                gate = None;
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {},
                    _ = notify.notified() => {},
                }
                continue;
            }
            tokio::select! {
                _ = tokio::time::sleep(wait.min(Duration::from_secs(60))) => {},
                _ = changed => {},
            }
        }
    }
    /// The run loop's call when a place kept for a woken turn lapses: the
    /// place goes to the next parked turn, if that turn has not come back.
    pub fn lapse(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(ramp) = state.ramp.as_mut() {
            ramp.wake(Instant::now(), &self.this);
        }
        drop(state);
        self.changed.notify_one();
    }
    /// How much longer a rate limit closes this pool, if it does.
    pub fn blocked_for(&self) -> Option<Duration> {
        let state = self.state.lock().unwrap();
        state
            .blocked_until
            .map(|until| until.saturating_duration_since(Instant::now()))
            .filter(|left| !left.is_zero())
    }
    /// The provider refused for pace; nothing is admitted before `after`.
    /// A refusal that names no delay closes the pool for one second, doubled
    /// up to 32 each time the pool reopens only to be refused again, until a
    /// call succeeds. Refusals of calls already in flight while it is closed
    /// are the same overload and do not escalate it, though one arriving
    /// during a named delay starts the streak. Providers that publish no
    /// allowance, such as Bedrock, are paced by this alone.
    pub fn limited(&self, after: Option<Duration>) {
        self.refused(after, false);
    }
    /// The provider refused because the pool's traffic grew too fast
    /// (Anthropic's `ramp_limit_exceeded`): closed as `limited`, and let
    /// back in through a ramp even when it knows its allowance.
    pub fn ramp_limited(&self, after: Option<Duration>) {
        self.refused(after, true);
    }
    fn refused(&self, after: Option<Duration>, ramp: bool) {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        // Calls already in flight while it is closed were refused by the
        // same overload: the window narrows once a round.
        let open = state.blocked_until.is_none_or(|until| until <= now);
        let blind = state.requests.per_minute.is_none()
            && state
                .tokens
                .iter()
                .all(|dim| dim.bucket.per_minute.is_none());
        // A ramp limit starts a ramp whenever none runs, closed or not.
        match state.ramp.as_mut() {
            None if ramp || (open && blind) => {
                state.ramps += 1;
                state.ramp = Some(Ramp::new(state.ramps))
            }
            Some(running) if open && (ramp || blind) => {
                let half = (running.window / 2).max(1);
                (running.window, running.threshold, running.credit) = (half, Some(half), 0);
            }
            _ => {}
        }
        let delay = after.unwrap_or_else(|| {
            if state.unnamed_blocks == 0 || state.blocked_until.is_none_or(|until| until <= now) {
                state.unnamed_blocks = (state.unnamed_blocks + 1).min(6);
            }
            Duration::from_secs(1 << (state.unnamed_blocks - 1))
        });
        let until = now + delay;
        state.blocked_until = Some(state.blocked_until.map_or(until, |u| u.max(until)));
        state.counts.closed += 1;
        drop(state);
        // Wake the FIFO head even when it was waiting for allowance refill.
        // It parks and releases the gate; queued callers then observe the block.
        self.changed.notify_one();
    }
    /// What a call of this pool is likely to write, for a provider that
    /// counts output as it is written: the 90th percentile of the latest
    /// calls' output, at most `cap`, or `unknown` until enough have
    /// reported. A call that writes more is charged the rest when it settles.
    /// Output is written over time, so a reply longer than the limit can
    /// still be served: the estimate waits for a full allowance at most,
    /// and is never refused. Under a total limit, the input estimate
    /// leaves room for it.
    pub fn output_estimate(&self, unknown: u64, cap: u64) -> u64 {
        let state = self.state.lock().unwrap();
        let mut estimate = p90(&state.outputs).map_or(unknown, |p90| p90.min(cap));
        for dim in [OUTPUT, TOTAL] {
            if let Some(limit) = state.tokens[dim].bucket.per_minute {
                estimate = estimate.min(limit as u64);
            }
        }
        estimate
    }
    /// How much of a call's `input` is likely to count beside `output`,
    /// for a provider that leaves cache reads out: the 90th percentile of
    /// the share that counted in the latest calls, or all of it until enough
    /// have reported. A call that counts more is charged the rest when it
    /// settles. A prompt larger than the limit can still be read from the
    /// cache, so it waits for a full allowance rather than being refused.
    pub fn input_estimate(&self, input: u64, output: u64) -> u64 {
        let state = self.state.lock().unwrap();
        let mut estimate =
            p90(&state.counted).map_or(input, |share| (input * share).div_ceil(SHARE).min(input));
        if let Some(limit) = state.tokens[INPUT].bucket.per_minute {
            estimate = estimate.min(limit as u64);
        }
        if let Some(limit) = state.tokens[TOTAL].bucket.per_minute {
            estimate = estimate.min((limit as u64).saturating_sub(output));
        }
        estimate
    }
    /// A call completed: the next unnamed refusal starts again at a second.
    pub fn accepted(&self) {
        let mut state = self.state.lock().unwrap();
        state.unnamed_blocks = 0;
        if let Some(ramp) = state.ramp.as_mut() {
            match ramp.threshold {
                Some(threshold) if ramp.window >= threshold => {
                    ramp.credit += 1;
                    if ramp.credit >= ramp.window {
                        ramp.credit = 0;
                        ramp.window += 1;
                    }
                }
                _ => ramp.window += 1,
            }
            let done = ramp.window >= RAMP_DONE;
            match done {
                false => ramp.wake(Instant::now(), &self.this),
                // Every turn still parked on it goes now.
                true => {
                    for waiter in state.ramp.take().into_iter().flat_map(|r| r.queued) {
                        (waiter.wake)(Woken::Turn(waiter.bot, waiter.turn));
                    }
                }
            }
            // A wider window may let the gate's head go.
            self.changed.notify_one();
        }
        state.overloads.at(Instant::now()).answered += 1;
    }
    /// An overload refused a call. With a delay named it closes the pool as
    /// a rate limit does. With none, the refused call backs off alone, as
    /// Codex and the providers' SDKs retry an overload per request, unless
    /// overloads cluster: then the pool closes as for an unnamed rate limit.
    /// Whether it closed.
    pub fn overloaded(&self, after: Option<Duration>) -> bool {
        let clustered = {
            let mut state = self.state.lock().unwrap();
            state.counts.overloads += 1;
            after.is_some() || {
                let window = state.overloads.at(Instant::now());
                window.refused += 1;
                window.refused >= OVERLOAD_CLUSTER && window.refused * 3 >= window.answered
            }
        };
        if clustered {
            self.limited(after);
        }
        clustered
    }
    /// Learn the allowance from headers that precede every call they could
    /// count, such as a WebSocket upgrade's. Outstanding reservations stay
    /// reserved and are settled by their own usage.
    pub fn seed(&self, headers: &reqwest::header::HeaderMap, limits: impl Into<Limits>) {
        self.learn(headers, limits, [0; DIMS], 0);
    }
    /// Learn the allowance from a response's rate-limit headers, per
    /// dimension the provider publishes. Returns which token dimensions
    /// accounted for and released this estimate. Request reservations are
    /// always resolved at headers; their debit is retained unless a request
    /// balance accounts for it. The conservative minimum and release share a
    /// lock so no other admission can spend an intermediate balance. Stream
    /// completion must not settle a reported dimension again.
    fn learn(
        &self,
        headers: &reqwest::header::HeaderMap,
        limits: impl Into<Limits>,
        estimate: [u64; DIMS],
        request_count: u64,
    ) -> [bool; DIMS] {
        let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let number = |name: &str| get(name).and_then(|v| v.trim().parse::<f64>().ok());
        let pair = |limit: &str, remaining: &str| match (number(limit), number(remaining)) {
            (Some(limit), Some(remaining)) => Some((limit, remaining)),
            _ => None,
        };
        // The allowance refills continuously, so the reset header is only
        // how long a full refill takes; the level itself is what to learn.
        // Blocking until a reset would idle the pool for a minute while the
        // provider was already accepting again.
        let (requests, tokens): (_, [Option<(f64, f64)>; DIMS]) = match limits.into() {
            Limits::Responses => (
                pair(
                    "x-ratelimit-limit-requests",
                    "x-ratelimit-remaining-requests",
                ),
                [
                    pair("x-ratelimit-limit-tokens", "x-ratelimit-remaining-tokens"),
                    None,
                    None,
                ],
            ),
            Limits::AnthropicFast => (
                None,
                [
                    None,
                    pair(
                        "anthropic-fast-input-tokens-limit",
                        "anthropic-fast-input-tokens-remaining",
                    ),
                    pair(
                        "anthropic-fast-output-tokens-limit",
                        "anthropic-fast-output-tokens-remaining",
                    ),
                ],
            ),
            Limits::Anthropic => (
                pair(
                    "anthropic-ratelimit-requests-limit",
                    "anthropic-ratelimit-requests-remaining",
                ),
                [
                    pair(
                        "anthropic-ratelimit-tokens-limit",
                        "anthropic-ratelimit-tokens-remaining",
                    ),
                    pair(
                        "anthropic-ratelimit-input-tokens-limit",
                        "anthropic-ratelimit-input-tokens-remaining",
                    ),
                    pair(
                        "anthropic-ratelimit-output-tokens-limit",
                        "anthropic-ratelimit-output-tokens-remaining",
                    ),
                ],
            ),
        };
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        if let Some((limit, remaining)) = requests {
            // Keep server balances net of every outstanding reservation,
            // including calls acquired before the limit became known, and of
            // what parked calls booked, which the server has not seen.
            let reserved = (state.reserved_requests + state.booked.len() as u64) as f64;
            state.requests.learn(limit, remaining - reserved, now);
            state.requests.available = (state.requests.available + request_count as f64).min(limit);
        }
        // Receiving headers proves this request was sent. Without request
        // headers its debit stays spent, but it is no longer a reservation.
        state.reserved_requests -= request_count;
        let mut reported = [false; DIMS];
        for index in 0..DIMS {
            let Some((limit, remaining)) = tokens[index] else {
                continue;
            };
            // Header balances exclude reservations and bookings, our bucket
            // includes them. Put both on the same basis before taking the
            // conservative minimum. Every reservation is returned exactly
            // once, even on cancellation.
            let booked = state.booked.held(index);
            let dim = &mut state.tokens[index];
            dim.bucket
                .learn(limit, remaining - (dim.reserved + booked) as f64, now);
            dim.reserved -= estimate[index];
            dim.bucket.available = (dim.bucket.available + estimate[index] as f64).min(limit);
            reported[index] = true;
        }
        drop(state);
        if reported.iter().any(|r| *r) || requests.is_some() {
            self.changed.notify_one();
        }
        reported
    }
    pub fn status(&self) -> serde_json::Value {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        state.refill(now);
        let blocked_ms = state
            .blocked_until
            .map_or(0, |u| u.saturating_duration_since(now).as_millis() as u64);
        let level = |dim: &Dim| dim.bucket.per_minute.map(|_| dim.bucket.available.floor());
        serde_json::json!({
            "requests_per_minute": state.requests.per_minute,
            "requests_available": state.requests.per_minute.map(|_| state.requests.available.floor()),
            "tokens_per_minute": state.tokens[TOTAL].bucket.per_minute,
            "tokens_available": level(&state.tokens[TOTAL]),
            "input_tokens_per_minute": state.tokens[INPUT].bucket.per_minute,
            "input_tokens_available": level(&state.tokens[INPUT]),
            "output_tokens_per_minute": state.tokens[OUTPUT].bucket.per_minute,
            "output_tokens_available": level(&state.tokens[OUTPUT]),
            "reserved_tokens": state.tokens[TOTAL].reserved,
            "reserved_requests": state.reserved_requests,
            "blocked_ms": blocked_ms,
            "admitted": state.counts.admitted,
            "waited": state.counts.waited,
            "waited_ms": state.counts.waited_ms,
            "parked": state.counts.parked,
            "closed": state.counts.closed,
            "overloads": state.counts.overloads,
        })
    }
    #[cfg(test)]
    pub fn snapshot(&self) -> (Option<f64>, f64, Option<f64>, f64, bool) {
        let state = self.state.lock().unwrap();
        (
            state.requests.per_minute,
            state.requests.available,
            state.tokens[TOTAL].bucket.per_minute,
            state.tokens[TOTAL].bucket.available,
            state.blocked_until.is_some_and(|u| u > Instant::now()),
        )
    }
    #[cfg(test)]
    pub fn split_snapshot(&self) -> [(Option<f64>, f64); DIMS] {
        let state = self.state.lock().unwrap();
        std::array::from_fn(|i| {
            (
                state.tokens[i].bucket.per_minute,
                state.tokens[i].bucket.available,
            )
        })
    }
}

/// The longest delay a provider's words are read as naming. A number past
/// it (a gateway's stray header or message) is read as this, so no clock
/// it is added to overflows: it bounds what a number can mean, not a pool.
pub const LONGEST_DELAY: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// A Go-style duration such as `6m0s`, `1.2s`, or `120ms`.
pub fn go_duration(text: &str) -> Option<Duration> {
    let mut total = 0.0;
    let mut number = String::new();
    let mut chars = text.trim().chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            number.push(c);
            continue;
        }
        let mut unit = c.to_string();
        while let Some(&next) = chars.peek() {
            if next.is_ascii_alphabetic() {
                unit.push(next);
                chars.next();
            } else {
                break;
            }
        }
        let value: f64 = number.parse().ok()?;
        number.clear();
        total += match unit.as_str() {
            "h" => value * 3600.0,
            "m" => value * 60.0,
            "s" => value,
            "ms" => value / 1000.0,
            "µs" | "us" => value / 1_000_000.0,
            _ => return None,
        };
        any = true;
    }
    if !number.is_empty() {
        total += number.parse::<f64>().ok()?;
        any = true;
    }
    // One too long for a `Duration` to hold is the longest, not a panic.
    any.then(|| {
        Duration::try_from_secs_f64(total.max(0.0))
            .unwrap_or(LONGEST_DELAY)
            .min(LONGEST_DELAY)
    })
}

/// The delay a provider names inside a message, `... try again in 2.106s`.
pub fn named_delay(message: &str) -> Option<Duration> {
    let index = message.find("try again in ")?;
    let rest = &message["try again in ".len() + index..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == 'µ'))
        .unwrap_or(rest.len());
    go_duration(rest[..end].trim_end_matches('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    async fn sent(pace: &Pace, tokens: u64) -> crate::Result<Reservation<'_>> {
        let mut call = admitted(pace, tokens).await?;
        call.dispatch();
        Ok(call)
    }

    /// A call admitted as a turn is: parked until its allowance refills and
    /// resumed then, waiting the whole time.
    async fn admitted(pace: &Pace, tokens: u64) -> crate::Result<Reservation<'_>> {
        let started = Instant::now();
        loop {
            let mut park_for = None;
            match pace.acquire(tokens, &mut park_for).await {
                Err(error) if error.code == "provider_paced" => {
                    tokio::time::sleep(park_for.unwrap()).await;
                }
                result => {
                    let mut call = result?;
                    call.waited = started.elapsed();
                    return Ok(call);
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_pool_admits_a_burst_without_waiting_for_headers() {
        let pace = Pace::default();
        let held = tokio::time::timeout(Duration::from_millis(1), async {
            let mut held = Vec::new();
            for _ in 0..128 {
                held.push(sent(&pace, 10).await.unwrap());
            }
            held
        })
        .await
        .expect("unknown capacity must not impose an admission cap");
        assert!(held.iter().all(|r| r.waited.is_zero()));
        assert_eq!(pace.state.lock().unwrap().reserved_requests, 128);
    }

    #[tokio::test]
    async fn the_output_estimate_follows_what_calls_wrote() {
        async fn wrote(pace: &Pace, calls: usize, output: u64) {
            for _ in 0..calls {
                let usage = Usage {
                    input_tokens: 1,
                    output_tokens: output,
                    ..Usage::default()
                };
                sent(pace, 2).await.unwrap().settle_usage(
                    Some(&usage),
                    Cost::default(),
                    crate::codec::Family::Anthropic,
                );
            }
        }
        let pace = Pace::default();
        // Until eight calls report, a call reserves what it did before.
        wrote(&pace, 7, 100).await;
        assert_eq!(pace.output_estimate(512, 4096), 512);
        wrote(&pace, 1, 100).await;
        assert_eq!(pace.output_estimate(512, 4096), 100);
        // The 90th percentile of the latest 32: three long replies in 32
        // still leave it at the short ones; a fourth moves it.
        wrote(&pace, 21, 100).await;
        wrote(&pace, 3, 3000).await;
        assert_eq!(pace.output_estimate(512, 4096), 100);
        wrote(&pace, 1, 3000).await;
        assert_eq!(pace.output_estimate(512, 4096), 3000);
        // Never above the cap the call asks for.
        assert_eq!(pace.output_estimate(512, 1000), 1000);
        // Nor above the limit, which a reply written over time can pass.
        pace.seed(
            &headers(&[
                ("anthropic-ratelimit-output-tokens-limit", "2000"),
                ("anthropic-ratelimit-output-tokens-remaining", "2000"),
            ]),
            crate::codec::Family::Anthropic,
        );
        assert_eq!(pace.output_estimate(512, 4096), 2000);
        assert!(sent(&pace, 0).await.is_ok());
        // Beside its output, a call never asks for more than the total
        // limit: its input estimate yields.
        pace.seed(
            &headers(&[
                ("anthropic-ratelimit-tokens-limit", "10000"),
                ("anthropic-ratelimit-tokens-remaining", "10000"),
            ]),
            crate::codec::Family::Anthropic,
        );
        assert_eq!(pace.output_estimate(512, 4096), 2000);
        assert_eq!(pace.input_estimate(9000, 2000), 8000);
        assert_eq!(pace.input_estimate(3000, 2000), 3000);
    }

    #[tokio::test(start_paused = true)]
    async fn the_input_estimate_leaves_out_what_cache_reads_spared() {
        async fn read(
            pace: &Pace,
            calls: usize,
            input: u64,
            cached: u64,
            family: crate::codec::Family,
        ) {
            for _ in 0..calls {
                let usage = Usage {
                    input_tokens: input,
                    cached_input_tokens: cached,
                    output_tokens: 1,
                    ..Usage::default()
                };
                sent(pace, 2)
                    .await
                    .unwrap()
                    .settle_usage(Some(&usage), Cost::default(), family);
            }
        }
        let pace = Pace::default();
        // Until eight calls report, a call reserves all of its input.
        read(&pace, 7, 10_000, 9_500, crate::codec::Family::Anthropic).await;
        assert_eq!(pace.input_estimate(100_000, 0), 100_000);
        read(&pace, 1, 10_000, 9_500, crate::codec::Family::Anthropic).await;
        assert_eq!(pace.input_estimate(100_000, 0), 5_000);
        // Four misses in 32 move the 90th percentile to a whole prompt.
        read(&pace, 20, 10_000, 9_500, crate::codec::Family::Anthropic).await;
        read(&pace, 4, 10_000, 0, crate::codec::Family::Anthropic).await;
        assert_eq!(pace.input_estimate(100_000, 0), 100_000);
        // The input a cache read spared is not charged when the call settles.
        let pace = Pace::default();
        pace.seed(
            &headers(&[
                ("anthropic-ratelimit-input-tokens-limit", "10000"),
                ("anthropic-ratelimit-input-tokens-remaining", "10000"),
            ]),
            crate::codec::Family::Anthropic,
        );
        let usage = Usage {
            input_tokens: 1_000,
            cached_input_tokens: 900,
            ..Usage::default()
        };
        sent(&pace, 1).await.unwrap().settle_usage(
            Some(&usage),
            Cost::default(),
            crate::codec::Family::Anthropic,
        );
        assert_eq!(pace.split_snapshot()[INPUT].1, 9_900.0);
        // A prompt larger than the limit waits for a full allowance: it may
        // be read from the cache.
        assert_eq!(pace.input_estimate(50_000, 0), 10_000);
        // A few uncached tokens beside a long cached prompt still reserve
        // their share.
        let pace = Pace::default();
        read(&pace, 8, 200_050, 200_000, crate::codec::Family::Anthropic).await;
        assert_eq!(pace.input_estimate(200_050, 0), 51);
        // A provider that counts cache reads learns nothing to leave out.
        let pace = Pace::default();
        read(&pace, 8, 10_000, 9_500, crate::codec::Family::Responses).await;
        assert_eq!(pace.input_estimate(100_000, 0), 100_000);
    }

    #[tokio::test(start_paused = true)]
    async fn refill_waits_park_one_behind_another_and_spend_their_booking() {
        let pace = Pace::default();
        // 600 tokens a minute, 10 a second, none left.
        pace.learn(
            &headers(&[
                ("x-ratelimit-limit-tokens", "600"),
                ("x-ratelimit-remaining-tokens", "0"),
            ]),
            crate::codec::Family::Responses,
            [0; DIMS],
            0,
        );
        async fn park(pace: &Pace) -> Duration {
            let mut park_for = None;
            let parked = pace.acquire(10, &mut park_for).await;
            assert_eq!(parked.err().unwrap().code, "provider_paced");
            park_for.unwrap()
        }
        // Each call parks behind the ones before it.
        let mut parks = Vec::new();
        for _ in 0..3 {
            parks.push(park(&pace).await);
        }
        assert_eq!(
            parks,
            [1, 2, 3].map(Duration::from_secs).to_vec(),
            "{parks:?}"
        );
        // Resumed when due, the first spends its booking and goes at once,
        // and the allowance it refilled is not spent twice.
        tokio::time::advance(Duration::from_secs(1)).await;
        let first = pace.acquire(10, &mut None).await.unwrap();
        assert!(first.waited.is_zero());
        drop(first);
        assert_eq!(pace.state.lock().unwrap().booked.len(), 2);
        // A booking no call spends returns once its grace has passed.
        tokio::time::advance(Duration::from_secs(10)).await;
        let late = pace.acquire(10, &mut None).await.unwrap();
        assert!(late.waited.is_zero());
        drop(late);
        assert!(pace.state.lock().unwrap().booked.is_empty());
        // Eleven seconds refilled 110; nothing booked or unsent is held.
        assert_eq!(pace.split_snapshot()[TOTAL].1, 110.0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_booking_goes_only_once_the_allowance_pays_for_it() {
        let limited = |remaining: &str| {
            let pace = Pace::default();
            pace.learn(
                &headers(&[
                    ("x-ratelimit-limit-tokens", "600"),
                    ("x-ratelimit-remaining-tokens", remaining),
                ]),
                crate::codec::Family::Responses,
                [0; DIMS],
                0,
            );
            pace
        };
        async fn park(pace: &Pace, tokens: u64) -> Duration {
            let mut park_for = None;
            let parked = pace.acquire(tokens, &mut park_for).await;
            assert_eq!(parked.err().unwrap().code, "provider_paced");
            park_for.unwrap()
        }
        let available = |pace: &Pace| pace.state.lock().unwrap().tokens[TOTAL].bucket.available;
        let about = |parked: Duration, secs: u64| {
            assert!(
                parked.abs_diff(Duration::from_secs(secs)) < Duration::from_millis(1),
                "{parked:?}"
            )
        };

        // A larger call spending a small booking waits for what it grew by.
        let pace = limited("0");
        assert_eq!(park(&pace, 10).await, Duration::from_secs(1));
        tokio::time::advance(Duration::from_secs(1)).await;
        about(park(&pace, 600).await, 59);

        // A wait that counted on a refund parks again when none came.
        let pace = limited("100");
        let flying = sent(&pace, 100).await.unwrap();
        assert_eq!(park(&pace, 200).await, Duration::from_secs(10));
        tokio::time::advance(Duration::from_secs(10)).await;
        let early =
            tokio::time::timeout(Duration::from_secs(3), pace.acquire(200, &mut None)).await;
        assert!(early.is_err(), "admitted before it was paid for");
        drop(flying);
        about(park(&pace, 200).await, 7);

        // A smaller call goes once the refill pays for it, and returns the
        // rest of the booking.
        let pace = limited("100");
        let _flying = sent(&pace, 100).await.unwrap();
        assert_eq!(park(&pace, 200).await, Duration::from_secs(10));
        tokio::time::advance(Duration::from_secs(10)).await;
        let small = pace.acquire(10, &mut None).await.unwrap();
        assert!(small.waited.is_zero());
        assert_eq!(available(&pace), 90.0);

        // Resumed late, a booking holds no more than the limit.
        let pace = limited("0");
        assert_eq!(park(&pace, 600).await, Duration::from_secs(60));
        tokio::time::advance(Duration::from_secs(61)).await;
        let _full = pace.acquire(600, &mut None).await.unwrap();
        assert_eq!(available(&pace), 0.0);

        // Headers leave what later calls booked owed.
        let pace = limited("0");
        park(&pace, 10).await;
        park(&pace, 10).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        let mut first = pace.acquire(10, &mut None).await.unwrap();
        first.dispatch();
        first.learn(
            &headers(&[
                ("x-ratelimit-limit-tokens", "600"),
                ("x-ratelimit-remaining-tokens", "0"),
            ]),
            crate::codec::Family::Responses,
        );
        assert_eq!(available(&pace), -10.0);
        tokio::time::advance(Duration::from_secs(1)).await;
        let second = pace.acquire(10, &mut None).await.unwrap();
        assert!(second.waited.is_zero());
        assert_eq!(available(&pace), 0.0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_lone_overload_leaves_the_pool_open_and_clustered_ones_close_it() {
        let pace = Pace::default();
        let answered = |pace: &Pace, calls: u32| (0..calls).for_each(|_| pace.accepted());
        // One refusal among many answers is that call's to back off.
        answered(&pace, 20);
        assert!(!pace.overloaded(None));
        assert!(!pace.overloaded(None));
        assert_eq!(pace.blocked_for(), None);
        // A third, still under a quarter of the answers, leaves it open too.
        answered(&pace, 10);
        assert!(!pace.overloaded(None));
        assert_eq!(pace.blocked_for(), None);
        // A new window: three refusals and few answers are the provider's.
        tokio::time::advance(OVERLOAD_WINDOW).await;
        answered(&pace, 2);
        assert!(!pace.overloaded(None));
        assert!(!pace.overloaded(None));
        assert!(pace.overloaded(None));
        assert!(pace.blocked_for().is_some());
        // A named delay closes it whatever the window holds.
        tokio::time::advance(Duration::from_secs(40)).await;
        assert_eq!(pace.blocked_for(), None);
        assert!(pace.overloaded(Some(Duration::from_secs(3))));
        assert_eq!(pace.blocked_for().unwrap().as_secs_f64().round(), 3.0);
    }

    #[tokio::test(start_paused = true)]
    async fn refusals_naming_no_delay_back_off_until_a_call_is_accepted() {
        let pace = Pace::default();
        let left = |pace: &Pace| pace.blocked_for().unwrap_or_default().as_secs_f64().round();
        pace.limited(None);
        assert_eq!(left(&pace), 1.0);
        // Refusals of calls already in flight are the same overload.
        pace.limited(None);
        pace.limited(None);
        assert_eq!(left(&pace), 1.0);
        for expected in [2.0, 4.0, 8.0, 16.0, 32.0, 32.0] {
            tokio::time::advance(Duration::from_secs(33)).await;
            pace.limited(None);
            assert_eq!(left(&pace), expected);
        }
        // A named delay is taken as given and leaves the streak alone.
        tokio::time::advance(Duration::from_secs(33)).await;
        pace.limited(Some(Duration::from_secs(3)));
        assert_eq!(left(&pace), 3.0);
        tokio::time::advance(Duration::from_secs(33)).await;
        pace.accepted();
        pace.limited(None);
        assert_eq!(left(&pace), 1.0);
        // An unnamed refusal in flight during a named delay joins that block
        // and starts the streak; the next one after it reopens escalates.
        tokio::time::advance(Duration::from_secs(33)).await;
        pace.accepted();
        pace.limited(Some(Duration::from_secs(5)));
        pace.limited(None);
        assert_eq!(left(&pace), 5.0);
        tokio::time::advance(Duration::from_secs(6)).await;
        pace.limited(None);
        assert_eq!(left(&pace), 2.0);
    }

    /// A pool that knows no allowance lets calls back one at a time after a
    /// refusal, one more for each answered, instead of all at once.
    #[tokio::test(start_paused = true)]
    async fn a_blind_pool_ramps_back_after_a_refusal() {
        let pace = Pace::default();
        // Before any refusal, a burst goes at once.
        let burst: Vec<_> = futures_util::future::join_all((0..4).map(|_| sent(&pace, 10)))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        drop(burst);
        pace.limited(None);
        tokio::time::advance(Duration::from_secs(2)).await;
        let first = sent(&pace, 10).await.unwrap();
        // The window is one call: the next waits for it to settle.
        let second = tokio::time::timeout(Duration::from_secs(5), sent(&pace, 10)).await;
        assert!(second.is_err(), "a second call flew beside the first");
        pace.accepted();
        // Answered, the window is two.
        let second = sent(&pace, 10).await.unwrap();
        drop(first);
        let third = sent(&pace, 10).await.unwrap();
        let fourth = tokio::time::timeout(Duration::from_secs(5), sent(&pace, 10)).await;
        assert!(fourth.is_err(), "the window grew without an answer");
        drop((second, third));
        // Answers widen it until it lets everything go again.
        for _ in 0..RAMP_DONE {
            pace.accepted();
        }
        assert!(pace.state.lock().unwrap().ramp.is_none());
    }

    /// A refusal while ramping halves the window, once for every call
    /// refused while the pool was closed, and growth goes on a call a round.
    #[tokio::test(start_paused = true)]
    async fn a_refusal_while_ramping_halves_the_window() {
        let pace = Pace::default();
        let window = |pace: &Pace| pace.state.lock().unwrap().ramp.as_ref().map(|r| r.window);
        pace.limited(None);
        assert_eq!(window(&pace), Some(1));
        for _ in 0..7 {
            pace.accepted();
        }
        assert_eq!(window(&pace), Some(8));
        tokio::time::advance(Duration::from_secs(2)).await;
        pace.limited(None);
        // Refused in flight while closed: the same refusal.
        pace.limited(None);
        assert_eq!(window(&pace), Some(4));
        // Past the threshold it grows by one for a window of answers.
        for _ in 0..3 {
            pace.accepted();
        }
        assert_eq!(window(&pace), Some(4));
        pace.accepted();
        assert_eq!(window(&pace), Some(5));
    }

    /// A call asked for as `waiter`, and how long it was told to park.
    async fn ask<'a>(
        pace: &'a Pace,
        waiter: Option<&Waiter>,
    ) -> (crate::Result<Reservation<'a>>, Option<Duration>) {
        let mut park_for = None;
        let cost = Cost {
            input: 10,
            output: 0,
        };
        let call = pace.acquire_cost(cost, &mut park_for, waiter).await;
        (call, park_for)
    }

    /// A turn that finds the ramp full parks, holding nothing live; a
    /// settle wakes the oldest, and keeps its place until it comes back.
    #[tokio::test(start_paused = true)]
    async fn a_settle_wakes_the_oldest_turn_parked_on_a_full_ramp() {
        let pace = Pace::default();
        let woken = Arc::new(Mutex::new(Vec::new()));
        let waiter = |turn: i64| {
            let woken = woken.clone();
            Waiter {
                bot: format!("bot{turn}"),
                turn,
                wake: Arc::new(move |w| {
                    if let Woken::Turn(_, turn) = w {
                        woken.lock().unwrap().push(turn)
                    }
                }),
            }
        };
        let (one, two) = (waiter(1), waiter(2));
        pace.limited(None);
        tokio::time::advance(Duration::from_secs(2)).await;
        let (first, _) = ask(&pace, None).await;
        let first = first.unwrap();
        // Both turns park, in order, for no longer than the fallback.
        for waiter in [&one, &two] {
            let (call, park_for) = ask(&pace, Some(waiter)).await;
            assert_eq!(call.err().unwrap().code, "provider_paced");
            assert_eq!(park_for, Some(RAMP_PARK));
        }
        // A turn coming back before it is woken keeps its place in line.
        assert!(ask(&pace, Some(&one)).await.0.is_err());
        pace.accepted();
        // The window grew to two: the oldest is woken for the place.
        assert_eq!(*woken.lock().unwrap(), [1]);
        // The place is kept for it: another caller finds the ramp full.
        let other = tokio::time::timeout(Duration::from_secs(1), ask(&pace, None)).await;
        assert!(
            other.is_err(),
            "a caller took the place kept for the woken turn"
        );
        let (call, _) = ask(&pace, Some(&one)).await;
        let call = call.unwrap();
        // A settle wakes the next.
        drop(first);
        assert_eq!(*woken.lock().unwrap(), [1, 2]);
        drop(call);
    }

    /// A caller that cannot park (a summary beside a turn, a search) queues
    /// on a full ramp as a turn does, and waits outside the gate, so a turn
    /// woken ahead of it can come back for its place.
    #[tokio::test(start_paused = true)]
    async fn a_caller_that_cannot_park_waits_its_turn_outside_the_gate() {
        let pace = Pace::default();
        let woken = Arc::new(Mutex::new(Vec::new()));
        let waiter = |turn: i64| {
            let woken = woken.clone();
            Waiter {
                bot: format!("bot{turn}"),
                turn,
                wake: Arc::new(move |w| {
                    if let Woken::Turn(_, turn) = w {
                        woken.lock().unwrap().push(turn)
                    }
                }),
            }
        };
        let one = waiter(1);
        pace.limited(None);
        tokio::time::advance(Duration::from_secs(2)).await;
        let first = ask(&pace, None).await.0.unwrap();
        assert!(ask(&pace, Some(&one)).await.0.is_err());
        let background = ask(&pace, None);
        tokio::pin!(background);
        let second = Duration::from_secs(1);
        assert!(tokio::time::timeout(second, &mut background).await.is_err());
        drop(first);
        assert_eq!(*woken.lock().unwrap(), [1]);
        let call = tokio::time::timeout(second, ask(&pace, Some(&one)))
            .await
            .expect("the waiting caller held the gate")
            .0
            .unwrap();
        assert!(tokio::time::timeout(second, &mut background).await.is_err());
        drop(call);
        let (later, _) = tokio::time::timeout(second, &mut background)
            .await
            .expect("a settle wakes the queued caller");
        later.unwrap();
    }

    /// A woken turn that never comes back, on a pool that sees no other
    /// call, does not hold its place past its lapse: the run loop calls the
    /// pool back then, and the place goes to the next parked turn.
    #[tokio::test(start_paused = true)]
    async fn the_run_loop_passes_on_a_place_its_turn_never_came_back_for() {
        let pools = Pools::default();
        let pace = pools.get("model");
        let (woken, lapses) = (
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
        );
        let waiter = |turn: i64| {
            let (woken, lapses) = (woken.clone(), lapses.clone());
            Waiter {
                bot: format!("bot{turn}"),
                turn,
                wake: Arc::new(move |w| match w {
                    Woken::Turn(_, turn) => woken.lock().unwrap().push(turn),
                    Woken::Lapse(at, pace) => lapses.lock().unwrap().push((at, pace)),
                }),
            }
        };
        pace.limited(None);
        tokio::time::advance(Duration::from_secs(2)).await;
        let first = ask(&pace, None).await.0.unwrap();
        for turn in [1, 2] {
            assert!(ask(&pace, Some(&waiter(turn))).await.0.is_err());
        }
        drop(first);
        assert_eq!(*woken.lock().unwrap(), [1]);
        let (at, kept_by) = lapses.lock().unwrap().pop().unwrap();
        assert_eq!(at, Instant::now() + PROMISE);
        tokio::time::advance(PROMISE).await;
        assert_eq!(*woken.lock().unwrap(), [1], "nothing looked again");
        kept_by.upgrade().unwrap().lapse();
        assert_eq!(*woken.lock().unwrap(), [1, 2]);
    }

    /// A caller that gives up while queued on a ramp gives its place up.
    #[tokio::test(start_paused = true)]
    async fn a_caller_that_gives_up_leaves_the_ramp_s_queue() {
        let pace = Pace::default();
        let woken = Arc::new(Mutex::new(Vec::new()));
        let two = Waiter {
            bot: "bot2".into(),
            turn: 2,
            wake: {
                let woken = woken.clone();
                Arc::new(move |w| {
                    if let Woken::Turn(_, turn) = w {
                        woken.lock().unwrap().push(turn)
                    }
                })
            },
        };
        pace.limited(None);
        tokio::time::advance(Duration::from_secs(2)).await;
        let first = ask(&pace, None).await.0.unwrap();
        let gave_up = tokio::time::timeout(Duration::from_secs(1), ask(&pace, None)).await;
        assert!(gave_up.is_err());
        assert!(ask(&pace, Some(&two)).await.0.is_err());
        drop(first);
        assert_eq!(*woken.lock().unwrap(), [2]);
    }

    /// A place kept for a woken turn that never comes back is let go, and
    /// the end of a ramp wakes every turn still parked on it.
    #[tokio::test(start_paused = true)]
    async fn kept_places_lapse_and_a_ramp_s_end_wakes_every_parked_turn() {
        let pace = Pace::default();
        let woken = Arc::new(Mutex::new(Vec::new()));
        let waiter = |turn: i64| {
            let woken = woken.clone();
            Waiter {
                bot: format!("bot{turn}"),
                turn,
                wake: Arc::new(move |w| {
                    if let Woken::Turn(_, turn) = w {
                        woken.lock().unwrap().push(turn)
                    }
                }),
            }
        };
        pace.limited(None);
        tokio::time::advance(Duration::from_secs(2)).await;
        let first = sent(&pace, 10).await.unwrap();
        for turn in 1..=3 {
            let mut park_for = None;
            let call = pace
                .acquire_cost(
                    Cost {
                        input: 10,
                        output: 0,
                    },
                    &mut park_for,
                    Some(&waiter(turn)),
                )
                .await;
            assert!(call.is_err());
        }
        drop(first);
        assert_eq!(*woken.lock().unwrap(), [1]);
        // Turn 1 never comes back: once its place lapses, the next caller
        // finds it given to turn 2, and parks behind turn 3.
        tokio::time::advance(PROMISE + Duration::from_secs(1)).await;
        assert!(ask(&pace, Some(&waiter(4))).await.0.is_err());
        assert_eq!(*woken.lock().unwrap(), [1, 2]);
        for _ in 0..RAMP_DONE {
            pace.accepted();
        }
        assert!(pace.state.lock().unwrap().ramp.is_none());
        assert_eq!(*woken.lock().unwrap(), [1, 2, 3, 4]);
    }

    /// A call admitted under a ramp that has since ended frees nothing in
    /// a later one.
    #[tokio::test(start_paused = true)]
    async fn a_call_from_an_ended_ramp_holds_no_place_in_a_new_one() {
        let pace = Pace::default();
        pace.limited(None);
        tokio::time::advance(Duration::from_secs(2)).await;
        let old = sent(&pace, 10).await.unwrap();
        for _ in 0..RAMP_DONE {
            pace.accepted();
        }
        assert!(pace.state.lock().unwrap().ramp.is_none());
        pace.limited(None);
        tokio::time::advance(Duration::from_secs(5)).await;
        let new = sent(&pace, 10).await.unwrap();
        drop(old);
        let flying = |pace: &Pace| pace.state.lock().unwrap().ramp.as_ref().map(|r| r.flying);
        assert_eq!(flying(&pace), Some(1));
        drop(new);
        assert_eq!(flying(&pace), Some(0));
    }

    /// A pool that knows its allowance paces by it and does not ramp; a
    /// ramp limit ramps it all the same.
    #[tokio::test(start_paused = true)]
    async fn only_a_blind_pool_or_a_ramp_limit_ramps() {
        let pace = Pace::default();
        pace.seed(
            &headers(&[
                ("x-ratelimit-limit-tokens", "100000"),
                ("x-ratelimit-remaining-tokens", "100000"),
            ]),
            Limits::Responses,
        );
        pace.limited(None);
        assert!(pace.state.lock().unwrap().ramp.is_none());
        // A ramp limit refused while the pool is still closed ramps it.
        pace.ramp_limited(Some(Duration::from_secs(1)));
        assert_eq!(
            pace.state.lock().unwrap().ramp.as_ref().map(|r| r.window),
            Some(1)
        );
    }

    /// Claude's fast mode publishes its own limits, input and output with
    /// no request count; its pool learns those and nothing of the standard
    /// ones, and a standard pool ignores them.
    #[test]
    fn a_fast_pool_learns_the_fast_mode_limits_alone() {
        let published = headers(&[
            ("anthropic-ratelimit-requests-limit", "50"),
            ("anthropic-ratelimit-requests-remaining", "50"),
            ("anthropic-ratelimit-input-tokens-limit", "80000"),
            ("anthropic-ratelimit-input-tokens-remaining", "80000"),
            ("anthropic-fast-input-tokens-limit", "30000"),
            ("anthropic-fast-input-tokens-remaining", "20000"),
            ("anthropic-fast-output-tokens-limit", "6000"),
            ("anthropic-fast-output-tokens-remaining", "6000"),
        ]);
        let fast = Pace::default();
        fast.learn(&published, Limits::AnthropicFast, [0; DIMS], 0);
        let [total, input, output] = fast.split_snapshot();
        assert_eq!(
            (total.0, input, output),
            (None, (Some(30000.0), 20000.0), (Some(6000.0), 6000.0))
        );
        let standard = Pace::default();
        standard.learn(&published, crate::codec::Family::Anthropic, [0; DIMS], 0);
        assert_eq!(standard.split_snapshot()[INPUT].0, Some(80000.0));
    }

    #[tokio::test(start_paused = true)]
    async fn anthropic_input_and_output_limits_pace_their_own_dimensions() {
        let pace = Arc::new(Pace::default());
        // Total is ample; output alone is tight. An output-heavy call waits
        // on the output bucket while an input-heavy one goes through.
        pace.learn(
            &headers(&[
                ("anthropic-ratelimit-tokens-limit", "100000"),
                ("anthropic-ratelimit-tokens-remaining", "100000"),
                ("anthropic-ratelimit-input-tokens-limit", "80000"),
                ("anthropic-ratelimit-input-tokens-remaining", "80000"),
                ("anthropic-ratelimit-output-tokens-limit", "1200"),
                ("anthropic-ratelimit-output-tokens-remaining", "600"),
            ]),
            crate::codec::Family::Anthropic,
            [0; DIMS],
            0,
        );
        let [total, input, output] = pace.split_snapshot();
        assert_eq!(
            (total.0, input.0, output.0),
            (Some(100000.0), Some(80000.0), Some(1200.0))
        );
        let light = pace
            .acquire_cost(
                Cost {
                    input: 5000,
                    output: 500,
                },
                &mut None,
                None,
            )
            .await
            .unwrap();
        assert!(light.waited < Duration::from_millis(10));
        assert_eq!(pace.split_snapshot()[OUTPUT].1, 100.0);
        let heavy = pace
            .acquire_cost(
                Cost {
                    input: 100,
                    output: 500,
                },
                &mut None,
                None,
            )
            .await
            .unwrap();
        // 400 more output tokens at 1,200 per minute is twenty seconds.
        assert!(
            heavy.waited >= Duration::from_secs(19),
            "{:?}",
            heavy.waited
        );
        // A dimension the headers never mention constrains nothing.
        drop((light, heavy));
        let responses = Pace::default();
        responses.learn(
            &headers(&[
                ("x-ratelimit-limit-tokens", "1000"),
                ("x-ratelimit-remaining-tokens", "1000"),
            ]),
            crate::codec::Family::Responses,
            [0; DIMS],
            0,
        );
        assert_eq!(responses.split_snapshot()[OUTPUT].0, None);
    }

    #[tokio::test(start_paused = true)]
    async fn closing_pool_releases_existing_fifo_waiters_without_reserving() {
        let pace = Arc::new(Pace::default());
        pace.learn(
            &headers(&[
                ("x-ratelimit-limit-requests", "1"),
                ("x-ratelimit-remaining-requests", "0"),
            ]),
            crate::codec::Family::Responses,
            [0; DIMS],
            0,
        );
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let pace = pace.clone();
            tasks.push(tokio::spawn(async move {
                let mut report = Report::default();
                let code = pace
                    .acquire_reported(
                        Cost {
                            input: 1,
                            output: 0,
                        },
                        &mut report,
                    )
                    .await
                    .err()
                    .unwrap()
                    .code;
                (code, report.park_for, report.dispatched)
            }));
        }
        // The head waits for refill; the other callers wait on its FIFO gate.
        tokio::task::yield_now().await;
        pace.limited(Some(Duration::from_secs(10)));
        for task in tasks {
            let (code, park, dispatched) = tokio::time::timeout(Duration::from_millis(100), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(code, "provider_paced");
            assert!(park.unwrap() >= Duration::from_secs(9));
            assert!(!dispatched);
        }
        let state = pace.state.lock().unwrap();
        assert_eq!(state.tokens[TOTAL].reserved, 0);
        assert_eq!(state.reserved_requests, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn unsent_cancellation_and_admission_timeout_refund_both_allowances() {
        let pace = Pace::default();
        pace.learn(
            &headers(&[
                ("x-ratelimit-limit-requests", "60"),
                ("x-ratelimit-remaining-requests", "1"),
                ("x-ratelimit-limit-tokens", "1000"),
                ("x-ratelimit-remaining-tokens", "900"),
            ]),
            crate::codec::Family::Responses,
            [0; DIMS],
            0,
        );
        let held = pace.acquire(800, &mut None).await.unwrap();
        let mut park = None;
        let (next, ()) = tokio::join!(pace.acquire(800, &mut park), async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(held);
        });
        let next = next.unwrap();
        assert!(next.waited < Duration::from_millis(20));
        drop(next);
        assert_eq!(pace.state.lock().unwrap().tokens[TOTAL].reserved, 0);
        // The timeout drops the reservation just like the production admission
        // timeout's early return, without ever marking the request dispatched.
        let semaphore = tokio::sync::Semaphore::new(0);
        let result = tokio::time::timeout(Duration::from_secs(60), async {
            let _reservation = pace.acquire(800, &mut None).await.unwrap();
            let _permit = semaphore.acquire().await.unwrap();
        })
        .await;
        assert!(result.is_err());
        let (_, requests, _, tokens, _) = pace.snapshot();
        assert!((1.0..1.1).contains(&requests), "{requests}");
        assert!((900.0..901.0).contains(&tokens), "{tokens}");
        assert_eq!(pace.state.lock().unwrap().tokens[TOTAL].reserved, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn unsent_refunds_do_not_credit_unknown_request_buckets() {
        let pace = Pace::default();
        let waiting = pace.acquire(800, &mut None).await.unwrap();
        let mut reporting = sent(&pace, 10).await.unwrap();
        reporting.learn(
            &headers(&[
                ("x-ratelimit-limit-requests", "60"),
                ("x-ratelimit-remaining-requests", "0"),
                ("x-ratelimit-limit-tokens", "1000"),
                ("x-ratelimit-remaining-tokens", "990"),
            ]),
            crate::codec::Family::Responses,
        );
        assert_eq!(pace.snapshot().3, 190.0);
        drop(waiting);
        assert_eq!(pace.snapshot().1, 0.0);
        assert_eq!(pace.snapshot().3, 990.0);
        drop(reporting);
        assert_eq!(pace.snapshot().3, 990.0);
        assert_eq!(pace.state.lock().unwrap().tokens[TOTAL].reserved, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn request_headers_reconcile_pending_calls_before_refunds() {
        let pace = Pace::default();
        let request_headers = |remaining| {
            headers(&[
                ("x-ratelimit-limit-requests", "60"),
                ("x-ratelimit-remaining-requests", remaining),
            ])
        };
        let family = crate::codec::Family::Responses;
        let mut seed = sent(&pace, 0).await.unwrap();
        seed.learn(&request_headers("4"), family);
        seed.settle(0);
        // Out-of-order headers must not restore requests already spent.
        let mut older = sent(&pace, 0).await.unwrap();
        let mut newer = sent(&pace, 0).await.unwrap();
        newer.learn(&request_headers("2"), family);
        older.learn(&request_headers("3"), family);
        drop(newer);
        drop(older);
        assert_eq!(pace.snapshot().1, 2.0);
        let unsent = admitted(&pace, 0).await.unwrap();
        let mut reporting = sent(&pace, 0).await.unwrap();
        // Another client spent the rest of the provider's allowance.
        reporting.learn(&request_headers("0"), family);
        drop(reporting);
        drop(unsent);
        assert_eq!(pace.snapshot().1, 0.0);
        let next = admitted(&pace, 0).await.unwrap();
        assert!(next.waited >= Duration::from_secs(1));
        drop(next);
        // No headers: a dispatched call still consumes one request.
        let mut no_headers = sent(&pace, 0).await.unwrap();
        no_headers.learn(&HeaderMap::new(), family);
        drop(no_headers);
        assert!(admitted(&pace, 0).await.unwrap().waited >= Duration::from_millis(990));
    }

    #[test]
    fn durations_and_named_delays_parse() {
        assert_eq!(go_duration("6m0s"), Some(Duration::from_secs(360)));
        assert_eq!(go_duration("120ms"), Some(Duration::from_millis(120)));
        assert_eq!(go_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(go_duration("2"), Some(Duration::from_secs(2)));
        assert_eq!(go_duration("soon"), None);
        assert_eq!(
            named_delay(
                "Rate limit reached on tokens per min (TPM): Limit 4000000. Please try again in 2.106s. Visit"
            ),
            Some(Duration::from_millis(2106))
        );
        assert_eq!(named_delay("no delay here"), None);
        // A delay no `Duration` holds is the longest, not a panic.
        assert_eq!(
            named_delay("try again in 99999999999999999999s"),
            Some(LONGEST_DELAY)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn oversized_estimates_fail_without_blocking_other_requests() {
        let pace = Pace::default();
        pace.learn(
            &headers(&[
                ("x-ratelimit-limit-tokens", "600"),
                ("x-ratelimit-remaining-tokens", "600"),
            ]),
            crate::codec::Family::Responses,
            [0; DIMS],
            0,
        );
        let (oversized, small) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(1), sent(&pace, 601)),
            tokio::time::timeout(Duration::from_secs(1), sent(&pace, 1)),
        );
        assert_eq!(
            oversized.unwrap().err().unwrap().code,
            "provider_pacing_limit"
        );
        assert!(small.unwrap().unwrap().waited.is_zero());
        // The rejected estimate did not consume any of the allowance.
        assert!(sent(&pace, 599).await.unwrap().waited.is_zero());
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_pools_admit_freely_and_learned_ones_pace_fairly() {
        let pace = Pace::default();
        let unknown = sent(&pace, 1_000_000).await.unwrap();
        assert!(unknown.waited.is_zero());
        unknown.settle(0);
        // Learn: 6 requests and 600 tokens per minute, 5 requests and 100 tokens left.
        pace.learn(
            &headers(&[
                ("x-ratelimit-limit-requests", "6"),
                ("x-ratelimit-remaining-requests", "5"),
                ("x-ratelimit-reset-requests", "1s"),
                ("x-ratelimit-limit-tokens", "600"),
                ("x-ratelimit-remaining-tokens", "100"),
                ("x-ratelimit-reset-tokens", "1s"),
            ]),
            crate::codec::Family::Responses,
            [0; DIMS],
            0,
        );
        let call = sent(&pace, 50).await.unwrap();
        assert!(call.waited.is_zero());
        call.settle(50);
        let call = sent(&pace, 50).await.unwrap();
        assert!(call.waited.is_zero());
        call.settle(50);
        // Tokens are exhausted: the third call waits for 50 tokens at 10 per second.
        let call = sent(&pace, 50).await.unwrap();
        let waited = call.waited;
        assert!(
            waited >= Duration::from_secs(5) && waited < Duration::from_secs(6),
            "{waited:?}"
        );
        // Settling a smaller bill than the estimate refunds the difference.
        call.settle(20);
        let (_, _, _, tokens, _) = pace.snapshot();
        assert!(tokens > 29.0 && tokens < 31.0, "{tokens}");
        // A stale report that says more is left does not raise the level; a
        // lower one does.
        pace.learn(
            &headers(&[
                ("x-ratelimit-limit-tokens", "600"),
                ("x-ratelimit-remaining-tokens", "500"),
            ]),
            crate::codec::Family::Responses,
            [0; DIMS],
            0,
        );
        assert!(pace.snapshot().3 < 31.0, "{}", pace.snapshot().3);
        pace.learn(
            &headers(&[
                ("x-ratelimit-limit-tokens", "600"),
                ("x-ratelimit-remaining-tokens", "10"),
            ]),
            crate::codec::Family::Responses,
            [0; DIMS],
            0,
        );
        assert!(pace.snapshot().3 <= 10.0, "{}", pace.snapshot().3);
        // A refusal blocks the pool until the named time, whatever the buckets say.
        pace.limited(Some(Duration::from_secs(3)));
        let mut report = Report::default();
        assert_eq!(
            pace.acquire_reported(
                Cost {
                    input: 1,
                    output: 0
                },
                &mut report
            )
            .await
            .err()
            .unwrap()
            .code,
            "provider_paced"
        );
        assert_eq!(report.park_for, Some(Duration::from_secs(3)));
        tokio::time::sleep(Duration::from_secs(3)).await;
        // A spent allowance is a level to refill from, not a block: 60 per
        // minute means the next request waits one second, not until a reset.
        pace.learn(
            &headers(&[
                ("anthropic-ratelimit-requests-limit", "60"),
                ("anthropic-ratelimit-requests-remaining", "0"),
                ("anthropic-ratelimit-requests-reset", "2999-01-01T00:00:00Z"),
            ]),
            crate::codec::Family::Anthropic,
            [0; DIMS],
            0,
        );
        assert!(!pace.snapshot().4);
        let waited = sent(&pace, 1).await.unwrap().waited;
        assert!(
            waited >= Duration::from_millis(900) && waited < Duration::from_millis(1100),
            "{waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn headers_release_estimates_without_raising_stale_balances() {
        let pace = Pace::default();
        let token_headers = |remaining: &str| {
            headers(&[
                ("x-ratelimit-limit-tokens", "1000"),
                ("x-ratelimit-remaining-tokens", remaining),
            ])
        };
        let family = crate::codec::Family::Responses;
        // Large estimates, small bills: each fresh balance remains spendable.
        for remaining in ["990", "980", "970"] {
            let mut call = sent(&pace, 800).await.unwrap();
            assert!(call.waited.is_zero());
            call.learn(&token_headers(remaining), family);
            call.settle(10);
            assert_eq!(pace.snapshot().3, remaining.parse::<f64>().unwrap());
        }
        // The later request responds first. The older high header cannot
        // replenish tokens already consumed by the newer response.
        let mut older = sent(&pace, 100).await.unwrap();
        let mut newer = sent(&pace, 100).await.unwrap();
        newer.learn(&token_headers("770"), family);
        older.learn(&token_headers("870"), family);
        assert_eq!(pace.snapshot().3, 770.0);
        newer.settle(100);
        older.settle(100);
        assert_eq!(pace.snapshot().3, 770.0);
        // Request-only headers must not suppress correction of token estimates.
        let mut call = sent(&pace, 700).await.unwrap();
        call.learn(
            &headers(&[
                ("x-ratelimit-limit-requests", "1000"),
                ("x-ratelimit-remaining-requests", "999"),
            ]),
            family,
        );
        call.settle(10);
        assert_eq!(pace.snapshot().3, 760.0);
    }

    #[tokio::test(start_paused = true)]
    async fn seeded_headers_leave_the_call_in_flight_to_its_usage() {
        let pace = Pace::default();
        let family = crate::codec::Family::Responses;
        let call = sent(&pace, 100).await.unwrap();
        // An upgrade response reports the balance before this call was sent.
        pace.seed(
            &headers(&[
                ("x-ratelimit-limit-requests", "10"),
                ("x-ratelimit-remaining-requests", "10"),
                ("x-ratelimit-limit-tokens", "1000"),
                ("x-ratelimit-remaining-tokens", "1000"),
            ]),
            family,
        );
        assert_eq!(pace.snapshot().1, 9.0);
        assert_eq!(pace.snapshot().3, 900.0);
        call.settle(300);
        assert_eq!(pace.snapshot().1, 9.0);
        assert_eq!(pace.snapshot().3, 700.0);
    }

    #[tokio::test(start_paused = true)]
    async fn headers_wake_waiters_before_completion_without_double_refunding() {
        let pace = Pace::default();
        let family = crate::codec::Family::Responses;
        pace.learn(
            &headers(&[
                ("x-ratelimit-limit-tokens", "1000"),
                ("x-ratelimit-remaining-tokens", "1000"),
            ]),
            family,
            [0; DIMS],
            0,
        );
        let mut first = sent(&pace, 800).await.unwrap();
        // The follower initially needs 36 seconds of refill. Token headers
        // must wake it without waiting for the first stream to finish.
        let (follower, ()) = tokio::join!(sent(&pace, 800), async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            first.learn(
                &headers(&[
                    ("x-ratelimit-limit-tokens", "1000"),
                    ("x-ratelimit-remaining-tokens", "990"),
                ]),
                family,
            );
        });
        let follower = follower.unwrap();
        assert!(follower.waited < Duration::from_millis(20));
        assert_eq!(pace.snapshot().3, 190.0);
        assert_eq!(pace.state.lock().unwrap().tokens[TOTAL].reserved, 800);
        // Cancelling the accounted stream cannot credit its estimate again.
        drop(first);
        assert_eq!(pace.snapshot().3, 190.0);
        follower.settle(10);
        assert_eq!(pace.state.lock().unwrap().tokens[TOTAL].reserved, 0);
        assert_eq!(pace.snapshot().3, 980.0);
        // Cancelling an unreported request retains its estimated charge.
        let unreported = sent(&pace, 800).await.unwrap();
        drop(unreported);
        assert_eq!(pace.state.lock().unwrap().tokens[TOTAL].reserved, 0);
        assert_eq!(pace.snapshot().3, 180.0);
    }

    #[test]
    fn a_parked_call_is_admitted_at_the_same_cost_however_many_are_parked() {
        // `parked` calls booked behind a known allowance, each due now and
        // each spending its booking as it comes back.
        let drain = |parked: u64| {
            let now = Instant::now();
            let mut state = State::default();
            state.requests.per_minute = Some(1e9);
            for dim in state.tokens.iter_mut() {
                dim.bucket.per_minute = Some(1e12);
            }
            state.refill(now);
            for _ in 0..parked {
                state.booked.book(now, [10, 5, 5]);
            }
            let started = std::time::Instant::now();
            for _ in 0..parked {
                state.refill(now);
                let due = state.booked.due_by(now + MIN_PARK);
                assert!(due.is_some());
                let wait = state.admit_or_book([10, 5, 5], due, now, &mut None);
                assert_eq!(wait.unwrap(), Duration::ZERO);
            }
            let elapsed = started.elapsed();
            assert!(state.booked.is_empty());
            assert_eq!(state.booked.held, [0; DIMS]);
            elapsed
        };
        let n = 8_000;
        let grew = crate::doubling::outgrows(|twice| drain(if twice { 2 * n } else { n }));
        assert_eq!(grew, None, "{n} parked calls, then twice as many");
    }

    #[test]
    fn bookings_keep_their_totals_and_go_soonest_due_first() {
        let now = Instant::now();
        let mut booked = Booked::default();
        let later = now + Duration::from_secs(2);
        booked.book(later, [10, 4, 6]);
        booked.book(now, [20, 20, 0]);
        booked.book(later, [1, 1, 0]);
        assert_eq!(booked.held, [31, 25, 6]);
        // The soonest goes first; among those due together, the first made.
        let soonest = booked.due_by(now).unwrap();
        assert_eq!(booked.costs(soonest), [20, 20, 0]);
        booked.hold(soonest, [5, 5, 0]);
        assert_eq!(booked.held, [16, 10, 6]);
        booked.defer(soonest, now + Duration::from_secs(3));
        assert_eq!(booked.due_by(now), None);
        let next = booked.due_by(later).unwrap();
        assert_eq!(booked.costs(next), [10, 4, 6]);
        // Grace runs out for the two due at `later` and not yet the third.
        let lapsed: Vec<_> = booked.lapsed(later + BOOKING_GRACE).collect();
        assert_eq!(lapsed, [[10, 4, 6], [1, 1, 0]]);
        assert_eq!(booked.held, [5, 5, 0]);
        assert_eq!(booked.len(), 1);
    }
}
