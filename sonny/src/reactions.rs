//! The person's reactions to a bot's replies. Each is attached to the
//! bot's next model call, so it starts no turn, and kept here by the
//! reply's node, so a replay draws it under that reply and picking it
//! again takes it off. A reaction to the bot's last reply is the emoji
//! alone; one to an older reply quotes that reply's first line, so the
//! bot can tell which it answers.
//!
//! A bot reacts too, with the `react` tool Sonny declares on it: the
//! daemon records the call and Sonny draws it under the person's message.
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What a bot's replies were reacted with, by node, in one file per bot.
#[derive(Default)]
pub struct Reactions {
    file: Option<PathBuf>,
    on: BTreeMap<i64, Vec<String>>,
}

impl Reactions {
    /// The reactions kept under `key`, the bot's [`crate::asks::key`];
    /// none kept, or no key, is none.
    pub fn load(key: Option<&str>) -> Self {
        let file = key.and_then(|key| {
            let home = std::env::var_os("HOME")?;
            Some(PathBuf::from(home).join(format!(".agent/sonny/reactions/{key}.json")))
        });
        let on = file
            .as_ref()
            .and_then(|file| std::fs::read(file).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Reactions { file, on }
    }

    pub fn on(&self, node: i64) -> &[String] {
        self.on.get(&node).map_or(&[], Vec::as_slice)
    }

    /// Adds `emoji` to the reply at `node`, or takes it off when it was
    /// there, and says whether it is on now.
    pub fn toggle(&mut self, node: i64, emoji: &str) -> Result<bool, String> {
        let on = self.on.entry(node).or_default();
        let added = match on.iter().position(|e| e == emoji) {
            Some(at) => {
                on.remove(at);
                false
            }
            None => {
                on.push(emoji.to_owned());
                true
            }
        };
        if on.is_empty() {
            self.on.remove(&node);
        }
        self.save()?;
        Ok(added)
    }

    fn save(&self) -> Result<(), String> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let failed = |e: std::io::Error| format!("{}: {e}", file.display());
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(failed)?;
        }
        let json = serde_json::to_vec(&self.on).map_err(|e| e.to_string())?;
        let next = file.with_extension("json.next");
        std::fs::write(&next, json).map_err(failed)?;
        std::fs::rename(&next, file).map_err(failed)
    }
}

/// What the bot is told: the emoji, after the first line of the reply it
/// answers when that is not the bot's last.
pub fn message(emoji: &str, older: Option<&str>) -> String {
    match older {
        Some(first) => format!("> {first}\n\n{emoji}"),
        None => emoji.to_owned(),
    }
}

/// Whether a message of the person's is a reaction [`message`] made,
/// which the reply's own row shows instead.
pub fn is_reaction(text: &str) -> bool {
    let reaction = match text.strip_prefix("> ") {
        Some(quoted) => match quoted.split_once("\n\n") {
            Some((first, rest)) if !first.contains('\n') => rest,
            _ => return false,
        },
        None => text,
    };
    crate::transcript::reaction(reaction).is_some()
}

/// The name of the tool a bot reacts with.
pub const TOOL: &str = "react";

/// The `react` tool as Sonny declares it on a bot.
pub fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": "React with one emoji to the person's newest message, as a colleague would in a chat: \
            👍 to acknowledge, 👀 while you look into it, 🎉 for good news. React rarely. A reaction never \
            replaces an answer: when the person asked something, answer in words. When a reaction is all \
            their message needs, react and end your turn without words. `remove` takes off a reaction you \
            added, such as 👀 once you have answered. React only to the person, never to another agent's \
            messages.",
        "parameters": {
            "type": "object",
            "properties": {
                "emoji": {"type": "string", "description": "One emoji, such as 👍."},
                "remove": {"type": "boolean", "description": "Take this reaction off instead."}
            },
            "required": ["emoji"],
            "additionalProperties": false
        },
        "result": "Done."
    })
}

