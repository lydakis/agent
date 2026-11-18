//! The write lane: a client's write that only records what it says (an
//! answer to a call or a gate, a call taken, a step put to its gates or
//! recorded) goes to the storage worker from its own task, and the run
//! loop answers it once its commit lands. As the observation lane does for
//! a read, its session is parked meanwhile, so the session's next request
//! never overtakes its write, while every other session goes on: a slow
//! commit holds the writer, not the loop. Writes from different sessions
//! meet in the worker's queue, which commits them in the order they reach
//! it; none was answered before its commit, so no one was told an order.
use std::collections::HashMap;
use tokio::task::{Id, JoinSet};

/// Writes on their way to the storage worker: each one's task, and what
/// the run loop keeps to answer it (`T`) once the task gives what the
/// write returned (`D`).
pub struct Writes<T, D> {
    running: JoinSet<D>,
    waiting: HashMap<Id, (u64, T)>,
    /// Sessions given a write since the run loop last looked: it parks them.
    parked: Vec<u64>,
}

impl<T, D: Send + 'static> Default for Writes<T, D> {
    fn default() -> Self {
        Writes {
            running: JoinSet::new(),
            waiting: HashMap::new(),
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
        let task = self.running.spawn(write).id();
        self.waiting.insert(task, (session, then));
    }

    /// The sessions given a write since the last call.
    pub fn parked(&mut self) -> std::vec::Drain<'_, u64> {
        self.parked.drain(..)
    }

    /// No write on its way.
    pub fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    /// The next write whose task ended: its session, what was kept for
    /// it, and what it returned, or `None` when its task failed.
    /// Cancel-safe.
    pub async fn next(&mut self) -> Option<(u64, T, Option<D>)> {
        let joined = self.running.join_next_with_id().await?;
        let (task, done) = match joined {
            Ok((task, done)) => (task, Some(done)),
            Err(error) => (error.id(), None),
        };
        let (session, then) = self.waiting.remove(&task).expect("every write is kept");
        Some((session, then, done))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn each_write_is_given_back_with_its_session_as_it_lands() {
        let mut writes = Writes::<&str, u32>::default();
        let (held, gate) = oneshot::channel::<()>();
        writes.write(1, "first", async move {
            let _ = gate.await;
            1
        });
        writes.write(2, "second", async { 2 });
        assert_eq!(writes.parked().collect::<Vec<_>>(), [1, 2]);
        // The second lands while the first is held: it is not kept behind it.
        let (session, then, done) = writes.next().await.unwrap();
        assert_eq!((session, then, done), (2, "second", Some(2)));
        let early = tokio::time::timeout(Duration::from_millis(20), writes.next()).await;
        assert!(early.is_err(), "a held write was given back");
        held.send(()).unwrap();
        let (session, then, done) = writes.next().await.unwrap();
        assert_eq!((session, then, done), (1, "first", Some(1)));
        assert!(writes.is_empty());
        assert!(writes.next().await.is_none());
    }

    #[tokio::test]
    async fn a_write_whose_task_failed_is_still_given_back() {
        let mut writes = Writes::<u64, u32>::default();
        writes.write(7, 70, async { panic!("a broken write") });
        assert_eq!(writes.next().await.unwrap(), (7, 70, None));
    }
}
