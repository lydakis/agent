//! cmux (cmux.com) as the place a project's work is laid out: one cmux
//! workspace per project, and in it a tab for each task the lead starts,
//! beside the lead's own tab, titled with the bot's name. cmux's tabs are
//! its surfaces, kept in the panes a workspace is split into, so a task's
//! tab goes in the lead's pane and a split makes a pane of its own. Every
//! call goes through `cmux rpc`, which prints the socket API's JSON result;
//! ids are kept because titles need not be unique. Written against cmux
//! 0.65's v2 socket API (`system.tree`, `workspace.create`,
//! `surface.create`, `surface.split`, `surface.close`, `tab.action`).
//!
//! cmux shows what Sonny is doing without any of this: the program status
//! Sonny writes to its terminal (OSC 7501) is the workspace's status row,
//! and its notifications (OSC 777) ring the pane they came from.
use crate::mux::{Backend, Block, Heard, Place, Places, Stream, Watch};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
};

const MAC_APP: &str = "/Applications/cmux.app/Contents/Resources/bin/cmux";

/// A cmux terminal's own place, which a call naming another must not be
/// read as; the socket's path and the terminal's capability, which let a
/// process cmux started connect, are kept.
const PLACE_VARS: [&str; 4] = [
    "CMUX_WORKSPACE_ID",
    "CMUX_SURFACE_ID",
    "CMUX_TAB_ID",
    "CMUX_PANEL_ID",
];

/// The cmux CLI: the one the terminal was given (`CMUX_BUNDLED_CLI_PATH`),
/// else `cmux` on the PATH, else the Mac app's.
pub fn find() -> Option<PathBuf> {
    if let Some(cmux) = std::env::var_os("CMUX_BUNDLED_CLI_PATH").filter(|c| !c.is_empty()) {
        return Some(PathBuf::from(cmux));
    }
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("cmux"))
            .find(|cmux| cmux.is_file())
    });
    on_path.or_else(|| Path::new(MAC_APP).is_file().then(|| PathBuf::from(MAC_APP)))
}

/// The cmux surface (terminal tab) Sonny runs in. Its ID stays with it
/// when it is moved to another pane or workspace.
pub fn surface() -> Option<String> {
    std::env::var("CMUX_SURFACE_ID")
        .ok()
        .filter(|s| !s.is_empty())
}

/// A v2 socket method's result.
fn rpc(bin: &Path, method: &str, params: &Value) -> Result<Value, String> {
    let mut command = Command::new(bin);
    for name in PLACE_VARS {
        command.env_remove(name);
    }
    let out = crate::mux::output(command.args(["rpc", method, &params.to_string()]))
        .map_err(|e| format!("cmux: {e}"))?;
    if !out.status.success() {
        // cmux says why, among others that its socket would not take this
        // process: Settings > Automation in cmux decides which may connect.
        return Err(format!(
            "cmux {method}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    if out.stdout.trim_ascii().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("cmux {method}: {e}"))
}

/// Every workspace of every cmux window: its ID, title and panes.
fn workspaces(bin: &Path) -> Result<Vec<Value>, String> {
    let tree = rpc(bin, "system.tree", &json!({"all_windows": true}))?;
    Ok(workspaces_of(&tree).collect())
}

fn workspaces_of(tree: &Value) -> impl Iterator<Item = Value> + '_ {
    tree["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|w| w["workspaces"].as_array().cloned().unwrap_or_default())
}

/// A workspace's surfaces, pane by pane: (pane, surface, title).
fn surfaces(workspace: &Value) -> impl Iterator<Item = (&str, &str, &str)> {
    workspace["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|pane| {
            let id = pane["id"].as_str().unwrap_or("");
            pane["surfaces"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(move |s| {
                    Some((id, s["id"].as_str()?, s["title"].as_str().unwrap_or("")))
                })
        })
}

/// Whether the project's lead is in a tab of the cmux this shell reaches.
pub fn has_lead(bin: &Path, project: &str) -> bool {
    let lead = format!("{project}.lead");
    workspaces(bin).is_ok_and(|all| {
        all.iter()
            .any(|w| surfaces(w).any(|(_, _, title)| title == lead))
    })
}

/// The tab Sonny runs in, whose resume command cmux keeps.
struct Resume {
    bin: PathBuf,
    surface: String,
}

static RESUME: OnceLock<Resume> = OnceLock::new();

/// Sonny runs on `bot` in this tab, when it is a cmux tab: cmux keeps
/// `resume` (`sonny` first) as the tab's resume command, which it runs
/// when it restores the tab after a relaunch, as it does for the agents
/// it supports, once the person allows commands that start with `sonny`.
/// In the background, so cmux never slows Sonny down. A Sonny in a Herdr
/// pane inside a cmux tab is Herdr's to restore, not the tab's.
pub fn announce(bot: &str, resume: Vec<String>) {
    if crate::herdr::pane().is_some() {
        return;
    }
    let (Some(surface), Some(bin)) = (surface(), find()) else {
        return;
    };
    let me: &'static Resume = RESUME.get_or_init(|| Resume { bin, surface });
    let on_path = resume.first().is_some_and(|first| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(first).is_file()))
    });
    if !on_path {
        return;
    }
    let bot = bot.to_owned();
    let cwd = std::env::current_dir().unwrap_or_default();
    std::thread::spawn(move || me.set(&bot, &cwd, &resume));
}

