//! `sonny task`: start one of the lead's tasks in a folder of its own, made
//! the way Sonny's `copies` setting says, then started there with the
//! daemon's `agent run`. A git worktree (the default) starts at the last
//! commit on a branch of its own. A sandfs copy (experimental), made with
//! the sandfs library built into Sonny so nothing needs installing, is the
//! checkout as it is, uncommitted changes included, on a branch of its own;
//! in a folder sandfs mounts, a fork of that workspace; in any other
//! folder, the folder as it is now, or on another disk than sandfs's home,
//! a workspace over the folder itself. A task that could not start leaves
//! no folder behind, and `sonny task rm` takes away one that is done.
//!
//! A brief names files as the lead sees them, so paths into the lead's
//! folder in it are rewritten to the task's copy: otherwise a task told an
//! absolute path writes the lead's folder, not its own.
//!
//! sandfs mounts are served by Sonny run again as `sonny sandfs daemon`,
//! which outlives the command that asked for them and shares sandfs's home
//! with the `sandfs` command.
use sandfs::{Change, ChangeKind, ChangesOptions, ForkOptions, Home, MountOptions};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const USAGE: &str = "usage: sonny task NAME [--setup] [RUN FLAGS...] -- BRIEF\n\
                     \x20      sonny task changes NAME [--all] [--apply [--force] [--only PATH...]]\n\
                     \x20      sonny task rm NAME [--discard]";

/// The task's folder, how its work comes back, and what undoes it.
struct Place {
    dir: PathBuf,
    /// The folder copied, and where its copy is, for the brief's paths.
    copied: Option<(PathBuf, PathBuf)>,
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
        [changes, name, rest @ ..] if changes == "changes" => list(&here, name, rest),
        [rm, name, rest @ ..] if rm == "rm" => remove(&here, name, rest),
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
    // A brief names files as the lead sees them; the task's own are in its
    // copy, and a path into the lead's folder would write there instead.
    let brief: Vec<String> = match &place.copied {
        Some((from, to)) => {
            let spellings = spellings(from);
            brief
                .iter()
                .map(|arg| repath(arg, &spellings, to))
                .collect()
        }
        None => brief.to_vec(),
    };
    let repathed = place.copied.as_ref().filter(|_| {
        brief
            .iter()
            .zip(&rest[split + 1..])
            .any(|(new, old)| new != old)
    });
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
        .args(&brief)
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
    if let Some((from, to)) = repathed {
        eprintln!(
            "{name}: the brief's paths into {} now name its copy, {}",
            shell_quote(from),
            shell_quote(to)
        );
    }
    Ok(())
}

/// The run flags with what `--agents` or `--profile ROLE` would compose
/// for a task in `dir` given as `--instructions`, with who it works for
/// after it, and the role's model and tools unless the flags name their
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
    let role = match profile {
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
        crate::project::with_person(&composed.text),
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
        copied: None,
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
    let Ok(top) = git(here, &["rev-parse", "--show-toplevel"]) else {
        return Ok(in_place(here));
    };
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
        copied: Some((PathBuf::from(top), path.clone())),
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
    // A linked worktree in a plain folder names its repository by an
    // absolute path, outside the copy: git there would change the real
    // repository while the files changed stay in the copy. Git is turned
    // off there instead, in the copy only.
    let mut stubbed = Vec::new();
    let mut looked_all = true;
    if !git_copy && matches!(undo, Undo::Sandfs { .. }) {
        let complete;
        (stubbed, complete) = linked_worktrees(&root);
        looked_all = complete;
        for worktree in &stubbed {
            if let Err(e) = std::fs::write(dir.join(worktree).join(".git"), GIT_OFF) {
                self::undo(&undo);
                return Err(format!("turning git off in {}: {e}", worktree.display()));
            }
        }
    }
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
                     home and so is not copied: the task's changes stay in the workspace, a file \
                     or folder it has looked at may not show what changes here after that, and \
                     --apply writes its version over anything changed here meanwhile, unchecked"
            }
            false => {
                "a sandfs copy of this folder as it is now, whose --apply refuses a file \
                     changed here since unless given --force"
            }
        };
        let mut note = format!(
            "{name}: {what}; `{me} task changes {name}` lists what the task changed, and \
             `{me} task changes {name} --apply` writes it here. Copy nothing from its folder \
             onto this one yourself. Commits it makes in a repository in this folder stay in \
             its copy, so a task that commits starts in that repository's own folder"
        );
        if !stubbed.is_empty() {
            let named: Vec<String> = stubbed.iter().map(|path| shell_quote(path)).collect();
            note.push_str(&format!(
                ". Git is off in its copy of {} linked worktrees, whose .git points outside \
                 the copy: {}",
                stubbed.len(),
                named.join(", ")
            ));
        }
        if !looked_all {
            note.push_str(
                ". This folder is too big to look for every linked worktree in, so git in one \
                 it missed still changes the repository it belongs to",
            );
        }
        note
    };
    let prefix = here.strip_prefix(&root).unwrap_or(Path::new(""));
    Ok(Place {
        dir: within(&dir, prefix),
        copied: Some((root, dir.clone())),
        note: Some(note),
        undo,
    })
}

