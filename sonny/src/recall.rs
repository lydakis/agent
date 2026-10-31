//! The prompts sent before, which Up and Down bring back into the
//! composer, as a shell's history does, and Codex's and Claude Code's.
//!
//! They are kept in `~/.agent/sonny/history.jsonl`, a line each, so every
//! window recalls what any window sent. This window's own come first,
//! newest first, then the file's from before it opened, as Codex walks a
//! session's own prompts before the file's: another window sending
//! meanwhile does not cut into the ones being walked.
//!
//! Up goes back from the composer's first row, Down forward from its
//! last, and past the newest what was being written comes back. A
//! recalled prompt once edited is a draft of its own, so Up goes back
//! from it, newest first again.
use serde_json::json;
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

/// How big the file grows before its older half goes: some ten
/// thousand prompts of a few lines.
const MOST_BYTES: u64 = 4 << 20;

#[derive(Default)]
pub struct Recall {
    file: Option<PathBuf>,
    /// When this window opened, in milliseconds: the file's prompts from
    /// before then are walked after this window's own.
    opened: i64,
    /// This window's prompts, oldest first.
    mine: Vec<String>,
    /// The file's, oldest first, read the first time Up is pressed.
    before: Option<Vec<String>>,
    walk: Option<Walk>,
}

/// Where Up and Down have got to.
struct Walk {
    /// How far back, 1 the newest.
    back: usize,
    /// What was being written when Up first went back.
    draft: String,
    /// What the composer was given.
    shown: String,
}

impl Recall {
    /// The prompts kept under `HOME`.
    pub fn load() -> Self {
        match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            Some(home) => Self::kept(&PathBuf::from(home).join(".agent/sonny/history.jsonl")),
            None => Self::default(),
        }
    }

    pub fn kept(file: &Path) -> Self {
        Self {
            file: Some(file.to_owned()),
            opened: now_ms(),
            ..Self::default()
        }
    }

    /// Whether the composer holds `text` as recalled, unchanged.
    pub fn showing(&self, text: &str) -> bool {
        self.walk.as_ref().is_some_and(|walk| walk.shown == text)
    }

    /// Notes `prompt` sent, and ends any walk.
    pub fn sent(&mut self, prompt: &str) {
        self.walk = None;
        if prompt.is_empty() || self.mine.last().is_some_and(|last| last == prompt) {
            return;
        }
        self.mine.push(prompt.to_owned());
        if let Some(file) = &self.file {
            let _ = append(file, prompt, now_ms());
        }
    }

    /// The prompt before the one shown, for the composer now holding
    /// `current`; none when there is none older.
    pub fn older(&mut self, current: &str) -> Option<String> {
        if !self.showing(current) {
            self.walk = None;
        }
        if self.before.is_none() {
            self.before = Some(match &self.file {
                Some(file) => read(file, self.opened),
                None => Vec::new(),
            });
        }
        let back = self.walk.as_ref().map_or(0, |walk| walk.back);
        // The same prompt twice in a row is shown once.
        let to = (back + 1..=self.len()).find(|&to| self.nth(to) != current)?;
        let text = self.nth(to).to_owned();
        let draft = match self.walk.take() {
            Some(walk) => walk.draft,
            None => current.to_owned(),
        };
        self.walk = Some(Walk {
            back: to,
            draft,
            shown: text.clone(),
        });
        Some(text)
    }

    /// The prompt after the one shown, or past the newest the draft Up
    /// set aside; none when the composer holds no recalled prompt.
    pub fn newer(&mut self, current: &str) -> Option<String> {
        if !self.showing(current) {
            self.walk = None;
            return None;
        }
        let walk = self.walk.take()?;
        match (1..walk.back).rev().find(|&to| self.nth(to) != current) {
            Some(to) => {
                let text = self.nth(to).to_owned();
                self.walk = Some(Walk {
                    back: to,
                    shown: text.clone(),
                    ..walk
                });
                Some(text)
            }
            None => Some(walk.draft),
        }
    }

    fn len(&self) -> usize {
        self.mine.len() + self.before.as_ref().map_or(0, Vec::len)
    }

    /// The prompt `back` from now, 1 the newest.
    fn nth(&self, back: usize) -> &str {
        let mine = self.mine.len();
        if back <= mine {
            return &self.mine[mine - back];
        }
        let before = self.before.as_deref().unwrap_or_default();
        &before[before.len() + mine - back]
    }
}

