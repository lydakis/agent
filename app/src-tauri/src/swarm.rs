//! Swarms: many agents on one goal, talking through a board. A swarm is the
//! app's; the daemon knows only its agents, which are ordinary bots named
//! `SWARM-N`. A swarm is a folder, `~/.agent/swarms/DAEMON/SWARM/`, where
//! DAEMON stands for the socket its agents' daemon listens on, so a window
//! attached to another store never sees them:
//!
//! - `swarm.toml`: its project, goal, folder, model, token budget, members
//!   with the id of the bot each one is, and whether you stopped it. Only the
//!   app writes it, under the board's lock.
//! - `board.jsonl`: one post a line, appended under a lock.
//! - `state.json`: what the board's lines add up to (roles, proposals and
//!   their votes, who is in which stream), rewritten under the board's lock
//!   with each line that changes it.
//! - `post` and `role`, and with a council `propose`, `vote` and `join`: the
//!   scripts its agents run. Each runs this executable with `--swarm-post`,
//!   so they need nothing else installed and cost one daemon connection
//!   whatever the swarm's size.
//!
//! A post reaches the others as a steer, so a working agent reads it at its
//! next step. An agent's post goes to the agents working now and wakes only
//! the ones it names with @NAME; your post wakes everyone, or only the ones
//! it names. Agents talking among themselves never wake a swarm that has
//! gone quiet.
//!
//! A swarm with a council organizes itself: an agent proposes a stream of
//! work, the council's seats (its first agents) vote, and a majority opens
//! the stream with its proposer as lead; you can approve or deny any open
//! proposal yourself. Once an agent joins a stream, its posts reach that
//! stream's agents unless it posts to everyone.
use agent_client::Client;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

pub const POST_FLAG: &str = "--swarm-post";
/// A post is a message, not a document: longer text goes in a file in the
/// swarm's folder and the post names it.
const MAX_POST: usize = 16 * 1024;
/// Bounds one read of the board, and where a first read starts from its end.
const MAX_READ: u64 = 1024 * 1024;
const TAIL: u64 = 256 * 1024;
/// Room for `-NNNN` after the swarm's name within the daemon's 128 bytes.
const MAX_NAME: usize = 100;
/// A project's `.agents/setup` gets this long, and its output is kept only
/// as far as a failure's reason needs.
const SETUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
const SETUP_KEEP: usize = 64 * 1024;
/// A role is a few words; a stream's name is a tag.
const MAX_ROLE: usize = 60;
const MAX_STREAM: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub struct Swarm {
    pub name: String,
    pub project: String,
    pub goal: String,
    pub workspace: String,
    pub model: String,
    pub budget_tokens: u64,
    pub members: Vec<String>,
    /// Each member's bot id: a name can be deleted and made again, and the
    /// new bot is not a member.
    pub ids: BTreeMap<String, i64>,
    pub stopped: bool,
    /// Its council's seats: 0 for a flat board, where nobody proposes.
    pub council: usize,
}

impl Swarm {
    fn read(dir: &Path) -> Result<Self, String> {
        let path = dir.join("swarm.toml");
        let text = read_capped(&path, MAX_SETTINGS)?;
        let table: toml::Table = text
            .parse()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let text = |key: &str| {
            table
                .get(key)
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("{}: {key} is missing", path.display()))
        };
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("swarm folder name is not UTF-8")?
            .to_owned();
        let swarm = Self {
            name,
            project: text("project")?,
            goal: text("goal")?,
            workspace: text("workspace")?,
            model: text("model")?,
            budget_tokens: table
                .get("budget_tokens")
                .and_then(toml::Value::as_integer)
                .and_then(|n| u64::try_from(n).ok())
                .ok_or_else(|| format!("{}: budget_tokens is missing", path.display()))?,
            members: table
                .get("members")
                .and_then(toml::Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|m| m.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            ids: table
                .get("ids")
                .and_then(toml::Value::as_table)
                .and_then(|ids| {
                    ids.iter()
                        .map(|(m, id)| Some((m.clone(), id.as_integer()?)))
                        .collect()
                })
                .ok_or_else(|| format!("{}: ids is missing or not bot ids", path.display()))?,
            stopped: table
                .get("stopped")
                .and_then(toml::Value::as_bool)
                .ok_or_else(|| format!("{}: stopped is missing", path.display()))?,
            council: table
                .get("council")
                .and_then(toml::Value::as_integer)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| format!("{}: council is missing", path.display()))?,
        };
        // Every member is pinned to its bot: a post never steers a name.
        if !swarm.members.iter().all(|m| swarm.ids.contains_key(m)) {
            return Err(format!("{}: a member has no bot id", path.display()));
        }
        Ok(swarm)
    }

    /// Replaced whole, through a rename, so a post never reads half a file.
    fn write(&self, dir: &Path) -> Result<(), String> {
        let mut table = toml::Table::new();
        table.insert("project".into(), self.project.clone().into());
        table.insert("goal".into(), self.goal.clone().into());
        table.insert("workspace".into(), self.workspace.clone().into());
        table.insert("model".into(), self.model.clone().into());
        table.insert("budget_tokens".into(), (self.budget_tokens as i64).into());
        table.insert(
            "members".into(),
            toml::Value::Array(self.members.iter().map(|m| m.clone().into()).collect()),
        );
        table.insert("stopped".into(), self.stopped.into());
        table.insert(
            "ids".into(),
            toml::Value::Table(
                self.ids
                    .iter()
                    .map(|(m, id)| (m.clone(), (*id).into()))
                    .collect(),
            ),
        );
        table.insert("council".into(), (self.council as i64).into());
        let text = toml::to_string(&table).map_err(|e| e.to_string())?;
        // Distinct per write, so two writes at once never share a temporary.
        static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temporary = dir.join(format!(".swarm.toml.{}.{n}", std::process::id()));
        replace(&temporary, &dir.join("swarm.toml"), text.as_bytes(), 0o644)
    }

    pub fn json(&self, dir: &Path) -> Value {
        json!({
            "swarm": self.name, "dir": dir.to_string_lossy(), "project": self.project,
            "goal": self.goal, "workspace": self.workspace, "model": self.model,
            "budget_tokens": self.budget_tokens, "members": self.members, "ids": self.ids,
            "stopped": self.stopped, "council": self.council, "seats": self.seats(),
        })
    }

    /// A member's name without the project, as posts and @names use it.
    fn short<'a>(&self, member: &'a str) -> &'a str {
        member
            .strip_prefix(&self.project)
            .and_then(|rest| rest.strip_prefix('.'))
            .unwrap_or(member)
    }

    /// The council's seats: its first agents, as many as it has seats.
    fn seats(&self) -> Vec<String> {
        self.members.iter().take(self.council).cloned().collect()
    }
}

/// `~/.agent/swarms`, which holds a folder of swarms per daemon.
pub fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".agent/swarms"))
        .ok_or_else(|| "no HOME for ~/.agent/swarms".into())
}

/// Write `bytes` to `temporary`, then rename it over `path`, both synced:
/// a reader sees the old file or the new one, and an acknowledged change
/// survives a power loss.
fn replace(temporary: &Path, path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(temporary, path)?;
        std::fs::File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    written.map_err(|e| format!("{}: {e}", path.display()))
}

/// The swarms of the store whose identity the attached daemon announced.
/// Their agents are that store's bots, so a daemon on another store, even on
/// the same socket, never sees them.
pub fn root(store: &str) -> Result<PathBuf, String> {
    if store.is_empty() || !store.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid_store: {store} is not a store identity"));
    }
    Ok(home()?.join(store))
}

/// Bounds a read of `swarm.toml`: a swarm's settings and its members, not
/// a document.
const MAX_SETTINGS: u64 = 1024 * 1024;

/// A file read whole only when it is no larger than `max`.
fn read_capped(path: &Path, max: u64) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut text = String::new();
    file.take(max + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if text.len() as u64 > max {
        return Err(format!("{}: larger than {max} bytes", path.display()));
    }
    Ok(text)
}

/// A goal is a brief's heart, not a document: it leaves each agent's first
/// message far inside what the daemon takes.
pub fn valid_goal(goal: &str) -> Result<(), String> {
    if goal.trim().is_empty() {
        return Err("goal_required: a swarm needs a goal".into());
    }
    if goal.len() > MAX_POST {
        return Err(format!(
            "goal_too_long: a goal is at most {MAX_POST} bytes; put the details in a file in the project and name it"
        ));
    }
    Ok(())
}

/// A swarm's name is `PROJECT.NAME`, used for its folder, its worktree and
/// branch, and its agents' names, so it is held to what all of them accept.
pub fn valid_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && !name.ends_with('.')
        && !name.contains("..")
        // Git refuses a branch whose name ends so.
        && !name.ends_with(".lock")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(format!("invalid_swarm_name: {name}"))
    }
}