/// What a linked worktree's `.git` holds in a plain folder's copy, so git
/// stops there with this as its reason.
const GIT_OFF: &str = "gitdir: /dev/null/sonny: this linked worktree's repository is outside \
                       the task's copy, so git is off here; start a task in that repository's \
                       own folder to use git on it\n";

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

/// This program, by the path that outlives an upgrade.
fn me() -> String {
    crate::skills::sonny_path().map_or("sonny".into(), |me| shell_quote(&me))
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

/// The ways a brief may spell the folder `root`: as it is, through the
/// symlinks in `/` and in the home folder that lead into it (macOS's
/// `/tmp`, a `~/work` link to another disk), through the shell's own
/// working directory, and each under the home folder as `~/` and
/// `$HOME/`. Longest first. None for the home folder or a folder above it,
/// whose paths are every program's, not the lead's.
fn spellings(root: &Path) -> Vec<String> {
    let home = home_dir().ok();
    if root == Path::new("/") || home.as_deref().is_some_and(|home| home.starts_with(root)) {
        return Vec::new();
    }
    let mut found = vec![root.to_owned()];
    let links = |dir: &Path| -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_symlink()))
            .map(|entry| entry.path())
            .collect()
    };
    let mut through = links(Path::new("/"));
    through.extend(home.iter().flat_map(|home| links(home)));
    for link in through {
        if let Ok(target) = std::fs::canonicalize(&link)
            && let Ok(rest) = root.strip_prefix(&target)
        {
            found.push(within(&link, rest));
        }
    }
    // The shell's spelling of where the lead is, as `cd` took it.
    if let Some(pwd) = std::env::var_os("PWD").map(PathBuf::from)
        && let Ok(real) = std::fs::canonicalize(&pwd)
        && let Ok(below) = real.strip_prefix(root)
    {
        let mut logical = pwd.clone();
        for _ in below.components() {
            logical.pop();
        }
        if logical.join(below) == pwd {
            found.push(logical);
        }
    }
    let mut spelled: Vec<String> = Vec::new();
    for path in &found {
        let path = path.to_string_lossy().into_owned();
        if let Some(home) = &home
            && let Ok(rest) = Path::new(&path).strip_prefix(home)
        {
            spelled.push(format!("~/{}", rest.display()));
            spelled.push(format!("$HOME/{}", rest.display()));
        }
        spelled.push(path);
    }
    spelled.sort_by_key(|path| std::cmp::Reverse(path.len()));
    spelled.dedup();
    spelled
}

