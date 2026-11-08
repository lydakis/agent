//! `/context`: what fills an agent's context, read-only, as Claude Code's
//! `/context` shows it. Built from what the window already has: the
//! context the last model round read and the model's window (each `usage`
//! event), the agent's record (`inspect`: its instructions and tools), and
//! its last compaction (the `compacted` event's room, #718). No field of
//! the daemon's is added for it.
//!
//! The daemon counts the conversation in tokens, its parts in bytes: a
//! part's tokens here are its bytes over [`BYTES_PER_TOKEN`], an estimate
//! said as one (`≈`); the conversation is what the measured total leaves.
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use serde_json::Value;

/// About how many bytes a token takes in English and code, as tokenizers
/// for these models average it.
const BYTES_PER_TOKEN: u64 = 4;

/// What `/context` reads for one agent.
#[derive(Default)]
pub struct Parts {
    /// The tokens its last model round read and wrote: what the next reads.
    pub used: Option<u64>,
    /// The model's context window, in tokens, when the daemon knew it.
    pub window: Option<u64>,
    /// Its instructions' bytes.
    pub instructions: u64,
    /// The tools it declares (Sonny's and MCP's): how many, their bytes.
    pub declared: (usize, u64),
    /// The daemon's own tools it may call, by name; their schemas are the
    /// daemon's, so only counted.
    pub built_in: usize,
    /// Its last compaction's `compacted` data, when one was seen.
    pub compacted: Option<Value>,
}

impl Parts {
    /// The agent's record as `inspect` answers it.
    pub fn from_record(record: &Value) -> Self {
        let declared = record["declared"].as_array().map_or(0, Vec::len);
        let declared_bytes = record["declared"].to_string().len() as u64;
        Parts {
            instructions: record["instructions"].as_str().map_or(0, str::len) as u64,
            declared: (declared, if declared == 0 { 0 } else { declared_bytes }),
            built_in: record["tools"].as_array().map_or(0, Vec::len),
            ..Parts::default()
        }
    }
}

fn tokens(bytes: u64) -> u64 {
    bytes.div_ceil(BYTES_PER_TOKEN)
}

/// Thousands apart, as `/usage` writes counts.
fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (at, c) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn kb(bytes: u64) -> String {
    match bytes {
        0..1_024 => format!("{bytes} B"),
        _ => format!("{:.1} KB", bytes as f64 / 1024.0),
    }
}

