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
//!
//! What a prompt was sent with (its pictures) comes back with this
//! window's own prompts, and the draft's with the draft; the file keeps
//! text alone, so a prompt from it comes back with nothing attached.
//! Only the newest prompts' attachments are kept, up to a budget of
//! bytes, so a long-lived window does not hold every picture it sent:
//! an older prompt comes back as text alone, as one from the file does.
use serde_json::json;
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

/// How big the file grows before its older half goes: some ten
/// thousand prompts of a few lines. A window keeps as many bytes of its
/// own prompts' words, so neither grows with a long conversation.
const MOST_BYTES: u64 = 4 << 20;

#[derive(Default)]
pub struct Recall<A> {
    file: Option<PathBuf>,
    /// When this window opened, in milliseconds: the file's prompts from
    /// before then are walked after this window's own.
    opened: i64,
    /// This window's prompts, oldest first, with what each was sent with
    /// and its size in bytes.
    mine: Vec<(String, A, usize)>,
    /// How many bytes of attachments the newest prompts keep together.
    keep: usize,
    /// The file's, oldest first, read the first time Up is pressed.
    before: Option<Vec<String>>,
    walk: Option<Walk<A>>,
}

/// Where Up and Down have got to.
struct Walk<A> {
    /// How far back, 1 the newest.
    back: usize,
    /// What was being written when Up first went back.
    draft: String,
    /// What the draft had attached.
    attached: A,
    /// What the composer was given.
    shown: String,
}