/// `text` with every path that starts with one of `spellings` starting at
/// `to` instead: a whole path, so `/a/shop` names no part of `/a/shops`
/// or `/b/a/shop`.
fn repath(text: &str, spellings: &[String], to: &Path) -> String {
    let wordy = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-');
    let to = to.to_string_lossy();
    let mut out = String::with_capacity(text.len());
    let mut last = None;
    let mut at = 0;
    'scan: while at < text.len() {
        let rest = &text[at..];
        let starts = !last.is_some_and(|c: char| wordy(c) || matches!(c, '/' | '.' | '~' | '$'));
        if starts {
            for spelling in spellings {
                let Some(after) = rest.strip_prefix(spelling.as_str()) else {
                    continue;
                };
                let mut next = after.chars();
                let ends = match next.next() {
                    None => true,
                    Some('.') => !next.next().is_some_and(wordy),
                    Some(c) => !wordy(c),
                };
                if ends {
                    out.push_str(&to);
                    at += spelling.len();
                    last = spelling.chars().last();
                    continue 'scan;
                }
            }
        }
        let c = rest.chars().next().unwrap_or_default();
        out.push(c);
        at += c.len_utf8();
        last = Some(c);
    }
    out
}

/// The linked worktrees in the plain folder `root`, relative to it: each
/// folder whose `.git` file names its repository by an absolute path or a
/// path out of `root`, which a copy of `root` still reaches; and whether
/// every folder was looked in. Found at most `DEEP` folders down, and in
/// at most `LOOKED` folders, so a start stays quick in a folder of
/// thousands of projects; repositories' own lists name their linked
/// worktrees wherever in `root` they are.
fn linked_worktrees(root: &Path) -> (Vec<PathBuf>, bool) {
    const LOOKED: usize = 50_000;
    const DEEP: usize = 6;
    let outbound = |dir: &Path| {
        let Ok(text) = std::fs::read_to_string(dir.join(".git")) else {
            return false;
        };
        let Some(gitdir) = text.trim().strip_prefix("gitdir:").map(str::trim) else {
            return false;
        };
        let gitdir = Path::new(gitdir);
        gitdir.is_absolute()
            || std::fs::canonicalize(dir.join(gitdir)).is_ok_and(|real| !real.starts_with(root))
    };
    let mut found = Vec::new();
    let mut looked = 0;
    let mut next = vec![(root.to_owned(), 0)];
    while let Some((dir, depth)) = next.pop() {
        looked += 1;
        if looked > LOOKED {
            return (tidy(root, found), false);
        }
        let git = dir.join(".git");
        if git.is_file() && outbound(&dir) {
            found.push(dir.clone());
        }
        if git.is_dir() {
            // A repository's linked worktrees, wherever in `root` they are.
            let listed = std::fs::read_dir(git.join("worktrees"))
                .into_iter()
                .flatten();
            for entry in listed.flatten() {
                let Ok(file) = std::fs::read_to_string(entry.path().join("gitdir")) else {
                    continue;
                };
                if let Some(worktree) = Path::new(file.trim()).parent()
                    && worktree.starts_with(root)
                    && outbound(worktree)
                {
                    found.push(worktree.to_owned());
                }
            }
        }
        if depth == DEEP {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        // Inside repositories and worktrees too: one may hold another
        // repository's worktree, which its own list doesn't name.
        for entry in entries.flatten() {
            let skip = matches!(
                entry.file_name().to_str(),
                Some(".git" | "node_modules" | "target" | "__pycache__")
            );
            if !skip && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                next.push((entry.path(), depth + 1));
            }
        }
    }
    (tidy(root, found), true)
}

