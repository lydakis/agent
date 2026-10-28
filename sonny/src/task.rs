//! `sonny task`: start one of the lead's tasks in a folder of its own, made
//! the way Sonny's `copies` setting says, then started there with the
//! daemon's `agent run`. A git worktree (the default) starts at the last
//! commit on a branch of its own. A sandfs copy (experimental), made with
//! the sandfs library built into Sonny so nothing needs installing, is the
//! checkout as it is, uncommitted changes included, on a branch of its own;
//! in a folder sandfs mounts, a fork of that workspace; in any other
//! folder, the folder as it is now, or on another disk than sandfs's home,
//! a workspace over the folder itself. A task that could not start leaves
//! no folder behind.
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
    // The daemon's binary in an agent's shell; in a terminal, the one
    // Sonny would start.
    let agent = crate::agent_bin().ok_or("Agent's binary was not found; set AGENT_BIN")?;
    // With the provider keys the app keeps, should this start the daemon;
    // read before the folder is made, so a bad env file leaves nothing.
    let mut command = crate::agent_command(&agent)?;

    let info = agent_client::project::read(here)?;
    let place = if info["threads_in"] == "project" {
        in_place(here)
    } else {
        match crate::settings::Settings::load().copies.as_str() {
            "sandfs" => sandfs_copy(here, name)?,
            _ => worktree(here, name)?,
        }
    };
    // Only a run that started can have made the task, which then keeps
    // its folder; anything that failed before leaves nothing behind.
    let failed = |place: &Place, ran: bool, error: String| {
        if !ran || !listed(&agent, name) {
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
            return failed(&place, false, format!("{} failed", script.display()));
        }
    }
    let flags = match told(&place.dir, &flags) {
        Ok(flags) => flags,
        Err(error) => return failed(&place, false, error),
    };
    let status = command
        .args(["run", "--detach", "--new", "--bot", name, "--workspace"])
        .arg(&place.dir)
        .args(flags)
        .arg("--")
        .args(brief)
        .stdin(Stdio::null())
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(_) => return failed(&place, true, format!("{name} did not start")),
        Err(e) => {
            return failed(&place, false, format!("running {}: {e}", agent.display()));
        }
    }
    if let Some(note) = &place.note {
        eprintln!("{note}");
    }
    Ok(())
}

/// The run flags with what `--agents` or `--profile ROLE` would compose
/// for a task in `dir` given as `--instructions`, with what every task is
/// told (`task.md`) after it, and the role's model and tools unless the flags name their
/// own. Other flags pass as they are.
fn told(dir: &Path, flags: &[&String]) -> Result<Vec<String>, String> {
    let mut kept = Vec::new();
    let (mut agents, mut profile) = (false, None);
    let mut flags = flags.iter().map(|flag| flag.as_str());
    while let Some(flag) = flags.next() {
        match flag {
            "--agents" => agents = true,
            "--profile" => profile = Some(flags.next().ok_or("--profile needs a ROLE")?),
            _ => match flag.strip_prefix("--profile=") {
                Some(role) => profile = Some(role),
                None => kept.push(flag.to_owned()),
            },
        }
    }
    if !agents && profile.is_none() {
        return Ok(kept);
    }
    // A value flag is --flag VALUE or --flag=VALUE.
    let named = |flag: &str| {
        (kept.iter()).any(|k| k.split_once('=').map_or(k.as_str(), |(name, _)| name) == flag)
    };
    if named("--instructions") || named("--instructions-file") {
        return Err("--agents and --profile compose instructions; --instructions sets them".into());
    }
    // What every task is told comes after any role, so it is never the role too.
    let role = match profile.filter(|name| !name.eq_ignore_ascii_case("task")) {
        Some(name) => Some(
            agent_client::policy::profile(dir, name)
                .map_err(|e| format!("{}: {e}", e.code()))?
                .ok_or(format!(
                    "no .agents/agents/{name}.md in the workspace or ~/.agents/agents"
                ))?,
        ),
        None => None,
    };
    let memory = agent_client::policy::memory_indexes(dir).map_err(|e| e.to_string())?;
    let composed = agent_client::policy::instructions(dir, role.as_ref(), &memory)
        .map_err(|e| format!("{}: {e}", e.code()))?;
    let mut added = Vec::new();
    if let Some(role) = &role {
        if let Some(model) = role.model.as_ref().filter(|_| !named("--model")) {
            added.extend(["--model".to_owned(), model.clone()]);
        }
        if let Some(tools) = role.tools.as_ref().filter(|_| !named("--tools")) {
            added.extend(["--tools".to_owned(), tools.join(",")]);
        }
    }
    added.extend([
        "--instructions".to_owned(),
        crate::project::as_task(dir, &composed.text)?,
    ]);
    kept.extend(added);
    Ok(kept)
}

