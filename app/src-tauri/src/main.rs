//! The desktop client's Rust core: a transport between the page and the
//! daemon. The page owns the state model and the protocol logic, exactly as
//! the prototype did; this side connects, forwards notifications as window
//! events, and relays requests. Beside that it reads the files the app owns
//! (projects, profiles, swarms, schedules) and makes a swarm's shared
//! worktree; run with `--swarm-post` it is a swarm's post tool (see
//! `swarm`), and with `--schedule` or `--schedule-fire` it adds, lists,
//! removes or fires schedules (see `schedule`).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod daemon;
mod project;
mod schedule;
mod session;
mod settings;
mod swarm;
mod worktree;

use agent_client::Client;
use serde_json::{Map, Value, json};
use session::SessionSlot;
use std::{path::PathBuf, sync::Arc};
use tauri::{Manager, State};
use tokio::sync::Mutex;

struct Config {
    socket: PathBuf,
    /// The store the socket was derived from; a daemon is started only for
    /// a store, never behind an explicit socket.
    store: Option<PathBuf>,
    workspace: String,
}

struct Shared {
    config: Config,
    client: Mutex<Option<Arc<Client>>>,
    /// The `agent` packaged beside the app, which starts a missing daemon.
    agent: Option<PathBuf>,
    starts: Mutex<daemon::Starts>,
    /// Counts attachments; a pull names the session it reads for, so a
    /// batch from a session the page has left is never mistaken for new.
    session: std::sync::atomic::AtomicU64,
    /// The attached session's notifications. `pull` takes the receiver out
    /// while it waits and puts it back, so an attach never waits on a pull.
    events: Mutex<SessionSlot<agent_client::Events>>,
    /// The attached daemon's store identity, which names its swarms' folder.
    store: std::sync::Mutex<Option<String>>,
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
    let (mut socket, mut store, mut workspace) = (None, None, None);
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
            "--workspace" => workspace = Some(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let (socket, store) = match socket {
        Some(socket) => (socket, None),
        None => match (store, std::env::var_os("AGENT_SOCKET")) {
            (None, Some(env)) => (PathBuf::from(env), None),
            (store, _) => {
                let store = store
                    .or_else(|| std::env::var_os("AGENT_STORE").map(PathBuf::from))
                    .or_else(|| {
                        std::env::var_os("HOME")
                            .map(|home| PathBuf::from(home).join(".agent/state.sqlite"))
                    })
                    .ok_or("no store path; pass --socket or --store")?;
                let socket =
                    agent_client::socket::default_socket(&store).map_err(|e| e.to_string())?;
                (socket, Some(store))
            }
        },
    };
    let workspace = workspace_path(&match workspace {
        Some(dir) => PathBuf::from(dir),
        None => default_workspace(
            std::env::current_dir().map_err(|e| e.to_string())?,
            std::env::var_os("HOME").map(PathBuf::from),
        ),
    })?;
    Ok(Config {
        socket,
        store,
        workspace,
    })
}

/// The launching directory, except the root a window opened from the Dock or
/// Finder starts in: that is nobody's project, so home stands in for it.
fn default_workspace(current: PathBuf, home: Option<PathBuf>) -> PathBuf {
    match home {
        Some(home) if current == std::path::Path::new("/") => home,
        _ => current,
    }
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
    use super::{default_workspace, workspace_path};
    use std::path::PathBuf;

    #[test]
    fn a_launch_from_the_root_defaults_to_home() {
        let home = Some(PathBuf::from("/Users/a"));
        assert_eq!(
            default_workspace("/".into(), home.clone()),
            PathBuf::from("/Users/a")
        );
        assert_eq!(
            default_workspace("/tmp/p".into(), home),
            PathBuf::from("/tmp/p")
        );
        assert_eq!(default_workspace("/".into(), None), PathBuf::from("/"));
    }

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

/// The tools a bot gets unless its profile names its own.
const TOOLS: [&str; 6] = ["shell", "read", "write", "edit", "wait", "history"];

/// What the page needs to create bots and to say where it is. There is no
/// default model: each project and agent is given its own.
#[tauri::command]
fn setup(state: State<'_, Shared>) -> Result<Value, String> {
    Ok(json!({
        "socket": state.config.socket.to_string_lossy(),
        "workspace": state.config.workspace,
        // Whether this window starts the daemon for its store, and so may replace it.
        "managed": state.agent.is_some() && state.config.store.is_some(),
        "tools": TOOLS,
    }))
}

/// The shared client policy for a workspace (the app's own by default),
/// composed now so an edited AGENTS.md reaches the next bot: preamble,
/// AGENTS.md files, skills and profiles, and the role `profile` names.
#[tauri::command]
fn policy(
    state: State<'_, Shared>,
    workspace: Option<String>,
    profile: Option<String>,
) -> Result<Value, String> {
    let dir = match workspace {
        Some(dir) => workspace_path(std::path::Path::new(&dir))?,
        None => state.config.workspace.clone(),
    };
    compose(std::path::Path::new(&dir), profile.as_deref(), None)
}

/// The profiles a folder offers as identities for a swarm's agents, with
/// what each says it is and the model it names: the folder's and the
/// user's, not the roles the app gives a coordinator and a swarm's agents.
#[tauri::command]
fn profiles(dir: String) -> Result<Value, String> {
    let dir = workspace_path(std::path::Path::new(&dir))?;
    let failed = |error: agent_client::policy::Failure| format!("{}: {error}", error.code());
    let workspace = std::path::Path::new(&dir);
    let listed = agent_client::policy::instructions(workspace, None).map_err(failed)?;
    let mut out = Vec::new();
    for entry in listed.profiles {
        if BUILT_IN.iter().any(|(name, _)| *name == entry.name) {
            continue;
        }
        let model = agent_client::policy::profile(workspace, &entry.name)
            .map_err(failed)?
            .and_then(|p| p.model);
        out.push(json!({"name": entry.name, "summary": entry.summary, "model": model}));
    }
    Ok(json!(out))
}

/// The roles the app ships, used where neither the folder nor the user has
/// a file of that name.
const BUILT_IN: [(&str, &str); 2] = [
    ("coordinator", include_str!("../../agents/coordinator.md")),
    ("swarm", include_str!("../../agents/swarm.md")),
];

/// The file a role of the app's is read from in every project: yours,
/// `~/.agents/agents/NAME.md`, which a project's own file of that name
/// overrides. Where you have none yet, it starts as the app's text.
fn own_role(home: &std::path::Path, name: &str) -> Result<PathBuf, String> {
    let (_, text) = BUILT_IN
        .iter()
        .find(|(built, _)| *built == name)
        .ok_or_else(|| format!("profile_not_found: the app has no {name} role"))?;
    let dir = home.join(".agents/agents");
    let path = dir.join(format!("{name}.md"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if path.exists() {
        return Ok(path);
    }
    // Written whole beside it, then linked into place only if still absent:
    // a failed write leaves no half a role that later reads take for yours.
    // Each call writes its own file, so two at once never share one.
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = dir.join(format!(".{name}.md.{}.{call}", std::process::id()));
    let made = (|| {
        use std::io::Write;
        let mut file = std::fs::File::create_new(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        match std::fs::hard_link(&temporary, &path) {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => Err(error),
            _ => std::fs::File::open(&dir)?.sync_all(),
        }
    })();
    let _ = std::fs::remove_file(&temporary);
    made.map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Open your file for one of the app's roles in your text editor, made
/// from the app's text first when you have none.
#[tauri::command]
async fn edit_role(name: String) -> Result<String, String> {
    let home = std::env::var_os("HOME").ok_or("no HOME for ~/.agents")?;
    let path = own_role(std::path::Path::new(&home), &name)?;
    let mut open = if cfg!(target_os = "macos") {
        let mut open = tokio::process::Command::new("open");
        open.arg("-t");
        open
    } else {
        tokio::process::Command::new("xdg-open")
    };
    let status = open
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| format!("open: {e}"))?;
    if !status.success() {
        return Err(format!("open: {} could not be opened", path.display()));
    }
    Ok(path.to_string_lossy().into_owned())
}

/// Which of the app's roles you have your own file for.
#[tauri::command]
fn roles() -> Value {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let own = |name: &str| {
        home.as_ref()
            .map(|h| h.join(format!(".agents/agents/{name}.md")))
            .filter(|p| p.is_file())
            .map(|p| p.to_string_lossy().into_owned())
    };
    Value::Array(
        BUILT_IN
            .iter()
            .map(|(name, _)| json!({"name": name, "file": own(name)}))
            .collect(),
    )
}

/// The project in a folder: its `.agents/project.toml`, or the defaults a
/// new project there would take.
#[tauri::command]
fn project(dir: String) -> Result<Value, String> {
    project::read(std::path::Path::new(&workspace_path(
        std::path::Path::new(&dir),
    )?))
}

/// Write a new project's `.agents/project.toml`; an existing one is kept.
#[tauri::command]
fn write_project(dir: String, name: String, model: String) -> Result<(), String> {
    project::write(
        std::path::Path::new(&workspace_path(std::path::Path::new(&dir))?),
        &name,
        &model,
    )
}

/// The models to offer, read from `~/.agent/models` each time, so an edit
/// shows without a restart. The daemon has no list.
/// The branch a bot's folder has checked out when it is a linked git
/// worktree; read from files, no git run.
#[tauri::command]
fn branch(dir: String) -> Option<String> {
    worktree::linked_branch(std::path::Path::new(&dir))
}

#[tauri::command]
fn models() -> Result<Value, String> {
    let path = agent_client::models::path().ok_or("no HOME for ~/.agent/models")?;
    let models = agent_client::models::read(&path)
        .map_err(|error| format!("{}: {}", error.code, error.detail.unwrap_or_default()))?;
    Ok(models.iter().map(|model| model.json()).collect())
}

/// What a daemon this app starts would run with: its providers, the AWS
/// region and profile, and which keys are set (never their values), and
/// whether this window can restart its daemon to apply a change.
#[tauri::command]
async fn settings(state: State<'_, Shared>) -> Result<Value, String> {
    // A daemon this window did not start runs its own settings: the page asks
    // it for its providers, and this machine's shell is not read at all.
    if state.agent.is_none() || state.config.store.is_none() {
        return Ok(
            json!({"providers": [], "region": null, "profile": null, "keys": [], "restartable": false}),
        );
    }
    let file = match daemon::env_file() {
        Some(path) => daemon::read_env_file(&path)?,
        None => Vec::new(),
    };
    // Without a login shell a started daemon inherits this process's environment.
    let inherited: Vec<_>;
    let environment = match daemon::login().await {
        Some(login) => login,
        None => {
            inherited = std::env::vars_os().collect();
            &inherited
        }
    };
    let mut view = settings::view(&file, Some(environment));
    view["restartable"] = json!(true);
    Ok(view)
}

/// Set or remove settings in `~/.agent/env`; they reach the daemon when it
/// restarts.
#[tauri::command]
fn save_settings(changes: serde_json::Map<String, Value>) -> Result<(), String> {
    let path = daemon::env_file().ok_or("no HOME for ~/.agent/env")?;
    // An unreadable or unsafe file is reported, never replaced.
    daemon::read_env_file(&path)?;
    let text = match std::fs::read_to_string(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        read => read.map_err(|e| format!("env_file_unreadable: {}: {e}", path.display()))?,
    };
    let edited = settings::edit(&text, &changes)?;
    // A file the next start would refuse is never written.
    if edited.len() as u64 > daemon::MAX_ENV_FILE {
        return Err(format!(
            "env_file_invalid: {} would exceed {} bytes",
            path.display(),
            daemon::MAX_ENV_FILE
        ));
    }
    settings::replace(&path, &edited, 0o600)
}

/// Stop the store's daemon so the next attach starts one with the current
/// settings. Only a daemon this app would start can be restarted.
#[tauri::command]
async fn restart_daemon(state: State<'_, Shared>) -> Result<(), String> {
    let (Some(agent), Some(store)) = (&state.agent, &state.config.store) else {
        return Err("restart_unavailable: this window did not start its daemon".into());
    };
    if let Some(old) = state.client.lock().await.take() {
        old.close().await;
    }
    daemon::stop(agent, store).await?;
    state.starts.lock().await.forget();
    Ok(())
}

/// Ask the daemon's providers what they offer and write `~/.agent/models`
/// from it. A provider that fails keeps its lines from the last list, and a
/// list with no model at all is not written. Answers per provider.
#[tauri::command]
async fn discover_models(state: State<'_, Shared>) -> Result<Value, String> {
    let client = state.client.lock().await.clone().ok_or("detached")?;
    let listing = client
        .request("provider_models", json!({}))
        .await
        .map_err(|e| e.to_string())?;
    let path = agent_client::models::path().ok_or("no HOME for ~/.agent/models")?;
    let failed = |error: agent_client::Error| {
        format!("{}: {}", error.code, error.detail.unwrap_or_default())
    };
    // Refresh replaces the list, so one that no longer reads keeps nothing.
    let kept = agent_client::models::read(&path).unwrap_or_default();
    let text = agent_client::models::render(&listing, &kept).map_err(failed);
    if let Ok(text) = &text {
        settings::replace(&path, text, 0o644)?;
    }
    let providers: Map<String, Value> = listing["providers"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, listed)| {
            let answer = match listed["models"].as_array() {
                Some(models) => json!({"models": models.len()}),
                None => json!({"error": listed["error"], "detail": listed["detail"]}),
            };
            (name.clone(), answer)
        })
        .collect();
    Ok(json!({"providers": providers, "written": text.is_ok(), "error": text.err()}))
}

/// Too much or unreadable text fails with the CLI's `--agents` code, and
/// `/new` creates nothing: a bot without its workspace's rules is worse
/// than no bot. `also` names a role whose text follows the profile's, as a
/// swarm's rules follow an identity's; the profile's model and tools stand.
fn compose(
    workspace: &std::path::Path,
    profile: Option<&str>,
    also: Option<&str>,
) -> Result<Value, String> {
    use agent_client::policy::Profile;
    let failed = |error: agent_client::policy::Failure| format!("{}: {error}", error.code());
    let find = |name: &str| -> Result<Profile, String> {
        match agent_client::policy::profile(workspace, name).map_err(failed)? {
            Some(role) => Ok(role),
            None => BUILT_IN
                .iter()
                .find(|(built, _)| *built == name)
                .map(|(_, text)| Profile::parse(name, None, text))
                .ok_or_else(|| format!("profile_not_found: no .agents/agents/{name}.md")),
        }
    };
    let mut role = profile.map(find).transpose()?;
    if let (Some(role), Some(also)) = (role.as_mut(), also) {
        let also = find(also)?;
        role.body = [role.body.as_str(), also.body.as_str()]
            .iter()
            .filter(|body| !body.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    let composed = agent_client::policy::instructions(workspace, role.as_ref()).map_err(failed)?;
    let mut note = format!(
        "preamble + {} AGENTS.md + {} skills + {} profiles",
        composed.sources.len(),
        composed.skills.len(),
        composed.profiles.len()
    );
    if let Some(role) = &role {
        let from = role.path.as_ref().map_or_else(
            || format!("{} (built in)", role.name),
            |p| p.display().to_string(),
        );
        note.push_str(&format!(" · {from}"));
    }
    Ok(json!({
        "instructions": composed.text,
        "compaction_instructions": agent_client::policy::DEFAULT_COMPACTION_INSTRUCTIONS,
        "model": role.as_ref().and_then(|r| r.model.clone()),
        "tools": role.as_ref().and_then(|r| r.tools.clone()),
        "note": note,
    }))
}

#[cfg(test)]
mod policy_tests {
    use super::{BUILT_IN, compose, own_role};

    #[test]
    fn a_workspace_policy_that_cannot_compose_is_an_error_not_the_preamble() {
        let root = std::env::temp_dir().join(format!("agent-app-policy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let file = root.join("AGENTS.md");
        std::fs::write(&file, "rule").unwrap();
        let composed = compose(&root, None, None).unwrap();
        let rule = format!("{}\n\nrule", file.display());
        assert!(composed["instructions"].as_str().unwrap().contains(&rule));
        std::fs::write(&file, "x".repeat(agent_client::policy::MAX_INSTRUCTIONS)).unwrap();
        let error = compose(&root, None, None).unwrap_err();
        assert!(error.starts_with("instructions_limit: "), "{error}");
        assert!(error.contains(file.to_str().unwrap()), "{error}");
        std::fs::write(&file, [0xff, 0xfe]).unwrap();
        let error = compose(&root, None, None).unwrap_err();
        assert!(error.starts_with("instructions_unreadable: "), "{error}");
        assert!(error.contains(file.to_str().unwrap()), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn your_role_file_starts_as_the_apps_text_and_is_then_yours() {
        let home = std::env::temp_dir().join(format!("agent-app-own-role-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let path = own_role(&home, "coordinator").unwrap();
        assert_eq!(path, home.join(".agents/agents/coordinator.md"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), BUILT_IN[0].1);
        std::fs::write(&path, "---\nname: coordinator\n---\nDelegate more.").unwrap();
        own_role(&home, "coordinator").unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .ends_with("Delegate more.")
        );
        assert!(
            own_role(&home, "reviewer")
                .unwrap_err()
                .starts_with("profile_not_found")
        );
        // Many made at once each write their own file, and the role is whole.
        let calls: Vec<_> = (0..8)
            .map(|_| {
                let home = home.clone();
                std::thread::spawn(move || own_role(&home, "swarm"))
            })
            .collect();
        for call in calls {
            call.join().unwrap().unwrap();
        }
        let swarm = home.join(".agents/agents/swarm.md");
        assert_eq!(std::fs::read_to_string(swarm).unwrap(), BUILT_IN[1].1);
        // Nothing but the roles is left beside them.
        let mut names: Vec<_> = std::fs::read_dir(home.join(".agents/agents"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["coordinator.md", "swarm.md"]);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn the_coordinator_role_is_the_folders_own_file_else_the_built_in_one() {
        let root = std::env::temp_dir().join(format!("agent-app-role-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let built = compose(&root, Some("coordinator"), None).unwrap();
        let text = built["instructions"].as_str().unwrap();
        assert!(text.contains("# Role: coordinator\n\nYou coordinate the work in this folder."));
        assert!(text.contains("git worktree add -b agent/NAME"));
        assert!(
            !text.contains("name: coordinator"),
            "front matter is not instructions"
        );
        assert!(
            built["note"]
                .as_str()
                .unwrap()
                .ends_with("coordinator (built in)")
        );
        std::fs::create_dir_all(root.join(".agents/agents")).unwrap();
        std::fs::write(
            root.join(".agents/agents/coordinator.md"),
            "---\nmodel: openai/gpt-6-luna\ntools: shell, wait\n---\nOur own way.",
        )
        .unwrap();
        let own = compose(&root, Some("coordinator"), None).unwrap();
        assert!(
            own["instructions"]
                .as_str()
                .unwrap()
                .ends_with("# Role: coordinator\n\nOur own way.")
        );
        assert_eq!(own["model"], "openai/gpt-6-luna");
        assert_eq!(own["tools"], serde_json::json!(["shell", "wait"]));
        assert!(
            compose(&root, Some("nobody"), None)
                .unwrap_err()
                .starts_with("profile_not_found: ")
        );
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
    let (client, events) = match Client::connect(&state.config.socket).await {
        Ok(connected) => connected,
        // Nothing listens: start the daemon for the store, then connect.
        Err(error) if error.code == "daemon_unavailable" => {
            let (Some(agent), Some(store)) = (&state.agent, &state.config.store) else {
                return Err(error.to_string());
            };
            state.starts.lock().await.start(agent, store).await?;
            Client::connect(&state.config.socket)
                .await
                .map_err(|e| e.to_string())?
        }
        Err(error) if error.code == "daemon_protocol_mismatch" => {
            return Err(daemon::age(&error));
        }
        Err(error) => return Err(error.to_string()),
    };
    let session = state
        .session
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;
    client
        .request("follow", json!({"bot": "*", "after": after}))
        .await
        .map_err(|e| e.to_string())?;
    *state.store.lock().unwrap() = client.store().map(str::to_owned);
    *state.client.lock().await = Some(client);
    state.events.lock().await.replace(session, events);
    Ok(json!({"session": session}))
}

/// An older daemon owns the store's socket, as after an upgrade: stop it so
/// the next attach starts the one this app carries. Only for a store this
/// app starts daemons for; a daemon it was pointed at is its owner's.
#[tauri::command]
async fn replace_daemon(state: State<'_, Shared>) -> Result<(), String> {
    if state.agent.is_none() || state.config.store.is_none() {
        return Err("daemon_not_ours: this window was given a daemon's socket; stop that daemon with its own agent".into());
    }
    daemon::replace_older(&state.config.socket).await?;
    state.starts.lock().await.forget();
    Ok(())
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

/// Every swarm in `~/.agent/swarms`, and the folders there that are not
/// readable swarms.
#[tauri::command]
fn swarms(state: State<'_, Shared>) -> Result<Value, String> {
    swarm::list(&swarms_of(&state)?)
}

/// The swarms of the store this window's daemon runs.
fn swarms_of(state: &Shared) -> Result<PathBuf, String> {
    let store = state.store.lock().unwrap().clone();
    swarm::root(&store.ok_or("detached: swarms are read once the window is attached")?)
}

/// A change to a swarm's files waits for its board's lock, which an agent's
/// post holds while it sends; it waits on the blocking pool, never on the
/// window's thread or the async workers.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| e.to_string())?
}

/// Start a swarm in a project folder, agents and all (see `swarm::start`).
/// `mix` is its rows of identity, model and share.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn swarm_start(
    state: State<'_, Shared>,
    project: String,
    folder: String,
    goal: String,
    shared: bool,
    mix: Vec<Value>,
    agents: usize,
    budget_tokens: u64,
    council: usize,
) -> Result<Value, String> {
    let client = state.client.lock().await.clone().ok_or("detached")?;
    let mix = (mix.iter())
        .map(swarm::Mix::from_json)
        .collect::<Option<Vec<_>>>()
        .ok_or("invalid_mix: each row is an identity, a model and a share")?;
    let start = swarm::Start {
        project,
        folder: workspace_path(std::path::Path::new(&folder))?.into(),
        goal,
        shared,
        mix,
        agents,
        budget_tokens,
        council,
    };
    let app = std::env::current_exe().map_err(|e| e.to_string())?;
    swarm::start(&client, &swarms_of(&state)?, &app, start).await
}

/// One more agent, from `row` of the swarm's mix.
#[tauri::command]
async fn swarm_add(state: State<'_, Shared>, swarm: String, row: usize) -> Result<Value, String> {
    let client = state.client.lock().await.clone().ok_or("detached")?;
    swarm::add(&client, &swarms_of(&state)?, &swarm, row).await
}

#[tauri::command]
async fn swarm_leave(
    state: State<'_, Shared>,
    swarm: String,
    member: String,
) -> Result<Value, String> {
    let root = swarms_of(&state)?;
    blocking(move || swarm::leave(&root, &swarm, &member)).await
}

#[tauri::command]
async fn swarm_stop(state: State<'_, Shared>, swarm: String) -> Result<Value, String> {
    let client = state.client.lock().await.clone().ok_or("detached")?;
    swarm::stop(&client, &swarms_of(&state)?, &swarm).await
}

#[tauri::command]
async fn swarm_board(
    state: State<'_, Shared>,
    swarm: String,
    offset: Option<u64>,
) -> Result<Value, String> {
    let root = swarms_of(&state)?;
    // It may wait on the board's lock to settle a change a crash cut short.
    blocking(move || swarm::board(&root, &swarm, offset)).await
}

/// Your post, over the window's own connection.
#[tauri::command]
async fn swarm_post(
    state: State<'_, Shared>,
    swarm: String,
    text: String,
) -> Result<Value, String> {
    let client = state.client.lock().await.clone().ok_or("detached")?;
    let act = swarm::Act::Post { text, all: true };
    swarm::act(&client, &swarms_of(&state)?, &swarm, None, act).await
}

/// Tell the swarm's working agents when it has passed a share of its
/// budget, once each; the page asks as its agents finish turns.
#[tauri::command]
async fn swarm_check(state: State<'_, Shared>, swarm: String) -> Result<Value, String> {
    let client = state.client.lock().await.clone().ok_or("detached")?;
    swarm::act(
        &client,
        &swarms_of(&state)?,
        &swarm,
        None,
        swarm::Act::Check,
    )
    .await
}

/// You approve or deny an open proposal, which decides it.
#[tauri::command]
async fn swarm_decide(
    state: State<'_, Shared>,
    swarm: String,
    id: String,
    approve: bool,
    reason: String,
) -> Result<Value, String> {
    let client = state.client.lock().await.clone().ok_or("detached")?;
    let act = swarm::Act::Vote {
        id,
        yes: approve,
        reason,
    };
    swarm::act(&client, &swarms_of(&state)?, &swarm, None, act).await
}

/// Every schedule, with what its last fire did.
#[tauri::command]
fn schedules() -> Result<Value, String> {
    Ok(schedule::list(&schedule::Places::home()?))
}

#[tauri::command]
fn schedule_remove(name: String) -> Result<(), String> {
    schedule::remove(&schedule::Places::home()?, &name, &schedule::launchctl)
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
    // A swarm's `post` script, a coordinator's `start`, `~/.agent/schedule`
    // and launchd's fires run this executable; each acts and exits without
    // a window.
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some(swarm::POST_FLAG) => std::process::exit(swarm::cli(&args[2..])),
        Some(swarm::START_FLAG) => std::process::exit(swarm::start_cli(&args[2..])),
        Some(schedule::FLAG) => std::process::exit(schedule::cli(&args[2..])),
        Some(schedule::FIRE_FLAG) => std::process::exit(schedule::fire_cli(&args[2..])),
        _ => {}
    }
    if let (Ok(home), Ok(app)) = (swarm::home(), std::env::current_exe()) {
        swarm::refresh_scripts(&home, &app);
        if let Err(error) = swarm::write_start_script(&home, &app) {
            eprintln!("agent-app: {error}");
        }
        if let Some(state) = home.parent()
            && let Err(error) = schedule::write_script(state, &app)
        {
            eprintln!("agent-app: {error}");
        }
        if cfg!(target_os = "macos")
            && let Ok(places) = schedule::Places::home()
        {
            schedule::refresh(&places, &app, &schedule::launchctl);
        }
    }
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
            agent: daemon::bundled(),
            starts: Mutex::new(daemon::Starts::default()),
            session: std::sync::atomic::AtomicU64::new(0),
            events: Mutex::new(SessionSlot::default()),
            store: std::sync::Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            setup,
            replace_daemon,
            policy,
            profiles,
            roles,
            edit_role,
            branch,
            models,
            project,
            write_project,
            settings,
            save_settings,
            restart_daemon,
            discover_models,
            attach,
            pull,
            request,
            log,
            swarms,
            swarm_start,
            swarm_add,
            swarm_leave,
            swarm_stop,
            swarm_board,
            swarm_post,
            swarm_check,
            swarm_decide,
            schedules,
            schedule_remove
        ])
        .setup(|app| {
            let _ = app.get_webview_window("main");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("agent-app: window failed");
}
