//! Swarms are the swarm skill's (`app/skills/swarm`): its script starts,
//! adds to, stops and posts to them, and keeps each one's folder,
//! `~/.agent/swarms/STORE/SWARM/`. The app runs that script for you and
//! reads a board as a file; it knows nothing of a swarm's rules.
use serde_json::{Value, json};
use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

/// What one board read returns at most.
const MAX_READ: u64 = 1024 * 1024;
/// How far back a first read, or one fallen too far behind, starts.
const TAIL: u64 = 256 * 1024;
/// The script's own bound on a name, which is also its folder's.
const MAX_NAME: usize = 100;

/// The swarms of the store whose identity the attached daemon announced.
/// Their agents are that store's bots, so a daemon on another store, even on
/// the same socket, never sees them.
pub fn root(store: &str) -> Result<PathBuf, String> {
    if store.is_empty() || !store.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid_store: {store} is not a store identity"));
    }
    let home = std::env::var_os("HOME").ok_or("no HOME for ~/.agent/swarms")?;
    Ok(PathBuf::from(home).join(".agent/swarms").join(store))
}

/// A swarm's name is `PROJECT.NAME`, as the script names its folder.
fn valid_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(format!("invalid_swarm_name: {name}"))
    }
}

/// The script: yours in `~/.agents/skills/swarm` (where the app links its
/// own), else the one this app ships, else this checkout's.
fn script() -> PathBuf {
    let linked =
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agents/skills/swarm/swarm"));
    let shipped = std::env::current_exe()
        .ok()
        .and_then(|exe| crate::skills::bundled(&exe))
        .map(|skills| skills.join("swarm/swarm"));
    (linked.into_iter().chain(shipped))
        .find(|path| path.is_file())
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../skills/swarm/swarm"))
}

/// Run the script for you: with no agent in its environment it acts as the
/// person, on the window's daemon, with `agent` for the agents it makes. It
/// gets the login shell's ordinary variables, so python3 and git are on
/// PATH, and nothing else: no provider or cloud keys. Its answer, or its
/// refusal as `error: detail`.
pub async fn run(socket: &Path, agent: &Path, args: &[String]) -> Result<Value, String> {
    const KEEP: &[&str] = &[
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TMPDIR", "TERM",
    ];
    let env = match crate::daemon::login().await {
        Some(login) => login.to_vec(),
        None => std::env::vars_os().collect(),
    };
    let output = tokio::process::Command::new(script())
        .args(args)
        .env_clear()
        .envs(env.into_iter().filter(|(key, _)| {
            key.to_str()
                .is_some_and(|key| KEEP.contains(&key) || key.starts_with("LC_"))
        }))
        .env("AGENT_SOCKET", socket)
        .env("AGENT_BIN", agent)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("swarm_script: {e}"))?;
    if output.status.success() {
        return serde_json::from_slice(&output.stdout).map_err(|e| format!("swarm_script: {e}"));
    }
    match serde_json::from_slice::<Value>(&output.stderr) {
        Ok(refused) if refused["error"].is_string() => Err(format!(
            "{}: {}",
            refused["error"].as_str().unwrap_or_default(),
            refused["detail"].as_str().unwrap_or_default()
        )),
        _ => Err(format!(
            "swarm_script: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// The board's complete lines from `offset`, where the next read starts,
/// and what the whole board adds up to, as the script last folded it. With
/// no offset, or one so far behind that the lines between would not be
/// kept, it reads the board's last stretch and says `reset`: the lines
/// replace what the reader has. A line that is not JSON comes back as its
/// text, so a hand edit shows instead of vanishing. Nothing is locked: the
/// script writes whole lines, and replaces the state whole.
pub fn board(root: &Path, swarm: &str, offset: Option<u64>) -> Result<Value, String> {
    valid_name(swarm)?;
    let dir = root.join(swarm);
    let path = dir.join("board.jsonl");
    let mut file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    // Shorter than we read before means someone rewrote it: read it again.
    let (mut start, reset) = match offset {
        Some(offset) if offset <= size && size - offset <= TAIL => (offset, false),
        _ => (size.saturating_sub(TAIL), true),
    };
    file.seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_READ)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let mut text = &bytes[..];
    if reset && start > 0 {
        let cut = text
            .iter()
            .position(|&b| b == b'\n')
            .map_or(text.len(), |at| at + 1);
        start += cut as u64;
        text = &text[cut..];
    }
    let end = text
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |at| at + 1);
    let lines: Vec<Value> = text[..end]
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice(line)
                .unwrap_or_else(|_| json!({"text": String::from_utf8_lossy(line)}))
        })
        .collect();
    let state = std::fs::read(dir.join("state.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    Ok(json!({
        "lines": lines, "offset": start + end as u64, "more": start + (end as u64) < size,
        "reset": reset, "state": state,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_board_is_read_in_whole_lines_from_where_the_last_read_ended() {
        let root = std::env::temp_dir().join(format!("agent-app-board-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("p.widget");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("board.jsonl");
        std::fs::write(
            &path,
            "{\"from\":\"user\",\"text\":\"goal\"}\nnot json\n{\"from\":\"w",
        )
        .unwrap();
        let first = board(&root, "p.widget", None).unwrap();
        assert_eq!(first["lines"][0]["text"], "goal");
        assert_eq!(first["lines"][1]["text"], "not json");
        assert_eq!(first["lines"].as_array().unwrap().len(), 2);
        assert_eq!(
            (&first["reset"], &first["more"]),
            (&json!(true), &json!(true))
        );
        assert_eq!(first["state"], Value::Null);
        // The half line is read once it is whole, with the state beside it.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("idget-1\",\"text\":\"hi\"}\n");
        std::fs::write(&path, text).unwrap();
        std::fs::write(dir.join("state.json"), "{\"offset\":1,\"tasks\":{}}").unwrap();
        let next = board(&root, "p.widget", first["offset"].as_u64()).unwrap();
        assert_eq!(next["lines"], json!([{"from": "widget-1", "text": "hi"}]));
        assert_eq!(
            (&next["reset"], &next["state"]["tasks"]),
            (&json!(false), &json!({}))
        );
        // A board shorter than the reader's offset was rewritten: read again.
        assert_eq!(
            board(&root, "p.widget", Some(1 << 20)).unwrap()["reset"],
            true
        );
        assert!(
            board(&root, "../x", None)
                .unwrap_err()
                .starts_with("invalid_swarm_name")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
