//! Herdr (herdr.dev) as the place a project's work is laid out: one Herdr
//! workspace per project, a tab in it per task the lead starts, and panes
//! labelled with each bot's name. Every call goes through the `herdr` CLI,
//! which prints the socket API's JSON reply; ids are kept because labels
//! need not be unique. Written against herdr 0.9.5's CLI and API schema.
use crate::mux::{Backend, Block, Place, Places};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// The herdr CLI: `$HERDR`, else `herdr` on the PATH.
pub fn find() -> Option<PathBuf> {
    if let Some(herdr) = std::env::var_os("HERDR").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(herdr));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("herdr"))
        .find(|herdr| herdr.is_file())
}

/// The Herdr pane Sonny runs in, by the ID it was launched with: a pane
/// moved since keeps that ID as an alias, which `pane get` resolves.
pub fn pane() -> Option<String> {
    std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|p| !p.is_empty())
}

/// Whether the project's lead is in a pane of the Herdr server this shell
/// reaches.
pub fn has_lead(bin: &Path, project: &str) -> bool {
    let lead = format!("{project}.lead");
    Command::new(bin)
        .env_remove("HERDR_PANE_ID")
        .args(["pane", "list"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| serde_json::from_slice::<Value>(&out.stdout).ok())
        .is_some_and(|reply| {
            (reply["result"]["panes"].as_array().into_iter().flatten())
                .any(|p| p["label"] == lead.as_str())
        })
}

/// Sonny as an agent that supports Herdr (herdr.dev/docs/add-herdr-support),
/// when it runs in a Herdr pane: the agent panel lists its bot by name as
/// working, idle or waiting on the person, the workspace rolls that up and
/// notifies, `herdr agent wait` and `herdr agent prompt` reach it by name,
/// and after a Herdr restart the pane runs Sonny on the bot again.
struct Presence {
    bin: PathBuf,
    pane: String,
    /// The bot's name in its project, as the panel shows it: `Sonny: fix`.
    shown: String,
    /// The bot's name as Herdr names agents: `shop-fix`, when it can be one.
    name: Option<String>,
    bot: String,
    /// What reopens this Sonny after a Herdr restart, when Herdr can run it.
    resume: Option<Vec<String>>,
    /// Whether this Herdr refused a report for its resume command: before
    /// 0.9.2 its CLI takes none, and refuses the report that carries one.
    no_resume: AtomicBool,
    /// The last report's number: Herdr drops one that is not above it,
    /// so a late report cannot undo a newer one.
    seq: AtomicU64,
    /// Whether Herdr shows the bot by name: it keeps that until Sonny
    /// releases the pane, so it is said once, not with every report.
    named: AtomicBool,
}

static PRESENCE: OnceLock<Presence> = OnceLock::new();

/// Who reports the bot's state, and who its name in the panel.
const SOURCE: &str = "sonny";
const DISPLAY: &str = "sonny-display";

/// Sonny runs on `bot` in this pane, when it is a Herdr pane; `resume` is
/// what runs it on the bot again, `sonny` first.
pub fn announce(bot: &str, resume: Vec<String>) {
    if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
        return;
    }
    let Some(pane) = pane() else {
        return;
    };
    let bin = std::env::var_os("HERDR_BIN_PATH")
        .filter(|b| !b.is_empty())
        .map(PathBuf::from)
        .or_else(find);
    let Some(bin) = bin else {
        return;
    };
    let short = bot.split_once('.').map_or(bot, |(_, name)| name);
    let _ = PRESENCE.set(Presence {
        bin,
        pane,
        shown: format!("Sonny: {short}"),
        name: agent_name(bot),
        bot: bot.to_owned(),
        resume: resumable(resume),
        no_resume: AtomicBool::new(false),
        seq: AtomicU64::new(0),
        named: AtomicBool::new(false),
    });
}

/// Tell Herdr the bot's state, as `status_report` names it: `working`,
/// `blocked` (on the person), `idle`, `done` or `error` (both ready for
/// input), or `clear` as Sonny quits. In the background, so Herdr never
/// slows Sonny down, except for the last, which Sonny waits on briefly.
pub fn report(state: &str) {
    if let Some(presence) = PRESENCE.get() {
        presence.report(state);
    }
}