fn folder(root: &Path, swarm: &str) -> Result<PathBuf, String> {
    valid_name(swarm)?;
    Ok(root.join(swarm))
}

/// Every swarm, and the folders that are not readable swarms with why.
pub fn list(root: &Path) -> Result<Value, String> {
    let mut swarms = Vec::new();
    let mut broken = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("{}: {error}", root.display())),
    };
    if let Some(entries) = entries {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.join("swarm.toml").exists() {
                continue;
            }
            match Swarm::read(&dir) {
                Ok(swarm) => swarms.push(swarm.json(&dir)),
                Err(error) => broken.push(error),
            }
        }
    }
    Ok(json!({"swarms": swarms, "broken": broken}))
}

/// Take a new swarm's name by making its folder, before anything else is
/// made for it.
pub fn claim(root: &Path, swarm: &str) -> Result<PathBuf, String> {
    let dir = folder(root, swarm)?;
    std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    std::fs::create_dir(&dir).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => format!("swarm_exists: {swarm}"),
        _ => format!("{}: {e}", dir.display()),
    })?;
    Ok(dir)
}

/// A claimed folder's files, with your goal as the board's first post.
/// Nobody is a member yet: the app adds each agent once the daemon has
/// created it. A folder that cannot be filled goes, name and all.
pub fn fill(dir: &Path, swarm: &Swarm, app: &Path) -> Result<(), String> {
    let made = (|| {
        swarm.write(dir)?;
        std::fs::File::create(dir.join("board.jsonl")).map_err(|e| e.to_string())?;
        locked(dir, |_| {
            Ok((
                vec![json!({"at": now_ms(), "from": "user", "text": swarm.goal})],
                (),
            ))
        })?;
        for &tool in tools(swarm) {
            write_script(dir, app, tool)?;
        }
        Ok(())
    })();
    if made.is_err() {
        let _ = std::fs::remove_dir_all(dir);
    }
    made
}

/// Where a new swarm's agents work: the project folder itself, or one
/// worktree they share, `~/.agent/worktrees/SWARM` on branch `agent/SWARM`,
/// at the project's own subfolder. As for a coordinator's tasks, the
/// project's `.agents/setup` runs inside a new worktree with `AGENT_SOURCE`
/// naming the project; if that fails or outlasts its time, the worktree and
/// its branch go.
pub async fn place(project: &Path, swarm: &str, shared: bool) -> Result<String, String> {
    valid_name(swarm)?;
    if !shared {
        return Ok(project.to_string_lossy().into_owned());
    }
    let git = |args: &[&str]| {
        let mut command = tokio::process::Command::new("git");
        command
            .arg("-C")
            .arg(project)
            .args(args)
            .stdin(std::process::Stdio::null());
        command
    };
    let prefix = git(&["rev-parse", "--show-prefix"])
        .output()
        .await
        .map_err(|e| format!("git: {e}"))?;
    if !prefix.status.success() {
        return Err(format!(
            "not_a_git_repository: {} has no repository for a worktree; choose the project folder",
            project.display()
        ));
    }
    let prefix = String::from_utf8_lossy(&prefix.stdout).trim().to_owned();
    let tree = worktrees()?.join(swarm);
    let branch = format!("agent/{swarm}");
    // Worktrees and branches are shared by every store: a name another
    // store's swarm holds is taken here too, and the page picks another.
    let branched = git(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/heads/{branch}"),
    ])
    .output()
    .await
    .is_ok_and(|out| out.status.success());
    if tree.exists() || branched {
        return Err(format!(
            "swarm_exists: {swarm} has a worktree or branch already"
        ));
    }
    let added = git(&["worktree", "add", "-b", &branch])
        .arg(&tree)
        .arg("HEAD")
        .output()
        .await
        .map_err(|e| format!("git: {e}"))?;
    if !added.status.success() {
        return Err(format!("worktree_failed: {}", tail(&added.stderr)));
    }
    let workspace = tree.join(&prefix);
    let setup = project.join(".agents/setup");
    if setup.is_file() {
        use std::os::unix::fs::PermissionsExt;
        let runnable = setup
            .metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
        let mut command = if runnable {
            tokio::process::Command::new(&setup)
        } else {
            let mut sh = tokio::process::Command::new("/bin/sh");
            sh.arg(&setup);
            sh
        };
        let login = crate::daemon::login().await;
        let environment = match login {
            Some(login) => setup_env(login.iter().cloned()),
            None => setup_env(std::env::vars_os()),
        };
        command
            .current_dir(&workspace)
            .env_clear()
            .envs(environment)
            .env("AGENT_SOURCE", project);
        let failed = match run_setup(command, SETUP_TIMEOUT).await {
            Ok(()) => None,
            Err(error) => Some(format!("setup_failed: {error}")),
        };
        if let Some(failed) = failed {
            let _ = git(&["worktree", "remove", "--force"])
                .arg(&tree)
                .output()
                .await;
            let _ = git(&["branch", "-D", &branch]).output().await;
            return Err(failed);
        }
    }
    Ok(workspace.to_string_lossy().trim_end_matches('/').to_owned())
}

/// Setup is the project's code, run before any agent exists. It gets the
/// login shell's ordinary variables, so its tools are on PATH, and nothing
/// else: no provider or cloud keys, which the daemon's shell withholds too.
fn setup_env(from: impl Iterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    const KEEP: &[&str] = &[
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TMPDIR", "TERM",
    ];
    from.filter(|(key, _)| {
        key.to_str()
            .is_some_and(|key| KEEP.contains(&key) || key.starts_with("LC_"))
    })
    .collect()
}

/// Runs setup for at most `limit`, keeping the end of its output: a script
/// that hangs or never stops writing is killed, not waited on, with every
/// process it started (its own process group).
async fn run_setup(
    mut command: tokio::process::Command,
    limit: std::time::Duration,
) -> Result<(), String> {
    use tokio::io::{AsyncRead, AsyncReadExt};
    async fn drain(mut from: impl AsyncRead + Unpin) -> Vec<u8> {
        let (mut kept, mut buffer) = (Vec::new(), [0u8; 8192]);
        while let Ok(n @ 1..) = from.read(&mut buffer).await {
            kept.extend_from_slice(&buffer[..n]);
            if kept.len() > 2 * SETUP_KEEP {
                kept.drain(..kept.len() - SETUP_KEEP);
            }
        }
        kept
    }
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let group = child.id();
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let ran = async {
        let (out, err, status) = tokio::join!(
            async { drain(stdout?).await.into() },
            async { drain(stderr?).await.into() },
            child.wait()
        );
        let out: Option<Vec<u8>> = out;
        let err: Option<Vec<u8>> = err;
        Ok::<_, std::io::Error>((out.unwrap_or_default(), err.unwrap_or_default(), status?))
    };
    match tokio::time::timeout(limit, ran).await {
        Err(_) => {
            if let Some(group) = group {
                let _ = tokio::process::Command::new("kill")
                    .args(["-KILL", "--", &format!("-{group}")])
                    .status()
                    .await;
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(format!(
                ".agents/setup ran past {} s and was stopped",
                limit.as_secs()
            ))
        }
        Ok(Err(error)) => Err(error.to_string()),
        Ok(Ok((_, _, status))) if status.success() => Ok(()),
        Ok(Ok((out, err, _))) => Err(tail(&[out, err].concat())),
    }
}

fn worktrees() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".agent/worktrees"))
        .ok_or_else(|| "no HOME for ~/.agent/worktrees".into())
}

/// The end of a command's output, enough to say why it failed.
fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    let start = text.char_indices().rev().nth(600).map_or(0, |(at, _)| at);
    text[start..].to_owned()
}

/// A script its agents run, and what it says it does.
#[derive(Clone, Copy)]
pub struct Tool {
    pub name: &'static str,
    flag: &'static str,
    usage: &'static str,
}

const TOOLS: [Tool; 5] = [
    Tool {
        name: "post",
        flag: "",
        usage: "post [--all] TEXT: post to this swarm's board; @NAME wakes that agent; --all reaches everyone, not only your stream.",
    },
    Tool {
        name: "role",
        flag: "--role",
        usage: "role ROLE: say what you are doing here, in a few words.",
    },
    Tool {
        name: "propose",
        flag: "--propose",
        usage: "propose STREAM WHY: propose a stream of work for the council to vote on.",
    },
    Tool {
        name: "vote",
        flag: "--vote",
        usage: "vote ID yes|no REASON: vote on a proposal, if you hold a council seat.",
    },
    Tool {
        name: "join",
        flag: "--join",
        usage: "join STREAM: work in an approved stream; your posts then reach its agents.",
    },
];

/// A flat board has no council, so nothing to propose, vote on or join.
fn tools(swarm: &Swarm) -> &'static [Tool] {
    if swarm.council == 0 {
        &TOOLS[..2]
    } else {
        &TOOLS
    }
}

