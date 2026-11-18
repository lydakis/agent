//! The write lane: a client's write that only records what it says (an
//! answer to a call or a gate, a call taken, a step put to its gates or
//! recorded) goes to the storage worker from its own task, and the run
//! loop answers it once its commit lands. As the observation lane does for
//! a read, its session is parked meanwhile, so the session's next request
//! never overtakes its write, while every other session goes on: a slow
//! commit holds the writer, not the loop. Writes are answered in the order
//! they were sent, whichever session sent them, as when each was awaited
//! in turn: a call taken or a step allowed is answered before an answer to
//! that call sent after it.
//!
//! A parked turn's claim, which `resume` makes, goes the same way
//! ([`Ordered`]), with no session to park: its turn starts once the claim
//! lands, in the order the claims were made.
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// Store jobs on their way, each in its own task, given back oldest first:
/// what the run loop keeps for each (`T`), and what its job returned (`D`).
pub struct Ordered<T, D> {
    running: VecDeque<(T, JoinHandle<(D, Instant)>)>,
    /// How long each landed job waited for the loop to take it up: behind
    /// an older job still on its way, or the loop's other work.
    pub behind: Behind,
}

/// Jobs given back, and how long they waited after landing.
#[derive(Default, Clone, Copy)]
pub struct Behind {
    pub count: u64,
    pub total: Duration,
    pub most: Duration,
}

impl<T, D> Default for Ordered<T, D> {
    fn default() -> Self {
        Ordered {
            running: VecDeque::new(),
            behind: Behind::default(),
        }
    }
}

impl<T, D> Drop for Ordered<T, D> {
    fn drop(&mut self) {
        for (_, task) in &self.running {
            task.abort();
        }
    }
}

impl<T, D: Send + 'static> Ordered<T, D> {
    /// Run `job` in its own task, keeping `then` for when it lands.
    pub fn push(&mut self, then: T, job: impl Future<Output = D> + Send + 'static) {
        let task = tokio::spawn(async move { (job.await, Instant::now()) });
        self.running.push_back((then, task));
    }

    /// No job on its way.
    pub fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    /// Jobs on their way.
    pub fn len(&self) -> usize {
        self.running.len()
    }

    /// What was kept for each job on its way, oldest first.
    pub fn pending(&self) -> impl Iterator<Item = &T> {
        self.running.iter().map(|(then, _)| then)
    }

    /// The oldest job if its task already ended, as `next` gives it,
    /// without waiting for one still on its way.
    pub async fn landed(&mut self) -> Option<(T, Option<D>)> {
        if !self.running.front()?.1.is_finished() {
            return None;
        }
        self.next().await
    }

    /// The oldest job, once its task ended: what was kept for it, and what
    /// it returned, or `None` when its task failed. Cancel-safe.
    pub async fn next(&mut self) -> Option<(T, Option<D>)> {
        let (_, task) = self.running.front_mut()?;
        let done = task.await.ok();
        let (then, _) = self.running.pop_front().expect("the job just awaited");
        Some((
            then,
            done.map(|(done, landed)| {
                let waited = landed.elapsed();
                self.behind.count += 1;
                self.behind.total += waited;
                self.behind.most = self.behind.most.max(waited);
                done
            }),
        ))
    }
}

impl Behind {
    /// For `stats`.
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({"landed": self.count,
            "behind_ms": self.total.as_millis() as u64,
            "behind_most_ms": self.most.as_millis() as u64})
    }
}

/// Clients' writes on their way: [`Ordered`], each with the session it
/// parks.
pub struct Writes<T, D> {
    pub ordered: Ordered<(u64, T), D>,
    /// Sessions given a write since the run loop last looked: it parks them.
    parked: Vec<u64>,
}

impl<T, D> Default for Writes<T, D> {
    fn default() -> Self {
        Writes {
            ordered: Ordered::default(),
            parked: Vec::new(),
        }
    }
}

impl<T, D: Send + 'static> Writes<T, D> {
    /// Run `write` for `session` in its own task, keeping `then` for its
    /// answer; the session is parked until `next` gives it back.
    pub fn write(
        &mut self,
        session: u64,
        then: T,
        write: impl Future<Output = D> + Send + 'static,
    ) {
        self.parked.push(session);
        self.ordered.push((session, then), write);
    }

    /// The sessions given a write since the last call.
    pub fn parked(&mut self) -> std::vec::Drain<'_, u64> {
        self.parked.drain(..)
    }

    /// No write on its way.
    pub fn is_empty(&self) -> bool {
        self.ordered.is_empty()
    }

    /// The oldest write, once its task ended: its session, what was kept
    /// for it, and what it returned, or `None` when its task failed.
    /// Cancel-safe.
    pub async fn next(&mut self) -> Option<(u64, T, Option<D>)> {
        let ((session, then), done) = self.ordered.next().await?;
        Some((session, then, done))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn writes_are_given_back_with_their_sessions_in_the_order_they_were_sent() {
        let mut writes = Writes::<&str, u32>::default();
        let (held, gate) = oneshot::channel::<()>();
        writes.write(1, "first", async move {
            let _ = gate.await;
            1
        });
        writes.write(2, "second", async { 2 });
        assert_eq!(writes.parked().collect::<Vec<_>>(), [1, 2]);
        // The second lands while the first is held: it waits behind it.
        tokio::task::yield_now().await;
        let early = tokio::time::timeout(Duration::from_millis(20), writes.next()).await;
        assert!(early.is_err(), "a later write overtook a held one");
        held.send(()).unwrap();
        assert_eq!(writes.next().await.unwrap(), (1, "first", Some(1)));
        assert_eq!(writes.next().await.unwrap(), (2, "second", Some(2)));
        assert!(writes.is_empty());
        assert!(writes.next().await.is_none());
        // The second waited behind the first for at least the 20 ms.
        let behind = writes.ordered.behind;
        assert_eq!(behind.count, 2);
        assert!(
            behind.most >= Duration::from_millis(20),
            "{:?}",
            behind.most
        );
    }

    #[tokio::test]
    async fn a_write_whose_task_failed_is_still_given_back() {
        let mut writes = Writes::<u64, u32>::default();
        writes.write(7, 70, async { panic!("a broken write") });
        assert_eq!(writes.next().await.unwrap(), (7, 70, None));
    }
}
