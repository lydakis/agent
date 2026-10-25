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
    /// Each task's block, by bot name.
    blocks: HashMap<String, String>,
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
        serde_json::from_slice(&out.stdout).map_err(|e| format!("rex {}: {e}", args[0]))
    }

    /// The project's session, made when there is none.
    pub fn open(bin: PathBuf, host: Option<&str>, label: &str) -> Result<Self, String> {
        let mut rex = Self {
            bin,
            host: host
                .map(|h| vec!["-S".into(), h.into()])
                .unwrap_or_default(),
            session: String::new(),
            label: label.to_owned(),
            blocks: HashMap::new(),
        };
        let listed = rex.call(&["ls", "--json", "--autostart=false"]);
        let found = listed.ok().and_then(|ls| {
            ls["sessions"]
                .as_array()?
                .iter()
                .find(|s| s["label"] == label)
                .and_then(|s| s["session_id"].as_str().map(str::to_owned))
        });
        rex.session = match found {
            Some(id) => id,
            None => rex.call(&["new", label, "--json"])?["session_id"]
                .as_str()
                .ok_or("rex new: no session_id")?
                .to_owned(),
        };
        Ok(rex)
    }

    fn open_blocks(&self) -> Vec<String> {
        self.call(&["block", "ls", "-s", &self.session, "--json"])
            .ok()
            .and_then(|ls| {
                Some(
                    ls["blocks"]
                        .as_array()?
                        .iter()
                        .filter_map(|b| b["block_id"].as_str().map(str::to_owned))
                        .collect(),
                )
            })
            .unwrap_or_default()
    }

    /// A window for the task running `command`, unless its block is still
    /// open. Returns whether one was opened. The person's focus stays where
    /// it is.
    pub fn show(&mut self, bot: &str, command: &[String]) -> Result<bool, String> {
        if let Some(block) = self.blocks.get(bot)
            && self.open_blocks().contains(block)
        {
            return Ok(false);
        }
        let mut args: Vec<&str> = vec!["-s", &self.session, "--label", bot, "--shell=none"];
        args.extend(["--focus=false", "--json", "--"]);
        args.extend(command.iter().map(String::as_str));
        // A window of its own; a build without `run` gets a split.
        let made = self
            .call(&[&["run"], args.as_slice()].concat())
            .or_else(|_| self.call(&[&["split"], args.as_slice()].concat()))?;
        let block = made["block_ids"][0]
            .as_str()
            .or_else(|| made["block_id"].as_str())
            .ok_or("rex: no block id in the reply")?;
        self.blocks.insert(bot.to_owned(), block.to_owned());
        Ok(true)
    }

    /// Close a task's block, as when its bot is deleted.
    pub fn close(&mut self, bot: &str) {
        if let Some(block) = self.blocks.remove(bot) {
            let _ = Command::new(&self.bin)
                .args(&self.host)
                .args(["block", "close", &block])
                .output();
        }
    }
}

/// The skill that lets any agent run long work where the person can see it.
/// Written once into `~/.agents/skills/rex`, naming this machine's rex.
pub fn install_skill(bin: &Path) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("HOME")?).join(".agents/skills/rex");
    let path = dir.join("SKILL.md");
    if path.exists() {
        return None;
    }
    let rex = bin.to_string_lossy();
    let text = include_str!("../skills/rex/SKILL.md").replace("REX_BIN", &format!("\"{rex}\""));
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::write(&path, text).ok()?;
    Some(path)
}