/// The script names this executable and this folder, single-quoted.
fn script(app: &Path, dir: &Path, tool: Tool) -> String {
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"));
    let flag = if tool.flag.is_empty() {
        String::new()
    } else {
        format!(" {}", tool.flag)
    };
    format!(
        "#!/bin/sh\n# {}\nexec {} {POST_FLAG} {}{flag} \"$@\"\n",
        tool.usage,
        quote(app),
        quote(dir)
    )
}

/// The app moves when it is updated, so it writes its path again on start,
/// in every daemon's swarms.
pub fn refresh_scripts(home: &Path, app: &Path) {
    let Ok(daemons) = std::fs::read_dir(home) else {
        return;
    };
    for entry in daemons
        .flatten()
        .flat_map(|d| std::fs::read_dir(d.path()))
        .flatten()
    {
        let Ok(entry) = entry else { continue };
        let dir = entry.path();
        for tool in TOOLS {
            let path = dir.join(tool.name);
            if path.exists()
                && std::fs::read_to_string(&path).is_ok_and(|have| have != script(app, &dir, tool))
            {
                let _ = write_script(&dir, app, tool);
            }
        }
    }
}

/// Replaced whole, so an agent running it meanwhile runs one version or
/// the other, never half a file.
fn write_script(dir: &Path, app: &Path, tool: Tool) -> Result<(), String> {
    let temporary = dir.join(format!(".{}.{}", tool.name, std::process::id()));
    let text = script(app, dir, tool);
    replace(&temporary, &dir.join(tool.name), text.as_bytes(), 0o755)
}

/// Read, change and write `swarm.toml` under the board's lock, so two
/// windows changing one swarm never lose each other's change.
fn update(
    dir: &Path,
    change: impl FnOnce(&mut Swarm) -> Result<(), String>,
) -> Result<Swarm, String> {
    let _board = lock_board(dir)?;
    let mut s = Swarm::read(dir)?;
    change(&mut s)?;
    s.write(dir)?;
    Ok(s)
}

/// Members join in order and never twice, each pinned to its bot's id. An
/// agent added later brings its own budget, which the swarm's grows by, so
/// the budget shown is always what its agents may spend.
pub fn join(
    root: &Path,
    swarm: &str,
    members: &[(String, i64)],
    added_budget: u64,
) -> Result<Value, String> {
    let dir = folder(root, swarm)?;
    let s = update(&dir, |s| {
        s.budget_tokens = s.budget_tokens.saturating_add(added_budget);
        for (member, id) in members {
            if !member.starts_with(&format!("{swarm}-")) {
                return Err(format!("invalid_member: {member} is not named {swarm}-N"));
            }
            if !s.members.contains(member) {
                s.members.push(member.clone());
            }
            s.ids.insert(member.clone(), *id);
        }
        Ok(())
    })?;
    Ok(s.json(&dir))
}

/// A deleted agent leaves: the swarm stops counting it and posting to it.
pub fn leave(root: &Path, swarm: &str, member: &str) -> Result<Value, String> {
    let dir = folder(root, swarm)?;
    let s = update(&dir, |s| {
        s.members.retain(|m| m != member);
        s.ids.remove(member);
        Ok(())
    })?;
    Ok(s.json(&dir))
}

/// A stopped swarm refuses its agents' posts; your next post resumes it.
pub fn set_stopped(root: &Path, swarm: &str, stopped: bool) -> Result<Value, String> {
    let dir = folder(root, swarm)?;
    let s = update(&dir, |s| {
        s.stopped = stopped;
        Ok(())
    })?;
    Ok(s.json(&dir))
}

/// A swarm that never got an agent goes with its worktree and branch, as
/// when its start fails before any agent exists.
pub async fn discard(root: &Path, swarm: &str) -> Result<(), String> {
    let dir = folder(root, swarm)?;
    let s = Swarm::read(&dir)?;
    if !s.members.is_empty() {
        return Err(format!(
            "swarm_has_agents: {swarm} has agents; stop it instead"
        ));
    }
    if Path::new(&s.workspace).starts_with(worktrees()?.join(swarm)) {
        unplace(swarm).await?;
    }
    std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())
}

/// Remove a swarm's worktree and its branch, as `place` made them.
pub async fn unplace(swarm: &str) -> Result<(), String> {
    let tree = worktrees()?.join(swarm);
    // The repository the worktree came from, found before it goes.
    let common = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&tree)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .await
        .map_err(|e| format!("git: {e}"))?;
    let common = String::from_utf8_lossy(&common.stdout).trim().to_owned();
    if !common.is_empty() {
        let git = |args: &[&str]| {
            let mut command = tokio::process::Command::new("git");
            command.arg("--git-dir").arg(&common).args(args);
            command
        };
        let _ = git(&["worktree", "remove", "--force"])
            .arg(&tree)
            .output()
            .await;
        let _ = git(&["branch", "-D", &format!("agent/{swarm}")])
            .output()
            .await;
    }
    Ok(())
}

/// The board's complete lines from `offset`, where the next read starts,
/// and what the whole board adds up to. With no offset, or one so far behind
/// that the lines between would not be kept, it reads the board's last
/// stretch and says `reset`: the lines replace what the reader has. A line
/// that is not JSON comes back as its text, so a hand edit shows instead of
/// vanishing.
pub fn board(root: &Path, swarm: &str, offset: Option<u64>) -> Result<Value, String> {
    let dir = folder(root, swarm)?;
    let state = State::read(&dir)?;
    let path = dir.join("board.jsonl");
    let mut file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    // Shorter than we read before means someone rewrote it: read it again.
    let (mut start, reset) = match offset {
        Some(offset) if offset <= size && size - offset <= TAIL => (offset, false),
        _ => (size.saturating_sub(TAIL), true),
    };
    let skip_partial = reset && start > 0;
    file.seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_READ)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let mut text = &bytes[..];
    if skip_partial {
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
    Ok(json!({
        "lines": lines, "offset": start + end as u64, "more": start + (end as u64) < size,
        "reset": reset,
        "state": state.json(),
    }))
}

/// What the board's lines add up to, kept beside it so a reader never needs
/// the whole board. Agents are named as the board names them, without the
/// project.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct State {
    pub roles: BTreeMap<String, String>,
    pub streams: BTreeMap<String, String>,
    pub proposals: Vec<Proposal>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub id: String,
    pub stream: String,
    pub why: String,
    pub by: String,
    pub at: u64,
    /// Each seat's vote: yes or no, and why.
    pub votes: BTreeMap<String, (bool, String)>,
    /// `open`, `approved` or `denied`.
    pub status: String,
    /// `council`, or `user` when you decided it.
    pub decided_by: Option<String>,
}

fn strings(value: &Value) -> BTreeMap<String, String> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
        .collect()
}

impl Proposal {
    fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| value[key].as_str().map(str::to_owned);
        Some(Self {
            id: text("id")?,
            stream: text("stream")?,
            why: text("why").unwrap_or_default(),
            by: text("by")?,
            at: value["at"].as_u64().unwrap_or(0),
            votes: value["votes"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(seat, vote)| {
                    Some((
                        seat.clone(),
                        (
                            vote["yes"].as_bool()?,
                            vote["reason"].as_str().unwrap_or("").to_owned(),
                        ),
                    ))
                })
                .collect(),
            status: text("status")?,
            decided_by: text("decided_by"),
        })
    }

    fn json(&self) -> Value {
        let votes: serde_json::Map<String, Value> = self
            .votes
            .iter()
            .map(|(seat, (yes, reason))| (seat.clone(), json!({"yes": yes, "reason": reason})))
            .collect();
        json!({
            "id": self.id, "stream": self.stream, "why": self.why, "by": self.by, "at": self.at,
            "votes": votes, "status": self.status, "decided_by": self.decided_by,
        })
    }
}

