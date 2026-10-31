//! Rex (superlogical.com/rex) as the place a project's work is laid out:
//! one Rex session per project, labelled with the project's name, and one
//! window in it per task the lead starts, running Sonny on that task. Every
//! call goes through the `rex` CLI with `--json`; ids are kept because
//! labels need not be unique. Checked against rex 0.0.1792787366 (API rev 7)
//! for `ls`, `new`, `split`, `block ls` and `block close`; `run`'s output is
//! read the way `split`'s is.
use crate::mux::{Backend, Block, Heard, Place, Places, Stream, Watch};
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

/// What a line of `rex events -s SESSION --json` says. A session's event
/// comes wrapped in a `session_event`; a terminal's, once more, in a
/// `block_event`, which moves no block, as focus, ownership and clients
/// coming and going (each CLI call is one) don't. A block renamed says
/// only `session_view_changed`. The stream goes on once its session is
/// gone, so `session_destroyed` ends it.
fn heard(line: &Value) -> Heard {
    if line["type"] == "session_destroyed" {
        return Heard::Over;
    }
    match line["payload"]["event"]["type"].as_str() {
        Some(
            "block_event"
            | "block_owner_changed"
            | "active_window_changed"
            | "client_connected"
            | "client_disconnected"
            | "client_connection_changed",
        ) => Heard::Same,
        _ => Heard::Changed,
    }
}

/// Where a floating block sits over its window, as fractions of it: the
/// middle, with a tenth of the window around it.
const FLOATING: [f64; 4] = [0.1, 0.1, 0.8, 0.8];

pub struct Rex {
    bin: PathBuf,
    /// `-S HOST` when the session lives on another Rex server.
    host: Vec<String>,
    session: String,
    /// The session's label, as the person sees it.
    name: String,
    /// Whether the session is the one Sonny runs in.
    here: bool,
    /// Whether the session was made just now.
    made: bool,
}

impl Rex {
    fn call(&self, args: &[&str]) -> Result<Value, String> {
        // Every call names its session: the shell's own Rex variables may
        // name another (a daemon's), and rex would look a bare ID up there.
        let out = crate::mux::output(
            Command::new(&self.bin)
                .env_remove("REX_SESSION")
                .env_remove("REX_BLOCK")
                .args(&self.host)
                .args(args),
        )
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
    /// would name the daemon's terminal: the one `kept` names while it is
    /// open, else the session the project's lead is in, else the one
    /// labelled with the project's name, made when there is none.
    pub fn open(
        bin: PathBuf,
        host: Option<&str>,
        label: &str,
        terminal: bool,
        kept: Option<&str>,
    ) -> Result<Self, String> {
        let mut rex = Self {
            bin,
            host: host
                .map(|h| vec!["-S".into(), h.into()])
                .unwrap_or_default(),
            session: String::new(),
            name: label.to_owned(),
            here: false,
            made: false,
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
        let kept = kept.and_then(|kept| sessions.iter().find(|(id, _)| id == kept));
        let lead = format!("{label}.lead");
        let leads = || {
            sessions.iter().find(|(id, _)| {
                (rex.blocks_in(id).unwrap_or_default().iter()).any(|b| b.label == lead)
            })
        };
        (rex.session, rex.name) = match current {
            Some((id, now)) => {
                rex.here = true;
                (id.clone(), now.clone())
            }
            None => match kept.or_else(leads).or(found) {
                Some((id, now)) => (id.clone(), now.clone()),
                None => {
                    let made = rex.call(&["new", label, "--json"])?;
                    let id = made["session_id"]
                        .as_str()
                        .ok_or("rex new: no session_id")?;
                    rex.made = true;
                    (id.to_owned(), label.to_owned())
                }
            },
        };
        Ok(rex)
    }

    fn blocks_in(&self, session: &str) -> Result<Vec<Block>, String> {
        let ls = self.call(&["block", "ls", "-s", session, "--json"])?;
        Ok(ls["blocks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| {
                Some(Block {
                    id: b["block_id"].as_str()?.to_owned(),
                    label: b["label"].as_str().unwrap_or("").to_owned(),
                    window: b["window_id"].as_str().unwrap_or("").to_owned(),
                })
            })
            .collect())
    }

    /// The new block's ID from `split`'s, `run`'s or `window new`'s reply.
    fn made(&self, args: &[&str]) -> Result<(String, Value), String> {
        let made = self.call(args)?;
        let block = made["block_ids"][0]
            .as_str()
            .ok_or("rex: no block id in the reply")?
            .to_owned();
        Ok((block, made))
    }
}

impl Backend for Rex {
    fn home(&self) -> String {
        format!("Rex session {}", self.name)
    }

