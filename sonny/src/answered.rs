//! What the person answered in a window for an agent below its own: a
//! task's question, or a call held for them, that came up here and that
//! they answered from here. The question or call and what came of it
//! are the agent's below, kept in its history; this window's agent keeps
//! none of it. So that
//! the answer stays where it was given, as a reply in a Slack thread also
//! sent to the channel shows in both, it is kept here, by the window's
//! agent, beside its reactions and its margin: by the node it was drawn
//! after, so a replay draws it there, folded as it was left.
//!
//! One file per agent, by its [`crate::reactions::key`]: written when the
//! person answers, read when a window opens the agent. It grows with what
//! the person answers, never with how many agents there are.
use std::path::{Path, PathBuf};

use crate::asks;

/// One answer given here: the node of this window's agent it was drawn
/// after, and what it answered.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub after: i64,
    #[serde(flatten)]
    pub what: What,
}

/// What an answer given here answered.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum What {
    /// A task's question, and the answer as its card says it.
    Ask { ask: asks::Ask, answer: String },
    /// A call of an agent below held for the person: the agent, the turn
    /// and the call, what it was, the request's rows as written out, and
    /// what came of it, the person's verdict here until the call's own.
    Call {
        bot: String,
        turn: i64,
        call: String,
        name: String,
        arguments: serde_json::Value,
        rows: Vec<String>,
        decided: crate::transcript::Decided,
        /// The call's own verdict was heard: `decided` is final. Until
        /// then it is the person's answer, which another gate may still
        /// overrule, read again from the audit trail since `at_ms`.
        heard: bool,
        at_ms: i64,
    },
}

/// Which answer the person gave here to call `call` of `bot` in turn
/// `turn`: one per call.
pub fn call_key(bot: &str, turn: i64, call: &str) -> String {
    format!("@{bot}/call:{turn}/{call}")
}

impl Entry {
    /// Which answer it is: the question's card key, one per question.
    pub fn key(&self) -> String {
        match &self.what {
            What::Ask { ask, .. } => ask.key(),
            What::Call {
                bot, turn, call, ..
            } => call_key(bot, *turn, call),
        }
    }
}

/// The answers given in one agent's windows for agents below it, oldest
/// node first.
#[derive(Default)]
pub struct Answered {
    file: Option<PathBuf>,
    entries: Vec<Entry>,
}

impl Answered {
    /// The answers kept under `key`, the agent's [`crate::reactions::key`];
    /// none kept, or no key, is none.
    pub fn load(key: Option<&str>) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let file = key.zip(home).map(|(key, home)| file(&home, key));
        let entries = file.as_deref().and_then(read).unwrap_or_default();
        Answered { file, entries }
    }

    /// Kept in `file`, for a test.
    #[cfg(test)]
    pub fn in_file(file: PathBuf) -> Self {
        let entries = read(&file).unwrap_or_default();
        Answered {
            file: Some(file),
            entries,
        }
    }

    /// The answers drawn after nodes `from` through `to`, oldest first.
    pub fn within(&self, from: i64, to: i64) -> Vec<Entry> {
        (self.entries.iter())
            .filter(|entry| (from..=to).contains(&entry.after))
            .cloned()
            .collect()
    }

    /// Whether the answer to the question at `key` is kept.
    pub fn has(&self, key: &str) -> bool {
        self.entries.iter().any(|entry| entry.key() == key)
    }

    /// `entry` kept, in place of one with its key. What another window
    /// wrote since is read first, under the file's lock, so neither write
    /// undoes the other's.
    pub fn keep(&mut self, entry: Entry) -> Result<(), String> {
        self.change(|entries| {
            let key = entry.key();
            entries.retain(|kept| kept.key() != key);
            let at = entries.partition_point(|kept| kept.after <= entry.after);
            entries.insert(at, entry);
            true
        })
    }

    /// The answer to the question at `key` let go: it never reached the
    /// task, which took it back unread, so nothing was answered.
    pub fn forget(&mut self, key: &str) -> Result<(), String> {
        self.change(|entries| {
            let before = entries.len();
            entries.retain(|kept| kept.key() != key);
            entries.len() < before
        })
    }

    fn change(&mut self, change: impl FnOnce(&mut Vec<Entry>) -> bool) -> Result<(), String> {
        let Some(file) = self.file.clone() else {
            change(&mut self.entries);
            return Ok(());
        };
        let failed = |e: std::io::Error| format!("{}: {e}", file.display());
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(failed)?;
        }
        let lock = (std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true))
        .open(file.with_extension("lock"))
        .map_err(failed)?;
        lock.lock().map_err(failed)?;
        self.entries = read(&file).unwrap_or_default();
        if !change(&mut self.entries) {
            return Ok(());
        }
        let json = serde_json::to_vec(&self.entries).map_err(|e| e.to_string())?;
        let next = file.with_extension("json.next");
        std::fs::write(&next, json).map_err(failed)?;
        std::fs::rename(&next, &file).map_err(failed)
    }
}

fn file(home: &Path, key: &str) -> PathBuf {
    home.join(format!(".agent/sonny/answered/{key}.json"))
}

fn read(file: &Path) -> Option<Vec<Entry>> {
    serde_json::from_slice(&std::fs::read(file).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(bot: &str, id: &str) -> asks::Ask {
        asks::Ask {
            id: id.into(),
            question: "Ship it?".into(),
            why: "It decides the release.".into(),
            options: Vec::new(),
            recommended: None,
            unanswered: None,
            deadline: None,
            covers: None,
            task: Some(asks::Asker {
                bot: bot.into(),
                name: "fix".into(),
            }),
        }
    }

    fn entry(after: i64, bot: &str, id: &str, answer: &str) -> Entry {
        Entry {
            after,
            what: What::Ask {
                ask: ask(bot, id),
                answer: answer.into(),
            },
        }
    }

    #[test]
    fn answers_are_kept_by_node_and_read_back_by_another_window() {
        let dir = std::env::temp_dir().join(format!("sonny-answered-{}", std::process::id()));
        let file = dir.join("answered/k.json");
        let mut here = Answered::in_file(file.clone());
        here.keep(entry(9, "shop.fix", "t3/c1", "Yes")).unwrap();
        here.keep(entry(4, "shop.fix", "t2/c1", "No")).unwrap();
        here.keep(entry(9, "shop.docs", "t1/c1", "Later")).unwrap();
        // The same question answered again keeps the newest answer only.
        here.keep(entry(12, "shop.fix", "t3/c1", "Yes, now"))
            .unwrap();
        let there = Answered::in_file(file);
        let keys: Vec<String> = there
            .within(i64::MIN, i64::MAX)
            .iter()
            .map(Entry::key)
            .collect();
        assert_eq!(
            keys,
            ["@shop.fix/t2/c1", "@shop.docs/t1/c1", "@shop.fix/t3/c1"]
        );
        assert_eq!(there.within(5, 9).len(), 1);
        assert!(there.has("@shop.fix/t3/c1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_answer_taken_back_unread_is_forgotten() {
        let dir = std::env::temp_dir().join(format!("sonny-forgotten-{}", std::process::id()));
        let file = dir.join("k.json");
        let mut here = Answered::in_file(file.clone());
        here.keep(entry(3, "shop.fix", "t1/c1", "Yes")).unwrap();
        here.forget("@shop.fix/t1/c1").unwrap();
        assert!(
            Answered::in_file(file)
                .within(i64::MIN, i64::MAX)
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
