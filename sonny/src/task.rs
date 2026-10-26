//! `sonny task`: start one of the lead's tasks in a folder of its own, made
//! the way Sonny's `copies` setting says, then started there with the
//! daemon's `agent run`. A git worktree (the default) starts at the last
//! commit on a branch of its own. A sandfs copy (experimental), made with
//! the sandfs library built into Sonny so nothing needs installing, is the
//! checkout as it is, uncommitted changes included, on a branch of its own;
//! in a folder sandfs mounts, a fork of that workspace; in any other
//! folder, the folder as it is now. A task that could not start leaves no
//! folder behind.
//!
//! sandfs mounts are served by Sonny run again as `sonny sandfs daemon`,
//! which outlives the command that asked for them and shares sandfs's home
//! with the `sandfs` command.
use sandfs::{ChangeKind, ChangesOptions, ForkOptions, Home, MountOptions};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const USAGE: &str = "usage: sonny task NAME [--setup] [RUN FLAGS...] -- BRIEF\n\
                     \x20      sonny task changes NAME [--apply]";

/// The task's folder, how its work comes back, and what undoes it.
struct Place {
    dir: PathBuf,
    note: Option<String>,
    undo: Undo,
}

/// What a start that failed takes away: only what it made.
enum Undo {
    Nothing,
    Worktree {
        here: PathBuf,
        path: PathBuf,
        branch: String,
    },
    Sandfs {
        name: String,
    },
}

/// `sonny task ...`, from the lead's shell.
pub fn run(args: &[String]) -> Result<(), String> {
    let here = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|e| e.to_string())?;
    match args {
        [changes, name] if changes == "changes" => list(name, None),
        [changes, name, apply] if changes == "changes" && apply == "--apply" => {
            list(name, Some(here))
        }
        [name, rest @ ..] if !name.starts_with('-') => start(&here, name, rest),
        _ => Err(USAGE.into()),
    }
}

fn start(here: &Path, name: &str, rest: &[String]) -> Result<(), String> {
    let split = rest.iter().position(|arg| arg == "--").ok_or(USAGE)?;
    let (flags, brief) = (&rest[..split], &rest[split + 1..]);
    if brief.is_empty() {
        return Err(USAGE.into());
    }
    let setup = flags.iter().any(|flag| flag == "--setup");
    let flags: Vec<&String> = flags.iter().filter(|flag| *flag != "--setup").collect();
    let agent = std::env::var_os("AGENT_BIN")
        .ok_or("run this from the lead's shell (AGENT_BIN is not set)")?;

    let info = agent_client::project::read(here)?;
    let place = if info["threads_in"] == "project" {
        in_place(here)
    } else {
        match crate::settings::Settings::load().copies.as_str() {
            "sandfs" => sandfs_copy(here, name)?,
            _ => worktree(here, name)?,
        }
    };
    let failed = |place: &Place, error: String| {
        if !listed(&agent, name) {
            undo(&place.undo);
        }
        Err(error)
    };
    // The folder's setup script, run in the task's own folder, as for a
    // worktree; a task in the lead's folder has nothing to set up.
    let script = here.join(".agents/setup");
    if setup && place.dir != here && script.exists() {
        let status = Command::new(&script)
            .current_dir(&place.dir)
            .env("AGENT_SOURCE", here)
            .stdin(Stdio::null())
            .status();
        if !status.is_ok_and(|status| status.success()) {
            return failed(&place, format!("{} failed", script.display()));
        }
    }
    let status = Command::new(&agent)
        .args(["run", "--detach", "--new", "--bot", name, "--workspace"])
        .arg(&place.dir)
        .args(flags)
        .arg("--")
        .args(brief)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("running {}: {e}", agent.to_string_lossy()))?;
    if !status.success() {
        return failed(&place, format!("{name} did not start"));
    }
    if let Some(note) = &place.note {
        eprintln!("{note}");
    }
    Ok(())
}

/// Whether the daemon has a task `name`: one whose start failed after it
/// was made keeps its folder, since a retry continues it there.
fn listed(agent: &std::ffi::OsStr, name: &str) -> bool {
    let Ok(output) = Command::new(agent)
        .args(["ls", "--name", name])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return true;
    };
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(Value::Array(bots)) => bots.iter().any(|bot| bot["name"] == name),
        // Unreadable: keep the folder rather than lose a task's work.
        _ => true,
    }
}