/// Sonny quits on its own: the tab is the person's shell again, and cmux
/// restores it as one. A Sonny that goes with cmux keeps its resume
/// command, since it never gets here.
pub fn release() {
    if let Some(me) = RESUME.get() {
        me.clear();
    }
}

impl Resume {
    fn cli(&self) -> Command {
        let mut command = Command::new(&self.bin);
        for name in PLACE_VARS {
            command.env_remove(name);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    /// Whether cmux took the resume command; it shows the tab as
    /// `Sonny: NAME`.
    fn set(&self, bot: &str, cwd: &Path, resume: &[String]) -> bool {
        let short = bot.split_once('.').map_or(bot, |(_, name)| name);
        self.cli()
            .args(["surface", "resume", "set", "--surface", &self.surface])
            .args(["--name", &format!("Sonny: {short}")])
            .args(["--kind", "sonny", "--source", "sonny", "--cwd"])
            .arg(cwd)
            .arg("--")
            .args(resume)
            .status()
            .is_ok_and(|status| status.success())
    }

    fn clear(&self) -> bool {
        self.cli()
            .args(["surface", "resume", "clear", "--surface", &self.surface])
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// What a line of `cmux events` says: an event, or its stream's `ack` or
/// `heartbeat`.
fn heard(line: &Value) -> Heard {
    match line["type"].as_str() {
        Some("event") => Heard::Changed,
        _ => Heard::Same,
    }
}

pub struct Cmux {
    bin: PathBuf,
    workspace: String,
    /// The workspace's title, as the person sees it.
    name: String,
    /// The project's lead, whose pane takes its tasks' tabs.
    lead: String,
    /// Whether the workspace is the one Sonny runs in.
    here: bool,
    /// The surface Sonny runs in.
    me: Option<String>,
    /// Whether the workspace was made just now.
    made: bool,
}

impl Cmux {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        rpc(&self.bin, method, &params)
    }

    /// The project's workspace. From the tab Sonny runs in (`terminal`),
    /// the workspace that tab is in now, so the lead stays where the
    /// person opened it and tasks open beside it. Otherwise, as from an
    /// agent's shell: the one `kept` names while it is open, else the
    /// workspace the project's lead is in, else the one titled with the
    /// project's name, made when there is none.
    pub fn open(
        bin: PathBuf,
        label: &str,
        terminal: Option<&str>,
        kept: Option<&str>,
    ) -> Result<Self, String> {
        let mut cmux = Self {
            bin,
            workspace: String::new(),
            name: label.to_owned(),
            lead: format!("{label}.lead"),
            here: false,
            me: None,
            made: false,
        };
        let all = workspaces(&cmux.bin)?;
        let id = |w: &Value| w["id"].as_str().map(str::to_owned);
        let current = terminal.and_then(|me| {
            all.iter()
                .find(|w| surfaces(w).any(|(_, surface, _)| surface == me))
        });
        let kept = kept.and_then(|kept| all.iter().find(|w| w["id"] == kept));
        let leads = || {
            all.iter()
                .find(|w| surfaces(w).any(|(_, _, title)| title == cmux.lead))
        };
        let found = || all.iter().find(|w| w["title"] == label);
        if current.is_some() {
            cmux.here = true;
            cmux.me = terminal.map(str::to_owned);
        }
        match current.or(kept).or_else(leads).or_else(found) {
            Some(workspace) => {
                cmux.workspace = id(workspace).ok_or("cmux system.tree: no workspace id")?;
                cmux.name = workspace["title"].as_str().unwrap_or(label).to_owned();
            }
            None => {
                let made =
                    cmux.call("workspace.create", json!({"title": label, "focus": false}))?;
                cmux.workspace = made["workspace_id"]
                    .as_str()
                    .ok_or("cmux workspace.create: no workspace_id")?
                    .to_owned();
                cmux.made = true;
            }
        }
        Ok(cmux)
    }

    /// This workspace, as `system.tree` has it.
    fn tree(&self) -> Option<Value> {
        let tree = self
            .call(
                "system.tree",
                json!({"all_windows": true, "workspace_id": self.workspace}),
            )
            .ok()?;
        workspaces_of(&tree).find(|w| w["id"] == self.workspace.as_str())
    }

    /// The pane a task's tab goes in: the lead's, else this tab's, else the
    /// workspace's first.
    fn home_pane(&self, blocks: &[Block]) -> Option<String> {
        let mine = |b: &&Block| self.me.as_ref() == Some(&b.id);
        (blocks.iter().find(|b| b.label == self.lead))
            .or_else(|| blocks.iter().find(mine))
            .or(blocks.first())
            .map(|b| b.window.clone())
    }

    /// The line typed into a new tab's shell: `command` in its place
    /// (`exec`), so the tab goes with it, unless it is to be read once it
    /// exits.
    fn input(command: &[String], keep: bool) -> String {
        let mut line: Vec<String> = command.iter().map(|w| crate::herdr::quote(w)).collect();
        if !keep {
            line.insert(0, "exec".into());
        }
        format!("{}\r", line.join(" "))
    }
}

impl Backend for Cmux {
    fn home(&self) -> String {
        format!("cmux workspace {}", self.name)
    }

    fn here(&self) -> bool {
        self.here
    }

    fn made(&self) -> bool {
        self.made
    }

    /// Each tab, pane by pane: its title is its label, and its pane stands
    /// for its window.
    fn blocks(&self) -> Vec<Block> {
        let Some(workspace) = self.tree() else {
            return Vec::new();
        };
        surfaces(&workspace)
            .map(|(pane, surface, title)| Block {
                id: surface.to_owned(),
                label: title.to_owned(),
                window: pane.to_owned(),
            })
            .collect()
    }

    fn close_block(&self, block: &str) {
        let _ = self.call(
            "surface.close",
            json!({"workspace_id": self.workspace, "surface_id": block, "force": true}),
        );
    }

    /// A tab's title, which stays over the one its program sets.
    fn rename_block(&self, block: &str, label: &str) {
        let _ = self.call(
            "tab.action",
            json!({"action": "rename", "workspace_id": self.workspace,
                "surface_id": block, "title": label}),
        );
    }

    /// A pane has no name of its own: its tabs' titles are the bots'
    /// labels, and what each is doing is the workspace's status row.
    fn rename_window(&self, _: &str, _: &str) {}

    fn window(
        &self,
        label: &str,
        _: &str,
        cwd: &Path,
        command: &[String],
        keep: bool,
    ) -> Result<String, String> {
        let mut params = json!({
            "workspace_id": self.workspace,
            "initial_input": Self::input(command, keep),
            "focus": false,
        });
        if let Some(pane) = self.home_pane(&self.blocks()) {
            params["pane_id"] = pane.into();
        }
        if keep {
            params["working_directory"] = cwd.to_string_lossy().into();
        }
        let made = self.call("surface.create", params)?;
        let surface = made["surface_id"]
            .as_str()
            .ok_or("cmux surface.create: no surface_id")?
            .to_owned();
        self.rename_block(&surface, label);
        Ok(surface)
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
        // The divider's place from the left or top: a small split keeps
        // most of the room for the tab it is beside.
        let (direction, divider) = match side {
            "right" => ("right", 0.65),
            "left" => ("left", 0.35),
            "below" => ("down", 0.65),
            "above" => ("up", 0.35),
            _ => return Err(format!("no side {side}: right, left, above or below")),
        };
        let mut params = json!({
            "workspace_id": self.workspace,
            "surface_id": anchor,
            "direction": direction,
            "initial_input": Self::input(command, keep),
            "focus": false,
        });
        if small {
            params["initial_divider_position"] = divider.into();
        }
        if keep {
            params["working_directory"] = cwd.to_string_lossy().into();
        }
        let made = self.call("surface.split", params)?;
        let surface = made["surface_id"]
            .as_str()
            .ok_or("cmux surface.split: no surface_id")?
            .to_owned();
        self.rename_block(&surface, label);
        Ok(surface)
    }

    fn own_block(&self) -> Option<String> {
        self.me.clone()
    }

    fn place(&self) -> Place {
        Place::Cmux {
            workspace: self.workspace.clone(),
        }
    }

    /// cmux's heartbeats keep the stream from outliving Sonny: the first
    /// one written after it is gone ends the command.
    fn watch(&self) -> Option<Watch> {
        let mut command = Command::new(&self.bin);
        for name in PLACE_VARS {
            command.env_remove(name);
        }
        command.args(["events", "--category", "workspace"]);
        command.args(["--category", "pane", "--category", "surface"]);
        Some(Watch {
            from: Stream::Command(command),
            heard: Box::new(heard),
        })
    }

    /// Where each tab sits, by label: its number in its pane, and when the
    /// workspace is split, which side of the first other pane its pane is.
    fn places(&self) -> Places {
        let mut places = HashMap::new();
        let Some(workspace) = self.tree() else {
            return places;
        };
        let panes: Vec<&Value> = workspace["panes"]
            .as_array()
            .into_iter()
            .flatten()
            .collect();
        let frames: HashMap<String, Value> = match panes.len() {
            0 | 1 => HashMap::new(),
            _ => self
                .call("pane.list", json!({"workspace_id": self.workspace}))
                .ok()
                .and_then(|listed| listed["panes"].as_array().cloned())
                .into_iter()
                .flatten()
                .filter_map(|p| Some((p["id"].as_str()?.to_owned(), p["pixel_frame"].clone())))
                .collect(),
        };
        let label = |pane: &Value| {
            let selected = pane["selected_surface_id"].clone();
            let surfaces = pane["surfaces"].as_array()?;
            let shown = (surfaces.iter())
                .find(|s| s["id"] == selected)
                .or(surfaces.first())?;
            shown["title"].as_str().map(str::to_owned)
        };
        for pane in &panes {
            let id = pane["id"].as_str().unwrap_or("");
            let side = panes
                .iter()
                .find(|other| other["id"] != pane["id"])
                .and_then(|other| {
                    let side = side_of(frames.get(id)?, frames.get(other["id"].as_str()?)?);
                    Some((side.to_owned(), label(other)?))
                });
            for (n, surface) in pane["surfaces"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                if let Some(title) = surface["title"].as_str() {
                    places.insert(title.to_owned(), (n + 1, side.clone()));
                }
            }
        }
        places
    }
}

/// Which side of `other` a pane at `frame` is, by their centres, in pixels.
fn side_of(frame: &Value, other: &Value) -> &'static str {
    let centre = |r: &Value, at: &str, size: &str| {
        r[at].as_f64().unwrap_or(0.0) + r[size].as_f64().unwrap_or(0.0) / 2.0
    };
    let dx = centre(frame, "x", "width") - centre(other, "x", "width");
    let dy = centre(frame, "y", "height") - centre(other, "y", "height");
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

    #[test]
    fn cmux_news_is_its_events_not_its_heartbeats() {
        assert_eq!(heard(&json!({"type": "ack", "version": 1})), Heard::Same);
        assert_eq!(heard(&json!({"type": "heartbeat"})), Heard::Same);
        let closed = json!({"type": "event", "name": "surface.closed", "category": "surface"});
        assert_eq!(heard(&closed), Heard::Changed);
    }
    use crate::mux::Layout;

    /// A stand-in cmux over a JSON file of workspaces, panes and tabs, that
    /// logs each call it gets and answers as `cmux rpc` does.
    const FAKE: &str = r#"#!/usr/bin/env python3
import json, os, sys
here = os.path.dirname(os.path.abspath(sys.argv[0]))
state = json.load(open(os.path.join(here, "state.json")))
args = sys.argv[1:]
open(os.path.join(here, "calls"), "a").write(json.dumps(args) + "\n")
def save():
    json.dump(state, open(os.path.join(here, "state.json"), "w"))
if args[0] != "rpc":
    save()
    sys.exit(0)
method, params = args[1], json.loads(args[2])
def new_id(kind):
    state["n"] += 1
    return "%s-%d" % (kind, state["n"])
def find(surface):
    for w in state["workspaces"]:
        for p in w["panes"]:
            for s in p["surfaces"]:
                if s["id"] == surface:
                    return w, p, s
def tab(title="zsh"):
    return {"id": new_id("sf"), "title": title}
out = {}
if method == "system.tree":
    ws = [w for w in state["workspaces"] if params.get("workspace_id") in (None, w["id"])]
    out = {"windows": [{"id": "win-1", "workspaces": ws}]}
elif method == "workspace.create":
    s = tab()
    w = {"id": new_id("ws"), "title": params["title"],
         "panes": [{"id": new_id("pn"), "surfaces": [s], "selected_surface_id": s["id"]}]}
    state["workspaces"].append(w)
    out = {"workspace_id": w["id"], "surface_id": s["id"]}
elif method == "surface.create":
    w = [w for w in state["workspaces"] if w["id"] == params["workspace_id"]][0]
    p = [p for p in w["panes"] if p["id"] == params.get("pane_id")][0]
    s = tab()
    p["surfaces"].append(s)
    out = {"workspace_id": w["id"], "pane_id": p["id"], "surface_id": s["id"]}
elif method == "surface.split":
    w, p, _ = find(params["surface_id"])
    s = tab()
    w["panes"].insert(w["panes"].index(p) + 1,
        {"id": new_id("pn"), "surfaces": [s], "selected_surface_id": s["id"]})
    out = {"workspace_id": w["id"], "surface_id": s["id"]}
elif method == "surface.close":
    w, p, s = find(params["surface_id"])
    p["surfaces"].remove(s)
    if not p["surfaces"]:
        w["panes"].remove(p)
elif method == "tab.action":
    find(params["surface_id"])[2]["title"] = params["title"]
elif method == "pane.list":
    w = [w for w in state["workspaces"] if w["id"] == params["workspace_id"]][0]
    out = {"panes": [{"id": p["id"], "pixel_frame": p.get("frame")} for p in w["panes"]]}
save()
print(json.dumps(out))
"#;

    fn fake(name: &str, state: Value) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sonny-cmux-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("cmux");
        crate::testing::script(&bin, FAKE);
        std::fs::write(dir.join("state.json"), state.to_string()).unwrap();
        (bin, dir)
    }

    fn calls(dir: &Path) -> Vec<Vec<String>> {
        std::fs::read_to_string(dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// The last call of `method`, its params.
    fn last(dir: &Path, method: &str) -> Value {
        let call = calls(dir)
            .into_iter()
            .rfind(|c| c[0] == "rpc" && c[1] == method)
            .unwrap();
        serde_json::from_str(&call[2]).unwrap()
    }

    /// Each pane's tab titles, workspace by workspace.
    fn titles(dir: &Path) -> Vec<Vec<Vec<String>>> {
        let state: Value =
            serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
        let list = |v: &Value, key: &str| v[key].as_array().cloned().unwrap_or_default();
        list(&state, "workspaces")
            .iter()
            .map(|w| {
                list(w, "panes")
                    .iter()
                    .map(|p| {
                        (list(p, "surfaces").iter())
                            .map(|s| s["title"].as_str().unwrap().to_owned())
                            .collect()
                    })
                    .collect()
            })
            .collect()
    }

    fn pane(id: &str, tabs: &[(&str, &str)]) -> Value {
        let surfaces: Vec<Value> = (tabs.iter())
            .map(|(id, title)| json!({"id": id, "title": title}))
            .collect();
        json!({"id": id, "surfaces": surfaces, "selected_surface_id": tabs[0].0})
    }

    #[test]
    fn an_agent_finds_its_leads_workspace_and_its_commands_close_with_it() {
        let (bin, dir) = fake(
            "home",
            json!({
                "n": 10,
                "workspaces": [
                    {"id": "ws-1", "title": "lydakis",
                     "panes": [pane("pn-1", &[("sf-1", "graft.lead"), ("sf-2", "graft.fix")])]},
                    {"id": "ws-2", "title": "graft", "panes": [pane("pn-2", &[("sf-3", "zsh")])]},
                ],
            }),
        );
        let cmux = Cmux::open(bin, "graft", None, None).unwrap();
        assert_eq!(
            (cmux.workspace.as_str(), cmux.home().as_str()),
            ("ws-1", "cmux workspace lydakis")
        );
        let mut layout = Layout::new(Box::new(cmux));
        layout
            .split(
                "graft.fix",
                "tests",
                Path::new("/work"),
                &["cargo".into(), "test it's".into()],
                Some("below"),
            )
            .unwrap();
        assert_eq!(
            last(&dir, "surface.split"),
            json!({"workspace_id": "ws-1", "surface_id": "sf-2", "direction": "down",
                "initial_input": "cargo 'test it'\\''s'\r", "focus": false,
                "initial_divider_position": 0.65, "working_directory": "/work"})
        );
        // A task's own Sonny runs in its tab's place, in the lead's pane.
        let sonny = ["/bin/sonny".into(), "--agent".into(), "graft.docs".into()];
        assert!(layout.show("graft.docs", "docs", &sonny).unwrap());
        let made = last(&dir, "surface.create");
        assert_eq!(
            (&made["pane_id"], &made["initial_input"]),
            (
                &json!("pn-1"),
                &json!("exec /bin/sonny --agent graft.docs\r")
            )
        );
        assert_eq!(made.get("working_directory"), None);
        assert_eq!(
            titles(&dir)[0],
            [
                vec!["graft.lead", "graft.fix", "graft.docs"],
                vec!["graft.fix/tests"]
            ]
        );
        assert!(
            calls(&dir)
                .iter()
                .all(|c| !c.iter().any(|a| a.contains("ws-2")))
        );
        layout.close("graft.fix");
        assert_eq!(titles(&dir)[0], [vec!["graft.lead", "graft.docs"]]);
        assert_eq!(last(&dir, "surface.close")["force"], true);
    }

    #[test]
    fn sonny_finds_its_tab_after_it_moved_and_names_it() {
        let (bin, dir) = fake(
            "moved",
            json!({
                "n": 10,
                "workspaces": [
                    {"id": "ws-1", "title": "notes", "panes": [pane("pn-1", &[("sf-1", "zsh")])]},
                    {"id": "ws-2", "title": "shop", "panes": [pane("pn-2", &[("sf-7", "zsh")])]},
                ],
            }),
        );
        let cmux = Cmux::open(bin, "shop", Some("sf-7"), None).unwrap();
        assert!(cmux.here());
        assert_eq!(cmux.workspace, "ws-2");
        let mut layout = Layout::new(Box::new(cmux));
        layout.adopt_own("shop.lead");
        let sleep = ["sleep".to_owned(), "60".into()];
        assert!(layout.show("shop.fix", "fix", &sleep).unwrap());
        assert!(!layout.show("shop.fix", "fix", &sleep).unwrap());
        assert_eq!(
            titles(&dir),
            [vec![vec!["zsh"]], vec![vec!["shop.lead", "shop.fix"]]]
        );
    }

    #[test]
    fn a_project_without_a_workspace_gets_one_holding_only_its_lead() {
        let (bin, dir) = fake("new", json!({"n": 0, "workspaces": []}));
        let cmux = Cmux::open(bin, "shop", None, None).unwrap();
        assert_eq!(cmux.home(), "cmux workspace shop");
        assert!(!cmux.here());
        assert_eq!(
            last(&dir, "workspace.create"),
            json!({"title": "shop", "focus": false})
        );
        let mut layout = Layout::new(Box::new(cmux));
        let sonny = ["sonny".to_owned()];
        assert!(layout.show("shop.lead", "lead", &sonny).unwrap());
        assert_eq!(titles(&dir), [vec![vec!["shop.lead"]]]);
    }

    #[test]
    fn an_agent_lays_work_out_in_the_workspace_its_lead_was_last_in() {
        let state = json!({
            "n": 10,
            "workspaces": [
                {"id": "ws-1", "title": "shop", "panes": [pane("pn-1", &[("sf-1", "shop.lead")])]},
                {"id": "ws-2", "title": "notes", "panes": [pane("pn-2", &[("sf-2", "zsh")])]},
            ],
        });
        let (bin, _) = fake("kept", state);
        let cmux = Cmux::open(bin.clone(), "shop", None, Some("ws-2")).unwrap();
        assert_eq!(
            cmux.place(),
            Place::Cmux {
                workspace: "ws-2".into()
            }
        );
        // A kept workspace closed since: the lead's, as before.
        let cmux = Cmux::open(bin.clone(), "shop", None, Some("ws-9")).unwrap();
        assert_eq!(cmux.workspace, "ws-1");
        assert!(has_lead(&bin, "shop"));
        assert!(!has_lead(&bin, "notes"));
    }

    #[test]
    fn places_count_tabs_in_their_pane_and_say_which_side_a_split_is() {
        let mut lead = pane("pn-1", &[("sf-1", "shop.lead"), ("sf-2", "shop.fix")]);
        lead["frame"] = json!({"x": 0, "y": 0, "width": 800, "height": 900});
        let mut right = pane("pn-2", &[("sf-3", "shop.lead/server")]);
        right["frame"] = json!({"x": 800, "y": 0, "width": 400, "height": 900});
        let (bin, _) = fake(
            "places",
            json!({"n": 10, "workspaces": [{"id": "ws-1", "title": "shop", "panes": [lead, right]}]}),
        );
        let places = Cmux::open(bin, "shop", None, None).unwrap().places();
        let side = |s: &str, of: &str| Some((s.to_owned(), of.to_owned()));
        assert_eq!(
            places["shop.lead"],
            (1, side("left of", "shop.lead/server"))
        );
        assert_eq!(places["shop.fix"], (2, side("left of", "shop.lead/server")));
        assert_eq!(
            places["shop.lead/server"],
            (1, side("right of", "shop.lead"))
        );
    }

    #[test]
    fn cmux_keeps_how_to_reopen_sonny_until_it_quits() {
        let (bin, dir) = fake("resume", json!({"n": 0, "workspaces": []}));
        let me = Resume {
            bin,
            surface: "sf-4".into(),
        };
        let resume = ["sonny".to_owned(), "--agent".into(), "shop.fix-cart".into()];
        assert!(me.set("shop.fix-cart", Path::new("/work"), &resume));
        assert!(me.clear());
        let calls = calls(&dir);
        assert_eq!(
            calls[0],
            [
                "surface",
                "resume",
                "set",
                "--surface",
                "sf-4",
                "--name",
                "Sonny: fix-cart",
                "--kind",
                "sonny",
                "--source",
                "sonny",
                "--cwd",
                "/work",
                "--",
                "sonny",
                "--agent",
                "shop.fix-cart"
            ]
        );
        assert_eq!(
            calls[1],
            ["surface", "resume", "clear", "--surface", "sf-4"]
        );
    }

    #[test]
    fn sides_follow_the_panes_centres() {
        let r = |x: u16, y: u16, w: u16, h: u16| json!({"x": x, "y": y, "width": w, "height": h});
        assert_eq!(
            side_of(&r(600, 0, 600, 800), &r(0, 0, 600, 800)),
            "right of"
        );
        assert_eq!(side_of(&r(0, 500, 1200, 300), &r(0, 0, 1200, 500)), "below");
        assert_eq!(side_of(&r(0, 0, 1200, 500), &r(0, 500, 1200, 300)), "above");
    }
}
