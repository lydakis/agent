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
//! One folder per agent, by its [`crate::reactions::key`], with one file
//! per span of the agent's nodes: written when the person answers, read
//! when a replay draws those nodes. What a window holds grows with what it
//! draws, never with all the agent ever answered, nor with how many
//! agents there are.
use std::collections::{BTreeMap, BTreeSet};
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
    Ask {
        ask: asks::Ask,
        answer: String,
        /// The message the answer went as, while the task may still take
        /// it back unread: a window that opens the agent reads it again,
        /// and lets the answer go if it never reached the task.
        sent: Option<i64>,
    },
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

/// How many of the agent's nodes one file covers: a replay reads only the
/// files of the nodes it draws, and an answer rewrites only its own file,
/// so neither grows with all the agent ever answered.
const SPAN: i64 = 1024;

/// The file holding the answers drawn after `after`, by its number.
fn span(after: i64) -> i64 {
    after.div_euclid(SPAN)
}

/// The answers given in one agent's windows for agents below it, in one
/// file per [`SPAN`] of its nodes, each read when a replay first draws
/// its nodes.
#[derive(Default)]
pub struct Answered {
    dir: Option<PathBuf>,
    /// The files there, by number.
    files: BTreeSet<i64>,
    /// The files read, with what each holds, oldest node first.
    read: BTreeMap<i64, Vec<Entry>>,
}

impl Answered {
    /// The answers kept under `key`, the agent's [`crate::reactions::key`];
    /// none kept, or no key, is none. Only the folder is listed now.
    pub fn load(key: Option<&str>) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let dir = key.zip(home).map(|(key, home)| dir(&home, key));
        Self::in_dir(dir)
    }

    fn in_dir(dir: Option<PathBuf>) -> Self {
        let files = (dir.as_deref())
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let name = entry.ok()?.file_name();
                name.to_str()?.strip_suffix(".json")?.parse().ok()
            })
            .collect();
        Answered {
            dir,
            files,
            read: BTreeMap::new(),
        }
    }

    /// Kept in `dir`, for a test.
    #[cfg(test)]
    pub fn in_folder(dir: PathBuf) -> Self {
        Self::in_dir(Some(dir))
    }

    /// The entries of file `at`, read once.
    fn entries(&mut self, at: i64) -> &mut Vec<Entry> {
        let path = self.dir.as_deref().map(|dir| file(dir, at));
        (self.read.entry(at)).or_insert_with(|| path.as_deref().and_then(read).unwrap_or_default())
    }

    /// The answers drawn after nodes `from` through `to`, oldest first.
    pub fn within(&mut self, from: i64, to: i64) -> Vec<Entry> {
        let spans: Vec<i64> = (self.files.range(span(from)..=span(to)).copied())
            .chain(self.read.range(span(from)..=span(to)).map(|(at, _)| *at))
            .collect::<BTreeSet<i64>>()
            .into_iter()
            .collect();
        let mut within = Vec::new();
        for at in spans {
            let entries = self.entries(at);
            within.extend(
                (entries.iter())
                    .filter(|entry| (from..=to).contains(&entry.after))
                    .cloned(),
            );
        }
        within
    }

    /// Whether the answer to the question at `key` is kept, among those
    /// kept or drawn here.
    pub fn has(&self, key: &str) -> bool {
        (self.read.values().flatten()).any(|entry| entry.key() == key)
    }

    /// The file, among those read, that holds the answer at `key`.
    fn holding(&self, key: &str) -> Option<i64> {
        (self.read.iter())
            .find(|(_, entries)| entries.iter().any(|entry| entry.key() == key))
            .map(|(at, _)| *at)
    }

    /// `entry` kept, in place of one with its key. What another window
    /// wrote since is read first, under the folder's lock, so neither
    /// write undoes the other's.
    pub fn keep(&mut self, entry: Entry) -> Result<(), String> {
        let key = entry.key();
        let at = span(entry.after);
        if let Some(was) = self.holding(&key).filter(|was| *was != at) {
            self.change(was, |entries| {
                entries.retain(|kept| kept.key() != key);
                true
            })?;
        }
        self.change(at, |entries| {
            entries.retain(|kept| kept.key() != key);
            let at = entries.partition_point(|kept| kept.after <= entry.after);
            entries.insert(at, entry);
            true
        })
    }

    /// The questions whose answers may still be taken back unread: each
    /// one's key, its task and the message its answer went as. They were
    /// answered lately, so only the two newest files are read for them.
    pub fn unsettled(&mut self) -> Vec<(String, String, i64)> {
        let newest: Vec<i64> = self.files.iter().rev().take(2).copied().collect();
        for at in newest {
            self.entries(at);
        }
        (self.read.values().flatten())
            .filter_map(|entry| match &entry.what {
                What::Ask {
                    ask,
                    sent: Some(sent),
                    ..
                } => Some((ask.key(), ask.task.as_ref()?.bot.clone(), *sent)),
                _ => None,
            })
            .collect()
    }

    /// The answer to the question at `key` reached its task: nothing
    /// takes it back now.
    pub fn delivered(&mut self, key: &str) -> Result<(), String> {
        let Some(at) = self.holding(key) else {
            return Ok(());
        };
        self.change(at, |entries| {
            let mut changed = false;
            for entry in entries.iter_mut().filter(|entry| entry.key() == key) {
                if let What::Ask { sent, .. } = &mut entry.what {
                    changed |= sent.take().is_some();
                }
            }
            changed
        })
    }

    /// The answer to the question at `key` let go: it never reached the
    /// task, which took it back unread, so nothing was answered.
    pub fn forget(&mut self, key: &str) -> Result<(), String> {
        let Some(at) = self.holding(key) else {
            return Ok(());
        };
        self.change(at, |entries| {
            let before = entries.len();
            entries.retain(|kept| kept.key() != key);
            entries.len() < before
        })
    }

    /// File `at` changed by `change`, which says whether it changed it.
    fn change(
        &mut self,
        at: i64,
        change: impl FnOnce(&mut Vec<Entry>) -> bool,
    ) -> Result<(), String> {
        let Some(dir) = self.dir.clone() else {
            change(self.entries(at));
            return Ok(());
        };
        let file = file(&dir, at);
        let failed = |e: std::io::Error| format!("{}: {e}", file.display());
        std::fs::create_dir_all(&dir).map_err(failed)?;
        let lock = (std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true))
        .open(dir.join("lock"))
        .map_err(failed)?;
        lock.lock().map_err(failed)?;
        let entries = self.read.entry(at).or_default();
        *entries = read(&file).unwrap_or_default();
        if !change(entries) {
            return Ok(());
        }
        let json = serde_json::to_vec(&*entries).map_err(|e| e.to_string())?;
        let next = file.with_extension("json.next");
        std::fs::write(&next, json).map_err(failed)?;
        std::fs::rename(&next, &file).map_err(failed)?;
        self.files.insert(at);
        Ok(())
    }
}

