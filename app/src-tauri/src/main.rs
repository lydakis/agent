//! The desktop client's Rust core: a transport between the page and the
//! daemon. The page owns the state model and the protocol logic, exactly as
//! the prototype did; this side connects, forwards notifications as window
//! events, and relays requests. Nothing else lives here.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod session;

use agent_client::Client;
use serde_json::{Value, json};
use session::SessionSlot;
use std::{path::PathBuf, sync::Arc};
use tauri::{Manager, State};
use tokio::sync::Mutex;

struct Config {
    socket: PathBuf,
    model: Option<String>,
    workspace: String,
}

struct Shared {
    config: Config,
    client: Mutex<Option<Arc<Client>>>,
    /// Counts attachments; a pull names the session it reads for, so a
    /// batch from a session the page has left is never mistaken for new.
    session: std::sync::atomic::AtomicU64,
    /// The attached session's notifications. `pull` takes the receiver out
    /// while it waits and puts it back, so an attach never waits on a pull.
    events: Mutex<SessionSlot<agent_client::Events>>,
}

/// Notifications handed to the page per pull. Small enough that the page
/// applies a batch and asks again before the transport's queue matters.
const PULL: usize = 256;

/// Socket selection matches the CLI: --socket, then AGENT_SOCKET, then the
/// daemon's rendezvous for --store, AGENT_STORE, or ~/.agent/state.sqlite,
/// resolved by the shared client crate so a deep store path finds the same
/// short socket the daemon listens on.
fn config() -> Result<Config, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut socket, mut store, mut model, mut workspace) = (None, None, None, None);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let mut value = || {
            let value = inline
                .clone()
                .or_else(|| iter.next().cloned())
                .ok_or_else(|| format!("{flag} needs a value"))?;
            // A flag-looking value must use '=' to be unambiguous, as in the CLI.
            if inline.is_none() && value.starts_with("--") {
                return Err(format!("{flag} needs a value; use {flag}=VALUE"));
            }
            Ok(value)
        };
        match flag {
            "--socket" => socket = Some(PathBuf::from(value()?)),
            "--store" => store = Some(PathBuf::from(value()?)),
            "--model" => model = Some(value()?),
            "--workspace" => workspace = Some(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let socket = match socket {
        Some(socket) => socket,
        None => match (store, std::env::var_os("AGENT_SOCKET")) {
            (None, Some(env)) => PathBuf::from(env),
            (store, _) => {
                let store = store
                    .or_else(|| std::env::var_os("AGENT_STORE").map(PathBuf::from))
                    .or_else(|| {
                        std::env::var_os("HOME")
                            .map(|home| PathBuf::from(home).join(".agent/state.sqlite"))
                    })
                    .ok_or("no store path; pass --socket or --store")?;
                agent_client::socket::default_socket(&store).map_err(|e| e.to_string())?
            }
        },
    };
    let workspace = workspace_path(&match workspace {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    })?;
    Ok(Config {
        socket,
        model: model.or_else(|| std::env::var("AGENT_MODEL").ok()),
        workspace,
    })
}

/// Resolve the default once at startup. The protocol requires an existing
/// absolute UTF-8 directory; lossy conversion could name a different path.
fn workspace_path(path: &std::path::Path) -> Result<String, String> {
    path.to_str().ok_or("--workspace must be valid UTF-8")?;
    let path = path
        .canonicalize()
        .map_err(|e| format!("--workspace {}: {e}", path.display()))?;
    if !path.is_dir() {
        return Err("--workspace must be a directory".into());
    }
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "--workspace must be valid UTF-8".into())
}

#[cfg(test)]
mod config_tests {
    use super::workspace_path;