/// The tools the bot `record` describes should declare so it can react:
/// its own with Sonny's `react`, or `None` when it has that already.
pub fn declare(record: &Value) -> Option<Value> {
    let mut declared = record["declared"].as_array().cloned().unwrap_or_default();
    let tool = tool();
    if declared.contains(&tool) {
        return None;
    }
    declared.retain(|other| other["name"] != TOOL);
    declared.push(tool);
    // Within what the daemon takes, or left as it is: the bot's own
    // tools come first.
    let bytes = serde_json::to_vec(&declared).map_or(usize::MAX, |json| json.len());
    (declared.len() <= MAX_DECLARED && bytes <= DECLARED_BYTES).then_some(Value::Array(declared))
}

/// The most tools a bot may have declared, and the most bytes they take,
/// as the daemon counts them.
const MAX_DECLARED: usize = 16;
const DECLARED_BYTES: usize = 64 * 1024;

/// What a `react` call asks: an emoji, and whether it comes off.
pub fn call(arguments: &Value) -> Option<(String, bool)> {
    let emoji = arguments["emoji"].as_str().map(str::trim)?;
    (!emoji.is_empty()).then(|| (emoji.to_owned(), arguments["remove"] == true))
}

/// Whether a call's kept result is the daemon refusing it.
pub fn refused(output: &Value) -> bool {
    output["output"]
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .is_some_and(|result| result.get("error").is_some())
}

/// `on` after a call: the emoji added, or taken off.
pub fn apply(on: &mut Vec<String>, emoji: &str, remove: bool) {
    on.retain(|e| e != emoji);
    if !remove {
        on.push(emoji.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reaction_is_kept_by_its_reply_and_picking_it_again_takes_it_off() {
        let dir = std::env::temp_dir().join(format!("sonny-reactions-{}", std::process::id()));
        let file = dir.join("b.json");
        let mut kept = Reactions {
            file: Some(file.clone()),
            on: BTreeMap::new(),
        };
        assert_eq!(kept.toggle(7, "👍"), Ok(true));
        assert_eq!(kept.toggle(7, "🎉"), Ok(true));
        assert_eq!(kept.toggle(9, "👀"), Ok(true));
        assert_eq!(kept.toggle(9, "👀"), Ok(false));
        let read: BTreeMap<i64, Vec<String>> =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(read, BTreeMap::from([(7, vec!["👍".into(), "🎉".into()])]));
        assert!(kept.on(9).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_bot_is_told_which_reply_unless_it_is_the_last() {
        assert_eq!(message("👍", None), "👍");
        let older = message("🎉", Some("Merged it."));
        assert_eq!(older, "> Merged it.\n\n🎉");
        assert!(is_reaction("👍") && is_reaction(&older));
        for text in ["> Merged it.\n\nnice", "> a\nb\n\n👍", "ok 👍"] {
            assert!(!is_reaction(text), "{text:?}");
        }
    }

    #[test]
    fn a_bot_is_given_react_beside_what_it_declares_once() {
        let other = json!({"name": "plan", "description": "d", "parameters": {"type": "object"},
            "result": "ok"});
        let wanted = declare(&json!({"declared": [other]})).unwrap();
        assert_eq!(wanted, json!([other, tool()]));
        assert_eq!(declare(&json!({"declared": wanted})), None);
        let old = json!({"name": "react", "description": "old", "parameters": {"type": "object"},
            "result": "ok"});
        assert_eq!(declare(&json!({"declared": [old]})), Some(json!([tool()])));
        // A bot whose own tools leave no room keeps them, without react.
        let full: Vec<Value> = (0..16)
            .map(|n| {
                json!({"name": format!("t{n}"), "description": "d",
                "parameters": {"type": "object"}, "result": "ok"})
            })
            .collect();
        assert_eq!(declare(&json!({"declared": full})), None);
        let big = json!({"name": "big", "description": "d".repeat(64 * 1024),
            "parameters": {"type": "object"}, "result": "ok"});
        assert_eq!(declare(&json!({"declared": [big]})), None);
    }

    #[test]
    fn a_refused_call_is_told_from_a_taken_one() {
        assert!(!refused(&json!({"output": "Done."})));
        assert!(refused(
            &json!({"output": "{\"error\":\"invalid_arguments\"}"})
        ));
    }
}