fn undo(undo: &Undo) {
    match undo {
        Undo::Nothing => {}
        Undo::Worktree { here, path, branch } => {
            let path = path.to_string_lossy();
            let _ = git(here, &["worktree", "remove", "--force", &path]);
            let _ = git(here, &["branch", "-D", branch]);
        }
        Undo::Sandfs { name } => {
            let _ = sandfs().and_then(|home| home.remove(name, true).map_err(|e| format!("{e:#}")));
        }
    }
}

/// Every task works in the lead's folder.
fn in_place(here: &Path) -> Place {
    Place {
        dir: here.to_owned(),
        note: None,
        undo: Undo::Nothing,
    }
}

fn home_dir() -> Result<PathBuf, String> {
    Ok(PathBuf::from(
        std::env::var_os("HOME").ok_or("HOME is not set")?,
    ))
}

/// A git worktree on branch agent/NAME at the last commit, in the lead's
/// subfolder of it. Outside git the task works in the lead's folder.
fn worktree(here: &Path, name: &str) -> Result<Place, String> {
    if git(here, &["rev-parse", "--show-toplevel"]).is_err() {
        return Ok(in_place(here));
    }
    let prefix = git(here, &["rev-parse", "--show-prefix"])?;
    let path = home_dir()?.join(".agent/worktrees").join(name);
    let branch = format!("agent/{name}");
    let undo = if path.exists() {
        Undo::Nothing
    } else {
        let target = path.to_string_lossy();
        git(
            here,
            &["worktree", "add", "-q", "-b", &branch, &target, "HEAD"],
        )?;
        Undo::Worktree {
            here: here.to_owned(),
            path: path.clone(),
            branch: branch.clone(),
        }
    };
    Ok(Place {
        dir: within(&path, Path::new(prefix.trim_end_matches('/'))),
        note: Some(format!(
            "{name}: a worktree at {} on branch {branch}, from the last commit (uncommitted \
             changes here are not in it); its commits are on {branch} here as it makes them",
            shell_quote(&path)
        )),
        undo,
    })
}

/// Where a task's sandfs copy is mounted, beside `~/.agent/worktrees`.
fn tasks() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".agent/tasks"))
}

fn sandfs() -> Result<Home, String> {
    Home::locate().map_err(|e| format!("{e:#}"))
}

/// What the lead's folder is, and so what a sandfs copy of it is.
enum Source {
    /// A folder in the sandfs workspace `name`, mounted at `root`.
    Mount {
        name: String,
        root: PathBuf,
        git: bool,
    },
    /// A git repository's checkout, whose top is `top`.
    Git { top: PathBuf },
    /// Any other folder.
    Folder,
}

fn source(here: &Path) -> Result<Source, String> {
    if let Some((name, root)) = mounted_at(here)? {
        let git = root.join(".git").exists();
        return Ok(Source::Mount { name, root, git });
    }
    match git(here, &["rev-parse", "--show-toplevel"]) {
        Ok(top) => Ok(Source::Git {
            top: PathBuf::from(top),
        }),
        Err(_) => Ok(Source::Folder),
    }
}

