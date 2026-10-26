//! Herdr (herdr.dev) as the place a project's work is laid out: one Herdr
//! workspace per project, a tab in it per task the lead starts, and panes
//! labelled with each bot's name. Every call goes through the `herdr` CLI,
//! which prints the socket API's JSON reply; ids are kept because labels
//! need not be unique. Written against herdr 0.9.5's CLI and API schema.
use crate::mux::{Backend, Block, Places};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
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

/// Whether Sonny runs in a Herdr pane.
pub fn inside() -> bool {
    std::env::var_os("HERDR_PANE_ID").is_some_and(|p| !p.is_empty())
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

pub struct Herdr {
    bin: PathBuf,
    workspace: String,
    /// The workspace's label, as the person sees it.
    name: String,
    /// Whether the workspace is the one Sonny runs in.
    here: bool,
}

/// A word as a POSIX shell reads it back, for `pane run`, which types its
/// command into the pane's shell.
fn quote(word: &str) -> String {
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
        let reply: Value =
            serde_json::from_slice(&out.stdout).map_err(|e| format!("herdr {what}: {e}"))?;
        Ok(reply["result"].clone())
    }

    /// The project's workspace. From the terminal Sonny runs in
    /// (`terminal`), that pane's workspace, so the lead stays where the
    /// person opened it and tasks open beside it. Otherwise, as from an
    /// agent's shell: the workspace the project's lead is in, else the one
    /// labelled with the project's name, made when there is none.
    pub fn open(bin: PathBuf, label: &str, terminal: bool) -> Result<Self, String> {
        let mut herdr = Self {
            bin,
            workspace: String::new(),
            name: label.to_owned(),
            here: false,
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
        let mine = std::env::var("HERDR_WORKSPACE_ID").unwrap_or_default();
        let current = workspaces
            .iter()
            .find(|(id, _)| terminal && !mine.is_empty() && *id == mine);
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
        (herdr.workspace, herdr.name) = match current {
            Some((id, now)) => {
                herdr.here = true;
                (id.clone(), now.clone())
            }
            None => match leads().or_else(found) {
                Some((id, now)) => (id.clone(), now.clone()),
                None => {
                    let made =
                        herdr.call(&["workspace", "create", "--label", label, "--no-focus"])?;
                    let id = made["workspace"]["workspace_id"]
                        .as_str()
                        .ok_or("herdr workspace create: no workspace_id")?;
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
        let mine = std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|p| !p.is_empty())?;
        self.blocks()
            .into_iter()
            .find(|b| b.id == mine)
            .map(|b| b.id)
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
else:
    out = {"type": "ok"}
json.dump(state, open(os.path.join(here, "state.json"), "w"))
print(json.dumps({"id": "cli:request", "result": out}))
"#;

    fn fake(name: &str, state: Value) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sonny-herdr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("herdr");
        std::fs::write(&bin, FAKE).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
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
        let herdr = Herdr::open(bin, "graft", false).unwrap();
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
                false,
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
                &["/bin/sonny".into(), "--bot".into(), "graft.docs".into()],
            )
            .unwrap();
        let last = super::tests::calls(&dir).pop().unwrap();
        assert_eq!(last[..2], ["pane", "run"]);
        assert_eq!(last[3], "exec /bin/sonny --bot graft.docs");
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
    fn a_project_without_a_workspace_gets_one() {
        let (bin, dir) = fake("new", json!({"n": 0, "workspaces": [], "panes": []}));
        let herdr = Herdr::open(bin, "shop", false).unwrap();
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
        let herdr = Herdr::open(bin, "sonnycheck", false).unwrap();
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
                false,
            )
            .unwrap();
        layout
            .split(
                "sonnycheck.lead",
                "server",
                Path::new("/tmp"),
                &["echo".into(), "serving".into()],
                true,
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
        let check = Herdr::open(find().unwrap(), "sonnycheck", false).unwrap();
        let read = check
            .call(&[
                "pane",
                "read",
                &tests,
                "--source",
                "recent-unwrapped",
                "--lines",
                "20",
            ])
            .unwrap_or(Value::Null);
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
    fn sides_follow_the_panes_centres() {
        let r = |x: u16, y: u16, w: u16, h: u16| json!({"x": x, "y": y, "width": w, "height": h});
        assert_eq!(side_of(&r(60, 0, 60, 40), &r(0, 0, 60, 40)), "right of");
        assert_eq!(side_of(&r(0, 26, 120, 14), &r(0, 0, 120, 26)), "below");
        assert_eq!(side_of(&r(0, 0, 120, 26), &r(0, 26, 120, 14)), "above");
    }
}