/// The card: the total against the window, then each part, its tokens
/// in a column, and what it is.
pub fn card(name: &str, parts: &Parts) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::raw(" "),
        Span::raw(name.to_owned()).bold(),
    ])];
    let summary = (parts.compacted.as_ref()).and_then(|data| data["summary_bytes"].as_u64());
    let fixed = tokens(parts.instructions) + tokens(parts.declared.1) + summary.map_or(0, tokens);
    match (parts.used, parts.window) {
        (Some(used), Some(window)) if window > 0 => lines.push(Line::from(format!(
            " {} of {} tokens used ({}%), {} free",
            count(used),
            count(window),
            used * 100 / window,
            count(window.saturating_sub(used))
        ))),
        (Some(used), _) => lines.push(Line::from(format!(
            " {} tokens used; the model's window is not known yet",
            count(used)
        ))),
        (None, _) => lines.push(
            Line::from(" No model call yet: the parts below are what its first one reads.").dim(),
        ),
    }
    lines.push(Line::default());
    let row = |label: &str, tokens: String, about: String| {
        Line::from(vec![
            Span::raw(format!(" {label:<13}")),
            Span::raw(format!("{tokens:>8}")),
            Span::raw(format!("  {about}")).dim(),
        ])
    };
    lines.push(row(
        "Instructions",
        format!("≈{}", count(tokens(parts.instructions))),
        kb(parts.instructions),
    ));
    let (declared, declared_bytes) = parts.declared;
    let built_in = match parts.built_in {
        0 => String::new(),
        n => format!(", {n} built in"),
    };
    lines.push(row(
        "Tools",
        format!("≈{}", count(tokens(declared_bytes))),
        format!("{declared} declared, {}{built_in}", kb(declared_bytes)),
    ));
    match (summary, parts.compacted.as_ref()) {
        (Some(bytes), Some(data)) => {
            let turns = &data["covered_turns"];
            let covered = match (turns[0].as_i64(), turns[1].as_i64()) {
                (Some(from), Some(to)) if from < to => format!("turns {from} to {to}"),
                (_, Some(to)) => format!("turn {to}"),
                _ => "earlier turns".to_owned(),
            };
            lines.push(row(
                "Summary",
                format!("≈{}", count(tokens(bytes))),
                format!("{covered}, {}", kb(bytes)),
            ));
        }
        _ => lines.push(row("Summary", "0".into(), "no compaction seen".into())),
    }
    if let Some(used) = parts.used {
        lines.push(row(
            "Conversation",
            format!("≈{}", count(used.saturating_sub(fixed))),
            "messages, calls and results".into(),
        ));
    }
    if let (Some(used), Some(window)) = (parts.used, parts.window) {
        lines.push(row(
            "Free",
            count(window.saturating_sub(used)),
            "before the window fills".into(),
        ));
    }
    if let Some(data) = &parts.compacted {
        lines.push(Line::default());
        let mut said = format!(
            " Last compaction freed {}",
            kb(data["reclaimed_bytes"].as_u64().unwrap_or(0))
        );
        if let Some(room) = data["room_bytes"].as_i64().filter(|room| *room > 0) {
            said.push_str(&format!(", and left {} before the next", kb(room as u64)));
        }
        said.push('.');
        lines.push(Line::from(said).dim());
    }
    lines.push(Line::default());
    lines.push(
        Line::from(format!(
            " ≈ at {BYTES_PER_TOKEN} bytes a token; the total is the model's count."
        ))
        .dim(),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect()
    }

    #[test]
    fn the_card_counts_each_part_and_leaves_the_conversation_the_rest() {
        let record = json!({"instructions": "x".repeat(8_000), "tools": ["shell", "apply_patch"],
            "declared": [{"name": "browser", "description": "y".repeat(3_000)}]});
        let mut parts = Parts::from_record(&record);
        parts.used = Some(50_000);
        parts.window = Some(200_000);
        parts.compacted = Some(json!({"summary_bytes": 4_000, "covered_turns": [1, 12],
            "reclaimed_bytes": 120_000, "room_bytes": 300_000}));
        let shown = text(&card("tally.lead", &parts));
        assert_eq!(
            shown[1],
            " 50,000 of 200,000 tokens used (25%), 150,000 free"
        );
        assert!(
            shown[3].starts_with(" Instructions") && shown[3].ends_with("≈2,000  7.8 KB"),
            "{shown:?}"
        );
        assert!(shown[4].contains("1 declared") && shown[4].ends_with("2 built in"));
        assert!(
            shown[5].contains("≈1,000  turns 1 to 12, 3.9 KB"),
            "{shown:?}"
        );
        // The rest of the measured total, after the parts' estimates.
        let tools = tokens(parts.declared.1);
        let rest = 50_000 - 2_000 - tools - 1_000;
        assert!(shown[6].contains(&format!("≈{}", count(rest))), "{shown:?}");
        assert!(shown[7].starts_with(" Free") && shown[7].contains("150,000"));
        assert!(shown.iter().any(
            |row| row == " Last compaction freed 117.2 KB, and left 293.0 KB before the next."
        ));
    }

    #[test]
    fn before_any_call_it_says_so_and_counts_what_the_first_reads() {
        let parts =
            Parts::from_record(&json!({"instructions": "abcd", "tools": [], "declared": []}));
        let shown = text(&card("calc.fix", &parts));
        assert!(shown[1].starts_with(" No model call yet"));
        assert!(
            shown
                .iter()
                .any(|row| row.starts_with(" Tools") && row.contains("0 declared, 0 B"))
        );
        assert!(shown.iter().any(|row| row.contains("no compaction seen")));
        assert!(!shown.iter().any(|row| row.starts_with(" Free")));
    }
}