impl Presence {
    /// The report, in the background but for `clear`: its thread, to wait on.
    fn report(&'static self, state: &str) -> Option<std::thread::JoinHandle<()>> {
        let seq = self.next().to_string();
        let common = ["--source", SOURCE, "--agent", SOURCE, "--seq", &seq];
        if state == "clear" {
            let mut args = vec!["pane", "release-agent", &self.pane];
            args.extend(common);
            run(&self.bin, &args, Duration::from_secs(1));
            return None;
        }
        let semantic = match state {
            "working" | "blocked" => state,
            _ => "idle",
        };
        let mut args: Vec<String> = ["pane", "report-agent", &self.pane, "--state", semantic]
            .into_iter()
            .chain(common)
            .map(str::to_owned)
            .collect();
        if semantic == "blocked" {
            args.extend(["--message".into(), "waiting for your approval".into()]);
        }
        args.extend(["--agent-session-id".into(), self.bot.clone()]);
        let display = [
            "pane",
            "report-metadata",
            &self.pane,
            "--source",
            DISPLAY,
            "--agent",
            SOURCE,
            "--display-agent",
            &self.shown,
            "--seq",
            &seq,
        ]
        .map(str::to_owned);
        Some(std::thread::spawn(move || {
            let limit = Duration::from_secs(5);
            let report = |args: &[String]| {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                run(&self.bin, &args, limit)
            };
            let reported = match &self.resume {
                Some(resume) if !self.no_resume.load(Ordering::Relaxed) => {
                    report(&[&args[..], &["--".to_owned()], resume].concat()) || {
                        let reported = report(&args);
                        self.no_resume.store(reported, Ordering::Relaxed);
                        reported
                    }
                }
                _ => report(&args),
            };
            if !reported {
                return;
            }
            // Shown by name once Herdr holds the agent; a name another live
            // agent has is left to it.
            if self.named.load(Ordering::Relaxed) {
                return;
            }
            let display: Vec<&str> = display.iter().map(String::as_str).collect();
            if !run(&self.bin, &display, limit) {
                return;
            }
            self.named.store(true, Ordering::Relaxed);
            if let Some(name) = &self.name {
                run(&self.bin, &["agent", "rename", &self.pane, name], limit);
            }
        }))
    }

    /// A number above every one this pane had from a Sonny before: the
    /// time in milliseconds, or one more than the last.
    fn next(&self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let last = self
            .seq
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
                Some(now.max(last + 1))
            })
            .unwrap_or_default();
        now.max(last + 1)
    }
}

/// A Herdr agent name for a bot (`[a-z][a-z0-9_-]{0,31}`): `shop.fix-cart`
/// is `shop-fix-cart`.
fn agent_name(bot: &str) -> Option<String> {
    let name: String = bot
        .to_ascii_lowercase()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '_' | '-' => c,
            _ => '-',
        })
        .take(32)
        .collect();
    name.starts_with(|c: char| c.is_ascii_lowercase())
        .then_some(name)
}

/// A resume command Herdr will run: its first word a command on the PATH,
/// no apostrophe or control character in any word, at most 64 words and
/// 8 KiB.
fn resumable(command: Vec<String>) -> Option<Vec<String>> {
    let first = command.first()?;
    let path = std::env::var_os("PATH")?;
    let found = std::env::split_paths(&path).any(|dir| dir.join(first).is_file());
    let plain = command
        .iter()
        .all(|word| !word.contains('\'') && !word.chars().any(char::is_control));
    let size: usize = command.iter().map(|word| word.len() + 1).sum();
    (found && plain && !first.contains('/') && command.len() <= 64 && size <= 8192)
        .then_some(command)
}

/// Run the herdr CLI for at most `limit`; whether it succeeded.
fn run(bin: &Path, args: &[&str], limit: Duration) -> bool {
    let child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    let until = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

pub struct Herdr {
    bin: PathBuf,
    workspace: String,
    /// The workspace's label, as the person sees it.
    name: String,
    /// Whether the workspace is the one Sonny runs in.
    here: bool,
    /// The pane Sonny runs in, by its current ID.
    me: Option<String>,
    /// Whether the workspace was made just now.
    made: bool,
}

/// A word as a POSIX shell reads it back, for `pane run`, which types its
/// command into the pane's shell.
pub(crate) fn quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=@%+,".contains(&b));
    match plain {
        true => word.to_owned(),
        false => format!("'{}'", word.replace('\'', r"'\''")),
    }
}