impl<A: Clone + Default + PartialEq> Recall<A> {
    /// The prompts kept under `HOME`, the newest keeping `keep` bytes of
    /// attachments together.
    pub fn load(keep: usize) -> Self {
        match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            Some(home) => Self::kept(
                &PathBuf::from(home).join(".agent/sonny/history.jsonl"),
                keep,
            ),
            None => Self {
                keep,
                ..Self::default()
            },
        }
    }

    pub fn kept(file: &Path, keep: usize) -> Self {
        Self {
            file: Some(file.to_owned()),
            opened: now_ms(),
            keep,
            ..Self::default()
        }
    }

    /// Whether the composer holds `text` as recalled, unchanged.
    pub fn showing(&self, text: &str) -> bool {
        self.walk.as_ref().is_some_and(|walk| walk.shown == text)
    }

    /// Notes `prompt` sent with `attached`, of `bytes`, and ends any
    /// walk. Older prompts let go of their attachments past the budget.
    pub fn sent(&mut self, prompt: &str, attached: A, bytes: usize) {
        self.walk = None;
        if prompt.is_empty()
            || (self.mine.last()).is_some_and(|last| last.0 == prompt && last.1 == attached)
        {
            return;
        }
        self.mine.push((prompt.to_owned(), attached, bytes));
        let (mut held, mut words) = (0, 0);
        let mut from = 0;
        for (at, (text, attached, bytes)) in self.mine.iter_mut().enumerate().rev() {
            held += *bytes;
            if held > self.keep {
                *attached = A::default();
                *bytes = 0;
            }
            words += text.len() as u64;
            if words > MOST_BYTES && from == 0 {
                from = at + 1;
            }
        }
        // The oldest go past the words' budget; the newest always stays.
        self.mine.drain(..from.min(self.mine.len() - 1));
        if let Some(file) = &self.file {
            let _ = append(file, prompt, now_ms());
        }
    }

    /// The prompt before the one shown, for the composer now holding
    /// `current` with `attached`, which becomes what that prompt was sent
    /// with; none when there is none older.
    pub fn older(&mut self, current: &str, attached: &mut A) -> Option<String> {
        if !self.showing(current) {
            self.walk = None;
        }
        self.read_file();
        let back = self.walk.as_ref().map_or(0, |walk| walk.back);
        // The same prompt twice in a row, with the same attached, is
        // shown once.
        let to = (back + 1..=self.len()).find(|&to| !self.same(to, current, attached))?;
        let (text, with) = self.nth(to);
        let (text, with) = (text.to_owned(), with.cloned().unwrap_or_default());
        let (draft, draft_attached) = match self.walk.take() {
            Some(walk) => (walk.draft, walk.attached),
            None => (current.to_owned(), std::mem::take(attached)),
        };
        *attached = with;
        self.walk = Some(Walk {
            back: to,
            draft,
            attached: draft_attached,
            shown: text.clone(),
        });
        Some(text)
    }

    /// The prompt after the one shown, or past the newest the draft Up
    /// set aside, `attached` becoming what it has; none when the composer
    /// holds no recalled prompt.
    pub fn newer(&mut self, current: &str, attached: &mut A) -> Option<String> {
        if !self.showing(current) {
            self.walk = None;
            return None;
        }
        let walk = self.walk.take()?;
        match (1..walk.back)
            .rev()
            .find(|&to| !self.same(to, current, attached))
        {
            Some(to) => {
                let (text, with) = self.nth(to);
                let text = text.to_owned();
                *attached = with.cloned().unwrap_or_default();
                self.walk = Some(Walk {
                    back: to,
                    shown: text.clone(),
                    ..walk
                });
                Some(text)
            }
            None => {
                *attached = walk.attached;
                Some(walk.draft)
            }
        }
    }

    /// Whether the prompt `back` from now is `text` with `attached`.
    fn same(&self, back: usize, text: &str, attached: &A) -> bool {
        let (prompt, with) = self.nth(back);
        prompt == text && with.cloned().unwrap_or_default() == *attached
    }

    /// Every prompt, newest first, each once (its newest), as `back` and
    /// its words: what Ctrl+R searches.
    pub fn all(&mut self) -> Vec<(usize, &str)> {
        self.read_file();
        let mut seen = std::collections::HashSet::new();
        (1..=self.len())
            .map(|back| (back, self.nth(back).0))
            .filter(|(_, text)| seen.insert(*text))
            .collect()
    }

    /// The prompt `back` from now, picked by a search, for the composer
    /// now holding `current` with `attached`: Up and Down go on from it,
    /// and past the newest `current` comes back.
    pub fn jump(&mut self, back: usize, current: &str, attached: &mut A) -> Option<String> {
        if back == 0 || back > self.len() {
            return None;
        }
        let (text, with) = self.nth(back);
        let (text, with) = (text.to_owned(), with.cloned().unwrap_or_default());
        let (draft, draft_attached) = match self.walk.take() {
            Some(walk) if walk.shown == current => (walk.draft, walk.attached),
            _ => (current.to_owned(), std::mem::take(attached)),
        };
        *attached = with;
        self.walk = Some(Walk {
            back,
            draft,
            attached: draft_attached,
            shown: text.clone(),
        });
        Some(text)
    }

    /// The file's prompts, read once.
    fn read_file(&mut self) {
        if self.before.is_none() {
            self.before = Some(match &self.file {
                Some(file) => read(file, self.opened),
                None => Vec::new(),
            });
        }
    }

    fn len(&self) -> usize {
        self.mine.len() + self.before.as_ref().map_or(0, Vec::len)
    }

    /// The prompt `back` from now, 1 the newest, with what it was sent
    /// with when this window sent it.
    fn nth(&self, back: usize) -> (&str, Option<&A>) {
        let mine = self.mine.len();
        if back <= mine {
            let (text, with, _) = &self.mine[mine - back];
            return (text, Some(with));
        }
        let before = self.before.as_deref().unwrap_or_default();
        (&before[before.len() + mine - back], None)
    }
}

