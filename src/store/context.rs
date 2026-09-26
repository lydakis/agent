//! Encoded context accounting shared by request construction and compaction.
use super::{CompactionView, Window};
use crate::{Result, codec::Family};
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct ContextUsage {
    pub bytes: usize,
    pub items: usize,
}
impl ContextUsage {
    pub fn with_prefix(self, prefix: &ContextPrefix) -> Self {
        Self {
            bytes: self.bytes + self.items.saturating_sub(1) + prefix.bytes.len(),
            items: self.items + prefix.items,
        }
    }
    pub fn fits(self, limit: Self) -> bool {
        self.bytes <= limit.bytes && self.items <= limit.items
    }
}

pub struct ContextPrefix {
    /// Every prefix item includes its separating comma before the tail.
    pub bytes: bytes::Bytes,
    pub items: usize,
}

impl Window {
    pub fn prefix(&self, listed: &[(i64, String)], prefix_budget: usize) -> Result<ContextPrefix> {
        context_prefix(
            self.family,
            self.compaction.as_ref(),
            self.note.as_ref(),
            self.omitted_items,
            self.omitted_turns,
            self.history,
            listed,
            prefix_budget,
        )
    }
}

impl CompactionView {
    /// Retained prompts are a convenience copy; originals remain retrievable.
    /// Bound the encoded summary block, trimming the middle as the global
    /// prompt budget does, so oldest and newest excerpts survive when they fit.
    /// Count each prompt once instead of repeatedly encoding the entire prefix.
    pub(crate) fn bound_prompt_bytes(&mut self, family: Family, budget: usize) -> Result<()> {
        let mut prompts = std::mem::take(&mut self.prompts);
        let base = context_prefix(family, Some(self), None, 0, 0, false, &[], 0)?
            .bytes
            .len();
        if base > budget {
            return crate::fail_with(
                "compaction_context_limit",
                "the encoded summary exceeds the pinned context budget",
            );
        }
        let mut used =
            base + serde_json::to_vec("\n\nUser messages from those turns, verbatim:")?.len() - 2;
        let mut costs = Vec::with_capacity(prompts.len());
        for (ordinal, prompt) in &prompts {
            let size = serde_json::to_vec(&format!("\n{ordinal}: {prompt}"))?.len() - 2;
            used += size;
            costs.push(size);
        }
        let (mut left, mut right) = (prompts.len() / 2, prompts.len() / 2);
        while used > budget && right - left < prompts.len() {
            let middle = (prompts.len() - (right - left)) / 2;
            let removed = if middle < left {
                left -= 1;
                left
            } else {
                right += 1;
                right - 1
            };
            used -= costs[removed];
        }
        prompts.drain(left..right);
        self.prompts = prompts;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn context_prefix(
    family: Family,
    compaction: Option<&CompactionView>,
    note: Option<&(i64, String)>,
    omitted_items: i64,
    omitted_turns: i64,
    history: bool,
    listed: &[(i64, String)],
    prefix_budget: usize,
) -> Result<ContextPrefix> {
    let mut encoded = Vec::new();
    let mut count = 0;
    let mut push = |mut item: Vec<u8>| {
        item.push(b',');
        encoded.extend_from_slice(&item);
        count += 1;
    };
    if let Some(view) = compaction {
        let mut text = format!(
            "[compaction summary, version {}, covering turns {} to {}]\n{}",
            view.version, view.covered.0, view.covered.1, view.summary
        );
        if !view.prompts.is_empty() {
            text.push_str("\n\nUser messages from those turns, verbatim:");
            for (ordinal, prompt) in &view.prompts {
                text.push_str(&format!("\n{ordinal}: {prompt}"));
            }
        }
        push(pinned_item(family, &text)?);
    }
    if let Some((version, text)) = note.filter(|(_, text)| !text.is_empty()) {
        push(pinned_item(
            family,
            &format!("[carry-forward note, version {version}]\n{text}"),
        )?);
    }
    let pinned_bytes = encoded.len();
    let mut push = |mut item: Vec<u8>| {
        item.push(b',');
        encoded.extend_from_slice(&item);
        count += 1;
    };
    if omitted_items > 0 {
        // Bound the optional listing before trimming transcript turns. Its
        // allowance stays fixed while requests fit, preserving caching, and
        // shrinks only when the current turn needs the space.
        let encode = |listed: &[(i64, String)]| -> Result<Vec<u8>> {
            let mut text = format!(
                "[context note] {omitted_turns} earlier turn(s) with {omitted_items} messages are not shown."
            );
            if history {
                text.push_str(&format!(
                    " Use the history tool with a turn number from 1 to {omitted_turns} to read any of them."
                ));
            }
            if !listed.is_empty() {
                text.push_str(" How they began, newest first:");
                for (ordinal, opening) in listed {
                    text.push_str(&format!("\n{ordinal}: {opening}"));
                }
                if let Some((oldest, _)) = listed.last()
                    && *oldest > 1
                {
                    text.push_str(&format!(
                        "\nTurns 1 to {} are older than this list.",
                        oldest - 1
                    ));
                }
            }
            family.user_item(&text)
        };
        let mut item = encode(listed)?;
        if pinned_bytes + item.len() >= prefix_budget && !listed.is_empty() {
            // Probe the full listing first (its last entry can remove the
            // older-turns footer). Proper nonempty prefixes grow monotonically.
            // Binary search avoids quadratic re-encoding for large listings.
            let (mut low, mut high) = (0, listed.len());
            item = encode(&[])?;
            while low + 1 < high {
                let mid = low + (high - low) / 2;
                let candidate = encode(&listed[..mid])?;
                if pinned_bytes + candidate.len() < prefix_budget {
                    low = mid;
                    item = candidate;
                } else {
                    high = mid;
                }
            }
        }
        push(item);
    }
    Ok(ContextPrefix {
        bytes: encoded.into(),
        items: count,
    })
}

/// An Anthropic assistant item without its thinking blocks. Newer Claude
/// models bind each thinking block to the exact conversation before it and
/// reject a replayed block whose earlier history changed, so blocks written
/// before the request's leading context last changed are sent without it.
/// `None` when the item is not an assistant message with thinking; empty
/// when thinking is all it holds, since an empty message is invalid and the
/// item is then left out of the request. Deterministic: the
/// store records the bytes this removes when the item is written, so a
/// request knows its length before reading it.
pub fn without_thinking(item: &[u8]) -> Option<Vec<u8>> {
    #[derive(serde::Deserialize)]
    struct Message<'a> {
        role: &'a str,
        #[serde(borrow)]
        content: Vec<&'a serde_json::value::RawValue>,
    }
    #[derive(serde::Deserialize)]
    struct Block<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
    }
    // Most items carry no thinking; a substring search skips parsing them.
    if !std::str::from_utf8(item).is_ok_and(|text| text.contains("thinking\"")) {
        return None;
    }
    let message: Message<'_> = serde_json::from_slice(item).ok()?;
    if message.role != "assistant" {
        return None;
    }
    let kept: Vec<_> = message
        .content
        .iter()
        .filter(|block| {
            !serde_json::from_str::<Block<'_>>(block.get())
                .is_ok_and(|b| matches!(b.kind, "thinking" | "redacted_thinking"))
        })
        .collect();
    if kept.len() == message.content.len() {
        return None;
    }
    if kept.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::with_capacity(item.len());
    out.extend_from_slice(b"{\"content\":[");
    for (index, block) in kept.iter().enumerate() {
        if index != 0 {
            out.push(b',');
        }
        out.extend_from_slice(block.get().as_bytes());
    }
    out.extend_from_slice(b"],\"role\":\"assistant\"}");
    Some(out)
}