impl State {
    fn read(dir: &Path) -> Result<Self, String> {
        let path = dir.join("state.json");
        match std::fs::read(&path) {
            Ok(bytes) => {
                let value: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                Ok(Self {
                    roles: strings(&value["roles"]),
                    streams: strings(&value["streams"]),
                    proposals: value["proposals"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Proposal::from_json)
                        .collect(),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn json(&self) -> Value {
        json!({
            "roles": self.roles, "streams": self.streams,
            "proposals": self.proposals.iter().map(Proposal::json).collect::<Vec<_>>(),
        })
    }

    fn write(&self, dir: &Path) -> Result<(), String> {
        let temporary = dir.join(format!(".state.json.{}", std::process::id()));
        let bytes = serde_json::to_vec(&self.json()).map_err(|e| e.to_string())?;
        replace(&temporary, &dir.join("state.json"), &bytes, 0o644)
    }

    fn proposal(&mut self, id: &str) -> Result<&mut Proposal, String> {
        self.proposals
            .iter_mut()
            .find(|p| p.id.eq_ignore_ascii_case(id))
            .ok_or_else(|| format!("no_proposal: {id} is not a proposal on this board"))
    }

    fn approved(&self, stream: &str) -> bool {
        self.proposals
            .iter()
            .any(|p| p.stream == stream && p.status == "approved")
    }
}

/// Under the board's exclusive lock: read the state, let `change` decide
/// the lines to add, append them in one write, then keep the new state. Two
/// posts never interleave, and each sees the other's effect.
fn locked<T>(
    dir: &Path,
    change: impl FnOnce(&mut State) -> Result<(Vec<Value>, T), String>,
) -> Result<T, String> {
    locked_with(&mut lock_board(dir)?, dir, change)
}

/// `locked`, on a board its caller already holds locked.
fn locked_with<T>(
    board: &mut std::fs::File,
    dir: &Path,
    change: impl FnOnce(&mut State) -> Result<(Vec<Value>, T), String>,
) -> Result<T, String> {
    let before = State::read(dir)?;
    let mut state = before.clone();
    let (lines, out) = change(&mut state)?;
    let mut bytes = Vec::new();
    for line in &lines {
        serde_json::to_writer(&mut bytes, line).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
    }
    board.write_all(&bytes).map_err(|e| e.to_string())?;
    // Synced before anyone is told, so a post an agent heard survives a
    // power loss on the board too.
    board.sync_data().map_err(|e| e.to_string())?;
    if state != before {
        state.write(dir)?;
    }
    Ok(out)
}

/// The board open for appending, locked until it is dropped. The lock also
/// guards `swarm.toml` and `state.json`.
fn lock_board(dir: &Path) -> Result<std::fs::File, String> {
    let file = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("board.jsonl"))
        .map_err(|e| e.to_string())?;
    file.lock().map_err(|e| e.to_string())?;
    Ok(file)
}

/// `lock_board` from async code: the wait runs on the blocking pool, so a
/// lock held across another post's sends never stalls the executor that
/// post needs.
async fn lock_board_async(dir: &Path) -> Result<std::fs::File, String> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || lock_board(&dir))
        .await
        .map_err(|e| e.to_string())?
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Who posts: an agent, as its shell's environment names it and its turn,
/// or you.
#[derive(Debug, Clone)]
pub struct Author {
    pub bot: String,
    pub id: i64,
    pub turn: i64,
}

#[derive(Debug, PartialEq)]
enum Reach {
    /// Into the turn running now; if it has ended, the post waits on the board.
    Running(i64),
    /// Into the running turn, or a new one if the agent is idle.
    Wake,
}

/// Names after `@`, as far as a name's characters go; a trailing full stop
/// ends a sentence, not the name.
fn named(text: &str) -> Vec<&str> {
    text.split('@')
        .skip(1)
        .map(|rest| {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || "-_.".contains(c)))
                .unwrap_or(rest.len());
            rest[..end].trim_end_matches('.')
        })
        .filter(|name| !name.is_empty())
        .collect()
}

/// Who a post reaches and how, given each member's running turn: the
/// working agents in `scope` (every member when there is none), and whoever
/// it names, woken if idle. Your post names nobody to wake everyone.
fn readers(
    swarm: &Swarm,
    author: Option<&str>,
    text: &str,
    scope: Option<&[String]>,
    running: &dyn Fn(&str) -> Option<i64>,
) -> Vec<(String, Reach)> {
    let named = named(text);
    let is_named = |m: &str| named.iter().any(|n| *n == m || *n == swarm.short(m));
    let mut out = Vec::new();
    for member in &swarm.members {
        if Some(member.as_str()) == author {
            continue;
        }
        let reach = if is_named(member) || (author.is_none() && named.is_empty()) {
            Some(Reach::Wake)
        } else if scope.is_none_or(|scope| scope.contains(member)) {
            running(member).map(Reach::Running)
        } else {
            None
        };
        if let Some(reach) = reach {
            out.push((member.clone(), reach));
        }
    }
    out
}

/// What an agent, or you, does on the board.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// A post; `all` reaches everyone rather than only the author's stream.
    Post {
        text: String,
        all: bool,
    },
    Role(String),
    Propose {
        stream: String,
        why: String,
    },
    Vote {
        id: String,
        yes: bool,
        reason: String,
    },
    Join(String),
}

/// Who hears a line the board gained.
#[derive(Debug, PartialEq)]
enum Audience {
    /// A post: see `readers`.
    Post {
        scope: Option<Vec<String>>,
        text: String,
    },
    /// These members, woken if idle; with `working`, every other working
    /// member hears it too.
    Wake { who: Vec<String>, working: bool },
}

#[derive(Debug, PartialEq)]
struct Notice {
    prompt: String,
    audience: Audience,
}

/// What an act adds to the board, whom it tells, and what the tool answers
/// besides who it reached. Pure, under the board's lock.
fn plan(
    swarm: &Swarm,
    state: &mut State,
    author: Option<&str>,
    act: Act,
    at: u64,
) -> Result<(Vec<Value>, Vec<Notice>, Value), String> {
    let me = author.map_or("user", |bot| swarm.short(bot)).to_owned();
    let full = |short: &str| {
        swarm
            .members
            .iter()
            .find(|m| swarm.short(m) == short)
            .cloned()
    };
    let agent = || author.ok_or_else(|| "agents_only: only an agent does this".to_owned());
    let council = || {
        if swarm.council == 0 {
            Err("no_council: this swarm has no council; post the piece you are taking".to_owned())
        } else {
            Ok(())
        }
    };
    let words = |text: &str, what: &str, max: usize| {
        let text = text.trim();
        if text.is_empty() {
            Err(format!("{what}_empty: say what it is"))
        } else if text.len() > max {
            Err(format!(
                "{what}_too_long: at most {max} bytes; write the details to a file in your folder and name it"
            ))
        } else {
            Ok(text.to_owned())
        }
    };
    let line = |fields: Value| {
        let mut line = json!({"at": at, "from": me});
        for (key, value) in fields.as_object().into_iter().flatten() {
            line[key] = value.clone();
        }
        line
    };
    match act {
        Act::Post { text, all } => {
            let text = words(&text, "post", MAX_POST)?;
            let stream = match author {
                Some(_) if swarm.council > 0 && !all => state.streams.get(&me).cloned(),
                _ => None,
            };
            let scope = stream.as_ref().map(|st| {
                state
                    .streams
                    .iter()
                    .filter(|(_, s)| *s == st)
                    .filter_map(|(m, _)| full(m))
                    .collect()
            });
            let prompt = match &stream {
                Some(st) => format!("[board #{st}] {me}: {text}"),
                None => format!("[board] {me}: {text}"),
            };
            let mut posted = line(json!({"text": text}));
            if let Some(st) = &stream {
                posted["stream"] = json!(st);
            }
            let notice = Notice {
                prompt,
                audience: Audience::Post { scope, text },
            };
            Ok((vec![posted], vec![notice], json!({"stream": stream})))
        }
        Act::Role(role) => {
            agent()?;
            let role = words(&role, "role", MAX_ROLE)?;
            if role.contains('\n') {
                return Err("role_too_long: a role is a few words on one line".into());
            }
            state.roles.insert(me.clone(), role.clone());
            Ok((
                vec![line(json!({"kind": "role", "role": role}))],
                vec![],
                json!({}),
            ))
        }
        Act::Propose { stream, why } => {
            council()?;
            let bot = agent()?;
            let stream = stream.trim().trim_start_matches('#').to_owned();
            let tag = !stream.is_empty()
                && stream.len() <= MAX_STREAM
                && stream.as_bytes()[0].is_ascii_alphanumeric()
                && stream
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !tag {
                return Err(format!(
                    "invalid_stream: name a stream like conn-pool: lowercase letters, digits and -, at most {MAX_STREAM}"
                ));
            }
            if state
                .proposals
                .iter()
                .any(|p| p.stream == stream && p.status != "denied")
            {
                return Err(format!(
                    "stream_exists: #{stream} is already proposed or open; join it or pick another name"
                ));
            }
            let why = words(&why, "proposal", MAX_POST)?;
            let id = format!("P{}", state.proposals.len() + 1);
            state.proposals.push(Proposal {
                id: id.clone(),
                stream: stream.clone(),
                why: why.clone(),
                by: me.clone(),
                at,
                votes: BTreeMap::new(),
                status: "open".into(),
                decided_by: None,
            });
            let seats: Vec<String> = swarm
                .seats()
                .into_iter()
                .filter(|seat| seat != bot)
                .collect();
            let notice = Notice {
                prompt: format!(
                    "[board] {me} proposes {id} #{stream}: {why}\nYou hold a council seat: vote {id} yes|no REASON"
                ),
                audience: Audience::Wake {
                    who: seats,
                    working: false,
                },
            };
            let lines = vec![line(
                json!({"kind": "propose", "id": id, "stream": stream, "text": why}),
            )];
            Ok((lines, vec![notice], json!({"id": id})))
        }
        Act::Vote { id, yes, reason } => {
            council()?;
            if let Some(bot) = author
                && !swarm.seats().iter().any(|seat| seat == bot)
            {
                return Err(
                    "not_a_seat: only the council's seats vote; post what you think instead".into(),
                );
            }
            let reason = match author {
                Some(_) => words(&reason, "reason", MAX_POST)?,
                None => reason.trim().to_owned(),
            };
            let seats = swarm.seats().len();
            let proposal = state.proposal(&id)?;
            if proposal.status != "open" {
                return Err(format!(
                    "decided: {} is already {}",
                    proposal.id, proposal.status
                ));
            }
            if proposal.votes.contains_key(&me) {
                return Err(format!("already_voted: you voted on {}", proposal.id));
            }
            proposal.votes.insert(me.clone(), (yes, reason.clone()));
            let ayes = proposal.votes.values().filter(|(yes, _)| *yes).count();
            let noes = proposal.votes.len() - ayes;
            // A majority of the seats decides; you decide alone.
            let need = seats / 2 + 1;
            let decided = match author {
                None => Some(yes),
                Some(_) if ayes >= need => Some(true),
                Some(_) if seats.saturating_sub(noes) < need => Some(false),
                Some(_) => None,
            };
            let (pid, stream, by) = (
                proposal.id.clone(),
                proposal.stream.clone(),
                proposal.by.clone(),
            );
            let mut lines = vec![line(
                json!({"kind": "vote", "id": pid, "yes": yes, "text": reason}),
            )];
            let mut notices = Vec::new();
            if let Some(approved) = decided {
                let by_whom = if author.is_none() { "user" } else { "council" };
                proposal.status = if approved { "approved" } else { "denied" }.into();
                proposal.decided_by = Some(by_whom.into());
                let who = if by_whom == "user" {
                    " by the user"
                } else {
                    ""
                };
                if approved {
                    state.streams.insert(by.clone(), stream.clone());
                }
                let mut decision = line(
                    json!({"kind": "decision", "id": pid, "stream": stream, "approved": approved, "lead": by}),
                );
                decision["from"] = json!(by_whom);
                lines.push(decision);
                let prompt = if approved {
                    format!(
                        "[board] {pid} #{stream} approved{who}: {by} leads it and is in it. Others join with: join {stream}"
                    )
                } else {
                    format!(
                        "[board] {pid} #{stream} denied{who}. Read the votes on the board before proposing again."
                    )
                };
                notices.push(Notice {
                    prompt,
                    audience: Audience::Wake {
                        who: full(&by).into_iter().collect(),
                        working: approved,
                    },
                });
            }
            let status = decided.map(|approved| if approved { "approved" } else { "denied" });
            Ok((lines, notices, json!({"id": pid, "decided": status})))
        }
        Act::Join(stream) => {
            council()?;
            agent()?;
            let stream = stream.trim().trim_start_matches('#').to_owned();
            if !state.approved(&stream) {
                return Err(format!(
                    "no_stream: #{stream} is not an approved stream; the board lists the proposals"
                ));
            }
            state.streams.insert(me.clone(), stream.clone());
            Ok((
                vec![line(json!({"kind": "join", "stream": stream}))],
                vec![],
                json!({"stream": stream}),
            ))
        }
    }
}

