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
    /// The session's label, as the person sees it.
    pub name: String,
    /// Whether the session is the one Sonny runs in.
    pub here: bool,
    /// Each task's block, by bot name.
    blocks: HashMap<String, String>,
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
        // Every call names its session: the shell's own Rex variables may
        // name another (a daemon's), and rex would look a bare ID up there.
        let out = Command::new(&self.bin)
            .env_remove("REX_SESSION")
            .env_remove("REX_BLOCK")
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

    /// The project's session. From the terminal Sonny runs in (`terminal`),
    /// that terminal's session, so the lead stays where the person opened
    /// it and tasks open beside it. Otherwise, as from an agent's shell, whose Rex variables
    /// would name the daemon's terminal: the session the project's lead is
    /// in, else the one labelled with the project's name, made when there
    /// is none.
    pub fn open(
        bin: PathBuf,
        host: Option<&str>,
        label: &str,
        terminal: bool,
    ) -> Result<Self, String> {
        let mut rex = Self {
            bin,
            host: host
                .map(|h| vec!["-S".into(), h.into()])
                .unwrap_or_default(),
            session: String::new(),
            name: label.to_owned(),
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
        let current = sessions.iter().find(|(id, _)| {
            terminal && host.is_none() && !mine.is_empty() && mine.ends_with(id.as_str())
        });
        let found = sessions.iter().find(|(_, l)| l == label);
        let lead = format!("{label}.lead");
        let leads = || {
            sessions
                .iter()
                .find(|(id, _)| rex.blocks_in(id).iter().any(|b| b.label == lead))
        };
        (rex.session, rex.name) = match current {
            Some((id, now)) => {
                rex.here = true;
                (id.clone(), now.clone())
            }
            None => match leads().or(found) {
                Some((id, now)) => (id.clone(), now.clone()),
                None => {
                    let made = rex.call(&["new", label, "--json"])?;
                    let id = made["session_id"]
                        .as_str()
                        .ok_or("rex new: no session_id")?;
                    (id.to_owned(), label.to_owned())
                }
            },
        };
        Ok(rex)
    }

    fn listed(&self) -> Vec<Listed> {
        self.blocks_in(&self.session)
    }

    fn blocks_in(&self, session: &str) -> Vec<Listed> {
        let Ok(ls) = self.call(&["block", "ls", "-s", session, "--json"]) else {
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
            self.close_block(&old.block);
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

    fn close_block(&self, block: &str) {
        let _ = self.call(&["block", "close", block, "-s", &self.session]);
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
            self.close_block(&other.block);
        }
    }

    /// Run a command an agent asked for in a block labelled `OWNER/NAME`, so
    /// the person can see whose it is and it closes with its owner's
    /// window: below the owner's block, or in a window of its own (named
    /// `name`) when asked or when the owner has none here. One already
    /// labelled so is replaced. The block stays open once the command
    /// exits, to be read. Returns the block's ID.
    pub fn run(
        &mut self,
        owner: &str,
        name: &str,
        cwd: &Path,
        command: &[String],
        window: bool,
    ) -> Result<String, String> {
        let label = format!("{owner}/{name}");
        let listed = self.listed();
        for old in listed.iter().filter(|b| b.label == label) {
            self.close_block(&old.block);
        }
        let anchor = listed
            .iter()
            .find(|b| b.label == owner)
            .filter(|_| !window)
            .map(|b| b.block.clone());
        let cwd = cwd.to_string_lossy();
        let mut args: Vec<&str> = match &anchor {
            Some(anchor) => vec!["split", "-b", anchor, "--split", "below", "--ratio", "35"],
            None => vec!["run"],
        };
        args.extend(["-s", &self.session, "-c", &cwd, "--label", &label]);
        args.extend(["--keep-open", "--focus=false", "--json", "--"]);
        args.extend(command.iter().map(String::as_str));
        let made = self.call(&args)?;
        let block = made["block_ids"][0]
            .as_str()
            .ok_or("rex: no block id in the reply")?
            .to_owned();
        if let Some(window) = made["window_id"].as_str().filter(|_| anchor.is_none()) {
            self.rename_window(window, name);
        }
        Ok(block)
    }

    /// Close a task's block, and every block it asked for, as when it is
    /// done or its bot is deleted.
    pub fn close(&mut self, bot: &str) {
        let known = self.blocks.remove(bot);
        let owned = format!("{bot}/");
        for block in self.listed().into_iter().filter(|b| {
            b.label == bot || b.label.starts_with(&owned) || Some(&b.block) == known.as_ref()
        }) {
            self.close_block(&block.block);
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
    let pictures = crate::skills::install("pictures", include_str!("../skills/pictures/SKILL.md"));
    crate::skills::install("rex-windows", &text).or(pictures)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// A stand-in rex, over a JSON file of sessions and their blocks, that
    /// logs each call it gets.
    const FAKE: &str = r#"#!/usr/bin/env python3
import json, os, sys
here = os.path.dirname(os.path.abspath(sys.argv[0]))
state = json.load(open(os.path.join(here, "state.json")))
args = sys.argv[1:]
open(os.path.join(here, "calls"), "a").write(" ".join(args) + "\n")
def flag(name):
    return args[args.index(name) + 1] if name in args else None
out = None
if args[0] == "ls":
    out = {"sessions": [{"session_id": s, "label": v["label"]} for s, v in state.items()]}
elif args[:2] == ["block", "ls"]:
    out = {"blocks": state[flag("-s")]["blocks"]}
elif args[:2] == ["block", "close"]:
    for v in state.values():
        v["blocks"] = [b for b in v["blocks"] if b["block_id"] != args[2]]
elif args[0] in ("split", "run"):
    blocks = state[flag("-s")]["blocks"]
    made = "block:%d" % (len(open(os.path.join(here, "calls")).readlines()))
    blocks.append({"block_id": made, "label": flag("--label"), "window_id": "window:1"})
    out = {"block_ids": [made], "window_id": "window:1"}
json.dump(state, open(os.path.join(here, "state.json"), "w"))
if out is not None:
    print(json.dumps(out))
"#;

    fn fake(name: &str, state: serde_json::Value) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sonny-rex-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("rex");
        std::fs::write(&bin, FAKE).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("state.json"), state.to_string()).unwrap();
        (bin, dir)
    }

    fn block(id: &str, label: &str) -> serde_json::Value {
        serde_json::json!({"block_id": id, "label": label, "window_id": "window:1"})
    }

    fn labels(dir: &Path, session: &str) -> Vec<String> {
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
        state[session]["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["label"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn an_agent_runs_its_commands_in_its_leads_session_and_they_close_with_it() {
        // The lead was opened in a session named for something else; a
        // session labelled with the project is a stray.
        let (bin, dir) = fake(
            "home",
            serde_json::json!({
                "01home": {"label": "lydakis", "blocks": [
                    block("block:lead", "graft.lead"),
                    block("block:task", "graft.fix"),
                ]},
                "01stray": {"label": "graft", "blocks": []},
            }),
        );
        let mut rex = super::Rex::open(bin, None, "graft", false).unwrap();
        assert_eq!(
            (rex.session.as_str(), rex.name.as_str()),
            ("01home", "lydakis")
        );
        assert!(!rex.here);

        let cwd = Path::new("/work");
        let tests = rex.run(
            "graft.fix",
            "tests",
            cwd,
            &["cargo".into(), "test".into()],
            false,
        );
        let tests = tests.unwrap();
        let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
        let split = calls.lines().last().unwrap();
        assert!(
            split.starts_with("split -b block:task --split below")
                && split.contains("-s 01home -c /work --label graft.fix/tests --keep-open")
                && split.ends_with("-- cargo test"),
            "{split}"
        );
        // Run again by name, the old block makes way.
        let again = rex.run(
            "graft.fix",
            "tests",
            cwd,
            &["cargo".into(), "test".into()],
            false,
        );
        assert_ne!(again.unwrap(), tests);
        rex.run(
            "graft.fix",
            "build",
            cwd,
            &["cargo".into(), "build".into()],
            true,
        )
        .unwrap();
        rex.run("graft.lead", "server", cwd, &["serve".into()], false)
            .unwrap();
        assert_eq!(
            labels(&dir, "01home"),
            [
                "graft.lead",
                "graft.fix",
                "graft.fix/tests",
                "graft.fix/build",
                "graft.lead/server"
            ]
        );
        let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
        assert!(calls.lines().all(|c| !c.contains("01stray")));
        // A bare ID is looked up in the shell's own session, maybe another.
        assert!(
            calls
                .lines()
                .filter(|c| c.starts_with("block close"))
                .all(|c| c.ends_with("-s 01home"))
        );

        // The task done, what it opened goes with it; the lead's stays.
        rex.close("graft.fix");
        assert_eq!(labels(&dir, "01home"), ["graft.lead", "graft.lead/server"]);
    }
}