/// Bytes a request saves when it sends an item without thinking, recorded
/// when the item is stored. A left-out item also takes its comma separator.
pub fn thinking_bytes(item: &[u8]) -> usize {
    match without_thinking(item) {
        None => 0,
        Some(kept) if kept.is_empty() => item.len() + 1,
        Some(kept) => item.len() - kept.len(),
    }
}

/// Stable pinned blocks retain their own Anthropic cache breakpoints.
pub fn pinned_item(family: Family, text: &str) -> Result<Vec<u8>> {
    match family {
        Family::Anthropic => Ok(serde_json::to_vec(&json!({
            "role":"user","content":[{"type":"text","text":text,"cache_control":{"type":"ephemeral"}}]
        }))?),
        _ => family.user_item(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_context_note_names_the_history_tool_only_for_bots_that_have_it() {
        let listed = [(2, "Second task".to_owned())];
        let note = |history| {
            let prefix = context_prefix(
                Family::Responses,
                None,
                None,
                6,
                3,
                history,
                &listed,
                1 << 20,
            )
            .unwrap();
            String::from_utf8(prefix.bytes.to_vec()).unwrap()
        };
        let with = note(true);
        assert!(with.contains("3 earlier turn(s) with 6 messages are not shown."));
        assert!(with.contains("Use the history tool with a turn number from 1 to 3"));
        let without = note(false);
        assert!(without.contains("3 earlier turn(s) with 6 messages are not shown."));
        assert!(!without.contains("history"), "{without}");
        assert!(without.contains("How they began, newest first:\\n2: Second task"));
    }

    #[test]
    fn thinking_is_removed_only_from_assistant_items() {
        let item = br#"{"content":[{"type":"thinking","thinking":"plan","signature":"s"},{"type":"redacted_thinking","data":"x"},{"type":"tool_use","id":"t","name":"echo","input":{"text":"thinking\""}}],"role":"assistant"}"#;
        let kept = without_thinking(item).unwrap();
        assert_eq!(
            kept,
            br#"{"content":[{"type":"tool_use","id":"t","name":"echo","input":{"text":"thinking\""}}],"role":"assistant"}"#
        );
        assert_eq!(thinking_bytes(item), item.len() - kept.len());
        // Only thinking: the item is left out, with its separator.
        let alone = br#"{"content":[{"type":"thinking","thinking":"a","signature":"s"}],"role":"assistant"}"#;
        assert_eq!(without_thinking(alone), Some(Vec::new()));
        assert_eq!(thinking_bytes(alone), alone.len() + 1);
        // After a fallback only the answering model's thinking is stored,
        // after the marker; removing it leaves the marker between the same
        // blocks, and nothing on the declined side for the API to check.
        let fallback = br#"{"content":[{"type":"text","text":"a"},{"type":"fallback","from":{"model":"x"},"to":{"model":"y"}},{"type":"thinking","thinking":"t","signature":"s"},{"type":"text","text":"b"}],"role":"assistant"}"#;
        assert_eq!(
            without_thinking(fallback).unwrap(),
            br#"{"content":[{"type":"text","text":"a"},{"type":"fallback","from":{"model":"x"},"to":{"model":"y"}},{"type":"text","text":"b"}],"role":"assistant"}"#
        );
        // A user item that mentions thinking is left alone, as is one without it.
        let user = br#"{"content":[{"type":"text","text":"\"thinking\""}],"role":"user"}"#;
        assert_eq!(thinking_bytes(user), 0);
        assert_eq!(
            thinking_bytes(br#"{"content":[{"type":"text","text":"hi"}],"role":"assistant"}"#),
            0
        );
    }
}