/// Each member's running turn, from the daemon's list: members are named
/// `SWARM-N`, so they sit together in its name order. A bot under a
/// member's name that is not the member's bot is left out.
async fn running_turns(
    client: &Client,
    swarm: &Swarm,
) -> Result<Vec<(String, Option<i64>)>, String> {
    let prefix = format!("{}-", swarm.name);
    let mut after = swarm.name.clone();
    let mut out = Vec::new();
    loop {
        let page = client
            .request("bots", json!({"after": after, "limit": 256}))
            .await
            .map_err(|e| e.to_string())?;
        let bots = page["bots"].as_array().cloned().unwrap_or_default();
        for bot in &bots {
            let name = bot["name"].as_str().unwrap_or_default();
            if name > prefix.as_str() && !name.starts_with(&prefix) {
                return Ok(out);
            }
            if swarm.members.iter().any(|m| m == name)
                && swarm.ids.get(name).copied() == bot["id"].as_i64()
            {
                out.push((name.to_owned(), bot["running_turn"].as_i64()));
            }
        }
        match page["next_after"].as_str() {
            Some(next) if out.len() < swarm.members.len() => after = next.to_owned(),
            _ => return Ok(out),
        }
    }
}

/// Do an act on the board, then steer what it says into the agents it
/// reaches, all at once over one connection.
pub async fn act(
    client: &Arc<Client>,
    root: &Path,
    swarm: &str,
    author: Option<Author>,
    act: Act,
) -> Result<Value, String> {
    let dir = folder(root, swarm)?;
    // Held until every steer is sent. Stop changes `swarm.toml` under this
    // lock, so it either comes first and this post is refused, or waits and
    // then ends whatever turns this post started. Waited for off the async
    // workers, which the holder needs to finish its sends.
    let mut board = lock_board_async(&dir).await?;
    let mut s = Swarm::read(&dir)?;
    if let Some(author) = &author {
        if s.ids.get(&author.bot) != Some(&author.id) {
            return Err(format!("not_a_member: {} is not in {}", author.bot, s.name));
        }
        if s.stopped {
            return Err("swarm_stopped: the user stopped this swarm; end your turn".into());
        }
    } else if s.stopped && matches!(act, Act::Post { .. }) {
        // Your post resumes a stopped swarm.
        s.stopped = false;
        s.write(&dir)?;
    }
    let bot = author.as_ref().map(|a| a.bot.as_str());
    let (notices, mut answer) = locked_with(&mut board, &dir, |state| {
        let (mut lines, notices, answer) = plan(&s, state, bot, act, now_ms())?;
        if let Some(author) = &author {
            for line in lines.iter_mut().filter(|line| line["from"] != "council") {
                line["bot"] = json!(author.bot);
                line["turn"] = json!(author.turn);
            }
        }
        Ok((lines, (notices, answer)))
    })?;
    // A swarm you stopped keeps what you decided on the board, and tells nobody.
    let notices = if s.stopped { Vec::new() } else { notices };
    let turns = if notices.is_empty() {
        Vec::new()
    } else {
        running_turns(client, &s).await?
    };
    let running = |m: &str| turns.iter().find(|(n, _)| n == m).and_then(|(_, t)| *t);
    // Distinct per post, even for two in one millisecond from one process.
    static POSTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = POSTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = format!("swarm-{}-{}-{n}", now_ms(), std::process::id());
    let mut sends = tokio::task::JoinSet::new();
    let mut i = 0;
    for notice in notices {
        let reach = match &notice.audience {
            Audience::Post { scope, text } => readers(&s, bot, text, scope.as_deref(), &running),
            Audience::Wake { who, working } => s
                .members
                .iter()
                .filter(|m| Some(m.as_str()) != bot)
                .filter_map(|m| match (who.contains(m), running(m)) {
                    (true, _) => Some((m.clone(), Reach::Wake)),
                    (false, Some(turn)) if *working => Some((m.clone(), Reach::Running(turn))),
                    _ => None,
                })
                .collect(),
        };
        for (member, reach) in reach {
            let mut params = json!({
                "bot": member, "bot_id": s.ids.get(&member), "request_id": format!("{stamp}-{i}"),
                "prompt": notice.prompt, "delivery": "steer",
            });
            i += 1;
            if let Reach::Running(turn) = reach {
                params["expected_turn"] = json!(turn);
            }
            if let Some(author) = &author {
                params["from"] = json!({"bot": author.bot, "turn": author.turn});
            }
            let client = client.clone();
            sends.spawn(async move { (member, reach, client.request("submit", params).await) });
        }
    }
    let (mut steered, mut woke, mut missed) = (Vec::new(), Vec::new(), Vec::new());
    while let Some(sent) = sends.join_next().await {
        let Ok((member, reach, result)) = sent else {
            missed.push(json!({"agent": "?", "error": "a send ended without an answer"}));
            continue;
        };
        let short = s.short(&member).to_owned();
        match result {
            Ok(done) if reach == Reach::Wake && done["status"] != "steered" => woke.push(short),
            Ok(_) => steered.push(short),
            // Its turn ended between the list and the steer: it reads the board when next woken.
            Err(error) if error.code == "stale_turn" => {}
            Err(error) => missed.push(json!({"agent": short, "error": error.to_string()})),
        }
    }
    for list in [&mut steered, &mut woke] {
        list.sort();
        list.dedup();
    }
    answer["posted"] = json!(true);
    answer["steered"] = json!(steered);
    answer["woke"] = json!(woke);
    answer["missed"] = json!(missed);
    Ok(answer)
}

