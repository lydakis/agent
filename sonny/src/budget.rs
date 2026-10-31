//! The processes a Sonny companion runs for agents at once: tool calls the
//! tool host answers in `sonny tool` processes, and goal checks. One budget
//! for all of them, as a systemd slice bounds a group of units, so a turn
//! that parks cheaply in the daemon cannot leave unbounded work running
//! here. Callers wait their turn in order (Tokio's semaphore is fair), and
//! no agent holds more than half of it, so one agent's burst leaves room
//! for the rest.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub struct Budget {
    all: Arc<Semaphore>,
    /// Each agent's share while it holds or waits for a place; gone once
    /// it holds none.
    shares: Mutex<HashMap<String, Arc<Semaphore>>>,
    share: usize,
}

/// A place in the budget, given back when dropped.
pub struct Permit {
    _all: OwnedSemaphorePermit,
    _share: OwnedSemaphorePermit,
    owner: String,
    budget: &'static Budget,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut shares = self.budget.shares.lock().unwrap_or_else(|e| e.into_inner());
        // The map holds the share once and this permit once (its place
        // comes back after this runs): nobody else holds or waits on it.
        if (shares.get(&self.owner)).is_some_and(|share| Arc::strong_count(share) <= 2) {
            shares.remove(&self.owner);
        }
    }
}

impl Budget {
    pub fn new(size: usize) -> Self {
        let size = size.max(1);
        Budget {
            all: Arc::new(Semaphore::new(size)),
            shares: Mutex::new(HashMap::new()),
            share: size.div_ceil(2),
        }
    }

    /// Wait for a place for `owner`: its share first, so an agent waiting
    /// on its own share holds none of the others'.
    pub async fn admit(&'static self, owner: &str) -> Permit {
        let share = {
            let mut shares = self.shares.lock().unwrap_or_else(|e| e.into_inner());
            (shares.entry(owner.to_owned()))
                .or_insert_with(|| Arc::new(Semaphore::new(self.share)))
                .clone()
        };
        let share = share.acquire_owned().await.expect("never closed");
        let all = self
            .all
            .clone()
            .acquire_owned()
            .await
            .expect("never closed");
        Permit {
            _all: all,
            _share: share,
            owner: owner.to_owned(),
            budget: self,
        }
    }
}

/// The companion's budget, sized by `tool_processes` in the config, or
/// twice the CPUs.
pub fn processes() -> &'static Budget {
    static BUDGET: OnceLock<Budget> = OnceLock::new();
    BUDGET.get_or_init(|| {
        let set = agent_client::config::load().map_or(0, |config| config.sonny.tool_processes);
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        Budget::new(match set {
            0 => 2 * cpus,
            set => usize::try_from(set).unwrap_or(usize::MAX),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn leaked(size: usize) -> &'static Budget {
        Box::leak(Box::new(Budget::new(size)))
    }

    #[tokio::test]
    async fn one_agent_takes_at_most_half_and_the_rest_get_in() {
        let budget = leaked(4);
        let a1 = budget.admit("a").await;
        let _a2 = budget.admit("a").await;
        // A third for `a` waits on its share, holding none of the others'.
        let third = tokio::spawn(async move { budget.admit("a").await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!third.is_finished());
        let _b1 = budget.admit("b").await;
        let _b2 = budget.admit("b").await;
        drop(a1);
        let _a3 = tokio::time::timeout(Duration::from_secs(1), third)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn an_agent_holding_none_leaves_no_share_behind() {
        let budget = leaked(2);
        drop(budget.admit("a").await);
        assert!(budget.shares.lock().unwrap().is_empty());
        let held = budget.admit("a").await;
        let second = budget.admit("a");
        drop(held);
        drop(second.await);
        assert!(budget.shares.lock().unwrap().is_empty());
    }
}
