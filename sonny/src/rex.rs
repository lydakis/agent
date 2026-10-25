//! Rex (superlogical.com/rex) as the place a project's work is laid out:
//! one Rex session per project, labelled with the project's name, and one
//! window in it per task the lead starts, running Sonny on that task. Every
//! call goes through the `rex` CLI with `--json`; ids are kept because
//! labels need not be unique. Checked against rex 0.0.1792787366 (API rev 7)
//! for `ls`, `new`, `split`, `block ls` and `block close`; `run`'s output is
//! read the way `split`'s is.
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
};

const MAC_APP: &str =
    "/Applications/Rex Beta.app/Contents/Helpers/Rex Server.rex/Contents/MacOS/rex";

/// The rex CLI: `$REX`, else `rex` on the PATH, else the Mac app's.
pub fn find() -> Option<PathBuf> {
    if let Some(rex) = std::env::var_os("REX").filter(|r| !r.is_empty()) {
        return Some(PathBuf::from(rex));
    }
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("rex"))
            .find(|rex| rex.is_file())
    });
    on_path.or_else(|| Path::new(MAC_APP).is_file().then(|| PathBuf::from(MAC_APP)))
}

pub struct Rex {
    bin: PathBuf,
    /// `-S HOST` when the session lives on another Rex server.
    host: Vec<String>,
    pub session: String,
    pub label: String,
    /// Whether the session is the one Sonny runs in.
    pub here: bool,
    /// Each task's block, by bot name.
    blocks: HashMap<String, String>,
}

/// A name Rex gave a session made without one: two lowercase words, as
/// `crisp-oasis`. One the person chose is kept.
fn made_up(label: &str) -> bool {
    let words: Vec<&str> = label.split('-').collect();
    words.len() == 2
        && words
            .iter()
            .all(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_lowercase()))
}

/// Where `sonny show` puts a task.
pub enum Placement {
    Window,
    /// Beside a block (a bot's, by its label), on one side.
    Split {
        side: String,
        beside: String,
    },
}

/// A block as `block ls` lists it.
struct Listed {
    block: String,
    label: String,
    window: String,
}

impl Rex {
    fn call(&self, args: &[&str]) -> Result<Value, String> {
        let out = Command::new(&self.bin)
            .args(&self.host)
            .args(args)
            .output()
            .map_err(|e| format!("rex: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "rex {}: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        if out.stdout.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&out.stdout).map_err(|e| format!("rex {}: {e}", args[0]))
    }

    /// The project's session: the one Sonny was started in, so the lead
    /// stays where the person opened it and tasks open beside it, renamed
    /// to the project when Rex made its name up; outside Rex, the session
    /// labelled with the project's name, made when there is none.
    pub fn open(bin: PathBuf, host: Option<&str>, label: &str) -> Result<Self, String> {
        let mut rex = Self {
            bin,
            host: host
                .map(|h| vec!["-S".into(), h.into()])
                .unwrap_or_default(),
            session: String::new(),
            label: label.to_owned(),
            here: false,
            blocks: HashMap::new(),
        };
        let listed = rex.call(&["ls", "--json", "--autostart=false"]);
        let sessions: Vec<(String, String)> = listed
            .ok()
            .and_then(|ls| ls["sessions"].as_array().cloned())
            .into_iter()
            .flatten()
            .filter_map(|s| {
                Some((
                    s["session_id"].as_str()?.to_owned(),
                    s["label"].as_str().unwrap_or("").to_owned(),
                ))
            })
            .collect();
        // Rex names the block's session `session:ID`; `ls` gives the ID.
        let mine = std::env::var("REX_SESSION").unwrap_or_default();
        let current = sessions
            .iter()
            .find(|(id, _)| host.is_none() && !mine.is_empty() && mine.ends_with(id.as_str()));
        let found = sessions.iter().find(|(_, l)| l == label);
        rex.session = match (current, found) {
            (Some((id, now)), found) => {
                if found.is_none() && made_up(now) {
                    let _ = rex.call(&["session", "rename", id, label]);
                }
                rex.here = true;
                id.clone()
            }
            (None, Some((id, _))) => id.clone(),
            (None, None) => rex.call(&["new", label, "--json"])?["session_id"]
                .as_str()
                .ok_or("rex new: no session_id")?
                .to_owned(),
        };
        Ok(rex)
    }

    fn listed(&self) -> Vec<Listed> {
        let Ok(ls) = self.call(&["block", "ls", "-s", &self.session, "--json"]) else {
            return Vec::new();
        };
        ls["blocks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| {
                Some(Listed {
                    block: b["block_id"].as_str()?.to_owned(),
                    label: b["label"].as_str().unwrap_or("").to_owned(),
                    window: b["window_id"].as_str().unwrap_or("").to_owned(),
                })
            })
            .collect()
    }