/// `found`, relative to `root`, sorted and once each.
fn tidy(root: &Path, found: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = found
        .into_iter()
        .filter_map(|dir| dir.strip_prefix(root).ok().map(Path::to_owned))
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Whether `path` is git's own, in a `.git` folder or a worktree's `.git`
/// file: never written into the lead's folder, since git runs what such
/// files name and a copy's commits come back by push instead.
fn in_git(path: &str) -> bool {
    path.split('/').any(|part| part == ".git")
}

/// Which of `paths`, relative to `here`, git ignores: each asked of the
/// innermost repository in `here` that holds it, such as a project's
/// `__pycache__` in a folder of projects.
fn ignored(here: &Path, paths: &[&str]) -> HashSet<String> {
    let mut by_repo: BTreeMap<PathBuf, Vec<(&str, String)>> = BTreeMap::new();
    for path in paths {
        let repo = Path::new(path)
            .ancestors()
            .skip(1)
            .find(|dir| here.join(dir).join(".git").exists());
        if let Some(repo) = repo {
            let inside = Path::new(path)
                .strip_prefix(repo)
                .unwrap_or(Path::new(path));
            by_repo
                .entry(here.join(repo))
                .or_default()
                .push((path, inside.to_string_lossy().into_owned()));
        }
    }
    let mut found = HashSet::new();
    for (repo, paths) in by_repo {
        let mut asked = Command::new("git");
        asked
            .arg("-C")
            .arg(&repo)
            .args(["check-ignore", "-z", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let Ok(mut child) = asked.spawn() else {
            continue;
        };
        // Fed while its answer is read, so neither waits on a full pipe.
        let list: Vec<u8> = paths
            .iter()
            .flat_map(|(_, inside)| inside.bytes().chain([0]))
            .collect();
        let feeding = child.stdin.take().map(|mut input| {
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = input.write_all(&list);
            })
        });
        // Exit 1 means none is ignored; any failure leaves them all shown.
        let output = child.wait_with_output();
        if let Some(feeding) = feeding {
            let _ = feeding.join();
        }
        let Ok(output) = output else {
            continue;
        };
        let said: HashSet<&[u8]> = output.stdout.split(|b| *b == 0).collect();
        found.extend(
            paths
                .iter()
                .filter(|(_, inside)| said.contains(inside.as_bytes()))
                .map(|(path, _)| path.to_string()),
        );
    }
    found
}

/// `sonny task changes NAME [--all] [--apply [--force] [--only PATH...]]`:
/// what the task's sandfs copy added, changed and deleted, and with
/// `--apply`, written into this folder: what is listed, or the `--only`
/// paths, and never git's own files. Files git ignores are left out unless
/// `--all` or `--only` asks for them.
fn list(here: &Path, name: &str, args: &[String]) -> Result<(), String> {
    let (mut apply, mut force, mut all, mut only) = (false, false, false, Vec::new());
    let mut words = args.iter().peekable();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--apply" => apply = true,
            "--force" => force = true,
            "--all" => all = true,
            "--only" => {
                while let Some(path) = words.next_if(|word| !word.starts_with("--")) {
                    only.push(path.trim_end_matches('/').to_owned());
                }
                if only.is_empty() {
                    return Err(USAGE.into());
                }
            }
            _ => return Err(USAGE.into()),
        }
    }
    if (force || !only.is_empty()) && !apply {
        return Err(USAGE.into());
    }
    let home = sandfs()?;
    let options = ChangesOptions {
        paths: only.clone(),
        ..Default::default()
    };
    let found = home.changes(name, &options).map_err(|e| format!("{e:#}"))?;
    let (git_own, rest): (Vec<&Change>, Vec<&Change>) = found
        .changes
        .iter()
        .partition(|change| in_git(&change.path));
    let skipped = match all || !only.is_empty() {
        true => HashSet::new(),
        false => ignored(
            here,
            &rest.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
        ),
    };
    let shown: Vec<&Change> = rest
        .into_iter()
        .filter(|change| !skipped.contains(&change.path))
        .collect();
    let mut held_back = found.held_back;
    let mut git_settings = found.git_settings.clone();
    if apply && !shown.is_empty() {
        // Exactly what is listed, so nothing the lead did not see is written.
        let options = ChangesOptions {
            paths: shown.iter().map(|change| change.path.clone()).collect(),
            copy_to: Some(here.to_owned()),
            force,
            ..Default::default()
        };
        let applied = home.changes(name, &options).map_err(|e| format!("{e:#}"))?;
        held_back = applied.held_back;
        git_settings = applied.git_settings;
    }
    for change in &shown {
        let kind = match change.kind {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
        };
        println!("{kind} {}", change.path);
    }
    let left = if apply {
        "listed or written here"
    } else {
        "listed"
    };
    if apply && shown.is_empty() {
        eprintln!("nothing to apply");
    }
    if !git_own.is_empty() {
        eprintln!(
            "{} changes in git's own files not {left}: commits made in the task's copy come \
             back only by push",
            git_own.len()
        );
    }
    if !skipped.is_empty() {
        eprintln!(
            "{} files git ignores not {left}; --all lists them",
            skipped.len()
        );
    }
    if held_back > 0 {
        eprintln!(
            "{held_back} changes in .git left out: git runs what is named there, so copy those by hand"
        );
    }
    for setting in &git_settings {
        eprintln!("the copy changed git's {setting}");
    }
    Ok(())
}

