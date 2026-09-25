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
        let pace = Arc::new(Pace::default());
        pools.insert(model.to_owned(), pace.clone());
        pace
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
}
impl Reservation<'_> {
    /// Call immediately before polling the HTTP send future.
    pub fn dispatch(&mut self) {
        debug_assert!(!self.dispatched);
        self.dispatched = true;
        self.actual = self.estimated;
    }
    pub fn learn(&mut self, headers: &reqwest::header::HeaderMap, family: super::Family) {
        debug_assert!(self.dispatched && !self.reported[TOTAL]);
        self.reported = self.pace.learn(headers, family, self.estimated, 1);
        self.request_reported = true;
    }
    /// Final accounting by total tokens; the split is taken as input.
    pub fn settle(self, actual: u64) {
        self.settle_cost(Cost {
            input: actual,
            output: 0,
        });
    }
    /// Final accounting by what the provider reported billing.
    pub fn settle_usage(self, usage: Option<&Usage>, estimate: Cost) {
        self.settle_cost(usage.map_or(estimate, |u| Cost {
            input: u.input_tokens,
            output: u.output_tokens,
        }));
    }
    fn settle_cost(mut self, actual: Cost) {
        debug_assert!(self.dispatched);
        self.actual = actual.dims();
    }
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut state = self.pace.state.lock().unwrap();
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
        self.acquire_cost(cost, &mut report.park_for).await
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
        )
        .await
    }
    pub async fn acquire_cost<'a>(
        &'a self,
        cost: Cost,
        park_for: &mut Option<Duration>,
    ) -> crate::Result<Reservation<'a>> {
        let started = Instant::now();
        let costs = cost.dims();
        let _turn = self.gate.lock().await;
        loop {
            let changed = self.changed.notified();
            let wait = {
                let mut state = self.state.lock().unwrap();
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
                            return Err(crate::Error::new("provider_paced"));
                        }
                        left
                    }
                    _ => {
                        state.blocked_until = None;
                        state.requests.refill(now);
                        let mut wait = state.requests.shortfall(1.0);
                        for (dim, cost) in state.tokens.iter_mut().zip(costs) {
                            dim.bucket.refill(now);
                            wait = wait.max(dim.bucket.shortfall(cost as f64));
                        }
                        if wait.is_zero() {
                            state.requests.take(1.0);
                            for (dim, cost) in state.tokens.iter_mut().zip(costs) {
                                dim.bucket.take(cost as f64);
                                dim.reserved += cost;
                            }
                            state.reserved_requests += 1;
                            return Ok(Reservation {
                                pace: self,
                                estimated: costs,
                                actual: [0; DIMS],
                                reported: [false; DIMS],
                                dispatched: false,
                                request_reported: false,
                                waited: started.elapsed(),
                            });
                        }
                        wait
                    }
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(wait.min(Duration::from_secs(60))) => {},
                _ = changed => {},
            }
        }
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
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let delay = after.unwrap_or_else(|| {
            if state.unnamed_blocks == 0 || state.blocked_until.is_none_or(|until| until <= now) {
                state.unnamed_blocks = (state.unnamed_blocks + 1).min(6);
            }
            Duration::from_secs(1 << (state.unnamed_blocks - 1))
        });
        let until = now + delay;
        state.blocked_until = Some(state.blocked_until.map_or(until, |u| u.max(until)));
        drop(state);
        // Wake the FIFO head even when it was waiting for allowance refill.
        // It parks and releases the gate; queued callers then observe the block.
        self.changed.notify_one();
    }
    /// A call completed: the next unnamed refusal starts again at a second.
    pub fn accepted(&self) {
        let mut state = self.state.lock().unwrap();
        state.unnamed_blocks = 0;
    }
    /// Learn the allowance from headers that precede every call they could
    /// count, such as a WebSocket upgrade's. Outstanding reservations stay
    /// reserved and are settled by their own usage.
    pub fn seed(&self, headers: &reqwest::header::HeaderMap, family: super::Family) {
        self.learn(headers, family, [0; DIMS], 0);
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
        family: super::Family,
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
        let (requests, tokens): (_, [Option<(f64, f64)>; DIMS]) = match family {
            super::Family::Responses => (
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
            super::Family::Anthropic => (
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
            // including calls acquired before the limit became known.
            let reserved = state.reserved_requests as f64;
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
            // Header balances exclude reservations, our bucket includes them.
            // Put both on the same basis before taking the conservative minimum.
            // Every reservation is returned exactly once, even on cancellation.
            let dim = &mut state.tokens[index];
            dim.bucket
                .learn(limit, remaining - dim.reserved as f64, now);
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
        state.requests.refill(now);
        for dim in state.tokens.iter_mut() {
            dim.bucket.refill(now);
        }
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
    any.then(|| Duration::from_secs_f64(total.max(0.0)))
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

/// Seconds until an RFC 3339 instant such as `2026-09-16T23:30:00Z`, the
/// form Anthropic's reset headers use; kept for reporting, not for blocking.
#[allow(dead_code)]
fn until_rfc3339(text: &str) -> Option<Duration> {
    let text = text.trim();
    let (date, time) = text.split_once('T')?;
    let mut parts = date.split('-');
    let (year, month, day): (i64, i64, i64) = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    let time = time.trim_end_matches('Z');
    let time = time.split(['+', '-']).next()?;
    let mut clock = time.split(':');
    let (hour, minute): (i64, i64) = (clock.next()?.parse().ok()?, clock.next()?.parse().ok()?);
    let second: f64 = clock.next()?.parse().ok()?;
    // Days from civil, Howard Hinnant's algorithm.
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let target = days as f64 * 86_400.0 + hour as f64 * 3600.0 + minute as f64 * 60.0 + second;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    Some(Duration::from_secs_f64((target - now).max(0.0)))
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
        let mut call = pace.acquire(tokens, &mut None).await?;
        call.dispatch();
        Ok(call)
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
        let unsent = pace.acquire(0, &mut None).await.unwrap();
        let mut reporting = sent(&pace, 0).await.unwrap();
        // Another client spent the rest of the provider's allowance.
        reporting.learn(&request_headers("0"), family);
        drop(reporting);
        drop(unsent);
        assert_eq!(pace.snapshot().1, 0.0);
        let next = pace.acquire(0, &mut None).await.unwrap();
        assert!(next.waited >= Duration::from_secs(1));
        drop(next);
        // No headers: a dispatched call still consumes one request.
        let mut no_headers = sent(&pace, 0).await.unwrap();
        no_headers.learn(&HeaderMap::new(), family);
        drop(no_headers);
        assert!(pace.acquire(0, &mut None).await.unwrap().waited >= Duration::from_millis(990));
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
        assert!(until_rfc3339("2000-01-01T00:00:00Z").unwrap().is_zero());
        assert!(until_rfc3339("2999-01-01T00:00:00Z").unwrap() > Duration::from_secs(1 << 30));
        assert!(until_rfc3339("not a time").is_none());
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
}
