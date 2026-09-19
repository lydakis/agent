//! The desktop client's Rust core: a transport between the page and the
//! daemon. The page owns the state model and the protocol logic, exactly as
//! the prototype did; this side connects, forwards notifications as window
//! events, and relays requests. Nothing else lives here.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use agent_client::Client;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;

struct Config {
    socket: PathBuf,
    model: Option<String>,
    workspace: String,
}

struct Shared {
    config: Config,
    client: Mutex<Option<Arc<Client>>>,
    /// Counts attachments; every forwarded event carries its session so the
    /// page can ignore the tail of one it has already left behind.
    session: std::sync::atomic::AtomicU64,
    /// An attachment whose events have not started flowing: the page asks
    /// for them once it has applied the snapshot, so nothing the replay
    /// says is overwritten by an older record.
    pending: Mutex<Option<Pending>>,
}

struct Pending {
    session: u64,
    events: tokio::sync::mpsc::Receiver<Value>,
    backlog: Vec<Value>,
}

/// Socket selection matches the CLI: --socket, then AGENT_SOCKET,
/// then the socket adjacent to --store, AGENT_STORE, or ~/.agent/state.sqlite.
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
                let canonical = std::fs::canonicalize(&store).unwrap_or(store);
                let mut adjacent = canonical.into_os_string();
                adjacent.push(".sock");
                PathBuf::from(adjacent)
            }
        },
    };
    // The daemon wants an existing absolute workspace; resolve what was given
    // the same way the default is resolved.
    let workspace = match workspace {
        Some(dir) => std::fs::canonicalize(&dir)
            .map_err(|e| format!("--workspace {dir}: {e}"))?
            .to_string_lossy()
            .into_owned(),
        None => std::env::current_dir()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned(),
    };
    Ok(Config {
        socket,
        model: model.or_else(|| std::env::var("AGENT_MODEL").ok()),
        workspace,
    })
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
/// AGENTS.md reaches the next bot: preamble, AGENTS.md files, skills. Too
/// much or unreadable text falls back to the preamble and says so.
#[tauri::command]
fn policy(state: State<'_, Shared>) -> Value {
    let workspace = std::path::Path::new(&state.config.workspace);
    match agent_client::policy::instructions(workspace) {
        Ok(composed) => json!({
            "instructions": composed.text,
            "note": format!(
                "preamble + {} AGENTS.md + {} skills",
                composed.sources.len(),
                composed.skills.len()
            ),
        }),
        Err(error) => json!({
            "instructions": agent_client::policy::PREAMBLE,
            "note": format!("preamble only: {error}"),
        }),
    }
}

/// Connect, follow `*` from the page's cursor, and list every bot. The
/// replay gathered meanwhile waits for `stream`: the page applies the
/// snapshot first, then asks for the events, which are all newer than it.
#[tauri::command]
async fn attach(state: State<'_, Shared>, after: i64) -> Result<Value, String> {
    let (client, mut events) = Client::connect(&state.config.socket)
        .await
        .map_err(|e| e.to_string())?;
    let session = state
        .session
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;
    // Subscribe before the snapshot: deletions are live-only notices, so
    // nothing can fall between listing and following.
    client
        .request("follow", json!({"bot": "*", "after": after}))
        .await
        .map_err(|e| e.to_string())?;
    // Page the snapshot on a task while this loop drains the replay the
    // subscription is already sending; unread, a large replay would fill
    // the client queue under the very request that lists the bots.
    let pager = {
        let client = client.clone();
        tauri::async_runtime::spawn(async move {
            let mut bots = Vec::new();
            let mut cursor: Option<String> = None;
            loop {
                let page = client
                    .request("bots", json!({"after": cursor, "limit": 256}))
                    .await
                    .map_err(|e| e.to_string())?;
                bots.extend(page["bots"].as_array().cloned().unwrap_or_default());
                match page["next_after"].as_str() {
                    Some(next) => cursor = Some(next.to_owned()),
                    None => break,
                }
            }
            Ok::<Vec<Value>, String>(bots)
        })
    };
    tokio::pin!(pager);
    let mut backlog: Vec<Value> = Vec::new();
    let bots = loop {
        tokio::select! {
            paged = &mut pager => break paged.map_err(|e| e.to_string())??,
            event = events.recv() => match event {
                Some(event) => backlog.push(event),
                None => return Err("daemon_disconnected".into()),
            },
        }
    };
    *state.client.lock().await = Some(client);
    *state.pending.lock().await = Some(Pending {
        session,
        events,
        backlog,
    });
    Ok(json!({"bots": bots, "session": session}))
}

/// Forward the attached session's events to the window as `daemon` events:
/// the replay gathered during `attach`, then live. A closed session is
/// reported the same way, as `{"event":"closed"}`.
#[tauri::command]
async fn stream(app: AppHandle, state: State<'_, Shared>) -> Result<(), String> {
    let Pending {
        session,
        mut events,
        backlog,
    } = state.pending.lock().await.take().ok_or("not attached")?;
    tauri::async_runtime::spawn(async move {
        let stamp = |mut event: Value| {
            event["session"] = json!(session);
            event
        };
        for event in backlog {
            if app.emit("daemon", stamp(event)).is_err() {
                return;
            }
        }
        while let Some(event) = events.recv().await {
            if app.emit("daemon", stamp(event)).is_err() {
                break;
            }
        }
        let _ = app.emit("daemon", json!({"event": "closed", "session": session}));
    });
    Ok(())
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
            pending: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            setup, policy, attach, stream, request, log
        ])
        .setup(|app| {
            let _ = app.get_webview_window("main");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("agent-app: window failed");
}