    /// A block already labelled with the bot's name.
    fn adopt(&mut self, bot: &str, listed: &[Listed]) -> bool {
        let Some(found) = listed.iter().find(|b| b.label == bot) else {
            return false;
        };
        self.blocks.insert(bot.to_owned(), found.block.clone());
        true
    }

    /// A window for the task running `command`, named `name`, unless the
    /// task has a block already. Returns whether one was opened. The
    /// person's focus stays where it is.
    pub fn show(&mut self, bot: &str, name: &str, command: &[String]) -> Result<bool, String> {
        let listed = self.listed();
        if let Some(block) = self.blocks.get(bot)
            && listed.iter().any(|b| &b.block == block)
        {
            return Ok(false);
        }
        if self.adopt(bot, &listed) {
            return Ok(false);
        }
        self.place(bot, name, command, &Placement::Window)?;
        Ok(true)
    }

    /// Open the task's block where asked, closing any it had.
    pub fn place(
        &mut self,
        bot: &str,
        name: &str,
        command: &[String],
        placement: &Placement,
    ) -> Result<(), String> {
        for old in self.listed().into_iter().filter(|b| b.label == bot) {
            let _ = self.call(&["block", "close", &old.block]);
        }
        let anchor;
        let mut args: Vec<&str> = match placement {
            Placement::Window => vec!["window", "new", "-s", &self.session, name],
            Placement::Split { side, beside } => {
                anchor = self
                    .listed()
                    .into_iter()
                    .find(|b| &b.label == beside)
                    .map(|b| b.block)
                    .ok_or(format!("no Rex block labelled {beside}"))?;
                let session = self.session.as_str();
                vec![
                    "split", "-s", session, "-b", &anchor, "--split", side, "--label", bot,
                ]
            }
        };
        args.extend(["--shell=none", "--focus=false", "--json", "--"]);
        args.extend(command.iter().map(String::as_str));
        let made = self.call(&args)?;
        let block = made["block_ids"][0]
            .as_str()
            .ok_or("rex: no block id in the reply")?
            .to_owned();
        if made["window_id"].is_string() {
            // `window new` names the window; the block is named after the
            // bot, so it can be found, and anchored on, by that name.
            let _ = self.call(&["block", "rename", &block, bot, "-s", &self.session]);
        }
        self.blocks.insert(bot.to_owned(), block);
        Ok(())
    }

    /// Each window and the labels of its blocks, in Rex's order.
    pub fn windows(&self) -> Vec<(String, Vec<String>)> {
        let mut windows: Vec<(String, Vec<String>)> = Vec::new();
        for block in self.listed() {
            match windows
                .iter_mut()
                .find(|(window, _)| *window == block.window)
            {
                Some((_, labels)) => labels.push(block.label),
                None => windows.push((block.window, vec![block.label])),
            }
        }
        windows
    }

