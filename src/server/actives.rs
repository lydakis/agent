//! The live turns, one per bot, and the count of active slots they hold.
//!
//! Every change to the map goes through [`Actives`], so the count stays
//! exact without walking the map: admission reads it on every pass of the
//! run loop, and a daemon may host thousands of bots.

use super::Active;
use std::{
    collections::HashMap,
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// One turn's claim on an active slot. It is released once, by whichever
/// comes first: the task submitting its end, or the turn leaving the map.
#[derive(Clone)]
pub(super) struct Slot {
    released: Arc<AtomicBool>,
    holding: Arc<AtomicUsize>,
}

impl Slot {
    /// Give the slot back. From then on the store, whose finishing commit
    /// also publishes `turn_finished`, says whether the bot is still busy,
    /// though the task is not reaped yet.
    pub(super) fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.holding.fetch_sub(1, Ordering::AcqRel);
        }
    }

    pub(super) fn held(&self) -> bool {
        !self.released.load(Ordering::Acquire)
    }
}

/// Per bot: the live turn. Reads see the map; changes go through the
/// methods below, which keep [`Actives::holding`] in step with it.
#[derive(Default)]
pub(super) struct Actives {
    map: HashMap<String, Active>,
    holding: Arc<AtomicUsize>,
}

impl Deref for Actives {
    type Target = HashMap<String, Active>;

    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl Actives {
    /// Start `bot`'s turn holding a new slot; a turn it replaces gives its
    /// slot back.
    pub(super) fn start(&mut self, bot: String, active: impl FnOnce(Slot) -> Active) -> &Active {
        self.holding.fetch_add(1, Ordering::AcqRel);
        let slot = Slot {
            released: Arc::default(),
            holding: self.holding.clone(),
        };
        let entry = self.map.entry(bot);
        let active = active(slot);
        match entry {
            std::collections::hash_map::Entry::Occupied(mut held) => {
                held.insert(active).slot.release();
                held.into_mut()
            }
            std::collections::hash_map::Entry::Vacant(free) => free.insert(active),
        }
    }

    pub(super) fn remove(&mut self, bot: &str) -> Option<Active> {
        let active = self.map.remove(bot)?;
        active.slot.release();
        Some(active)
    }

    /// Every live turn, leaving none.
    pub(super) fn drain(&mut self) -> impl Iterator<Item = Active> {
        let map = std::mem::take(&mut self.map);
        map.values().for_each(|active| active.slot.release());
        map.into_values()
    }

    /// The turns that hold a slot: one that has committed or is committing
    /// its end holds none, as its bot is no longer busy then
    /// ([`Active::running`]): a client that saw its `turn_finished` may
    /// start the next.
    pub(super) fn holding(&self) -> usize {
        self.holding.load(Ordering::Acquire)
    }
}

#[cfg(test)]
impl Slot {
    /// A slot of its own one-slot pool, for a turn outside any map.
    pub(super) fn alone() -> Self {
        Slot {
            released: Arc::default(),
            holding: Arc::new(AtomicUsize::new(1)),
        }
    }
}

#[cfg(test)]
impl Actives {
    pub(super) fn with(mut self, bot: String, active: impl FnOnce(Slot) -> Active) -> Self {
        self.start(bot, active);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(turn: i64) -> impl FnOnce(Slot) -> Active {
        move |slot| Active {
            turn,
            task: 0,
            cancel: tokio::sync::watch::channel(None).0,
            steers: Arc::default(),
            compact: Arc::default(),
            lift: Arc::default(),
            slot,
        }
    }

    #[test]
    fn each_turn_gives_its_slot_back_once_however_it_leaves() {
        let mut active = Actives::default();
        let a = active.start("A".into(), turn(1)).slot.clone();
        active.start("B".into(), turn(2));
        active.start("C".into(), turn(3));
        assert_eq!(active.holding(), 3);
        // A's task submits its end: its slot is free before it is reaped,
        // and reaping it frees nothing more.
        a.release();
        assert_eq!((active.holding(), active.len()), (2, 3));
        assert_eq!(active["A"].running(), None);
        active.remove("A");
        a.release();
        assert_eq!((active.holding(), active.len()), (2, 2));
        // A turn that replaces B's holds B's place, not a second slot.
        let b = active.start("B".into(), turn(4)).slot.clone();
        assert_eq!((active.holding(), active.len()), (2, 2));
        // C leaves before its task ends; the task's own release is a no-op.
        let c = active.remove("C").unwrap().slot;
        c.release();
        assert_eq!(active.holding(), 1);
        assert_eq!(active.drain().count(), 1);
        b.release();
        assert_eq!((active.holding(), active.len()), (0, 0));
    }
}