/// Whether the daemon has a task `name`: one whose start failed after it
/// was made keeps its folder, since a retry continues it there.
fn listed(agent: &Path, name: &str) -> bool {
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
        // Left by a task of this name before: its own only if it is a
        // worktree of this repository on the task's branch.
        if !ours(here, &path, &branch) {
            return Err(format!(
                "{} exists and is not this repository's worktree on {branch}; pick another NAME",
                path.display()
            ));
        }
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

/// Whether `path` is a worktree of the repository `here` is in, on `branch`.
fn ours(here: &Path, path: &Path, branch: &str) -> bool {
    let common = |dir: &Path| {
        git(
            dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .ok()
        .and_then(|dir| std::fs::canonicalize(dir).ok())
    };
    common(path).is_some_and(|theirs| common(here) == Some(theirs))
        && git(path, &["branch", "--show-current"]).is_ok_and(|current| current == branch)
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

/// The sandfs workspace `here` is in, and where it is mounted.
fn mounted_at(here: &Path) -> Result<Option<(String, PathBuf)>, String> {
    Ok(mount_of(here)?.and_then(|ws| Some((ws.name, ws.mounted?))))
}

/// The mounted sandfs workspace `here` is in, the innermost. A folder
/// that has never seen sandfs has no home to look in.
pub fn mount_of(here: &Path) -> Result<Option<sandfs::WorkspaceInfo>, String> {
    if !Home::locate().is_ok_and(|home| home.root().exists()) {
        return Ok(None);
    }
    let workspaces = sandfs()?.workspaces().map_err(|e| format!("{e:#}"))?;
    Ok(workspaces
        .into_iter()
        .filter(|ws| {
            ws.mounted
                .as_deref()
                .is_some_and(|root| here.starts_with(root))
        })
        .max_by_key(|ws| {
            ws.mounted
                .as_deref()
                .map_or(0, |root| root.components().count())
        }))
}

pub fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
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
    // As the mount table names it, so a mount found there compares equal.
    let dir = std::fs::canonicalize(&tasks).unwrap_or(tasks).join(name);
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
    // A plain folder's copy clones its files into sandfs's home, which
    // works only on one disk: one on another disk (an external volume) gets
    // a workspace over the folder itself instead.
    let over = matches!(source, Source::Folder) && !same_disk(&root, home.root());
    let undo = match exists {
        // The task's own copy, mounted or left unmounted by a restart.
        Some(ws) if ws.mounted.as_deref() == Some(dir.as_path()) => Undo::Nothing,
        Some(ws) if ws.mounted.is_none() && dir.is_dir() => {
            home.mount(name, &dir, &MountOptions::default())
                .map_err(|e| format!("{e:#}"))?;
            Undo::Nothing
        }
        // The home is shared with the sandfs command: never take over a
        // workspace something else made.
        Some(_) => {
            return Err(format!(
                "a sandfs workspace named {name} exists that is not this task's; pick another NAME"
            ));
        }
        None if over => {
            let options = MountOptions {
                name: Some(name.to_owned()),
                ..Default::default()
            };
            home.mount(&root.to_string_lossy(), &dir, &options)
                .map_err(|e| format!("{e:#}"))?;
            Undo::Sandfs {
                name: name.to_owned(),
            }
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
        let what = match over {
            true => {
                "a sandfs workspace over this folder, which is on another disk than sandfs's \
                     home and so is not copied: the task's changes stay in the workspace, and it \
                     sees what changes here meanwhile"
            }
            false => "a sandfs copy of this folder as it is now",
        };
        format!(
            "{name}: {what}; `{me} task changes {name}` lists what the task changed, and \
             `{me} task changes {name} --apply` writes it here"
        )
    };
    let prefix = here.strip_prefix(&root).unwrap_or(Path::new(""));
    Ok(Place {
        dir: within(&dir, prefix),
        note: Some(note),
        undo,
    })
}

/// Whether `folder` is on the disk sandfs's `home` is on, or will be made
/// on.
fn same_disk(folder: &Path, home: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let disk = |path: &Path| {
        path.ancestors()
            .find_map(|dir| std::fs::metadata(dir).ok())
            .map(|found| found.dev())
    };
    disk(folder) == disk(home)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_on_another_disk_is_told_apart() {
        let tmp = std::env::temp_dir();
        assert!(same_disk(&tmp, &tmp.join("not/made/yet")));
        // A device file system is never the one temporary files are on.
        assert!(!same_disk(Path::new("/dev"), &tmp));
    }

    #[test]
    fn a_task_is_told_who_it_works_for() {
        let dir = std::env::temp_dir().join(format!("sonny-task-told-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".agents/agents")).unwrap();
        std::fs::write(
            dir.join(".agents/agents/reviewer.md"),
            "---\nmodel: a/b\ntools: [shell, read]\n---\nReview it.\n",
        )
        .unwrap();
        let told = |flags: &[&str]| {
            let flags: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
            told(&dir, &flags.iter().collect::<Vec<_>>())
        };
        let person = |flags: &[String]| {
            let at = flags.iter().position(|f| f == "--instructions").unwrap();
            assert!(flags[at + 1].ends_with("helping them well is the care."));
            flags[at + 1].clone()
        };

        let flags = told(&["--agents", "--model", "x/y"]).unwrap();
        assert_eq!(flags[..2], ["--model", "x/y"]);
        assert!(!flags.contains(&"--agents".to_owned()));
        person(&flags);

        let flags = told(&["--profile", "reviewer"]).unwrap();
        assert_eq!(flags[..4], ["--model", "a/b", "--tools", "shell,read"]);
        assert!(person(&flags).contains("Review it."));
        // The task's own picks win over the role's.
        let flags = told(&["--profile", "reviewer", "--model", "x/y"]).unwrap();
        assert_eq!(flags[..4], ["--model", "x/y", "--tools", "shell,read"]);
        let flags = told(&["--profile=reviewer", "--model=x/y", "--tools=read"]).unwrap();
        assert_eq!(
            flags[..3],
            ["--model=x/y", "--tools=read", "--instructions"]
        );

        // The person's task.md replaces Sonny's, after any role, and once.
        std::fs::write(dir.join(".agents/agents/task.md"), "Be kind.\n").unwrap();
        let flags = told(&["--profile", "reviewer"]).unwrap();
        assert!(flags.last().unwrap().ends_with("Review it.\n\nBe kind."));
        let flags = told(&["--profile", "task"]).unwrap();
        assert_eq!(flags.last().unwrap().matches("Be kind.").count(), 1);
        std::fs::remove_file(dir.join(".agents/agents/task.md")).unwrap();

        assert_eq!(told(&["--model", "x/y"]).unwrap(), ["--model", "x/y"]);
        assert!(told(&["--profile", "nobody"]).is_err());
        assert!(told(&["--agents", "--instructions", "x"]).is_err());
        assert!(told(&["--agents", "--instructions-file=x"]).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