/// `sonny task rm NAME [--discard]`: remove the task and the folder Sonny
/// made it, once its work is here: a sandfs copy's changes applied, or for
/// a git copy, committed and pushed; a worktree's changes committed, its
/// branch kept unless merged. `--discard` removes them anyway. An agent
/// still working is not removed, and then neither is anything else.
fn remove(here: &Path, name: &str, args: &[String]) -> Result<(), String> {
    let discard = match args {
        [] => false,
        [flag] if flag == "--discard" => true,
        _ => return Err(USAGE.into()),
    };
    let agent = crate::agent_bin().ok_or("Agent's binary was not found; set AGENT_BIN")?;
    let folder = tasks()?.join(name);
    let folder = std::fs::canonicalize(&folder).unwrap_or(folder);
    let copy = match Home::locate().is_ok_and(|home| home.root().exists()) {
        true => sandfs()?
            .workspaces()
            .map_err(|e| format!("{e:#}"))?
            .into_iter()
            .find(|ws| ws.name == name),
        false => None,
    };
    if let Some(ws) = &copy
        && !(ws.mounted.as_deref() == Some(folder.as_path())
            || ws.mounted.is_none() && folder.is_dir())
    {
        return Err(format!(
            "the sandfs workspace {name} is not this task's copy, so it was left alone"
        ));
    }
    let branch = format!("agent/{name}");
    let worktree = home_dir()?.join(".agent/worktrees").join(name);
    let worktree = match worktree.exists() {
        true if ours(here, &worktree, &branch) => Some(worktree),
        true => {
            return Err(format!(
                "{} is not this repository's worktree on {branch}; run this in the repository \
                 the task was started from",
                worktree.display()
            ));
        }
        false => None,
    };
    if !discard {
        if copy.is_some() {
            let left = not_here(name, &folder, here)?;
            if !left.is_empty() {
                return Err(format!(
                    "{name} has work that is not here: {left}; pass --discard to remove it anyway"
                ));
            }
        }
        if let Some(path) = &worktree
            && !git(path, &["status", "--porcelain", "--untracked-files=all"])?.is_empty()
        {
            return Err(format!(
                "{name}'s worktree has uncommitted changes; commit them, or pass --discard to \
                 remove it anyway"
            ));
        }
    }
    if listed(&agent, name) {
        let output = crate::agent_command(&agent)?
            .args(["rm", "--bot", name])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("running {}: {e}", agent.display()))?;
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr);
            return Err(format!("{name} was not removed: {}", said.trim()));
        }
    }
    if copy.is_some() {
        sandfs()?.remove(name, true).map_err(|e| format!("{e:#}"))?;
    }
    // The empty folder a mount was on.
    let _ = std::fs::remove_dir(&folder);
    if let Some(path) = &worktree {
        let path = path.to_string_lossy();
        let mut args = vec!["worktree", "remove"];
        if discard {
            args.push("--force");
        }
        args.push(&path);
        git(here, &args)?;
        let delete = if discard { "-D" } else { "-d" };
        if git(here, &["branch", delete, &branch]).is_err() {
            eprintln!(
                "{branch} is kept, since it has commits not merged here; `git branch -D {branch}` \
                 deletes it"
            );
        }
    }
    eprintln!("{name}: removed");
    Ok(())
}