impl Herdr {
    /// The reply's `result`. Every call names what it acts on: the shell's
    /// own Herdr variables may name another pane (a daemon's).
    fn call(&self, args: &[&str]) -> Result<Value, String> {
        let out = Command::new(&self.bin)
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_TAB_ID")
            .env_remove("HERDR_WORKSPACE_ID")
            .args(args)
            .output()
            .map_err(|e| format!("herdr: {e}"))?;
        let what = args[..args.len().min(2)].join(" ");
        if !out.status.success() {
            let said = String::from_utf8_lossy(&out.stderr);
            let error: Value = serde_json::from_str(said.trim()).unwrap_or(Value::Null);
            let message = error["error"]["message"]
                .as_str()
                .map_or_else(|| said.trim().to_owned(), str::to_owned);
            return Err(format!("herdr {what}: {message}"));
        }
        // `pane run`, `send-text` and `send-keys` print nothing when they
        // succeed.
        if out.stdout.trim_ascii().is_empty() {
            return Ok(Value::Null);
        }
        let reply: Value =
            serde_json::from_slice(&out.stdout).map_err(|e| format!("herdr {what}: {e}"))?;
        Ok(reply["result"].clone())
    }

    /// The project's workspace. From the pane Sonny runs in (`terminal`),
    /// the workspace that pane is in now, so the lead stays where the
    /// person opened it and tasks open beside it. Otherwise, as from an
    /// agent's shell: the one `kept` names while it is open, else the
    /// workspace the project's lead is in, else the one labelled with the
    /// project's name, made when there is none.
    pub fn open(
        bin: PathBuf,
        label: &str,
        terminal: Option<&str>,
        kept: Option<&str>,
    ) -> Result<Self, String> {
        let mut herdr = Self {
            bin,
            workspace: String::new(),
            name: label.to_owned(),
            here: false,
            me: None,
            made: false,
        };
        let workspaces: Vec<(String, String)> = herdr
            .call(&["workspace", "list"])?
            .get("workspaces")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|w| {
                Some((
                    w["workspace_id"].as_str()?.to_owned(),
                    w["label"].as_str().unwrap_or("").to_owned(),
                ))
            })
            .collect();
        let me = terminal.and_then(|pane| herdr.call(&["pane", "get", pane]).ok());
        let current = me.as_ref().and_then(|me| {
            (workspaces.iter()).find(|(id, _)| me["pane"]["workspace_id"] == id.as_str())
        });
        let lead = format!("{label}.lead");
        let leads = || {
            let panes = herdr.call(&["pane", "list"]).ok()?;
            let pane = panes["panes"]
                .as_array()?
                .iter()
                .find(|p| p["label"] == lead.as_str())?;
            workspaces
                .iter()
                .find(|(id, _)| pane["workspace_id"] == id.as_str())
        };
        let found = || workspaces.iter().find(|(_, l)| l == label);
        let kept = kept.and_then(|kept| workspaces.iter().find(|(id, _)| id == kept));
        (herdr.workspace, herdr.name) = match current {
            Some((id, now)) => {
                herdr.here = true;
                herdr.me = me
                    .as_ref()
                    .and_then(|me| me["pane"]["pane_id"].as_str().map(str::to_owned));
                (id.clone(), now.clone())
            }
            None => match kept.or_else(leads).or_else(found) {
                Some((id, now)) => (id.clone(), now.clone()),
                None => {
                    let made =
                        herdr.call(&["workspace", "create", "--label", label, "--no-focus"])?;
                    let id = made["workspace"]["workspace_id"]
                        .as_str()
                        .ok_or("herdr workspace create: no workspace_id")?;
                    herdr.made = true;
                    (id.to_owned(), label.to_owned())
                }
            },
        };
        Ok(herdr)
    }

    /// Run `command` in a new pane's shell: in its place (`exec`), so the
    /// pane goes with it, unless it is to be read once it exits.
    fn start(&self, pane: &str, label: &str, command: &[String], keep: bool) -> Result<(), String> {
        self.rename_block(pane, label);
        let mut line: Vec<String> = command.iter().map(|w| quote(w)).collect();
        if !keep {
            line.insert(0, "exec".into());
        }
        self.call(&["pane", "run", pane, &line.join(" ")])?;
        Ok(())
    }
}

impl Backend for Herdr {
    fn home(&self) -> String {
        format!("Herdr workspace {}", self.name)
    }

    fn here(&self) -> bool {
        self.here
    }

    fn made(&self) -> bool {
        self.made
    }