    #[test]
    fn workspace_defaults_require_an_existing_utf8_directory() {
        let root = std::env::temp_dir().join(format!("agent-app-workspace-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("file");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(
            workspace_path(&root).unwrap(),
            root.canonicalize().unwrap().to_str().unwrap()
        );
        assert!(workspace_path(&file).unwrap_err().contains("directory"));
        assert!(workspace_path(&root.join("missing")).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let invalid = root.join(std::ffi::OsString::from_vec(vec![0xff]));
            assert!(workspace_path(&invalid).unwrap_err().contains("UTF-8"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// What the page needs to create bots and to say where it is.
#[tauri::command]
fn setup(state: State<'_, Shared>) -> Value {
    json!({
        "socket": state.config.socket.to_string_lossy(),
        "model": state.config.model,
        "workspace": state.config.workspace,
        "tools": ["shell", "read", "write", "edit", "wait", "history"],
    })
}

/// The shared client policy for this workspace, composed now so an edited
/// AGENTS.md reaches the next bot: preamble, AGENTS.md files, skills.
#[tauri::command]
fn policy(state: State<'_, Shared>) -> Result<Value, String> {
    compose(std::path::Path::new(&state.config.workspace))
}

/// Too much or unreadable text fails with the CLI's `--agents` code, and
/// `/new` creates nothing: a bot without its workspace's rules is worse
/// than no bot.
fn compose(workspace: &std::path::Path) -> Result<Value, String> {
    let composed = agent_client::policy::instructions(workspace)
        .map_err(|error| format!("{}: {error}", error.code()))?;
    Ok(json!({
        "instructions": composed.text,
        "compaction_instructions": agent_client::policy::DEFAULT_COMPACTION_INSTRUCTIONS,
        "note": format!(
            "preamble + {} AGENTS.md + {} skills",
            composed.sources.len(),
            composed.skills.len()
        ),
    }))
}

#[cfg(test)]
mod policy_tests {
    use super::compose;

    #[test]
    fn a_workspace_policy_that_cannot_compose_is_an_error_not_the_preamble() {
        let root = std::env::temp_dir().join(format!("agent-app-policy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let file = root.join("AGENTS.md");
        std::fs::write(&file, "rule").unwrap();
        let composed = compose(&root).unwrap();
        let rule = format!("{}\n\nrule", file.display());
        assert!(composed["instructions"].as_str().unwrap().contains(&rule));
        std::fs::write(&file, "x".repeat(agent_client::policy::MAX_INSTRUCTIONS)).unwrap();
        let error = compose(&root).unwrap_err();
        assert!(error.starts_with("instructions_limit: "), "{error}");
        assert!(error.contains(file.to_str().unwrap()), "{error}");
        std::fs::write(&file, [0xff, 0xfe]).unwrap();
        let error = compose(&root).unwrap_err();
        assert!(error.starts_with("instructions_unreadable: "), "{error}");
        assert!(error.contains(file.to_str().unwrap()), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// Connect and follow `*` from the page's cursor. The page then pages the
/// snapshot itself through `request` while it pulls the replay, so nothing
/// is staged here: the transport's bounded queue is the only buffer.
#[tauri::command]
async fn attach(state: State<'_, Shared>, after: i64) -> Result<Value, String> {
    // Let the previous session go first: its socket closes, its reader ends,
    // and a pull still waiting on it comes back closed.
    if let Some(old) = state.client.lock().await.take() {
        old.close().await;
    }
    let (client, events) = Client::connect(&state.config.socket)
        .await
        .map_err(|e| e.to_string())?;
    let session = state
        .session
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;
    client
        .request("follow", json!({"bot": "*", "after": after}))
        .await
        .map_err(|e| e.to_string())?;
    *state.client.lock().await = Some(client);
    state.events.lock().await.replace(session, events);
    Ok(json!({"session": session}))
}

/// The next batch of a session's notifications: waits for one, then takes
/// what else has already arrived, up to PULL. The page asks again once it
/// has applied them, so the pipeline from the daemon to the screen is
/// bounded end to end. `closed` is the daemon gone, or the session let go.
#[tauri::command]
async fn pull(state: State<'_, Shared>, session: u64) -> Result<Value, String> {
    let closed = json!({"events": [], "closed": true});
    let taken = state.events.lock().await.take(session);
    let Some(mut events) = taken else {
        return Ok(closed);
    };
    let Some(first) = events.recv().await else {
        return Ok(closed);
    };
    let mut bytes = first.to_string().len();
    let mut batch = vec![first];
    while batch.len() < PULL && bytes < 1024 * 1024 {
        match events.try_recv() {
            Ok(event) => {
                bytes += event.to_string().len();
                batch.push(event);
            }
            Err(_) => break,
        }
    }
    state.events.lock().await.restore(session, events);
    Ok(json!({"events": batch, "closed": false}))
}

/// Page diagnostics land on stderr, where a terminal can see them.
#[tauri::command]
fn log(message: String) {
    eprintln!("agent-app page: {message}");
}

/// Any protocol op, relayed as is. The page decides what to ask for.
#[tauri::command]
async fn request(state: State<'_, Shared>, op: String, params: Value) -> Result<Value, String> {
    let client = state.client.lock().await.clone().ok_or("detached")?;
    client.request(&op, params).await.map_err(|e| e.to_string())
}

fn main() {
    let config = match config() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("agent-app: {message}");
            std::process::exit(2);
        }
    };
    tauri::Builder::default()
        .manage(Shared {
            config,
            client: Mutex::new(None),
            session: std::sync::atomic::AtomicU64::new(0),
            events: Mutex::new(SessionSlot::default()),
        })
        .invoke_handler(tauri::generate_handler![
            setup, policy, attach, pull, request, log
        ])
        .setup(|app| {
            let _ = app.get_webview_window("main");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("agent-app: window failed");
}
