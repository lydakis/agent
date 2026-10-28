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
    note: Option<String>,
    undo: Undo,
    /// The lead's folder the task's is a copy of, and the copy's top: a
    /// brief that names a path in the one names it in the other.
    copy: Option<(PathBuf, PathBuf)>,
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
    // A path in the lead's folder would have the task write there instead
    // of in its copy, where nothing keeps it apart from the lead's work.
    let (brief, moved) = match &place.copy {
        Some((from, to)) => in_copy(brief, from, to),
        None => (brief.to_vec(), 0),
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
    if let (Some((from, to)), 1..) = (&place.copy, moved) {
        eprintln!(
            "{name}: the brief named {} {moved} time{}; it names the task's copy {} instead",
            shell_quote(from),
            if moved == 1 { "" } else { "s" },
            shell_quote(to)
        );
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

/// `brief` with each path in the folder `from` turned into the same path
/// in its copy `to`, and how many were. A folder has more names than its
/// own: the one the lead's shell was given (`$PWD`, through a link), a
/// link in the home to it or a folder holding it (`~/workplace` for
/// `/Volumes/workplace`), each also from `~`, and macOS's `/tmp` for
/// `/private/tmp`.
fn in_copy(brief: &[String], from: &Path, to: &Path) -> (Vec<String>, usize) {
    let names = names(from);
    let to = to.to_string_lossy();
    let mut moved = 0;
    let brief = brief
        .iter()
        .map(|arg| {
            let (arg, count) = replace_paths(arg, &names, &to);
            moved += count;
            arg
        })
        .collect();
    (brief, moved)
}

/// Every name of the folder `from` a brief might use, longest first.
fn names(from: &Path) -> Vec<String> {
    let mut names = vec![from.to_path_buf()];
    let canonical = std::fs::canonicalize(from).unwrap_or_else(|_| from.to_owned());
    names.push(canonical.clone());
    // The shell's own name for where it is, which may go through a link.
    if let (Some(pwd), Ok(here)) = (std::env::var_os("PWD"), std::env::current_dir()) {
        let pwd = PathBuf::from(pwd);
        let here = std::fs::canonicalize(here).unwrap_or_default();
        if std::fs::canonicalize(&pwd).is_ok_and(|real| real == here)
            && let Ok(below) = here.strip_prefix(&canonical)
        {
            let mut top = pwd;
            for _ in below.components() {
                top.pop();
            }
            // Entered through a link to a folder inside it, what is left names
            // something else.
            if std::fs::canonicalize(&top).is_ok_and(|real| real == canonical) {
                names.push(top);
            }
        }
    }
    let home = home_dir().ok();
    // Links in the home to the folder, or to a folder holding it.
    if let Some(entries) = home.as_ref().and_then(|home| std::fs::read_dir(home).ok()) {
        for entry in entries.flatten() {
            let link = entry.path();
            if !entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
                continue;
            }
            if let Ok(target) = std::fs::canonicalize(&link)
                && let Ok(below) = canonical.strip_prefix(&target)
            {
                names.push(within(&link, below));
            }
        }
    }
    let mut names: Vec<String> = names
        .iter()
        .flat_map(|name| {
            let name = name.to_string_lossy().into_owned();
            // macOS's `/tmp` and `/var` are `/private/tmp` and `/private/var`;
            // other folders under `/private` have no such second name.
            let short = name
                .strip_prefix("/private")
                .filter(|rest| rest.starts_with('/'))
                .filter(|rest| {
                    std::fs::canonicalize(rest)
                        .is_ok_and(|real| Some(real) == std::fs::canonicalize(&name).ok())
                });
            [Some(name.clone()), short.map(str::to_owned)]
        })
        .flatten()
        .collect();
    if let Some(home) = home
        .as_ref()
        .map(|home| home.to_string_lossy().into_owned())
    {
        let tilde: Vec<String> = names
            .iter()
            .filter_map(|name| name.strip_prefix(&home))
            .filter(|rest| rest.starts_with('/'))
            .map(|rest| format!("~{rest}"))
            .collect();
        names.extend(tilde);
    }
    names.retain(|name| name.len() > 1);
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    names.dedup();
    names
}

/// `text` with each of `names` that stands as a whole path, or as the
/// start of one, turned into `to`; and how many were.
fn replace_paths(text: &str, names: &[String], to: &str) -> (String, usize) {
    // A name inside a longer one (`/work` in `/workplace`, `/work+old` or
    // `/x/work`) is another path. Names end at spaces and at what sets a path
    // apart in prose and commands: quotes, brackets, `=`, `:` and the like.
    let part_of_name = |c: char| {
        !c.is_whitespace()
            && !matches!(
                c,
                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
            )
            && !matches!(c, ',' | ';' | ':' | '=' | '|' | '&' | '!' | '?' | '*')
    };
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut rest = text;
    'scan: while !rest.is_empty() {
        let at_start = out.chars().next_back().is_none_or(|c| !part_of_name(c));
        if at_start {
            for name in names {
                if let Some(after) = rest.strip_prefix(name.as_str())
                    && ends_name(after, part_of_name)
                    && stays_inside(after, part_of_name)
                {
                    out.push_str(to);
                    count += 1;
                    rest = after;
                    continue 'scan;
                }
            }
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    (out, count)
}

/// Whether the rest of a path, `after` the folder's name, stays inside the
/// folder: `/work/repo/../notes.md` names a file beside it, not in it.
fn stays_inside(after: &str, part_of_name: impl Fn(char) -> bool) -> bool {
    let end = after
        .find(|c: char| !part_of_name(c))
        .unwrap_or(after.len());
    let mut depth = 0usize;
    for part in after[..end].split('/') {
        match part {
            "" | "." => {}
            ".." => match depth.checked_sub(1) {
                Some(up) => depth = up,
                None => return false,
            },
            _ => depth += 1,
        }
    }
    true
}

/// Whether a name ends where `after` starts: at a `/` going on below it,
/// or at anything not part of a path, a sentence's last `.` or `,` too.
fn ends_name(after: &str, part_of_name: impl Fn(char) -> bool) -> bool {
    let mut chars = after.chars();
    match chars.next() {
        None | Some('/') => true,
        Some('.' | ',') => chars.next().is_none_or(char::is_whitespace),
        Some(c) => !part_of_name(c),
    }
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
        copy: None,
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
    let top = PathBuf::from(git(here, &["rev-parse", "--show-toplevel"])?);
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
        copy: Some((top, path)),
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
                     it has looked at stays as it first saw it, and `--apply` writes over what \
                     changed here meanwhile without asking"
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
        note: Some(note),
        undo,
        copy: Some((root, dir)),
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
        if let Some(ws) = &copy {
            // Left unmounted by a restart: its folder is empty until it is
            // mounted again, and an empty folder would pass for no work.
            if ws.mounted.is_none() {
                sandfs()?
                    .mount(name, &folder, &MountOptions::default())
                    .map_err(|e| {
                        format!("{name}'s copy could not be mounted to check it: {e:#}")
                    })?;
            }
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

/// What the repository at `repo` keeps that its origin lacks, by name:
/// each branch, tag and stash, and HEAD, whose object origin does not
/// have, with origin's URL. A tag is work as much as a branch, and a
/// stash or a detached HEAD holds commits no branch does.
fn unkept(repo: &Path) -> Result<(String, Vec<String>), String> {
    let origin = git(repo, &["remote", "get-url", "origin"]).unwrap_or_default();
    let refs = git(
        repo,
        &[
            "for-each-ref",
            "--format=%(objectname) %(objecttype) %(*objectname) %(*objecttype) %(refname:short)",
            "refs/heads",
            "refs/tags",
        ],
    )?;
    let stashes = git(repo, &["stash", "list", "--format=%H commit   %gd"])?;
    let head = format!("{} commit   HEAD", git(repo, &["rev-parse", "HEAD"])?);
    let local = Path::new(&origin).is_dir();
    let unkept = refs
        .lines()
        .chain(stashes.lines())
        .chain([head.as_str()])
        .filter_map(|line| {
            let mut parts = line.splitn(5, ' ');
            let (object, kind) = (parts.next()?, parts.next()?);
            // An annotated tag, and what it names.
            let (named, named_kind) = (parts.next()?, parts.next()?);
            let (named, named_kind) = match named.is_empty() {
                true => (object, kind),
                false => (named, named_kind),
            };
            Some((object, named, named_kind, parts.next()?))
        })
        .filter(|(object, named, kind, _)| match local {
            // Any object origin has, it has with all it names.
            true => git(Path::new(&origin), &["cat-file", "-e", object]).is_err(),
            false if *kind == "commit" => git(repo, &["branch", "-r", "--contains", named])
                .unwrap_or_default()
                .is_empty(),
            // A tag of a tree or blob, which no remote branch contains,
            // can't be checked against a remote without asking it.
            false => false,
        })
        .map(|(_, _, _, name)| name.to_owned())
        .collect();
    Ok((origin, unkept))
}

/// What of the task's work in its copy at `folder` is not in `here`, said
/// in a few words, or nothing: a git copy's uncommitted changes and
/// commits its origin lacks; any other copy's changes not applied here,
/// but for git's own files and those git ignores.
fn not_here(name: &str, folder: &Path, here: &Path) -> Result<String, String> {
    if folder.join(".git").exists() {
        // The copy's own repository, then each submodule checked out in it,
        // which a push of the copy's branch does not push.
        let subs = git(
            folder,
            &[
                "submodule",
                "foreach",
                "--quiet",
                "--recursive",
                "echo \"$displaypath\"",
            ],
        )?;
        for sub in std::iter::once("").chain(subs.lines()) {
            let repo = folder.join(sub);
            let at = |what: &str| match sub {
                "" => what.to_owned(),
                sub => format!("{what} in its submodule {sub}"),
            };
            if !git(&repo, &["status", "--porcelain", "--untracked-files=all"])?.is_empty() {
                return Ok(at("uncommitted changes in its copy"));
            }
            let (origin, unkept) = unkept(&repo)?;
            if !unkept.is_empty() {
                return Ok(at(&format!(
                    "commits not pushed to {origin}, on {}",
                    unkept.join(", ")
                )));
            }
        }
        return Ok(String::new());
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

/// The names of every agent the daemon on `socket` has, or None when it
/// can't say.
fn agents(agent: &Path, socket: &Path) -> Option<HashSet<String>> {
    let output = Command::new(agent)
        .arg("ls")
        .arg("--socket")
        .arg(socket)
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
/// is there but not mounted, and whose task the daemon on `socket`, the
/// one Sonny was opened on, still has. Returns
/// what could not be mounted, after a word on copies whose task is gone.
pub fn remount(socket: &Path) -> Vec<String> {
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
    let alive = crate::agent_bin().and_then(|agent| agents(&agent, socket));
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
    fn a_brief_names_the_copy_where_it_named_the_folder() {
        let names = ["/Volumes/workplace".to_owned(), "~/workplace".to_owned()];
        let to = "/Users/g/.agent/tasks/t";
        let said = "Write /Volumes/workplace/notes.md and ~/workplace/a, not /Volumes/workplace2, \
                    /x/Volumes/workplace or ~/workplaces. Look in /Volumes/workplace.";
        let (brief, count) = replace_paths(said, &names, to);
        assert_eq!(
            brief,
            "Write /Users/g/.agent/tasks/t/notes.md and /Users/g/.agent/tasks/t/a, not /Volumes/workplace2, \
             /x/Volumes/workplace or ~/workplaces. Look in /Users/g/.agent/tasks/t."
        );
        assert_eq!(count, 3);
        assert_eq!(replace_paths("`/Volumes/workplace`", &names, to).1, 1);
        // Paths that climb out of the folder name what is beside it.
        for beside in [
            "/Volumes/workplace/../notes.md",
            "/Volumes/workplace/a/../../b",
        ] {
            assert_eq!(replace_paths(beside, &names, to), (beside.to_owned(), 0));
        }
        assert_eq!(
            replace_paths("/Volumes/workplace/a/../b", &names, to).0,
            "/Users/g/.agent/tasks/t/a/../b"
        );
        // Sibling folders whose names only start the same stay as they are.
        for sibling in [
            "/Volumes/workplace+backup",
            "/Volumes/workplace@old",
            "/Volumes/workplace#2",
        ] {
            assert_eq!(replace_paths(sibling, &names, to), (sibling.to_owned(), 0));
        }
        assert_eq!(
            replace_paths(
                "--dir=/Volumes/workplace:/Volumes/workplace/b (see ~/workplace)",
                &names,
                to
            )
            .0,
            "--dir=/Users/g/.agent/tasks/t:/Users/g/.agent/tasks/t/b (see /Users/g/.agent/tasks/t)"
        );
    }

    #[test]
    fn a_folder_on_another_disk_is_told_apart() {
        let tmp = std::env::temp_dir();
        assert!(same_disk(&tmp, &tmp.join("not/made/yet")));
        // A device file system is never the one temporary files are on.
        assert!(!same_disk(Path::new("/dev"), &tmp));
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
    fn a_git_copy_keeps_what_its_origin_lacks_in_its_submodules_too() {
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("sonny-unkept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let run = |dir: &Path, args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(["-c", "protocol.file.allow=always"])
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}");
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        let (lib, top, copy) = (base.join("lib"), base.join("top"), base.join("copy"));
        for repo in [&lib, &top] {
            std::fs::create_dir_all(repo).unwrap();
            run(repo, &["init", "-q", "-b", "main"]);
            run(repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
        }
        run(
            &top,
            &["submodule", "add", "-q", &lib.to_string_lossy(), "lib"],
        );
        run(&top, &["commit", "-qm", "lib"]);
        // A tag of a blob that origin has too is no work of the copy's.
        let blob = run(&top, &["hash-object", "-w", ".gitmodules"]);
        run(&top, &["tag", "blob", &blob]);
        run(
            &base,
            &[
                "clone",
                "-q",
                "--recurse-submodules",
                &top.to_string_lossy(),
                "copy",
            ],
        );
        run(&copy, &["fetch", "-q", "origin", "refs/tags/*:refs/tags/*"]);
        let left = |copy: &Path| not_here("demo", copy, &base).unwrap();
        assert_eq!(left(&copy), "");
        // A commit in the submodule that only the copy has, with the copy's
        // own branch pushed.
        let sub = copy.join("lib");
        run(&sub, &["commit", "-q", "--allow-empty", "-m", "two"]);
        run(&copy, &["commit", "-qam", "lib two"]);
        run(&copy, &["push", "-q", "origin", "main:task"]);
        assert!(
            left(&copy).starts_with("commits not pushed to ")
                && left(&copy).ends_with(" in its submodule lib"),
            "{}",
            left(&copy)
        );
        run(&sub, &["push", "-q", "origin", "HEAD:refs/heads/task"]);
        assert_eq!(left(&copy), "");
        // With an origin by URL, a commit only an annotated tag keeps.
        let url = format!("file://{}", top.display());
        run(&copy, &["remote", "set-url", "origin", &url]);
        run(&copy, &["fetch", "-q", "origin"]);
        assert_eq!(left(&copy), "");
        run(&copy, &["checkout", "-q", "--detach"]);
        run(&copy, &["commit", "-q", "--allow-empty", "-m", "private"]);
        run(&copy, &["tag", "-a", "-m", "kept", "kept"]);
        run(&copy, &["switch", "-q", "main"]);
        assert_eq!(left(&copy), format!("commits not pushed to {url}, on kept"));
        std::fs::remove_dir_all(&base).unwrap();
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
        // Older git says the reason; newer git names the .git file that
        // holds it.
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(
            said.contains("git is off here")
                || said.contains("gitfile does not point to a valid repository"),
            "{said}"
        );
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