    fn blocks(&self) -> Vec<Block> {
        let Ok(listed) = self.call(&["pane", "list", "--workspace", &self.workspace]) else {
            return Vec::new();
        };
        listed["panes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                Some(Block {
                    id: p["pane_id"].as_str()?.to_owned(),
                    label: p["label"].as_str().unwrap_or("").to_owned(),
                    window: p["tab_id"].as_str().unwrap_or("").to_owned(),
                })
            })
            .collect()
    }

    fn close_block(&self, block: &str) {
        let _ = self.call(&["pane", "close", block]);
    }

    fn rename_block(&self, block: &str, label: &str) {
        let _ = self.call(&["pane", "rename", block, label]);
    }

    fn rename_window(&self, window: &str, name: &str) {
        let _ = self.call(&["tab", "rename", window, name]);
    }

    fn window(
        &self,
        label: &str,
        name: &str,
        cwd: &Path,
        command: &[String],
        keep: bool,
    ) -> Result<String, String> {
        let cwd = cwd.to_string_lossy();
        let mut args = vec!["tab", "create", "--workspace", &self.workspace];
        args.extend(["--label", name, "--no-focus"]);
        if keep {
            args.extend(["--cwd", &cwd]);
        }
        let made = self.call(&args)?;
        let pane = made["root_pane"]["pane_id"]
            .as_str()
            .ok_or("herdr tab create: no pane_id")?
            .to_owned();
        self.start(&pane, label, command, keep)?;
        Ok(pane)
    }

    fn split(
        &self,
        anchor: &str,
        side: &str,
        small: bool,
        label: &str,
        cwd: &Path,
        command: &[String],
        keep: bool,
    ) -> Result<String, String> {
        // Herdr splits to the right or down; the other sides are those,
        // swapped.
        let (direction, swap) = match side {
            "right" => ("right", false),
            "left" => ("right", true),
            "below" => ("down", false),
            "above" => ("down", true),
            _ => return Err(format!("no side {side}: right, left, above or below")),
        };
        let cwd = cwd.to_string_lossy();
        let mut args = vec!["pane", "split", anchor, "--direction", direction];
        if small {
            args.extend(["--ratio", "0.65"]);
        }
        if keep {
            args.extend(["--cwd", &cwd]);
        }
        args.push("--no-focus");
        let made = self.call(&args)?;
        let pane = made["pane"]["pane_id"]
            .as_str()
            .ok_or("herdr pane split: no pane_id")?
            .to_owned();
        if swap {
            let _ = self.call(&[
                "pane",
                "swap",
                "--source-pane",
                &pane,
                "--target-pane",
                anchor,
            ]);
        }
        self.start(&pane, label, command, keep)?;
        Ok(pane)
    }

    fn own_block(&self) -> Option<String> {
        self.me.clone()
    }

    fn place(&self) -> Place {
        Place::Herdr {
            workspace: self.workspace.clone(),
        }
    }

    /// Where each pane sits, by label: its tab's number, and beside the
    /// tab's first other pane, which side of it.
    fn places(&self) -> Places {
        let mut places = HashMap::new();
        let numbers: HashMap<String, usize> = self
            .call(&["tab", "list", "--workspace", &self.workspace])
            .ok()
            .and_then(|tabs| tabs["tabs"].as_array().cloned())
            .into_iter()
            .flatten()
            .filter_map(|t| {
                Some((
                    t["tab_id"].as_str()?.to_owned(),
                    t["number"].as_u64()? as usize,
                ))
            })
            .collect();
        let blocks = self.blocks();
        let mut tabs: Vec<&str> = Vec::new();
        for block in &blocks {
            if !tabs.contains(&block.window.as_str()) {
                tabs.push(&block.window);
            }
        }
        for (n, tab) in tabs.iter().enumerate() {
            let number = numbers.get(*tab).copied().unwrap_or(n + 1);
            let panes: Vec<&Block> = blocks.iter().filter(|b| b.window == *tab).collect();
            let rects: HashMap<String, Value> = match panes.len() {
                1 => HashMap::new(),
                _ => self
                    .call(&["pane", "layout", "--pane", &panes[0].id])
                    .ok()
                    .and_then(|l| l["layout"]["panes"].as_array().cloned())
                    .into_iter()
                    .flatten()
                    .filter_map(|p| Some((p["pane_id"].as_str()?.to_owned(), p["rect"].clone())))
                    .collect(),
            };
            for pane in &panes {
                let side = panes
                    .iter()
                    .find(|other| other.id != pane.id)
                    .and_then(|other| {
                        let side = side_of(rects.get(&pane.id)?, rects.get(&other.id)?);
                        Some((side.to_owned(), other.label.clone()))
                    });
                places.insert(pane.label.clone(), (number, side));
            }
        }
        places
    }
}