/// What of the task's work in its copy at `folder` is not in `here`, said
/// in a few words, or nothing: a git copy's uncommitted changes and
/// commits its origin lacks; any other copy's changes not applied here,
/// but for git's own files and those git ignores.
fn not_here(name: &str, folder: &Path, here: &Path) -> Result<String, String> {
    if folder.join(".git").exists() {
        if !git(folder, &["status", "--porcelain", "--untracked-files=all"])?.is_empty() {
            return Ok("uncommitted changes in its copy".into());
        }
        // Every branch's tip, not only the one checked out: the copy goes
        // with the commits that are only in it.
        let origin = git(folder, &["remote", "get-url", "origin"]).unwrap_or_default();
        let mut tips = git(
            folder,
            &[
                "for-each-ref",
                "--format=%(objectname) %(refname:short)",
                "refs/heads",
            ],
        )?;
        // A detached HEAD's commits are on no branch.
        tips.push_str(&format!("\n{} HEAD", git(folder, &["rev-parse", "HEAD"])?));
        let unpushed: Vec<&str> = tips
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(tip, _)| match Path::new(&origin).is_dir() {
                true => git(
                    Path::new(&origin),
                    &["cat-file", "-e", &format!("{tip}^{{commit}}")],
                )
                .is_err(),
                false => git(folder, &["branch", "-r", "--contains", tip])
                    .unwrap_or_default()
                    .is_empty(),
            })
            .map(|(_, branch)| branch)
            .collect();
        return Ok(match unpushed.is_empty() {
            true => String::new(),
            false => format!("commits not pushed to {origin}, on {}", unpushed.join(", ")),
        });
    }
    let found = sandfs()?
        .changes(name, &ChangesOptions::default())
        .map_err(|e| format!("{e:#}"))?;
    let mine: Vec<&Change> = found.changes.iter().filter(|c| !in_git(&c.path)).collect();
    // What --apply leaves out by default is no work to keep.
    let skipped = ignored(
        here,
        &mine.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
    );
    let missing: Vec<String> = mine
        .into_iter()
        .filter(|change| !skipped.contains(&change.path))
        .filter(|change| {
            let there = here.join(&change.path);
            match change.kind {
                ChangeKind::Deleted => there.symlink_metadata().is_ok(),
                _ => !same_entry(&there, &folder.join(&change.path)),
            }
        })
        .map(|change| change.path.clone())
        .collect();
    Ok(match missing.len() {
        0 => String::new(),
        n => format!(
            "{n} changes not applied here ({}{}); `{} task changes {name} --apply` here writes them",
            missing[..n.min(3)].join(", "),
            if n > 3 { ", ..." } else { "" },
            me()
        ),
    })
}

