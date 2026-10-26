//! `sonny copy`: a task's own copy of the lead's folder, made with the
//! sandfs library built into Sonny, so nothing needs installing. In a Graft
//! project the copy is Graft's own (`gft new`), so it lands the Graft way;
//! in a git repository it is the checkout as it is, uncommitted changes
//! included, on a branch of its own; in a folder sandfs mounts, a fork of
//! that workspace; in any other folder, the folder as it is now.
//!
//! Mounts are served by Sonny run again as `sonny sandfs daemon`, which
//! outlives the command that asked for them and shares sandfs's home with
//! the `sandfs` command and Graft.
use sandfs::{ChangeKind, ChangesOptions, ForkOptions, Home, MountOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const USAGE: &str =
    "usage: sonny copy NAME | sonny copy changes NAME [--copy-to DIR] | sonny copy rm NAME";

/// Where a task's copy is mounted, as a worktree goes in `~/.agent/worktrees`.
fn tasks() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".agent/tasks"))
}

fn sandfs() -> Result<Home, String> {
    Home::locate().map_err(|e| format!("{e:#}"))
}

/// What the lead's folder is, and so what a copy of it is.
#[derive(Debug, PartialEq)]
enum Source {
    /// A Graft project, home copy or copy: `gft new` makes the task's.
    Graft,
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

/// Whether `dir` is in a Graft project: `gft status` answers there.
pub fn graft(dir: &Path) -> bool {
    Command::new("gft")
        .arg("status")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn source(here: &Path) -> Result<Source, String> {
    if graft(here) {
        return Ok(Source::Graft);
    }
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
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if !output.status.success() {
        return Err(format!("git {} failed", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn gft(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("gft")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running gft: {e}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(said.trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// `sonny copy ...`, from the lead's shell.
pub fn run(args: &[String]) -> Result<(), String> {
    let here = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|e| e.to_string())?;
    match args {
        [name] if !name.starts_with('-') => {
            let (dir, note) = make(&here, name)?;
            println!("{}", dir.display());
            eprintln!("{note}");
            Ok(())
        }
        [changes, name] if changes == "changes" => list(name, None),
        [changes, name, flag, to] if changes == "changes" && flag == "--copy-to" => {
            list(name, Some(PathBuf::from(to)))
        }
        [rm, name] if rm == "rm" => sandfs()?.remove(name, true).map_err(|e| format!("{e:#}")),
        _ => Err(USAGE.into()),
    }
}

/// Make the task `name`'s copy of `here`, or mount it again where it was
/// (as after a restart). Returns the folder the task starts in, the one in
/// the copy that matches `here`, and how its work comes back.
fn make(here: &Path, name: &str) -> Result<(PathBuf, String), String> {
    let source = source(here)?;
    if source == Source::Graft {
        let name = graft_name(name);
        let dir = match gft(here, &["path", &name]) {
            Ok(dir) => PathBuf::from(dir),
            Err(_) => {
                gft(here, &["new", &name])?;
                PathBuf::from(gft(here, &["path", &name])?)
            }
        };
        // Graft's home copy is a sandfs mount; a plain folder's is the folder.
        let prefix = mounted_at(here)?
            .and_then(|(_, root)| here.strip_prefix(root).ok().map(Path::to_owned))
            .unwrap_or_default();
        let note = format!(
            "{name}: a gft copy on the newest main; `gft land {name} --draft -m TITLE` \
             lands it as its own pull request (again for later fixes), `gft rm {name}` removes it"
        );
        return Ok((within(&dir, &prefix), note));
    }
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
        _ => (here.to_owned(), false),
    };
    match exists {
        Some(ws) if ws.mounted.is_some() => {}
        Some(_) => {
            home.mount(name, &dir, &MountOptions::default())
                .map_err(|e| format!("{e:#}"))?;
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
        }
    }
    let prefix = here.strip_prefix(&root).unwrap_or(Path::new(""));
    let note = if git_copy {
        let origin = git(&dir, &["remote", "get-url", "origin"]).unwrap_or_default();
        format!(
            "{name}: on branch {branch}; `git -C {} push origin {branch}` sends its commits to {origin}",
            shell_quote(&dir)
        )
    } else {
        let me = me();
        format!(
            "{name}: this folder as it was copied; `{me} copy changes {name}` lists what the task \
             changed, and `{me} copy changes {name} --copy-to DIR` writes it into DIR"
        )
    };
    Ok((within(&dir, prefix), note))
}

/// A task's copy's name in Graft, which takes no dots: the task's name
/// without the project's part before its first dot, since Graft's copies
/// are the project's already (`shop.fix.cart` is `fix-cart`).
fn graft_name(task: &str) -> String {
    let own = task.split_once('.').map_or(task, |(_, own)| own);
    own.replace('.', "-")
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

/// `sonny copy changes NAME [--copy-to DIR]`: what the task's copy added,
/// changed and deleted, and with a folder, written into it.
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
    #[test]
    fn a_task_names_its_graft_copy_without_the_project_or_dots() {
        assert_eq!(super::graft_name("shop.fix-cart"), "fix-cart");
        assert_eq!(super::graft_name("shop.fix.cart"), "fix-cart");
        assert_eq!(super::graft_name("solo"), "solo");
    }
}
