//! Where you were: the bot on screen, the open peek, the rail, the folds.
//! Keyed by daemon socket and workspace, so each folder resumes its own
//! seat at the same daemon. Client state only; the daemon never sees it.
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub struct Session {
    pub selected: Option<String>,
    pub peek: Option<String>,
    pub rail: bool,
    pub thoughts: bool,
    pub output: bool,
}

fn file() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".agent").join("tui-sessions.json"))
}

fn key(socket: &Path, workspace: &str) -> String {
    format!("{}|{workspace}", socket.display())
}

fn read_all() -> Map<String, Value> {
    file()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

pub fn load(socket: &Path, workspace: &str) -> Session {
    let all = read_all();
    let Some(entry) = all.get(&key(socket, workspace)) else {
        return Session::default();
    };
    Session {
        selected: entry["selected"].as_str().map(str::to_owned),
        peek: entry["peek"].as_str().map(str::to_owned),
        rail: entry["rail"].as_bool().unwrap_or(false),
        thoughts: entry["thoughts"].as_bool().unwrap_or(false),
        output: entry["output"].as_bool().unwrap_or(false),
    }
}

/// Best effort: a session that cannot be written is simply not resumed.
pub fn save(socket: &Path, workspace: &str, session: &Session) {
    let Some(path) = file() else { return };
    let mut all = read_all();
    all.insert(
        key(socket, workspace),
        json!({"selected": session.selected, "peek": session.peek, "rail": session.rail,
            "thoughts": session.thoughts, "output": session.output}),
    );
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        &path,
        serde_json::to_vec_pretty(&Value::Object(all)).unwrap_or_default(),
    );
}