    /// Where each block sits, by label: its tab, counted from 1, and in a
    /// split, its side of its neighbour (`right of`, `below`, … a label).
    pub fn places(&self) -> HashMap<String, (usize, Option<(String, String)>)> {
        let mut places = HashMap::new();
        let Ok(ls) = self.call(&["window", "ls", "-s", &self.session, "--json"]) else {
            return places;
        };
        for (tab, window) in ls["windows"].as_array().into_iter().flatten().enumerate() {
            for layer in window["layers"].as_array().into_iter().flatten() {
                let labels: HashMap<&str, &str> = layer["blocks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|b| Some((b["block_id"].as_str()?, b["label"].as_str()?)))
                    .collect();
                let mut sides = HashMap::new();
                sides_of(&layer["layout"], &mut sides);
                for (block, label) in &labels {
                    let side = sides.get(*block).map(|(side, other): &(&str, String)| {
                        let other = labels.get(other.as_str()).copied().unwrap_or("");
                        (side.to_string(), other.to_owned())
                    });
                    places.insert(label.to_string(), (tab + 1, side));
                }
            }
        }
        places
    }

    pub fn rename_window(&self, window: &str, name: &str) {
        let _ = self.call(&["window", "rename", window, name, "-s", &self.session]);
    }

    /// Take on the block Sonny itself runs in, when in this session, named
    /// after the bot.
    pub fn adopt_own(&mut self, bot: &str) {
        let mine = std::env::var("REX_BLOCK").unwrap_or_default();
        let listed = self.listed();
        let own = listed
            .iter()
            .find(|b| !mine.is_empty() && (b.block == mine || b.block.ends_with(&mine)))
            .or_else(|| listed.iter().find(|b| b.label == bot));
        let Some(own) = own else {
            return;
        };
        if own.label != bot {
            let _ = self.call(&["block", "rename", &own.block, bot, "-s", &self.session]);
        }
        self.blocks.insert(bot.to_owned(), own.block.clone());
    }

    /// Close every other block labelled with the bot's name, as one Sonny
    /// opened for the task while this one placed it.
    pub fn close_others(&mut self, bot: &str) {
        let Some(mine) = self.blocks.get(bot).cloned() else {
            return;
        };
        for other in self
            .listed()
            .into_iter()
            .filter(|b| b.label == bot && b.block != mine)
        {
            let _ = self.call(&["block", "close", &other.block]);
        }
    }

    /// Close a task's block, as when its bot is deleted.
    pub fn close(&mut self, bot: &str) {
        let known = self.blocks.remove(bot);
        for block in self
            .listed()
            .into_iter()
            .filter(|b| b.label == bot || Some(&b.block) == known.as_ref())
        {
            let _ = self.call(&["block", "close", &block.block]);
        }
    }
}

/// The first block of a layout node, in reading order.
fn first_block(node: &Value) -> Option<String> {
    match node["block_id"].as_str() {
        Some(block) => Some(block.to_owned()),
        None => first_block(&node["split"]["before"]),
    }
}

/// Each block that is one side of a split, and which side of the other side
/// it is. A vertical split stacks its sides; a horizontal one sets them
/// side by side.
fn sides_of(node: &Value, sides: &mut HashMap<String, (&'static str, String)>) {
    let split = &node["split"];
    if split.is_null() {
        return;
    }
    let stacked = split["direction"] == "vertical";
    let (before, after) = (&split["before"], &split["after"]);
    if let (Some(block), Some(other)) = (before["block_id"].as_str(), first_block(after)) {
        sides.insert(
            block.to_owned(),
            (if stacked { "above" } else { "left of" }, other),
        );
    }
    if let (Some(block), Some(other)) = (after["block_id"].as_str(), first_block(before)) {
        sides.insert(
            block.to_owned(),
            (if stacked { "below" } else { "right of" }, other),
        );
    }
    sides_of(before, sides);
    sides_of(after, sides);
}

/// The skills that let any agent run long work where the person can see it,
/// and show the person pictures.
/// Kept in `~/.agents/skills/rex-windows`, naming this machine's rex and
/// Sonny; `rex` itself is the name of Rex's own skill.
pub fn install_skill(bin: &Path) -> Option<PathBuf> {
    let sonny = std::env::current_exe().ok()?;
    let text = include_str!("../skills/rex-windows/SKILL.md")
        .replace("REX_BIN", &format!("\"{}\"", bin.to_string_lossy()))
        .replace("SONNY_BIN", &format!("\"{}\"", sonny.to_string_lossy()));
    let pictures = install("pictures", include_str!("../skills/pictures/SKILL.md"));
    install("rex-windows", &text).or(pictures)
}

/// A skill of Sonny's in the person's skills, rewritten when it changed.
fn install(name: &str, text: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("HOME")?)
        .join(".agents/skills")
        .join(name);
    let path = dir.join("SKILL.md");
    if std::fs::read_to_string(&path).is_ok_and(|now| now == text) {
        return None;
    }
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::write(&path, text).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_a_name_rex_made_up_is_replaced() {
        assert!(super::made_up("crisp-oasis"));
        assert!(!super::made_up("beam"));
        assert!(!super::made_up("my-work-stuff"));
        assert!(!super::made_up("Beam-dev"));
    }
}