/// Whether `a` and `b` are the same kind of entry with the same contents:
/// for a symlink, its target, not what it leads to. Anything that can't be
/// read is not the same, since then that can't be told.
fn same_entry(a: &Path, b: &Path) -> bool {
    let (Ok(kind_a), Ok(kind_b)) = (a.symlink_metadata(), b.symlink_metadata()) else {
        return false;
    };
    let (kind_a, kind_b) = (kind_a.file_type(), kind_b.file_type());
    if kind_a.is_symlink() || kind_b.is_symlink() {
        return kind_a.is_symlink()
            && kind_b.is_symlink()
            && matches!((std::fs::read_link(a), std::fs::read_link(b)), (Ok(x), Ok(y)) if x == y);
    }
    if kind_a.is_dir() || kind_b.is_dir() {
        return kind_a.is_dir() && kind_b.is_dir();
    }
    // Whether it runs, as --apply keeps it.
    use std::os::unix::fs::PermissionsExt;
    let runs = |path: &Path| {
        path.symlink_metadata()
            .map(|found| found.permissions().mode() & 0o111 != 0)
            .ok()
    };
    runs(a) == runs(b) && matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

/// The names of every agent the daemon has, or None when it can't say.
fn agents(agent: &Path) -> Option<HashSet<String>> {
    let output = Command::new(agent)
        .arg("ls")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    match serde_json::from_slice::<Value>(&output.stdout).ok()? {
        Value::Array(bots) => Some(
            bots.iter()
                .filter_map(|bot| bot["name"].as_str().map(str::to_owned))
                .collect(),
        ),
        _ => None,
    }
}

/// Mount again the task copies a restart unmounted, so a task never works
/// in an empty folder: every workspace whose folder in `~/.agent/tasks`
/// is there but not mounted, and whose task the daemon still has. Returns
/// what could not be mounted, after a word on copies whose task is gone.
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
    // Unknown, as when the daemon can't be asked: mount them all rather
    // than leave a task in an empty folder.
    let alive = crate::agent_bin().and_then(|agent| agents(&agent));
    let real = std::fs::canonicalize(&tasks).unwrap_or(tasks.clone());
    let mut said = Vec::new();
    let mut gone = Vec::new();
    for ws in workspaces {
        // Only Sonny's task copies: the home is shared with the sandfs command.
        let ours = match &ws.mounted {
            Some(at) => *at == real.join(&ws.name),
            None => tasks.join(&ws.name).is_dir(),
        };
        if !ours {
            continue;
        }
        if alive
            .as_ref()
            .is_some_and(|alive| !alive.contains(&ws.name))
        {
            gone.push(ws.name);
        } else if ws.mounted.is_none()
            && let Err(e) = home.mount(&ws.name, &tasks.join(&ws.name), &MountOptions::default())
        {
            said.push(format!("task copy {}: {e:#}", ws.name));
        }
    }
    if !gone.is_empty() {
        said.insert(
            0,
            format!(
                "task copies whose task is gone: {}; `{} task rm NAME` in the folder one came \
                 from removes it",
                gone.join(", "),
                me()
            ),
        );
    }
    said
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
    fn a_brief_names_the_copy_where_it_named_the_folder() {
        let spellings = vec!["/work/shop".to_owned(), "~/shop".to_owned()];
        let to = Path::new("/home/me/.agent/tasks/shop.fix");
        for (brief, now) in [
            (
                "edit /work/shop/src/a.rs",
                "edit /home/me/.agent/tasks/shop.fix/src/a.rs",
            ),
            ("in /work/shop.", "in /home/me/.agent/tasks/shop.fix."),
            (
                "cd ~/shop && make",
                "cd /home/me/.agent/tasks/shop.fix && make",
            ),
            ("'/work/shop/x'", "'/home/me/.agent/tasks/shop.fix/x'"),
            // Other folders, and parts of other paths, stay as they are.
            ("/work/shops/a", "/work/shops/a"),
            ("/work/shop.bak", "/work/shop.bak"),
            ("/other/work/shop/a", "/other/work/shop/a"),
            ("x~/shop", "x~/shop"),
        ] {
            assert_eq!(repath(brief, &spellings, to), now, "{brief}");
        }
    }

    #[test]
    fn a_folder_is_spelled_through_links_into_it() {
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("sonny-spellings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("disk/shop")).unwrap();
        let root = base.join("disk/shop");
        // The home folder and `/` are no lead's own, so never rewritten.
        assert!(spellings(Path::new("/")).is_empty());
        assert!(spellings(&home_dir().unwrap()).is_empty());
        let found = spellings(&root);
        assert_eq!(found, vec![root.to_string_lossy().into_owned()]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn linked_worktrees_in_a_plain_folder_are_found() {
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("sonny-linked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("folder");
        let repo = root.join("src/shop");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        let run = |dir: &Path, args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&repo, &["init", "-q"]);
        run(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
        // One inside the repository, one beside it, both of the repository
        // in the folder; one of a repository outside it.
        run(&repo, &["worktree", "add", "-q", ".worktrees/a", "-b", "a"]);
        run(&repo, &["worktree", "add", "-q", "../shop-b", "-b", "b"]);
        let outside = base.join("elsewhere/repo");
        std::fs::create_dir_all(&outside).unwrap();
        run(&outside, &["init", "-q"]);
        run(&outside, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let far = root.join("deep/c");
        std::fs::create_dir_all(root.join("deep")).unwrap();
        run(
            &outside,
            &["worktree", "add", "-q", &far.to_string_lossy(), "-b", "c"],
        );
        // A submodule's .git names a folder inside the copy, which stays in it.
        std::fs::create_dir_all(root.join("sub/.git-modules/x")).unwrap();
        std::fs::create_dir_all(root.join("sub/lib")).unwrap();
        std::fs::write(root.join("sub/lib/.git"), "gitdir: ../.git-modules/x\n").unwrap();
        // Another repository's worktree inside a repository in the folder,
        // which that repository's own list doesn't name.
        let nested = repo.join("vendor/d");
        std::fs::create_dir_all(repo.join("vendor")).unwrap();
        run(
            &outside,
            &[
                "worktree",
                "add",
                "-q",
                &nested.to_string_lossy(),
                "-b",
                "d",
            ],
        );
        let (found, all) = linked_worktrees(&root);
        let _ = std::fs::remove_dir_all(&base);
        assert!(all);
        assert_eq!(
            found,
            [
                "deep/c",
                "src/shop/.worktrees/a",
                "src/shop/vendor/d",
                "src/shop-b"
            ]
            .map(PathBuf::from)
            .to_vec()
        );
    }

    #[test]
    fn git_off_stops_git_with_its_reason() {
        let base = std::env::temp_dir().join(format!("sonny-git-off-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("wt")).unwrap();
        std::fs::write(base.join("wt/.git"), GIT_OFF).unwrap();
        let output = Command::new("git")
            .arg("-C")
            .arg(base.join("wt"))
            .arg("status")
            .env("GIT_CEILING_DIRECTORIES", &base)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&base);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("git is off here"));
    }

    #[test]
    fn ignored_files_are_asked_of_the_repository_holding_them() {
        let base = std::env::temp_dir().join(format!("sonny-ignored-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("shop")).unwrap();
        let ok = Command::new("git")
            .arg("-C")
            .arg(base.join("shop"))
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        std::fs::write(base.join("shop/.gitignore"), "__pycache__/\n").unwrap();
        let found = ignored(
            &base,
            &[
                "shop/__pycache__/a.pyc",
                "shop/a.py",
                "notes.md",
                "__pycache__/b.pyc",
            ],
        );
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(found, HashSet::from(["shop/__pycache__/a.pyc".to_owned()]));
        assert!(in_git("shop/.git/index") && in_git("wt/.git") && !in_git("a.github/x"));
    }

    #[test]
    fn entries_are_the_same_only_when_they_can_be_told_apart_from_none() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("sonny-same-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let at = |name: &str| base.join(name);
        std::fs::write(at("a"), "x").unwrap();
        std::fs::write(at("b"), "x").unwrap();
        std::fs::write(at("c"), "y").unwrap();
        std::os::unix::fs::symlink("nowhere", at("link")).unwrap();
        std::os::unix::fs::symlink("a", at("to-a")).unwrap();
        std::os::unix::fs::symlink("b", at("to-b")).unwrap();
        std::fs::write(at("run"), "x").unwrap();
        std::fs::set_permissions(at("run"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let said = [
            same_entry(&at("a"), &at("b")),
            // The same bytes, but one runs and the other doesn't.
            same_entry(&at("a"), &at("run")),
            same_entry(&at("a"), &at("c")),
            // A dangling link is not the same as nothing there.
            same_entry(&at("missing"), &at("link")),
            // Links to the same contents by other targets differ.
            same_entry(&at("to-a"), &at("to-b")),
            same_entry(&at("to-a"), &at("a")),
        ];
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(said, [true, false, false, false, false, false]);
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
            assert!(flags[at + 1].ends_with(crate::project::PERSON));
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

        assert_eq!(told(&["--model", "x/y"]).unwrap(), ["--model", "x/y"]);
        assert!(told(&["--profile", "nobody"]).is_err());
        assert!(told(&["--agents", "--instructions", "x"]).is_err());
        assert!(told(&["--agents", "--instructions-file=x"]).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