/// The sandfs workspace `here` is in, and where it is mounted. A folder
/// that has never seen sandfs has no home to look in.
fn mounted_at(here: &Path) -> Result<Option<(String, PathBuf)>, String> {
    if !Home::locate().is_ok_and(|home| home.root().exists()) {
        return Ok(None);
    }
    let workspaces = sandfs()?.workspaces().map_err(|e| format!("{e:#}"))?;
    Ok(workspaces
        .into_iter()
        .filter_map(|ws| Some((ws.name, ws.mounted?)))
        .filter(|(_, root)| here.starts_with(root))
        .max_by_key(|(_, root)| root.components().count()))
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {}", args.join(" "), said.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The task `name`'s sandfs copy of `here`, or the one it has, mounted
/// again where it was (as after a restart), in the lead's subfolder of it.
fn sandfs_copy(here: &Path, name: &str) -> Result<Place, String> {
    let source = source(here)?;
    let tasks = tasks()?;
    std::fs::create_dir_all(&tasks).map_err(|e| format!("{}: {e}", tasks.display()))?;
    let dir = tasks.join(name);
    let home = sandfs()?;
    let exists = home
        .workspaces()
        .map_err(|e| format!("{e:#}"))?
        .into_iter()
        .find(|ws| ws.name == name);
    let branch = format!("agent/{name}");
    let (root, git_copy) = match &source {
        Source::Mount { root, git, .. } => (root.clone(), *git),
        Source::Git { top } => (top.clone(), true),
        Source::Folder => (here.to_owned(), false),
    };
    let undo = match exists {
        Some(ws) => {
            if ws.mounted.is_none() {
                home.mount(name, &dir, &MountOptions::default())
                    .map_err(|e| format!("{e:#}"))?;
            }
            Undo::Nothing
        }
        None => {
            let from = match &source {
                Source::Mount { name, .. } => name.clone(),
                _ => root.to_string_lossy().into_owned(),
            };
            let options = ForkOptions {
                name: Some(name.to_owned()),
                branch: git_copy.then(|| branch.clone()),
                ..Default::default()
            };
            home.fork(&from, Some(&dir), &options)
                .map_err(|e| format!("{e:#}"))?;
            Undo::Sandfs {
                name: name.to_owned(),
            }
        }
    };
    let note = if git_copy {
        let origin = git(&dir, &["remote", "get-url", "origin"]).unwrap_or_default();
        format!(
            "{name}: a sandfs copy on branch {branch}, uncommitted changes included; \
             `git -C {} push origin {branch}` sends its commits to {origin}",
            shell_quote(&dir)
        )
    } else {
        let me = me();
        format!(
            "{name}: a sandfs copy of this folder as it is now; `{me} task changes {name}` lists \
             what the task changed, and `{me} task changes {name} --apply` writes it here"
        )
    };
    let prefix = here.strip_prefix(&root).unwrap_or(Path::new(""));
    Ok(Place {
        dir: within(&dir, prefix),
        note: Some(note),
        undo,
    })
}

/// The folder in `copy` at `prefix`, without the trailing slash an empty
/// one would add.
fn within(copy: &Path, prefix: &Path) -> PathBuf {
    match prefix.as_os_str().is_empty() {
        true => copy.to_owned(),
        false => copy.join(prefix),
    }
}

/// This program, as the lead's shell runs it.
fn me() -> String {
    std::env::current_exe().map_or("sonny".into(), |me| shell_quote(&me))
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

/// `sonny task changes NAME [--apply]`: what the task's sandfs copy added,
/// changed and deleted, and with `--apply`, written into this folder.
fn list(name: &str, to: Option<PathBuf>) -> Result<(), String> {
    let options = ChangesOptions {
        copy_to: to,
        ..Default::default()
    };
    let found = sandfs()?
        .changes(name, &options)
        .map_err(|e| format!("{e:#}"))?;
    for change in &found.changes {
        let kind = match change.kind {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
        };
        println!("{kind} {}", change.path);
    }
    if found.held_back > 0 {
        eprintln!(
            "{} changes in .git left out: git runs what is named there, so copy those by hand",
            found.held_back
        );
    }
    for setting in &found.git_settings {
        eprintln!("the copy changed git's {setting}");
    }
    Ok(())
}

/// Mount again the task copies a restart unmounted, so a task never works
/// in an empty folder: every workspace whose folder in `~/.agent/tasks`
/// is there but not mounted. Returns what could not be mounted.
pub fn remount() -> Vec<String> {
    let Ok(tasks) = tasks() else {
        return Vec::new();
    };
    if !tasks.is_dir() {
        return Vec::new();
    }
    let Ok(home) = Home::locate() else {
        return Vec::new();
    };
    let Ok(workspaces) = home.workspaces() else {
        return Vec::new();
    };
    workspaces
        .into_iter()
        .filter(|ws| ws.mounted.is_none() && tasks.join(&ws.name).is_dir())
        .filter_map(|ws| {
            home.mount(&ws.name, &tasks.join(&ws.name), &MountOptions::default())
                .err()
                .map(|e| format!("task copy {}: {e:#}", ws.name))
        })
        .collect()
}