/// Which side of `other` a pane at `rect` is, by their centres; a cell is
/// about twice as tall as it is wide.
fn side_of(rect: &Value, other: &Value) -> &'static str {
    let centre = |r: &Value, at: &str, size: &str| {
        r[at].as_f64().unwrap_or(0.0) + r[size].as_f64().unwrap_or(0.0) / 2.0
    };
    let dx = centre(rect, "x", "width") - centre(other, "x", "width");
    let dy = 2.0 * (centre(rect, "y", "height") - centre(other, "y", "height"));
    match (dx.abs() >= dy.abs(), dx >= 0.0, dy >= 0.0) {
        (true, true, _) => "right of",
        (true, false, _) => "left of",
        (false, _, true) => "below",
        (false, _, false) => "above",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::{Layout, Placement};
    use serde_json::json;

    /// A stand-in herdr over a JSON file of workspaces, tabs and panes,
    /// that logs each call it gets and answers as herdr's CLI does.
    const FAKE: &str = r#"#!/usr/bin/env python3
import json, os, sys
here = os.path.dirname(os.path.abspath(sys.argv[0]))
state = json.load(open(os.path.join(here, "state.json")))
args = sys.argv[1:]
open(os.path.join(here, "calls"), "a").write(json.dumps(args) + "\n")
if state.get("before_resume") and "--" in args:
    sys.stderr.write("unknown option: --\n")
    sys.exit(2)
def flag(name):
    return args[args.index(name) + 1] if name in args else None
def pane(ws, tab, label=None):
    state["n"] += 1
    p = {"pane_id": "%s:p%d" % (ws, state["n"]), "workspace_id": ws, "tab_id": tab, "label": label}
    state["panes"].append(p)
    return p
out = None
what = args[:2]
if what == ["workspace", "list"]:
    out = {"type": "workspace_list", "workspaces": state["workspaces"]}
elif what == ["workspace", "create"]:
    ws = "w%d" % (len(state["workspaces"]) + 1)
    state["workspaces"].append({"workspace_id": ws, "label": flag("--label")})
    out = {"type": "workspace_created", "workspace": state["workspaces"][-1], "root_pane": pane(ws, ws + ":t1")}
elif what == ["pane", "list"]:
    ws = flag("--workspace")
    out = {"type": "pane_list", "panes": [p for p in state["panes"] if ws in (None, p["workspace_id"])]}
elif what == ["tab", "create"]:
    ws = flag("--workspace")
    state["n"] += 1
    tab = "%s:t%d" % (ws, state["n"])
    out = {"type": "tab_created", "tab": {"tab_id": tab}, "root_pane": pane(ws, tab)}
elif what == ["pane", "split"]:
    anchor = [p for p in state["panes"] if p["pane_id"] == args[2]][0]
    out = {"type": "pane_info", "pane": pane(anchor["workspace_id"], anchor["tab_id"])}
elif what == ["pane", "rename"]:
    for p in state["panes"]:
        if p["pane_id"] == args[2]:
            p["label"] = args[3]
    out = {"type": "pane_info"}
elif what == ["pane", "close"]:
    state["panes"] = [p for p in state["panes"] if p["pane_id"] != args[2]]
    out = {"type": "ok"}
elif what == ["pane", "get"]:
    pid = state.get("aliases", {}).get(args[2], args[2])
    out = {"type": "pane_info", "pane": [p for p in state["panes"] if p["pane_id"] == pid][0]}
elif what in (["pane", "run"], ["pane", "send-text"], ["pane", "send-keys"]):
    out = None
else:
    out = {"type": "ok"}
json.dump(state, open(os.path.join(here, "state.json"), "w"))
if out is not None:
    print(json.dumps({"id": "cli:request", "result": out}))
"#;

    fn fake(name: &str, state: Value) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sonny-herdr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("herdr");
        crate::testing::script(&bin, FAKE);
        std::fs::write(dir.join("state.json"), state.to_string()).unwrap();
        (bin, dir)
    }

    fn calls(dir: &Path) -> Vec<Vec<String>> {
        std::fs::read_to_string(dir.join("calls"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn labels(dir: &Path) -> Vec<String> {
        let state: Value =
            serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
        state["panes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["label"].as_str().unwrap_or("").to_owned())
            .collect()
    }

    #[test]
    fn an_agent_finds_its_leads_workspace_and_its_commands_close_with_it() {
        let (bin, dir) = fake(
            "home",
            json!({
                "n": 10,
                "workspaces": [
                    {"workspace_id": "w1", "label": "lydakis"},
                    {"workspace_id": "w2", "label": "graft"},
                ],
                "panes": [
                    {"pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1", "label": "graft.lead"},
                    {"pane_id": "w1:p2", "workspace_id": "w1", "tab_id": "w1:t2", "label": "graft.fix"},
                ],
            }),
        );
        let herdr = Herdr::open(bin, "graft", None, None).unwrap();
        assert_eq!(
            (herdr.workspace.as_str(), herdr.home().as_str()),
            ("w1", "Herdr workspace lydakis")
        );
        let mut layout = Layout::new(Box::new(herdr));
        let cwd = Path::new("/work");
        layout
            .split(
                "graft.fix",
                "tests",
                cwd,
                &["cargo".into(), "test it's".into()],
                Some("below"),
            )
            .unwrap();
        let calls = calls(&dir);
        let n = calls.len();
        assert_eq!(
            calls[n - 3..],
            [
                [
                    "pane",
                    "split",
                    "w1:p2",
                    "--direction",
                    "down",
                    "--ratio",
                    "0.65",
                    "--cwd",
                    "/work",
                    "--no-focus"
                ]
                .map(str::to_owned)
                .to_vec(),
                ["pane", "rename", "w1:p11", "graft.fix/tests"]
                    .map(str::to_owned)
                    .to_vec(),
                ["pane", "run", "w1:p11", r"cargo 'test it'\''s'"]
                    .map(str::to_owned)
                    .to_vec(),
            ]
        );
        // A task's own Sonny runs in its pane's place.
        layout
            .show(
                "graft.docs",
                "docs",
                &["/bin/sonny".into(), "--agent".into(), "graft.docs".into()],
            )
            .unwrap();
        let last = super::tests::calls(&dir).pop().unwrap();
        assert_eq!(last[..2], ["pane", "run"]);
        assert_eq!(last[3], "exec /bin/sonny --agent graft.docs");
        assert_eq!(
            labels(&dir),
            ["graft.lead", "graft.fix", "graft.fix/tests", "graft.docs"]
        );
        assert!(
            super::tests::calls(&dir)
                .iter()
                .all(|c| !c.contains(&"w2".to_owned()))
        );

        layout.close("graft.fix");
        assert_eq!(labels(&dir), ["graft.lead", "graft.docs"]);
    }

    #[test]
    fn sonny_finds_its_pane_after_the_pane_moved() {
        let (bin, dir) = fake(
            "moved",
            json!({
                "n": 10,
                "workspaces": [
                    {"workspace_id": "w1", "label": "notes"},
                    {"workspace_id": "w2", "label": "shop"},
                ],
                "panes": [{"pane_id": "w2:p7", "workspace_id": "w2", "tab_id": "w2:t1", "label": null}],
                "aliases": {"w1:p3": "w2:p7"},
            }),
        );
        let herdr = Herdr::open(bin, "shop", Some("w1:p3"), None).unwrap();
        assert!(herdr.here());
        assert_eq!(herdr.workspace, "w2");
        let mut layout = Layout::new(Box::new(herdr));
        layout.adopt_own("shop.lead");
        assert_eq!(labels(&dir), ["shop.lead"]);
        let sleep = ["sleep".to_owned(), "60".into()];
        assert!(layout.show("shop.fix", "fix", &sleep).unwrap());
        assert!(calls(&dir).iter().any(|c| c[..2] == ["pane", "run"]));
    }

    #[test]
    fn a_project_without_a_workspace_gets_one() {
        let (bin, dir) = fake("new", json!({"n": 0, "workspaces": [], "panes": []}));
        let herdr = Herdr::open(bin, "shop", None, None).unwrap();
        assert_eq!(herdr.home(), "Herdr workspace shop");
        assert!(!herdr.here());
        assert!(
            calls(&dir).contains(
                &["workspace", "create", "--label", "shop", "--no-focus"]
                    .map(str::to_owned)
                    .to_vec()
            )
        );
    }

    /// Against a real Herdr server: `HERDR_SESSION=check herdr server` in
    /// another terminal, then `HERDR_SESSION=check cargo test -p sonny
    /// real_herdr -- --ignored --nocapture`. It makes a `sonnycheck`
    /// workspace and closes it.
    #[test]
    #[ignore]
    fn real_herdr() {
        let bin = find().expect("herdr on the PATH");
        let herdr = Herdr::open(bin, "sonnycheck", None, None).unwrap();
        let workspace = herdr.workspace.clone();
        let lead = herdr.blocks()[0].id.clone();
        herdr.rename_block(&lead, "sonnycheck.lead");
        let mut layout = Layout::new(Box::new(herdr));
        let sleep = ["sh".to_owned(), "-c".into(), "echo task; sleep 60".into()];
        assert!(layout.show("sonnycheck.fix", "fix", &sleep).unwrap());
        assert!(!layout.show("sonnycheck.fix", "fix", &sleep).unwrap());
        let tests = layout
            .split(
                "sonnycheck.fix",
                "tests",
                Path::new("/tmp"),
                &["echo".into(), "it's done".into()],
                Some("below"),
            )
            .unwrap();
        layout
            .split(
                "sonnycheck.lead",
                "server",
                Path::new("/tmp"),
                &["echo".into(), "serving".into()],
                None,
            )
            .unwrap();
        let left = Placement::Split {
            side: "left".into(),
            beside: "sonnycheck.lead".into(),
        };
        layout
            .place("sonnycheck.docs", "docs", &sleep, &left)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        let windows = layout.windows();
        let places = layout.places();
        println!("windows: {windows:?}\nplaces: {places:?}");
        let check = Herdr::open(find().unwrap(), "sonnycheck", None, None).unwrap();
        // `pane read` prints the pane's text, not JSON.
        let read = Command::new(find().unwrap())
            .args([
                "pane",
                "read",
                &tests,
                "--source",
                "recent-unwrapped",
                "--lines",
                "20",
            ])
            .output()
            .unwrap();
        let read = String::from_utf8_lossy(&read.stdout);
        println!("tests pane: {read}");
        assert_eq!(places["sonnycheck.docs"].1.as_ref().unwrap().0, "left of");
        assert_eq!(
            places["sonnycheck.fix/tests"].1.as_ref().unwrap().0,
            "below"
        );
        layout.close("sonnycheck.fix");
        let left: Vec<String> = check.blocks().into_iter().map(|b| b.label).collect();
        println!("after closing the task: {left:?}");
        assert!(!left.iter().any(|l| l.starts_with("sonnycheck.fix")));
        let _ = check.call(&["workspace", "close", &workspace]);
    }

    #[test]
    fn sonny_tells_herdr_how_its_bot_is_doing_and_how_to_reopen_it() {
        let (bin, dir) = fake("presence", json!({"n": 0, "workspaces": [], "panes": []}));
        let me: &'static Presence = Box::leak(Box::new(Presence {
            bin,
            pane: "w1:p2".into(),
            shown: "Sonny: fix-cart".into(),
            name: agent_name("shop.fix-cart"),
            bot: "shop.fix-cart".into(),
            resume: Some(vec![
                "sonny".into(),
                "--agent".into(),
                "shop.fix-cart".into(),
            ]),
            no_resume: AtomicBool::new(false),
            seq: AtomicU64::new(0),
            named: AtomicBool::new(false),
        }));
        for state in ["working", "blocked", "done"] {
            me.report(state).unwrap().join().unwrap();
        }
        assert!(me.report("clear").is_none());
        let calls = calls(&dir);
        let flag = |call: &[String], name: &str| {
            let at = call.iter().position(|a| a == name)?;
            call.get(at + 1).cloned()
        };
        let what: Vec<String> = calls
            .iter()
            .map(|c| match flag(c, "--state") {
                Some(state) => format!("{} {state}", c[1]),
                None => c[1].clone(),
            })
            .collect();
        assert_eq!(
            what,
            [
                "report-agent working",
                "report-metadata",
                "rename",
                "report-agent blocked",
                "report-agent idle",
                "release-agent",
            ]
        );
        let first = &calls[0];
        assert_eq!(first[2], "w1:p2");
        assert_eq!(flag(first, "--source").as_deref(), Some("sonny"));
        assert_eq!(flag(first, "--agent").as_deref(), Some("sonny"));
        assert_eq!(
            flag(first, "--agent-session-id").as_deref(),
            Some("shop.fix-cart")
        );
        let resume = first.iter().position(|a| a == "--").unwrap();
        assert_eq!(first[resume + 1..], ["sonny", "--agent", "shop.fix-cart"]);
        assert_eq!(
            flag(&calls[1], "--display-agent").as_deref(),
            Some("Sonny: fix-cart")
        );
        assert_eq!(calls[2], ["agent", "rename", "w1:p2", "shop-fix-cart"]);
        assert_eq!(
            flag(&calls[3], "--message").as_deref(),
            Some("waiting for your approval")
        );
        // Every report is numbered above the one before it.
        let seqs: Vec<u64> = (calls.iter())
            .filter(|c| c[1] != "report-metadata" && c[1] != "rename")
            .map(|c| flag(c, "--seq").unwrap().parse().unwrap())
            .collect();
        assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
    }

    #[test]
    fn a_herdr_without_resume_commands_still_lists_sonny() {
        let (bin, dir) = fake(
            "before-resume",
            json!({"n": 0, "workspaces": [], "panes": [], "before_resume": true}),
        );
        let me: &'static Presence = Box::leak(Box::new(Presence {
            bin,
            pane: "w1:p2".into(),
            shown: "Sonny: lead".into(),
            name: agent_name("shop.lead"),
            bot: "shop.lead".into(),
            resume: Some(vec!["sonny".into()]),
            no_resume: AtomicBool::new(false),
            seq: AtomicU64::new(0),
            named: AtomicBool::new(false),
        }));
        for state in ["working", "idle"] {
            me.report(state).unwrap().join().unwrap();
        }
        let what: Vec<(String, bool)> = calls(&dir)
            .iter()
            .map(|c| (c[1].clone(), c.iter().any(|a| a == "--")))
            .collect();
        let call = |name: &str, resume| (name.to_owned(), resume);
        // Refused with its resume command once, then reported without it.
        assert_eq!(
            what,
            [
                call("report-agent", true),
                call("report-agent", false),
                call("report-metadata", false),
                call("rename", false),
                call("report-agent", false),
            ]
        );
    }

    #[test]
    fn herdr_names_and_resume_commands_follow_its_rules() {
        assert_eq!(
            agent_name("shop.fix-cart").as_deref(),
            Some("shop-fix-cart")
        );
        assert_eq!(
            agent_name("Shop.Web.lead").as_deref(),
            Some("shop-web-lead")
        );
        assert_eq!(agent_name("2048.lead"), None);
        assert_eq!(agent_name(&"a".repeat(40)).unwrap().len(), 32);
        let words = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert!(resumable(words("sh --agent shop.lead")).is_some());
        assert!(resumable(words("no-such-program-sonny --agent x")).is_none());
        assert!(resumable(words("/bin/sh --agent x")).is_none());
        assert!(resumable(words("sh --workspace it's")).is_none());
        assert!(resumable(vec!["sh".into(); 65]).is_none());
    }

    #[test]
    fn an_agent_lays_work_out_in_the_workspace_its_lead_was_last_in() {
        let state = json!({
            "n": 10,
            "workspaces": [
                {"workspace_id": "w1", "label": "shop"},
                {"workspace_id": "w2", "label": "notes"},
            ],
            "panes": [
                {"pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1", "label": "shop.lead"},
            ],
        });
        let (bin, _) = fake("kept", state);
        let herdr = Herdr::open(bin.clone(), "shop", None, Some("w2")).unwrap();
        assert_eq!(herdr.workspace, "w2");
        assert_eq!(
            herdr.place(),
            Place::Herdr {
                workspace: "w2".into()
            }
        );
        // A kept workspace closed since: the lead's, as before.
        let herdr = Herdr::open(bin, "shop", None, Some("w9")).unwrap();
        assert_eq!(herdr.workspace, "w1");
    }

    #[test]
    fn sides_follow_the_panes_centres() {
        let r = |x: u16, y: u16, w: u16, h: u16| json!({"x": x, "y": y, "width": w, "height": h});
        assert_eq!(side_of(&r(60, 0, 60, 40), &r(0, 0, 60, 40)), "right of");
        assert_eq!(side_of(&r(0, 26, 120, 14), &r(0, 0, 120, 26)), "below");
        assert_eq!(side_of(&r(0, 0, 120, 26), &r(0, 26, 120, 14)), "above");
    }
}
