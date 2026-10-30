//! The person's reactions to a bot's replies. Each is attached to the
//! bot's next model call, so it starts no turn, and kept here by the
//! reply's node, so a replay draws it under that reply and picking it
//! again takes it off. A reaction to the bot's last reply is the emoji
//! alone; one to an older reply quotes that reply's first line, so the
//! bot can tell which it answers.
//!
//! A bot reacts too, with the `react` tool Sonny declares on it: the
//! daemon records the call and Sonny draws it under the person's message.
//! A lead reacts to the person's newest message in one of its tasks by
//! naming the task: the lookout keeps that reaction in the task's
//! [`Picked`] file, which the task's window draws.
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Whose reactions a file holds: the bot by its store-wide id, which a
/// later bot of the same name never takes, in the store by its canonical
/// path, so two stores' bots never share one.
pub fn key(store: &Path, bot_id: i64) -> String {
    // FNV-1a: the same on every run and every machine.
    let hash = store
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}-{bot_id}")
}

/// What a bot's replies were reacted with, by node, in one file per bot.
#[derive(Default)]
pub struct Reactions {
    file: Option<PathBuf>,
    on: BTreeMap<i64, Vec<String>>,
}

impl Reactions {
    /// The reactions kept under `key`, the bot's [`key`];
    /// none kept, or no key, is none.
    pub fn load(key: Option<&str>) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        match key.zip(home) {
            Some((key, home)) => {
                let file = file(&home, key);
                let on = read(&file).unwrap_or_default();
                Reactions {
                    file: Some(file),
                    on,
                }
            }
            None => Reactions::default(),
        }
    }

    /// The reactions a window kept under `key` in `home`, or none when no
    /// window kept a file there, as when the key is not the window's.
    pub fn kept(home: &Path, key: &str) -> Option<Self> {
        let file = file(home, key);
        let on = read(&file)?;
        Some(Reactions {
            file: Some(file),
            on,
        })
    }

    pub fn on(&self, node: i64) -> &[String] {
        self.on.get(&node).map_or(&[], Vec::as_slice)
    }

    /// Whether a reply older than `node` has `emoji` on it.
    pub fn before(&self, node: i64, emoji: &str) -> bool {
        (self.on.range(..node)).any(|(_, on)| on.iter().any(|e| e == emoji))
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

fn file(home: &Path, key: &str) -> PathBuf {
    home.join(format!(".agent/sonny/reactions/{key}.json"))
}

fn read(file: &Path) -> Option<BTreeMap<i64, Vec<String>>> {
    serde_json::from_slice(&std::fs::read(file).ok()?).ok()
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
    parse(text).is_some()
}

/// A reaction [`message`] made: the first line of the reply it answers
/// when that was not the bot's last, and the emoji.
pub fn parse(text: &str) -> Option<(Option<&str>, &str)> {
    let (first, reaction) = match text.strip_prefix("> ") {
        Some(quoted) => match quoted.split_once("\n\n") {
            Some((first, rest)) if !first.contains('\n') => (Some(first), rest),
            _ => return None,
        },
        None => (None, text),
    };
    crate::transcript::reaction(reaction)
        .is_some()
        .then_some((first, reaction))
}

/// The name of the tool a bot reacts with.
pub const TOOL: &str = "react";

/// The `react` tool as Sonny declares it on a bot.
pub fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": "React with one emoji to the person's newest message, as a colleague would in a chat, \
            such as to acknowledge it, to show you are looking into it, or for good news. A reaction never \
            replaces an answer: when the person asked something, answer in words. When a reaction is all \
            their message needs, react and end your turn without words. `remove` takes off a reaction you \
            added, such as one that said you were looking into it, once you have answered. React only to \
            the person, never to another agent's messages. With `bot`, it reacts to the person's newest \
            message in that agent instead, such as what they wrote in one of your tasks that you picked up.",
        "parameters": {
            "type": "object",
            "properties": {
                "emoji": {"type": "string", "description": "One emoji, such as 👍."},
                "remove": {"type": "boolean", "description": "Take this reaction off instead."},
                "bot": {"type": "string", "minLength": 1,
                    "description": "Another agent, to react to the person's newest message there."}
            },
            "required": ["emoji"],
            "additionalProperties": false
        },
        "result": "Done."
    })
}

/// What a `react` call asks: an emoji, and whether it comes off.
pub fn call(arguments: &Value) -> Option<(String, bool)> {
    let emoji = arguments["emoji"].as_str().map(str::trim)?;
    (!emoji.is_empty()).then(|| (emoji.to_owned(), arguments["remove"] == true))
}

/// The other agent a `react` call names, whose person's message it is for
/// rather than the caller's own.
pub fn target(arguments: &Value) -> Option<&str> {
    arguments["bot"]
        .as_str()
        .map(str::trim)
        .filter(|bot| !bot.is_empty())
}

/// What other agents reacted with to the person's messages in one bot,
/// by node, each with who reacted, in a file beside the bot's own
/// reactions: the lookout writes it, and the bot's window draws it.
#[derive(Default)]
pub struct Picked {
    file: Option<PathBuf>,
    on: BTreeMap<i64, Vec<(String, String)>>,
    read: Option<std::time::SystemTime>,
}