/// The file's prompts sent before `opened`, oldest first, read under
/// the lock so a window trimming it meanwhile is not caught halfway.
fn read(file: &Path, opened: i64) -> Vec<String> {
    let mut text = String::new();
    let read = std::fs::File::open(file).and_then(|mut handle| {
        handle.lock_shared()?;
        handle.read_to_string(&mut text)
    });
    if read.is_err() {
        return Vec::new();
    }
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

    fn walk(recall: &mut Recall<()>, keys: &str, mut text: String) -> Vec<String> {
        keys.chars()
            .map(|key| {
                let next = match key {
                    '↑' => recall.older(&text, &mut ()),
                    _ => recall.newer(&text, &mut ()),
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
            recall.sent(prompt, (), 0);
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
            recall.sent(prompt, (), 0);
        }
        assert_eq!(walk(&mut recall, "↑↑", String::new()), ["three", "two"]);
        assert!(recall.showing("two"));
        // Down does nothing for an edited one; Up starts again from it.
        assert_eq!(recall.newer("two!", &mut ()), None);
        assert_eq!(walk(&mut recall, "↑↓", "two!".into()), ["three", "two!"]);
        // Sending ends the walk.
        recall.older("", &mut ());
        recall.sent("three", (), 0);
        assert!(!recall.showing("three"));
        assert_eq!(walk(&mut recall, "↑", String::new()), ["three"]);
    }

    #[test]
    fn what_a_prompt_was_sent_with_comes_back_with_it() {
        let dir = std::env::temp_dir().join(format!("sonny-recall-with-{}", std::process::id()));
        let file = dir.join("history.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        append(&file, "from the file [Image #1]", 0).unwrap();
        let mut recall = Recall::kept(&file, 2);
        recall.sent("look [Image #1]", vec!["sent picture"], 1);
        let mut attached = vec!["draft picture"];
        assert_eq!(
            recall.older("draft [Image #1]", &mut attached).as_deref(),
            Some("look [Image #1]")
        );
        assert_eq!(attached, ["sent picture"]);
        assert_eq!(
            recall.older("look [Image #1]", &mut attached).as_deref(),
            Some("from the file [Image #1]")
        );
        assert!(attached.is_empty(), "the file keeps text alone");
        recall.newer("from the file [Image #1]", &mut attached);
        assert_eq!(attached, ["sent picture"]);
        assert_eq!(
            recall.newer("look [Image #1]", &mut attached).as_deref(),
            Some("draft [Image #1]")
        );
        assert_eq!(attached, ["draft picture"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_same_words_with_other_pictures_are_two_steps_and_the_oldest_let_go() {
        let mut recall = Recall {
            keep: 2,
            ..Recall::default()
        };
        for shot in ["first", "second", "third"] {
            recall.sent("look [Image #1]", vec![shot], 1);
        }
        let mut attached = Vec::new();
        let shown: Vec<_> = (0..3)
            .map(|_| {
                let text = recall.older("look [Image #1]", &mut attached);
                (text, attached.clone())
            })
            .collect();
        assert_eq!(
            shown,
            [
                (Some("look [Image #1]".into()), vec!["third"]),
                (Some("look [Image #1]".into()), vec!["second"]),
                (Some("look [Image #1]".into()), vec![]),
            ],
            "the oldest's picture is past the budget"
        );
        assert_eq!(
            recall.newer("look [Image #1]", &mut attached),
            Some("look [Image #1]".into())
        );
        assert_eq!(attached, ["second"]);
    }

    #[test]
    fn a_window_keeps_its_newest_words_up_to_the_files_size() {
        let mut recall = Recall::default();
        let long = "x".repeat(1 << 20);
        for at in 0..6 {
            recall.sent(&format!("{at} {long}"), (), 0);
        }
        let kept: Vec<_> = recall.mine.iter().map(|(text, ..)| &text[..1]).collect();
        assert_eq!(
            kept,
            ["3", "4", "5"],
            "the newest within {MOST_BYTES} bytes"
        );
        recall.sent(&"y".repeat(5 << 20), (), 0);
        assert_eq!(
            recall.mine.len(),
            1,
            "one longer than the budget stays alone"
        );
    }

    #[test]
    fn a_search_lists_each_prompt_once_and_jumps_into_the_walk() {
        let mut recall = Recall::default();
        for prompt in ["one", "two", "one", "three"] {
            recall.sent(prompt, (), 0);
        }
        assert_eq!(recall.all(), [(1, "three"), (2, "one"), (3, "two")]);
        assert_eq!(recall.jump(3, "draft", &mut ()).as_deref(), Some("two"));
        assert_eq!(
            walk(&mut recall, "↑↓↓↓↓", "two".into()),
            ["one", "two", "one", "three", "draft"]
        );
        assert_eq!(recall.jump(9, "draft", &mut ()), None);
    }

    #[test]
    fn this_windows_prompts_come_before_the_files() {
        let dir = std::env::temp_dir().join(format!("sonny-recall-{}", std::process::id()));
        let file = dir.join("history.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        append(&file, "from before", 1).unwrap();
        append(&file, "older still", 0).unwrap();
        let mut other = Recall::kept(&file, 0);
        let mut recall = Recall::kept(&file, 0);
        other.sent("another window, after this one opened", (), 0);
        recall.sent("mine", (), 0);
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