/// The file's prompts sent before `opened`, oldest first. A line half
/// written by another window is skipped.
fn read(file: &Path, opened: i64) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry["at"].as_i64().is_some_and(|at| at < opened))
        .filter_map(|entry| Some(entry["text"].as_str()?.to_owned()))
        .filter(|text| !text.is_empty())
        .collect()
}

/// `prompt` added to the file as one line, only the person able to read
/// it, as Codex keeps its history; past [`MOST_BYTES`] the older half
/// goes, in place, so the windows appending meanwhile keep the same
/// file. The lock keeps one window's line from landing in another's.
fn append(file: &Path, prompt: &str, at: i64) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut open = std::fs::OpenOptions::new();
    open.read(true).append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    let mut handle = open.open(file)?;
    handle.lock()?;
    handle.write_all(format!("{}\n", json!({"at": at, "text": prompt})).as_bytes())?;
    if handle.metadata()?.len() > MOST_BYTES {
        let mut text = Vec::new();
        handle.seek(SeekFrom::Start(0))?;
        handle.read_to_end(&mut text)?;
        let half = text.len() - (MOST_BYTES / 2) as usize;
        let keep =
            (text[half..].iter().position(|&b| b == b'\n')).map_or(text.len(), |at| half + at + 1);
        handle.set_len(0)?;
        handle.write_all(&text[keep..])?;
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walk(recall: &mut Recall, keys: &str, mut text: String) -> Vec<String> {
        keys.chars()
            .map(|key| {
                let next = match key {
                    '↑' => recall.older(&text),
                    _ => recall.newer(&text),
                };
                if let Some(next) = next {
                    text = next;
                }
                text.clone()
            })
            .collect()
    }

    #[test]
    fn up_goes_back_and_down_comes_forward_to_the_draft() {
        let mut recall = Recall::default();
        for prompt in ["one", "two", "two", "three"] {
            recall.sent(prompt);
        }
        assert_eq!(
            walk(&mut recall, "↑↑↑↑↓↓↓↓", "half typed".into()),
            [
                "three",
                "two",
                "one",
                "one",
                "two",
                "three",
                "half typed",
                "half typed"
            ],
            "two sent twice is one step; the oldest stays; the draft comes back"
        );
        assert!(!recall.showing("half typed"));
        assert_eq!(walk(&mut recall, "↓", "half typed".into()), ["half typed"]);
    }

    #[test]
    fn an_edited_recall_is_a_draft_of_its_own() {
        let mut recall = Recall::default();
        for prompt in ["one", "two", "three"] {
            recall.sent(prompt);
        }
        assert_eq!(walk(&mut recall, "↑↑", String::new()), ["three", "two"]);
        assert!(recall.showing("two"));
        // Down does nothing for an edited one; Up starts again from it.
        assert_eq!(recall.newer("two!"), None);
        assert_eq!(walk(&mut recall, "↑↓", "two!".into()), ["three", "two!"]);
        // Sending ends the walk.
        recall.older("");
        recall.sent("three");
        assert!(!recall.showing("three"));
        assert_eq!(walk(&mut recall, "↑", String::new()), ["three"]);
    }

    #[test]
    fn this_windows_prompts_come_before_the_files() {
        let dir = std::env::temp_dir().join(format!("sonny-recall-{}", std::process::id()));
        let file = dir.join("history.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        append(&file, "from before", 1).unwrap();
        append(&file, "older still", 0).unwrap();
        let mut other = Recall::kept(&file);
        let mut recall = Recall::kept(&file);
        other.sent("another window, after this one opened");
        recall.sent("mine");
        std::fs::write(
            &file,
            format!("{}half a li", std::fs::read_to_string(&file).unwrap()),
        )
        .unwrap();
        assert_eq!(
            walk(&mut recall, "↑↑↑↑", String::new()),
            ["mine", "older still", "from before", "from before"],
            "the file's in the order written, after this window's own"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_file_keeps_its_newer_half() {
        let dir = std::env::temp_dir().join(format!("sonny-recall-big-{}", std::process::id()));
        let file = dir.join("history.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        let long = "x".repeat(1 << 16);
        for at in 0..70 {
            append(&file, &format!("{at} {long}"), at).unwrap();
        }
        let kept = std::fs::read_to_string(&file).unwrap();
        assert!((kept.len() as u64) < MOST_BYTES, "{}", kept.len());
        let first: serde_json::Value = serde_json::from_str(kept.lines().next().unwrap()).unwrap();
        assert!(
            first["at"].as_i64().unwrap() > 30,
            "whole lines, the newest kept"
        );
        assert!(kept.lines().last().unwrap().contains("\"69 "));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
