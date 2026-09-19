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
}

/// Socket selection matches the CLI and the TUI: --socket, then AGENT_SOCKET,
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
            inline
                .clone()
                .or_else(|| iter.next().cloned())
                .ok_or_else(|| format!("{flag} needs a value"))
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
    let workspace = match workspace {
        Some(dir) => dir,
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
    // The shared client policy for this workspace: preamble, AGENTS.md
    // files, skills. Too much text falls back to the preamble and says so.
    let workspace = std::path::Path::new(&state.config.workspace);
    let (instructions, note) = match agent_client::policy::instructions(workspace) {
        Ok(composed) => {
            let note = format!("preamble + {} AGENTS.md + {} skills", composed.sources.len(), composed.skills.len());
            (composed.text, note)
        }
        Err(error) => (agent_client::policy::PREAMBLE.to_owned(), format!("preamble only: {error}")),
    };
    json!({
        "socket": state.config.socket.to_string_lossy(),
        "model": state.config.model,
        "workspace": state.config.workspace,
        "instructions": instructions,
        "instructions_note": note,
        "tools": ["shell", "read", "write", "edit", "wait", "history"],
    })
}

/// Connect, list every bot, follow `*` from the page's cursor, and forward
/// every notification to the window as a `daemon` event. A closed session
/// is reported the same way, as `{"event":"closed"}`.
#[tauri::command]
async fn attach(app: AppHandle, state: State<'_, Shared>, after: i64) -> Result<Value, String> {
    let (client, mut events) = Client::connect(&state.config.socket)
        .await
        .map_err(|e| e.to_string())?;
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
    client
        .request("follow", json!({"bot": "*", "after": after}))
        .await
        .map_err(|e| e.to_string())?;
    *state.client.lock().await = Some(client);
    let window = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if window.emit("daemon", event).is_err() {
                break;
            }
        }
        let _ = window.emit("daemon", json!({"event": "closed"}));
    });
    Ok(json!({"bots": bots}))
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
        })
        .invoke_handler(tauri::generate_handler![setup, attach, request, log])
        .setup(|app| {
            let _ = app.get_webview_window("main");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("agent-app: window failed");
}