/// A script's words as an act: `post [--all] TEXT`, `role ROLE`,
/// `propose STREAM WHY`, `vote ID yes|no REASON`, `join STREAM`.
fn parse(words: &[String]) -> Result<Act, String> {
    let rest = |from: usize| words.get(from..).unwrap_or_default().join(" ");
    let usage = |flag: &str| {
        let tool = TOOLS.iter().find(|t| t.flag == flag).unwrap_or(&TOOLS[0]);
        format!("usage: {}", tool.usage)
    };
    match words.first().map(String::as_str) {
        Some("--role") => Ok(Act::Role(rest(1))),
        Some("--propose") => match words.get(1) {
            Some(stream) => Ok(Act::Propose {
                stream: stream.clone(),
                why: rest(2),
            }),
            None => Err(usage("--propose")),
        },
        Some("--vote") => match (words.get(1), words.get(2).map(|w| w.to_ascii_lowercase())) {
            (Some(id), Some(vote)) if vote == "yes" || vote == "no" => Ok(Act::Vote {
                id: id.clone(),
                yes: vote == "yes",
                reason: rest(3),
            }),
            _ => Err(usage("--vote")),
        },
        Some("--join") => match words.get(1) {
            Some(stream) => Ok(Act::Join(stream.clone())),
            None => Err(usage("--join")),
        },
        Some("--all") => Ok(Act::Post {
            text: rest(1),
            all: true,
        }),
        _ => Ok(Act::Post {
            text: rest(0),
            all: false,
        }),
    }
}

/// A script run by an agent: `APP --swarm-post DIR [FLAG] WORDS...`. The
/// agent is the bot and turn its shell's environment names; the daemon is
/// the one that shell belongs to.
pub fn cli(args: &[String]) -> i32 {
    let fail = |message: String| {
        eprintln!("{}", json!({"error": message}));
        1
    };
    let Some((dir, words)) = args.split_first() else {
        return fail("usage: post TEXT".into());
    };
    let action = match parse(words) {
        Ok(action) => action,
        Err(error) => return fail(error),
    };
    let dir = PathBuf::from(dir);
    let (Some(root), Some(swarm)) = (dir.parent(), dir.file_name().and_then(|n| n.to_str())) else {
        return fail(format!("not a swarm folder: {}", dir.display()));
    };
    let number = |key: &str| std::env::var(key).ok().and_then(|v| v.parse::<i64>().ok());
    let author = match (
        std::env::var("AGENT_BOT"),
        number("AGENT_BOT_ID"),
        number("AGENT_TURN"),
    ) {
        (Ok(bot), Some(id), Some(turn)) if !bot.is_empty() => Author { bot, id, turn },
        _ => {
            return fail(
                "post runs in a swarm agent's shell, which names AGENT_BOT, AGENT_BOT_ID and AGENT_TURN"
                    .into(),
            );
        }
    };
    let socket = match socket() {
        Ok(socket) => socket,
        Err(error) => return fail(error),
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(error.to_string()),
    };
    let result = runtime.block_on(async {
        let (client, _events) = Client::connect(&socket).await.map_err(|e| e.to_string())?;
        let posted = act(&client, root, swarm, Some(author), action).await;
        client.close().await;
        posted
    });
    match result {
        Ok(value) => {
            println!("{value}");
            0
        }
        Err(error) => fail(error),
    }
}