impl Picked {
    fn path(key: &str) -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(format!(".agent/sonny/reactions/{key}.by.json")))
    }

    /// The reactions kept for the bot under `key`, its [`key`].
    pub fn load(key: Option<&str>) -> Self {
        let mut picked = Picked {
            file: key.and_then(Self::path),
            ..Picked::default()
        };
        picked.refresh();
        picked
    }

    /// `by`'s reaction to the person's message at `node` kept, or taken
    /// off, as the lookout's write would.
    #[cfg(test)]
    pub fn set(&mut self, node: i64, by: &str, emoji: &str, remove: bool) {
        let at = self.on.entry(node).or_default();
        at.retain(|(b, e)| !(b == by && e == emoji));
        if !remove {
            at.push((by.to_owned(), emoji.to_owned()));
        }
    }

    /// Who reacted with what to the person's message at `node`.
    pub fn on(&self, node: i64) -> &[(String, String)] {
        self.on.get(&node).map_or(&[], Vec::as_slice)
    }

    /// Reads the file again when it changed since: the reactions it holds
    /// now that it did not, and those it no longer holds, by node, each
    /// with whether it was added.
    pub fn refresh(&mut self) -> Vec<(i64, String, String, bool)> {
        let Some(file) = &self.file else {
            return Vec::new();
        };
        let modified = std::fs::metadata(file).and_then(|m| m.modified()).ok();
        if modified.is_none() || modified == self.read {
            return Vec::new();
        }
        self.read = modified;
        let now: BTreeMap<i64, Vec<(String, String)>> = std::fs::read(file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let gone = |from: &BTreeMap<i64, Vec<(String, String)>>,
                    to: &BTreeMap<i64, Vec<(String, String)>>,
                    added: bool| {
            (from.iter())
                .flat_map(|(node, on)| on.iter().map(move |(by, emoji)| (*node, by, emoji)))
                .filter(|(node, by, emoji)| {
                    !(to.get(node).into_iter().flatten()).any(|(b, e)| b == *by && e == *emoji)
                })
                .map(|(node, by, emoji)| (node, by.clone(), emoji.clone(), added))
                .collect::<Vec<_>>()
        };
        let mut changed = gone(&now, &self.on, true);
        changed.extend(gone(&self.on, &now, false));
        self.on = now;
        changed
    }

    /// Keeps `by`'s reaction to the person's message at `node` in the bot
    /// under `key`, or takes it off.
    pub fn apply(key: &str, node: i64, by: &str, emoji: &str, remove: bool) -> Result<(), String> {
        let file = Self::path(key).ok_or("HOME is not set")?;
        Self::apply_in(&file, node, by, emoji, remove)
    }

    fn apply_in(file: &Path, node: i64, by: &str, emoji: &str, remove: bool) -> Result<(), String> {
        let failed = |e: std::io::Error| format!("{}: {e}", file.display());
        let mut on: BTreeMap<i64, Vec<(String, String)>> = std::fs::read(file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let at = on.entry(node).or_default();
        at.retain(|(b, e)| !(b == by && e == emoji));
        if !remove {
            at.push((by.to_owned(), emoji.to_owned()));
        }
        if at.is_empty() {
            on.remove(&node);
        }
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(failed)?;
        }
        let json = serde_json::to_vec(&on).map_err(|e| e.to_string())?;
        let next = file.with_extension("json.next");
        std::fs::write(&next, json).map_err(failed)?;
        std::fs::rename(&next, file).map_err(failed)
    }
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
    fn a_lead_s_reaction_in_a_task_is_kept_by_the_person_s_message_and_read_once() {
        let dir = std::env::temp_dir().join(format!("sonny-picked-{}", std::process::id()));
        let file = dir.join("t.by.json");
        let mut picked = Picked {
            file: Some(file.clone()),
            ..Picked::default()
        };
        assert!(picked.refresh().is_empty());
        Picked::apply_in(&file, 12, "calc.lead", "👀", false).unwrap();
        assert_eq!(
            picked.refresh(),
            [(12, "calc.lead".into(), "👀".into(), true)]
        );
        assert!(picked.refresh().is_empty());
        assert_eq!(picked.on(12), [("calc.lead".into(), "👀".into())]);
        // Taken off again, it is gone, and read as taken back once.
        std::thread::sleep(std::time::Duration::from_millis(5));
        Picked::apply_in(&file, 12, "calc.lead", "👀", true).unwrap();
        assert_eq!(
            picked.refresh(),
            [(12, "calc.lead".into(), "👀".into(), false)]
        );
        assert!(picked.refresh().is_empty());
        assert!(picked.on(12).is_empty());
        let target = |arguments: Value| target(&arguments).map(str::to_owned);
        assert_eq!(
            target(json!({"emoji": "👀", "bot": "calc.tui"})).as_deref(),
            Some("calc.tui")
        );
        assert_eq!(target(json!({"emoji": "👀", "bot": " "})), None);
        assert_eq!(target(json!({"emoji": "👀"})), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn reactions_are_kept_by_store_and_bot_id() {
        let key = key(Path::new("/home/me/.agent/state.sqlite"), 7);
        assert_eq!(
            key,
            super::key(Path::new("/home/me/.agent/state.sqlite"), 7)
        );
        assert!(key.ends_with("-7") && key.len() == 18, "{key}");
        assert_ne!(
            key,
            super::key(Path::new("/home/me/.agent/state.sqlite"), 8)
        );
        assert_ne!(key, super::key(Path::new("/tmp/other.sqlite"), 7));
    }

    #[test]
    fn a_refused_call_is_told_from_a_taken_one() {
        assert!(!refused(&json!({"output": "Done."})));
        assert!(refused(
            &json!({"output": "{\"error\":\"invalid_arguments\"}"})
        ));
    }
}