    fn here(&self) -> bool {
        self.here
    }

    fn made(&self) -> bool {
        self.made
    }

    fn blocks(&self) -> Result<Vec<Block>, String> {
        self.blocks_in(&self.session)
    }

    fn close_block(&self, block: &str) {
        let _ = self.call(&["block", "close", block, "-s", &self.session]);
    }

    fn rename_block(&self, block: &str, label: &str) {
        let _ = self.call(&["block", "rename", block, label, "-s", &self.session]);
    }

    fn rename_window(&self, window: &str, name: &str) {
        let _ = self.call(&["window", "rename", window, name, "-s", &self.session]);
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
        let session = self.session.as_str();
        let mut args: Vec<&str> = match keep {
            true => vec![
                "run",
                "-s",
                session,
                "-c",
                &cwd,
                "--label",
                label,
                "--keep-open",
            ],
            false => vec!["window", "new", "-s", session, name, "--shell=none"],
        };
        args.extend(["--focus=false", "--json", "--"]);
        args.extend(command.iter().map(String::as_str));
        let (block, made) = self.made(&args)?;
        match keep {
            true => {
                if let Some(window) = made["window_id"].as_str() {
                    self.rename_window(window, name);
                }
            }
            // `window new` names the window; the block is named after the
            // bot, so it can be found, and anchored on, by that name.
            false => self.rename_block(&block, label),
        }
        Ok(block)
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
        let cwd = cwd.to_string_lossy();
        let mut args = vec!["split", "-b", anchor, "--split", side];
        if small {
            args.extend(["--ratio", "35"]);
        }
        args.extend(["-s", &self.session]);
        match keep {
            true => args.extend(["-c", &cwd, "--label", label, "--keep-open"]),
            false => args.extend(["--label", label, "--shell=none"]),
        }
        args.extend(["--focus=false", "--json", "--"]);
        args.extend(command.iter().map(String::as_str));
        Ok(self.made(&args)?.0)
    }

    /// Split beside `anchor`, then floated over its window, as Rex floats
    /// a block only in its own window. Floating takes neither focus nor
    /// the active window; one Rex refuses stays split.
    fn float(
        &self,
        anchor: &str,
        label: &str,
        cwd: &Path,
        command: &[String],
        keep: bool,
    ) -> Result<String, String> {
        let block = self.split(anchor, "right", false, label, cwd, command, keep)?;
        let float = serde_json::json!({"block_id": block, "bounds": FLOATING});
        let float = float.to_string();
        let _ = self.call(&[
            "api",
            "call",
            "-s",
            &self.session,
            "session.float_block",
            &float,
        ]);
        Ok(block)
    }

    fn own_block(&self) -> Option<String> {
        let mine = std::env::var("REX_BLOCK").ok().filter(|b| !b.is_empty())?;
        (self.blocks().ok()?.into_iter())
            .find(|b| b.id == mine || b.id.ends_with(&mine))
            .map(|b| b.id)
    }

    fn place(&self) -> Place {
        Place::Rex {
            session: self.session.clone(),
            host: self.host.get(1).cloned(),
        }
    }

    fn watch(&self) -> Option<Watch> {
        let mut command = Command::new(&self.bin);
        (command.env_remove("REX_SESSION").env_remove("REX_BLOCK"))
            .args(&self.host)
            .args(["events", "-s", &self.session, "--json"]);
        Some(Watch {
            from: Stream::Command(command),
            heard: Box::new(heard),
        })
    }