/// The daemon a bot's shell belongs to: its socket when the daemon was given
/// one, else the one beside its store, as the CLI finds it.
fn socket() -> Result<PathBuf, String> {
    if let Some(socket) = std::env::var_os("AGENT_SOCKET") {
        return Ok(socket.into());
    }
    let store = std::env::var_os("AGENT_STORE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".agent/state.sqlite")))
        .ok_or("no AGENT_STORE or HOME to find the daemon")?;
    agent_client::socket::default_socket(&store).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One line, as a post adds it.
    fn append(dir: &Path, line: &Value) -> Result<(), String> {
        locked(dir, |_| Ok((vec![line.clone()], ())))
    }

    fn swarm(members: &[&str]) -> Swarm {
        Swarm {
            name: "agent.latency".into(),
            project: "agent".into(),
            goal: "Halve p99.".into(),
            workspace: "/w".into(),
            model: "openai/gpt-6-luna".into(),
            budget_tokens: 3_000_000,
            members: members.iter().map(|m| m.to_string()).collect(),
            ids: members
                .iter()
                .zip(1..)
                .map(|(m, id)| (m.to_string(), id))
                .collect(),
            stopped: false,
            council: 0,
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-swarm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn agents_reach_whoever_is_working_and_wake_only_whom_they_name() {
        let s = swarm(&[
            "agent.latency-1",
            "agent.latency-2",
            "agent.latency-3",
            "agent.latency-4",
        ]);
        // 2 works on turn 7; 3 and 4 are idle.
        let running = |m: &str| (m == "agent.latency-2").then_some(7);
        let reach = readers(&s, Some("agent.latency-1"), "profile is up", None, &running);
        assert_eq!(reach, vec![("agent.latency-2".into(), Reach::Running(7))]);
        let reach = readers(
            &s,
            Some("agent.latency-1"),
            "@latency-4 can you take the tests?",
            None,
            &running,
        );
        assert_eq!(
            reach,
            vec![
                ("agent.latency-2".into(), Reach::Running(7)),
                ("agent.latency-4".into(), Reach::Wake)
            ]
        );
        // Your post wakes everyone, or only the ones it names; nobody hears their own post.
        let reach = readers(&s, None, "Keep cargo test green.", None, &running);
        assert_eq!(reach.len(), 4);
        assert!(reach.iter().all(|(_, r)| *r == Reach::Wake));
        let reach = readers(&s, None, "@agent.latency-3, stop.", None, &running);
        assert_eq!(
            reach,
            vec![
                ("agent.latency-2".into(), Reach::Running(7)),
                ("agent.latency-3".into(), Reach::Wake)
            ]
        );
        let reach = readers(
            &s,
            Some("agent.latency-2"),
            "@latency-2 note to self",
            None,
            &running,
        );
        assert!(reach.is_empty());
    }

    fn council(members: &[&str]) -> Swarm {
        Swarm {
            council: 3,
            ..swarm(members)
        }
    }

    fn agent(n: usize) -> String {
        format!("agent.latency-{n}")
    }

    #[test]
    fn a_proposal_wakes_the_other_seats_and_a_majority_opens_its_stream() {
        let s = council(&[
            "agent.latency-1",
            "agent.latency-2",
            "agent.latency-3",
            "agent.latency-4",
        ]);
        let mut state = State::default();
        let propose = Act::Propose {
            stream: "#conn-pool".into(),
            why: "61% of p99 is the handshake.".into(),
        };
        let (lines, notices, answer) = plan(&s, &mut state, Some(&agent(4)), propose, 1).unwrap();
        assert_eq!(answer["id"], "P1");
        assert_eq!(lines[0]["kind"], "propose");
        assert_eq!(lines[0]["stream"], "conn-pool");
        // Seats are the first three agents; the proposer, not a seat here, votes nowhere.
        assert_eq!(
            notices[0].audience,
            Audience::Wake {
                who: vec![agent(1), agent(2), agent(3)],
                working: false
            }
        );
        assert!(notices[0].prompt.contains("vote P1 yes|no REASON"));
        let vote = |yes| Act::Vote {
            id: "p1".into(),
            yes,
            reason: "fine".into(),
        };
        assert!(
            plan(&s, &mut state, Some(&agent(4)), vote(true), 2)
                .unwrap_err()
                .starts_with("not_a_seat")
        );
        let (_, notices, answer) = plan(&s, &mut state, Some(&agent(1)), vote(true), 2).unwrap();
        assert!(notices.is_empty());
        assert_eq!(answer["decided"], Value::Null);
        assert!(
            plan(&s, &mut state, Some(&agent(1)), vote(false), 3)
                .unwrap_err()
                .starts_with("already_voted")
        );
        let (lines, notices, answer) =
            plan(&s, &mut state, Some(&agent(2)), vote(true), 4).unwrap();
        assert_eq!(answer["decided"], "approved");
        assert_eq!(lines[1]["kind"], "decision");
        assert_eq!(lines[1]["from"], "council");
        assert_eq!(lines[1]["lead"], "latency-4");
        assert_eq!(
            notices[0].audience,
            Audience::Wake {
                who: vec![agent(4)],
                working: true
            }
        );
        assert_eq!(
            state.streams.get("latency-4").map(String::as_str),
            Some("conn-pool")
        );
        assert!(
            plan(&s, &mut state, Some(&agent(3)), vote(false), 5)
                .unwrap_err()
                .starts_with("decided")
        );
        let again = Act::Propose {
            stream: "conn-pool".into(),
            why: "again".into(),
        };
        assert!(
            plan(&s, &mut state, Some(&agent(1)), again, 6)
                .unwrap_err()
                .starts_with("stream_exists")
        );
    }

    #[test]
    fn two_noes_of_three_deny_and_you_decide_alone() {
        let s = council(&["agent.latency-1", "agent.latency-2", "agent.latency-3"]);
        let mut state = State::default();
        for (n, stream) in [(1, "fsync"), (2, "cache")] {
            let propose = Act::Propose {
                stream: stream.into(),
                why: "why".into(),
            };
            plan(&s, &mut state, Some(&agent(n)), propose, 1).unwrap();
        }
        let no = || Act::Vote {
            id: "P1".into(),
            yes: false,
            reason: "measure first".into(),
        };
        plan(&s, &mut state, Some(&agent(2)), no(), 2).unwrap();
        let (_, notices, answer) = plan(&s, &mut state, Some(&agent(3)), no(), 3).unwrap();
        assert_eq!(answer["decided"], "denied");
        assert_eq!(
            notices[0].audience,
            Audience::Wake {
                who: vec![agent(1)],
                working: false
            }
        );
        assert!(state.streams.is_empty());
        // Denied, the name is free again.
        let retry = Act::Propose {
            stream: "fsync".into(),
            why: "measured".into(),
        };
        assert_eq!(
            plan(&s, &mut state, Some(&agent(1)), retry, 4).unwrap().2["id"],
            "P3"
        );
        let approve = Act::Vote {
            id: "P2".into(),
            yes: true,
            reason: String::new(),
        };
        let (lines, _, answer) = plan(&s, &mut state, None, approve, 5).unwrap();
        assert_eq!(answer["decided"], "approved");
        assert_eq!(lines[1]["from"], "user");
        assert_eq!(state.proposals[1].decided_by.as_deref(), Some("user"));
    }

    #[test]
    fn a_stream_member_posts_to_its_stream_unless_it_posts_to_everyone() {
        let s = council(&["agent.latency-1", "agent.latency-2", "agent.latency-3"]);
        let mut state = State::default();
        let join = |stream: &str| Act::Join(stream.into());
        assert!(
            plan(&s, &mut state, Some(&agent(2)), join("fsync"), 1)
                .unwrap_err()
                .starts_with("no_stream")
        );
        plan(
            &s,
            &mut state,
            Some(&agent(1)),
            Act::Propose {
                stream: "fsync".into(),
                why: "w".into(),
            },
            1,
        )
        .unwrap();
        plan(
            &s,
            &mut state,
            None,
            Act::Vote {
                id: "P1".into(),
                yes: true,
                reason: String::new(),
            },
            2,
        )
        .unwrap();
        plan(&s, &mut state, Some(&agent(2)), join("#fsync"), 3).unwrap();
        let post = |all| Act::Post {
            text: "fsync is 22% of p99".into(),
            all,
        };
        let (lines, notices, _) = plan(&s, &mut state, Some(&agent(2)), post(false), 4).unwrap();
        assert_eq!(lines[0]["stream"], "fsync");
        assert!(notices[0].prompt.starts_with("[board #fsync] latency-2:"));
        let Audience::Post { scope, text } = &notices[0].audience else {
            panic!()
        };
        assert_eq!(scope.as_deref(), Some(&[agent(1), agent(2)][..]));
        // 1 and 3 both work; only 1 is in the stream, so only 1 hears it, unless 3 is named.
        let running = |_: &str| Some(9);
        assert_eq!(
            readers(&s, Some(&agent(2)), text, scope.as_deref(), &running),
            vec![(agent(1), Reach::Running(9))]
        );
        assert_eq!(
            readers(
                &s,
                Some(&agent(2)),
                "@latency-3 see this",
                scope.as_deref(),
                &running
            )
            .len(),
            2
        );
        let (lines, notices, _) = plan(&s, &mut state, Some(&agent(2)), post(true), 5).unwrap();
        assert!(lines[0].get("stream").is_none());
        assert!(matches!(
            &notices[0].audience,
            Audience::Post { scope: None, .. }
        ));
        plan(
            &s,
            &mut state,
            Some(&agent(3)),
            Act::Role("keeps tests green".into()),
            6,
        )
        .unwrap();
        assert_eq!(
            state.roles.get("latency-3").map(String::as_str),
            Some("keeps tests green")
        );
        assert!(
            plan(&s, &mut state, None, Act::Role("x".into()), 7)
                .unwrap_err()
                .starts_with("agents_only")
        );
    }

    #[test]
    fn a_flat_board_has_no_council() {
        let s = swarm(&["agent.latency-1"]);
        let mut state = State::default();
        let propose = Act::Propose {
            stream: "x".into(),
            why: "w".into(),
        };
        assert!(
            plan(&s, &mut state, Some(&agent(1)), propose, 1)
                .unwrap_err()
                .starts_with("no_council")
        );
        let (lines, _, _) = plan(
            &s,
            &mut state,
            Some(&agent(1)),
            Act::Post {
                text: "hi".into(),
                all: false,
            },
            1,
        )
        .unwrap();
        assert!(lines[0].get("stream").is_none());
    }

    #[test]
    fn scripts_parse_into_acts() {
        let words = |text: &str| text.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            parse(&words("taking the profile")).unwrap(),
            Act::Post {
                text: "taking the profile".into(),
                all: false
            }
        );
        assert_eq!(
            parse(&words("--all done here")).unwrap(),
            Act::Post {
                text: "done here".into(),
                all: true
            }
        );
        assert_eq!(
            parse(&words("--role keeps tests green")).unwrap(),
            Act::Role("keeps tests green".into())
        );
        assert_eq!(
            parse(&words("--propose conn-pool reuse connections")).unwrap(),
            Act::Propose {
                stream: "conn-pool".into(),
                why: "reuse connections".into()
            }
        );
        assert_eq!(
            parse(&words("--vote P1 YES it is measured")).unwrap(),
            Act::Vote {
                id: "P1".into(),
                yes: true,
                reason: "it is measured".into()
            }
        );
        assert!(
            parse(&words("--vote P1 maybe"))
                .unwrap_err()
                .contains("vote ID yes|no")
        );
        assert_eq!(
            parse(&words("--join fsync")).unwrap(),
            Act::Join("fsync".into())
        );
    }

    #[test]
    fn the_state_survives_beside_the_board_and_a_council_gets_its_scripts() {
        let root = scratch("state");
        let s = council(&[]);
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/app")).unwrap();
        for tool in ["post", "role", "propose", "vote", "join"] {
            assert!(dir.join(tool).exists(), "{tool}");
        }
        assert!(
            std::fs::read_to_string(dir.join("vote"))
                .unwrap()
                .contains("--swarm-post '")
        );
        join(&root, &s.name, &[(agent(1), 1), (agent(2), 2)], 0).unwrap();
        let s = Swarm::read(&dir).unwrap();
        assert_eq!(s.council, 3);
        assert_eq!(s.seats(), vec![agent(1), agent(2)]);
        locked(&dir, |state| {
            let (lines, _, _) = plan(&s, state, Some(&agent(1)), Act::Role("profiler".into()), 1)?;
            Ok((lines, ()))
        })
        .unwrap();
        let read = board(&root, &s.name, None).unwrap();
        assert_eq!(read["lines"].as_array().unwrap().len(), 2);
        assert_eq!(read["state"]["roles"]["latency-1"], "profiler");
        let flat = Swarm {
            name: "agent.flat".into(),
            ..swarm(&[])
        };
        let dir = claim(&root, &flat.name).unwrap();
        fill(&dir, &flat, Path::new("/app")).unwrap();
        assert!(dir.join("role").exists() && !dir.join("vote").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn names_end_where_a_name_ends() {
        assert_eq!(
            named("@a-1. then @b_2, and @c.d! mail x@y"),
            vec!["a-1", "b_2", "c.d", "y"]
        );
        assert!(named("no names @ all").is_empty());
    }

    #[test]
    fn a_swarm_is_its_folder_and_members_join_once() {
        let home = scratch("folder");
        let root = home.join("0123456789abcdef");
        let s = swarm(&[]);
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/Applications/It's.app/agent-app")).unwrap();
        assert!(
            claim(&root, &s.name)
                .unwrap_err()
                .starts_with("swarm_exists")
        );
        let post = std::fs::read_to_string(dir.join("post")).unwrap();
        assert!(post.contains(r"exec '/Applications/It'\''s.app/agent-app' --swarm-post '"));
        let joined = join(
            &root,
            &s.name,
            &[
                ("agent.latency-1".into(), 11),
                ("agent.latency-2".into(), 12),
            ],
            0,
        )
        .unwrap();
        // An agent added later grows the budget by its own.
        let grown = join(&root, &s.name, &[("agent.latency-2".into(), 12)], 1_000).unwrap();
        assert_eq!(grown["budget_tokens"], 3_001_000);
        assert!(
            join(&root, &s.name, &[("other.x-1".into(), 13)], 0)
                .unwrap_err()
                .starts_with("invalid_member")
        );
        assert_eq!(
            joined["members"],
            json!(["agent.latency-1", "agent.latency-2"])
        );
        let read = Swarm::read(&dir).unwrap();
        assert_eq!(read.members.len(), 2);
        assert_eq!(read.ids["agent.latency-2"], 12);
        assert_eq!(read.goal, "Halve p99.");
        // A deleted agent leaves, id and all.
        let left = leave(&root, &s.name, "agent.latency-1").unwrap();
        assert_eq!(left["members"], json!(["agent.latency-2"]));
        assert_eq!(left["ids"], json!({"agent.latency-2": 12}));
        assert!(
            set_stopped(&root, &s.name, true).unwrap()["stopped"]
                .as_bool()
                .unwrap()
        );
        let listed = list(&root).unwrap();
        assert_eq!(listed["swarms"][0]["swarm"], "agent.latency");
        // A stop state or member ids that cannot be read make the swarm unreadable, not active.
        let toml = root.join("agent.latency/swarm.toml");
        let good = std::fs::read_to_string(&toml).unwrap();
        for bad in [
            good.replace("stopped = true", ""),
            good.replace("stopped = true", "stopped = \"no\""),
            good.replace("\"agent.latency-2\" = 12", "\"agent.latency-2\" = \"x\""),
            "goal = 1".into(),
        ] {
            std::fs::write(&toml, bad).unwrap();
            assert_eq!(list(&root).unwrap()["broken"].as_array().unwrap().len(), 1);
        }
        assert_eq!(list(&home.join("absent")).unwrap()["swarms"], json!([]));
        assert!(
            list(&toml).is_err(),
            "a root that cannot be read is an error, not no swarms"
        );
        for (goal, ok) in [
            ("Halve p99.", true),
            (" ", false),
            (&"x".repeat(MAX_POST + 1), false),
        ] {
            assert_eq!(valid_goal(goal).is_ok(), ok);
        }
        for bad in ["", ".x", "a..b", "a/b", "a.", "p.lock", &"a".repeat(101)] {
            assert!(valid_name(bad).is_err(), "{bad}");
        }
        refresh_scripts(&home, Path::new("/moved/agent-app"));
        assert!(
            std::fs::read_to_string(dir.join("post"))
                .unwrap()
                .contains("'/moved/agent-app'")
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn the_board_reads_whole_lines_from_where_it_left_off() {
        let root = scratch("board");
        let s = swarm(&[]);
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/app")).unwrap();
        let first = board(&root, &s.name, None).unwrap();
        assert_eq!(first["lines"][0]["from"], "user");
        assert_eq!(first["lines"][0]["text"], "Halve p99.");
        let offset = first["offset"].as_u64().unwrap();
        append(
            &dir,
            &json!({"from": "latency-1", "text": "a \"quoted\"\nline"}),
        )
        .unwrap();
        // Half a line (as a hand edit could leave) waits until it ends.
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("board.jsonl"))
            .unwrap()
            .write_all(b"not json")
            .unwrap();
        let next = board(&root, &s.name, Some(offset)).unwrap();
        assert_eq!(next["lines"].as_array().unwrap().len(), 1);
        assert_eq!(next["lines"][0]["text"], "a \"quoted\"\nline");
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("board.jsonl"))
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        let last = board(&root, &s.name, next["offset"].as_u64()).unwrap();
        assert_eq!(last["lines"][0]["text"], "not json");
        // A long board is read from its tail, starting at a whole line.
        let long = "x".repeat(1000);
        for _ in 0..400 {
            append(&dir, &json!({"from": "latency-2", "text": long})).unwrap();
        }
        let tail = board(&root, &s.name, None).unwrap();
        let lines = tail["lines"].as_array().unwrap();
        assert!(lines.len() > 200 && lines.len() < 400);
        assert!(lines.iter().all(|l| l["from"] == "latency-2"));
        assert_eq!(tail["reset"], true);
        // A reader far behind gets the same tail, not the backlog; one
        // close behind reads on.
        let behind = board(&root, &s.name, last["offset"].as_u64()).unwrap();
        assert_eq!(
            (&behind["reset"], &behind["lines"]),
            (&tail["reset"], &tail["lines"])
        );
        append(&dir, &json!({"from": "latency-3", "text": "done"})).unwrap();
        let next = board(&root, &s.name, tail["offset"].as_u64()).unwrap();
        assert_eq!(
            (
                next["reset"].as_bool(),
                next["lines"].as_array().map(Vec::len)
            ),
            (Some(false), Some(1))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_a_swarm_without_agents_is_discarded() {
        let root = scratch("discard");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let s = swarm(&[]);
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/app")).unwrap();
        join(&root, &s.name, &[("agent.latency-1".into(), 1)], 0).unwrap();
        let kept = runtime.block_on(discard(&root, &s.name)).unwrap_err();
        assert!(kept.starts_with("swarm_has_agents"), "{kept}");
        leave(&root, &s.name, "agent.latency-1").unwrap();
        runtime.block_on(discard(&root, &s.name)).unwrap();
        assert!(!dir.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn each_store_has_its_own_swarms() {
        let a = root("0123456789abcdef0123456789abcdef").unwrap();
        let b = root("fedcba9876543210fedcba9876543210").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent(), b.parent());
        assert_eq!(a, root("0123456789abcdef0123456789abcdef").unwrap());
        for bad in ["", "../x", "ab/cd"] {
            assert!(root(bad).unwrap_err().starts_with("invalid_store"), "{bad}");
        }
    }

    #[test]
    fn settings_reads_are_bounded() {
        let root = scratch("capped");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("swarm.toml");
        std::fs::write(&path, "x".repeat(9)).unwrap();
        assert_eq!(read_capped(&path, 9).unwrap().len(), 9);
        assert!(
            read_capped(&path, 8)
                .unwrap_err()
                .contains("larger than 8 bytes")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_member_without_a_bot_id_is_refused() {
        let root = scratch("unpinned");
        let s = swarm(&[]);
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/app")).unwrap();
        join(&root, &s.name, &[("agent.latency-1".into(), 1)], 0).unwrap();
        let path = dir.join("swarm.toml");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("\"agent.latency-1\" = 1", "")).unwrap();
        let err = Swarm::read(&dir).unwrap_err();
        assert!(err.contains("a member has no bot id"), "{err}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changes_from_two_windows_are_both_kept() {
        let root = scratch("race");
        let s = swarm(&[]);
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/app")).unwrap();
        let threads: Vec<_> = (1..=8)
            .map(|i| {
                let (root, name) = (root.clone(), s.name.clone());
                std::thread::spawn(move || {
                    join(&root, &name, &[(format!("{name}-{i}"), i)], 0).unwrap();
                    set_stopped(&root, &name, i % 2 == 0).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(Swarm::read(&dir).unwrap().ids.len(), 8);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_gets_no_keys_and_no_more_than_its_time() {
        let env = setup_env(
            [
                ("PATH", "/bin"),
                ("LC_ALL", "C"),
                ("OPENAI_API_KEY", "x"),
                ("AWS_SECRET_ACCESS_KEY", "x"),
                ("HOME", "/h"),
            ]
            .into_iter()
            .map(|(k, v)| (k.into(), v.into())),
        );
        let keys: Vec<_> = env.iter().map(|(k, _)| k.to_str().unwrap()).collect();
        assert_eq!(keys, ["PATH", "LC_ALL", "HOME"]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let sh = |script: &str| {
            let mut command = tokio::process::Command::new("/bin/sh");
            command.args(["-c", script]);
            command
        };
        let limit = std::time::Duration::from_millis(500);
        runtime.block_on(async {
            assert_eq!(run_setup(sh("true"), limit).await, Ok(()));
            let hung = run_setup(sh("sleep 30"), limit).await.unwrap_err();
            assert!(hung.contains("ran past"), "{hung}");
            // What it started goes with it.
            let pid =
                std::env::temp_dir().join(format!("agent-setup-child-{}", std::process::id()));
            let script = format!("sleep 30 & echo $! > '{}'; wait", pid.display());
            assert!(run_setup(sh(&script), limit).await.is_err());
            let child = std::fs::read_to_string(&pid).unwrap();
            let _ = std::fs::remove_file(&pid);
            // Gone, or a zombie nobody has reaped yet: either way not running.
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", child.trim()));
            let running = stat.is_ok_and(|s| !s.contains(") Z "));
            assert!(!running, "setup's background child outlived it");
            // A script that never stops writing is stopped too, holding only a tail.
            let noisy = run_setup(sh("yes"), limit).await.unwrap_err();
            assert!(noisy.contains("ran past"), "{noisy}");
            let failed = run_setup(sh("seq 1 100000; echo why >&2; exit 3"), limit)
                .await
                .unwrap_err();
            assert!(failed.ends_with("why") && failed.len() < 1000, "{failed}");
        });
    }
}