fn dir(home: &Path, key: &str) -> PathBuf {
    home.join(format!(".agent/sonny/answered/{key}"))
}

fn file(dir: &Path, at: i64) -> PathBuf {
    dir.join(format!("{at}.json"))
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
                sent: None,
            },
        }
    }

    #[test]
    fn answers_are_kept_by_node_and_read_back_by_another_window() {
        let dir = std::env::temp_dir().join(format!("sonny-answered-{}", std::process::id()));
        let folder = dir.join("answered/k");
        let mut here = Answered::in_folder(folder.clone());
        here.keep(entry(9, "shop.fix", "t3/c1", "Yes")).unwrap();
        here.keep(entry(4, "shop.fix", "t2/c1", "No")).unwrap();
        here.keep(entry(9, "shop.docs", "t1/c1", "Later")).unwrap();
        // The same question answered again keeps the newest answer only.
        here.keep(entry(12, "shop.fix", "t3/c1", "Yes, now"))
            .unwrap();
        let mut there = Answered::in_folder(folder.clone());
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
        // One far down the history is in a file of its own, which a replay
        // of the newest nodes never reads.
        here.keep(entry(5000, "shop.fix", "t9/c1", "Go")).unwrap();
        let mut newest = Answered::in_folder(folder);
        assert_eq!(newest.within(4990, 5010).len(), 1);
        assert_eq!(newest.read.len(), 1);
        assert!(!newest.has("@shop.fix/t3/c1"));
        assert_eq!(newest.within(0, 5010).len(), 4);
        // Answered again further on, it moves there.
        here.keep(entry(5001, "shop.fix", "t3/c1", "Yes, later"))
            .unwrap();
        let mut again = Answered::in_folder(dir.join("answered/k"));
        assert_eq!(again.within(0, 100).len(), 2);
        assert_eq!(again.within(4990, 5010).len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_answer_taken_back_unread_is_forgotten() {
        let dir = std::env::temp_dir().join(format!("sonny-forgotten-{}", std::process::id()));
        let mut here = Answered::in_folder(dir.clone());
        let mut sent = entry(3, "shop.fix", "t1/c1", "Yes");
        if let What::Ask { sent, .. } = &mut sent.what {
            *sent = Some(7);
        }
        here.keep(sent).unwrap();
        here.keep(entry(5, "shop.fix", "t2/c1", "No")).unwrap();
        // Read again by the next window to open the agent, as it may still
        // be taken back unread.
        let mut next = Answered::in_folder(dir.clone());
        let unsettled = vec![("@shop.fix/t1/c1".to_owned(), "shop.fix".to_owned(), 7)];
        assert_eq!(next.unsettled(), unsettled);
        here.forget("@shop.fix/t1/c1").unwrap();
        here.delivered("@shop.fix/t2/c1").unwrap();
        assert!(Answered::in_folder(dir.clone()).unsettled().is_empty());
        here.forget("@shop.fix/t2/c1").unwrap();
        assert!(
            Answered::in_folder(dir.clone())
                .within(i64::MIN, i64::MAX)
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