    /// Where each block sits, by label: its tab, counted from 1, and in a
    /// split, its side of its neighbour (`right of`, `below`, … a label).
    fn places(&self) -> Result<Places, String> {
        let mut places = HashMap::new();
        let ls = self.call(&["window", "ls", "-s", &self.session, "--json"])?;
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
        Ok(places)
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    #[test]
    fn rex_news_of_a_terminal_or_focus_moves_no_block() {
        use crate::mux::Heard;
        let event = |kind: &str| {
            serde_json::json!({"type": "session_event", "payload": {
                "session_id": "session:01", "event": {"type": kind, "payload": {}}}})
        };
        assert_eq!(super::heard(&event("block_event")), Heard::Same);
        assert_eq!(super::heard(&event("active_window_changed")), Heard::Same);
        assert_eq!(super::heard(&event("client_connected")), Heard::Same);
        for kind in ["window_created", "block_closed", "session_view_changed"] {
            assert_eq!(super::heard(&event(kind)), Heard::Changed);
        }
        let gone = serde_json::json!({"type": "session_destroyed", "payload": {"session_id": "session:01"}});
        assert_eq!(super::heard(&gone), Heard::Over);
    }

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
elif args[:2] == ["block", "rename"]:
    for v in state.values():
        for b in v["blocks"]:
            if b["block_id"] == args[2]:
                b["label"] = args[3]
elif args[:2] == ["block", "close"]:
    for v in state.values():
        v["blocks"] = [b for b in v["blocks"] if b["block_id"] != args[2]]
elif args[0] == "new":
    state["01made"] = {"label": args[1], "blocks": [{"block_id": "block:shell", "label": "", "window_id": "window:0"}]}
    out = {"session_id": "01made"}
elif args[0] in ("split", "run") or args[:2] == ["window", "new"]:
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
        crate::testing::script(&bin, FAKE);
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
    fn a_lead_opened_from_another_gets_a_session_of_its_own() {
        let (bin, dir) = fake(
            "lead",
            serde_json::json!({"01ws": {"label": "ws", "blocks": [block("block:ws", "ws.lead")]}}),
        );
        let rex = super::Rex::open(bin, None, "web", false, None).unwrap();
        let mut layout = crate::mux::Layout::new(Box::new(rex));
        let sonny = ["sonny".to_owned(), "--agent".into(), "web.lead".into()];
        assert!(layout.show("web.lead", "lead", &sonny).unwrap());
        // The shell the session came with goes; the other lead's is left.
        assert_eq!(labels(&dir, "01made"), ["web.lead"]);
        assert_eq!(labels(&dir, "01ws"), ["ws.lead"]);
        let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
        assert!(calls.contains("new web --json"), "{calls}");
        assert!(
            calls.contains("window new -s 01made lead --shell=none --focus=false --json -- sonny --agent web.lead"),
            "{calls}"
        );
        // Opened again, the lead's Sonny is there already.
        let rex = super::Rex::open(dir.join("rex"), None, "web", false, None).unwrap();
        let mut layout = crate::mux::Layout::new(Box::new(rex));
        assert!(!layout.show("web.lead", "lead", &sonny).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_command_floats_over_its_owners_window() {
        let (bin, dir) = fake(
            "float",
            serde_json::json!({"01home": {"label": "graft", "blocks": [block("block:task", "graft.fix")]}}),
        );
        let rex = super::Rex::open(bin, None, "graft", false, None).unwrap();
        let mut rex = crate::mux::Layout::new(Box::new(rex));
        let login = ["gh".into(), "auth".into(), "login".into()];
        let made = rex.split(
            "graft.fix",
            "login",
            Path::new("/work"),
            &login,
            Some("float"),
        );
        let made = made.unwrap();
        let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
        let calls: Vec<&str> = calls.lines().rev().take(2).collect();
        // Split beside its owner, then floated, as Rex floats a block only
        // in its own window.
        assert!(
            calls[1].starts_with("split -b block:task --split right")
                && calls[1].contains("--label graft.fix/login --keep-open"),
            "{calls:?}"
        );
        let float = format!(
            r#"api call -s 01home session.float_block {{"block_id":"{made}","bounds":[0.1,0.1,0.8,0.8]}}"#
        );
        assert_eq!(calls[0], float);
        let _ = std::fs::remove_dir_all(&dir);
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
        let rex = super::Rex::open(bin, None, "graft", false, None).unwrap();
        assert_eq!(
            (rex.session.as_str(), rex.name.as_str()),
            ("01home", "lydakis")
        );
        assert!(!rex.here);
        let mut rex = crate::mux::Layout::new(Box::new(rex));

        let cwd = Path::new("/work");
        let tests = rex.split(
            "graft.fix",
            "tests",
            cwd,
            &["cargo".into(), "test".into()],
            Some("below"),
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
        let again = rex.split(
            "graft.fix",
            "tests",
            cwd,
            &["cargo".into(), "test".into()],
            Some("below"),
        );
        assert_ne!(again.unwrap(), tests);
        rex.split(
            "graft.fix",
            "build",
            cwd,
            &["cargo".into(), "build".into()],
            None,
        )
        .unwrap();
        rex.split(
            "graft.lead",
            "server",
            cwd,
            &["serve".into()],
            Some("below"),
        )
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
