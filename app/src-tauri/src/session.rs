//! A receiver borrowed by a pending pull still belongs to its original
//! attachment. Retain that ownership even while the slot itself is empty.
pub struct SessionSlot<T> {
    session: u64,
    value: Option<T>,
}
impl<T> Default for SessionSlot<T> {
    fn default() -> Self {
        Self {
            session: 0,
            value: None,
        }
    }
}
impl<T> SessionSlot<T> {
    pub fn replace(&mut self, session: u64, value: T) {
        self.session = session;
        self.value = Some(value);
    }
    pub fn take(&mut self, session: u64) -> Option<T> {
        if self.session == session {
            self.value.take()
        } else {
            None
        }
    }
    pub fn restore(&mut self, session: u64, value: T) {
        if self.session == session && self.value.is_none() {
            self.value = Some(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SessionSlot;

    #[test]
    fn stale_pull_cannot_replace_a_borrowed_new_session_receiver() {
        let mut slot = SessionSlot::default();
        slot.replace(1, "old events");
        let old = slot.take(1).unwrap();
        slot.replace(2, "new events");
        let new = slot.take(2).unwrap();
        slot.restore(1, old);
        slot.restore(2, new);
        assert!(slot.take(1).is_none());
        assert_eq!(slot.take(2), Some("new events"));
    }
}
