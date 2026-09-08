use crate::{Result, fail};
use bytes::Bytes;
use serde_json::json;
use std::sync::Arc;

pub const MAX_HISTORY_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ITEMS: usize = 4096;

/// Immutable, pre-encoded provider items. Forking clones one Arc, not the prefix.
#[derive(Clone, Default)]
pub struct History(Option<Arc<Node>>);
struct Node {
    parent: History,
    item: Bytes,
    bytes: usize,
    len: usize,
}

impl History {
    pub fn len(&self) -> usize {
        self.0.as_ref().map_or(0, |n| n.len)
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }
    pub fn bytes(&self) -> usize {
        self.0.as_ref().map_or(0, |n| n.bytes)
    }

    pub fn append(&mut self, item: Bytes) -> Result<()> {
        let bytes = self
            .bytes()
            .checked_add(item.len())
            .ok_or(crate::Error("history_limit".into()))?;
        let len = self.len() + 1;
        if bytes > MAX_HISTORY_BYTES || len > MAX_ITEMS {
            return fail("history_limit");
        }
        self.0 = Some(Arc::new(Node {
            parent: self.clone(),
            item,
            bytes,
            len,
        }));
        Ok(())
    }
    pub fn user(&mut self, text: &str) -> Result<()> {
        self.append(
            serde_json::to_vec(&json!({"role":"user","content":[
            {"type":"input_text","text":text}]}))?
            .into(),
        )
    }
    pub fn items(&self) -> Vec<Bytes> {
        let mut items = Vec::with_capacity(self.len());
        let mut next = &self.0;
        while let Some(node) = next {
            items.push(node.item.clone());
            next = &node.parent.0;
        }
        items.reverse();
        items
    }
}

// Avoid recursive destruction of long, uniquely owned prefixes.
impl Drop for History {
    fn drop(&mut self) {
        let mut next = self.0.take();
        while let Some(node) = next {
            match Arc::try_unwrap(node) {
                Ok(mut node) => next = node.parent.0.take(),
                Err(_) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_forks_share_bytes_but_append_independently() {
        let mut bob = History::default();
        bob.user("before").unwrap();
        let checkpoint = bob.clone();
        bob.user("original direction").unwrap();
        let mut branch = checkpoint.clone();
        branch.user("alternative direction").unwrap();
        assert_eq!(checkpoint.len(), 1);
        assert_eq!(bob.items()[0].as_ptr(), branch.items()[0].as_ptr());
        assert_ne!(bob.items()[1], branch.items()[1]);
    }
    #[test]
    fn history_limit_rejects_without_changing_the_checkpoint() {
        let mut history = History::default();
        history.user("kept").unwrap();
        assert!(
            history
                .append(vec![b'x'; MAX_HISTORY_BYTES].into())
                .is_err()
        );
        assert_eq!(history.len(), 1);
    }
}
