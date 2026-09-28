//! Swarms: many agents on one goal, talking through a board. A swarm is the
//! app's; the daemon knows only its agents, which are ordinary bots named
//! `SWARM-N`. A swarm is a folder, `~/.agent/swarms/DAEMON/SWARM/`, where
//! DAEMON stands for the socket its agents' daemon listens on, so a window
//! attached to another store never sees them:
//!
//! - `swarm.toml`: its project, goal, folder, token budget, its mix (rows
//!   of an identity, a model and a share of the agents), members with the id
//!   of the bot each one is and the row it was made from, and whether you
//!   stopped it. Only the app writes it, under the board's lock.
//! - `board.jsonl`: one post a line, appended under a lock, each saying how
//!   many agents it reached.
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
    pub budget_tokens: u64,
    /// What its agents are: each row an identity, a model and a share.
    pub mix: Vec<Mix>,
    pub members: Vec<String>,
    /// Each member's bot id: a name can be deleted and made again, and the
    /// new bot is not a member.
    pub ids: BTreeMap<String, i64>,
    /// Each member's row of the mix.
    pub rows: BTreeMap<String, usize>,
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
            mix: table
                .get("mix")
                .and_then(toml::Value::as_array)
                .and_then(|rows| rows.iter().map(Mix::from_toml).collect::<Option<Vec<_>>>())
                .filter(|mix| valid_mix(mix).is_ok())
                .ok_or_else(|| format!("{}: mix is missing or not a valid mix", path.display()))?,
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
            rows: table
                .get("rows")
                .and_then(toml::Value::as_table)
                .and_then(|rows| {
                    rows.iter()
                        .map(|(m, row)| Some((m.clone(), usize::try_from(row.as_integer()?).ok()?)))
                        .collect()
                })
                .ok_or_else(|| {
                    format!("{}: rows is missing or not rows of the mix", path.display())
                })?,
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
        let placed = |m: &String| swarm.rows.get(m).is_some_and(|row| *row < swarm.mix.len());
        if !swarm.members.iter().all(placed) {
            return Err(format!(
                "{}: a member has no row of the mix",
                path.display()
            ));
        }
        Ok(swarm)
    }

    /// Replaced whole, through a rename, so a post never reads half a file.
    fn write(&self, dir: &Path) -> Result<(), String> {
        let mut table = toml::Table::new();
        table.insert("project".into(), self.project.clone().into());
        table.insert("goal".into(), self.goal.clone().into());
        table.insert("workspace".into(), self.workspace.clone().into());
        table.insert("budget_tokens".into(), (self.budget_tokens as i64).into());
        table.insert(
            "mix".into(),
            toml::Value::Array(self.mix.iter().map(Mix::to_toml).collect()),
        );
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
        table.insert(
            "rows".into(),
            toml::Value::Table(
                self.rows
                    .iter()
                    .map(|(m, row)| (m.clone(), (*row as i64).into()))
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
            "goal": self.goal, "workspace": self.workspace, "budget_tokens": self.budget_tokens,
            "mix": self.mix.iter().map(Mix::json).collect::<Vec<_>>(),
            "members": self.members, "ids": self.ids, "rows": self.rows,
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

    /// A member's row of the mix.
    fn row(&self, member: &str) -> Option<&Mix> {
        self.rows.get(member).and_then(|row| self.mix.get(*row))
    }
}

/// A row of a swarm's mix: its agents' identity (a profile, or none for a
/// plain agent), their model, and their share of the agents in percent.
#[derive(Debug, Clone, PartialEq)]
pub struct Mix {
    pub identity: String,
    pub model: String,
    pub share: u32,
}

/// Rows of a mix; the page offers fewer.
const MAX_MIX: usize = 8;
/// Agents a swarm starts with at most; Add can go on from there.
const MAX_START: usize = 64;

impl Mix {
    pub fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            identity: value["identity"].as_str()?.to_owned(),
            model: value["model"].as_str()?.to_owned(),
            share: u32::try_from(value["share"].as_u64()?).ok()?,
        })
    }

    fn from_toml(value: &toml::Value) -> Option<Self> {
        Some(Self {
            identity: value.get("identity")?.as_str()?.to_owned(),
            model: value.get("model")?.as_str()?.to_owned(),
            share: u32::try_from(value.get("share")?.as_integer()?).ok()?,
        })
    }

    fn to_toml(&self) -> toml::Value {
        let mut row = toml::Table::new();
        row.insert("identity".into(), self.identity.clone().into());
        row.insert("model".into(), self.model.clone().into());
        row.insert("share".into(), i64::from(self.share).into());
        toml::Value::Table(row)
    }

    fn json(&self) -> Value {
        json!({"identity": self.identity, "model": self.model, "share": self.share})
    }
}

/// Up to eight rows whose shares add up to 100, each with a model, and an
/// identity that is a profile's name or empty for a plain agent. The app's
/// own roles are not identities: every agent already has the swarm's, and
/// a coordinator is not a swarm's.
pub fn valid_mix(mix: &[Mix]) -> Result<(), String> {
    if mix.is_empty() || mix.len() > MAX_MIX {
        return Err(format!("invalid_mix: a mix has 1 to {MAX_MIX} rows"));
    }
    for row in mix {
        if row.model.trim().is_empty() {
            return Err("invalid_mix: every row needs a model".into());
        }
        if row.share == 0 || row.share > 100 {
            return Err(format!(
                "invalid_mix: a share is 1 to 100 percent, not {}",
                row.share
            ));
        }
        let name = row.identity.as_str();
        let word = name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
        if name.starts_with('.') || !word || ["swarm", "coordinator"].contains(&name) {
            return Err(format!("invalid_mix: {name} is not an identity"));
        }
    }
    let total: u32 = mix.iter().map(|row| row.share).sum();
    if total != 100 {
        return Err(format!(
            "invalid_mix: the shares add up to {total}%, not 100%"
        ));
    }
    Ok(())
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

/// Members join in order and never twice, each pinned to its bot's id and
/// its row of the mix. An agent added later brings its own budget, which the
/// swarm's grows by, so the budget shown is always what its agents may spend.
fn join(dir: &Path, members: &[(String, i64, usize)], added_budget: u64) -> Result<Swarm, String> {
    update(dir, |s| {
        s.budget_tokens = s.budget_tokens.saturating_add(added_budget);
        for (member, id, row) in members {
            if !member.starts_with(&format!("{}-", s.name)) {
                return Err(format!(
                    "invalid_member: {member} is not named {}-N",
                    s.name
                ));
            }
            if *row >= s.mix.len() {
                return Err(format!("invalid_member: the mix has no row {row}"));
            }
            if !s.members.contains(member) {
                s.members.push(member.clone());
            }
            s.ids.insert(member.clone(), *id);
            s.rows.insert(member.clone(), *row);
        }
        Ok(())
    })
}

/// `join` and `update` from async code: the board's lock may wait on a
/// post's sends, so the wait runs on the blocking pool, never on the
/// executor those sends need.
async fn join_async(
    dir: &Path,
    members: Vec<(String, i64, usize)>,
    added_budget: u64,
) -> Result<Swarm, String> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || join(&dir, &members, added_budget))
        .await
        .map_err(|e| e.to_string())?
}

async fn update_async(
    dir: &Path,
    change: impl FnOnce(&mut Swarm) -> Result<(), String> + Send + 'static,
) -> Result<Swarm, String> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || update(&dir, change))
        .await
        .map_err(|e| e.to_string())?
}

/// A deleted agent leaves: the swarm stops counting it and posting to it.
pub fn leave(root: &Path, swarm: &str, member: &str) -> Result<Value, String> {
    let dir = folder(root, swarm)?;
    let s = update(&dir, |s| {
        s.members.retain(|m| m != member);
        s.ids.remove(member);
        s.rows.remove(member);
        Ok(())
    })?;
    Ok(s.json(&dir))
}

/// A swarm that never got an agent goes with its worktree and branch.
async fn discard(dir: &Path, swarm: &Swarm) -> Result<(), String> {
    if Path::new(&swarm.workspace).starts_with(worktrees()?.join(&swarm.name)) {
        unplace(&swarm.name).await?;
    }
    std::fs::remove_dir_all(dir).map_err(|e| e.to_string())
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

/// What a new swarm is. The page picks its name and each agent's row of
/// the mix, since it shows those counts before you start.
pub struct Start {
    pub project: String,
    pub name: String,
    pub folder: PathBuf,
    pub goal: String,
    pub shared: bool,
    pub mix: Vec<Mix>,
    pub rows: Vec<usize>,
    pub budget_tokens: u64,
    pub council: usize,
}

/// Start a swarm: take its name, make the place its agents share, write its
/// folder, compose each identity's instructions, then create its agents,
/// have them join, and send each its brief. A step that fails undoes the
/// ones before it, so a swarm exists only with agents; an agent that could
/// not be made or briefed is reported beside the ones that were.
pub async fn start(
    client: &Arc<Client>,
    root: &Path,
    app: &Path,
    start: Start,
) -> Result<Value, String> {
    valid_goal(&start.goal)?;
    valid_mix(&start.mix)?;
    let n = start.rows.len();
    if n == 0 || n > MAX_START || start.rows.iter().any(|row| *row >= start.mix.len()) {
        return Err(format!(
            "invalid_agents: a swarm starts with 1 to {MAX_START} agents, each from a row of its mix"
        ));
    }
    let full = format!("{}.{}", start.project, start.name);
    let dir = claim(root, &full)?;
    let workspace = match place(&start.folder, &full, start.shared).await {
        Ok(workspace) => workspace,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(error);
        }
    };
    let swarm = Swarm {
        name: full,
        project: start.project,
        goal: start.goal.trim().to_owned(),
        workspace,
        budget_tokens: start.budget_tokens,
        mix: start.mix,
        members: Vec::new(),
        ids: BTreeMap::new(),
        rows: BTreeMap::new(),
        stopped: false,
        council: start.council,
    };
    if let Err(error) = fill(&dir, &swarm, app) {
        if start.shared {
            let _ = unplace(&swarm.name).await;
        }
        return Err(error);
    }
    // A folder whose instructions cannot compose gets no agents.
    let policies = match policies(&swarm, start.rows.iter().copied()) {
        Ok(policies) => policies,
        Err(error) => {
            let _ = discard(&dir, &swarm).await;
            return Err(error);
        }
    };
    let each = (swarm.budget_tokens / n as u64).max(1);
    let agents: Vec<(String, usize)> = (start.rows.iter().enumerate())
        .map(|(i, row)| (format!("{}-{}", swarm.name, i + 1), *row))
        .collect();
    let (made, mut failed) = create(client, &swarm, &agents, &policies, each).await;
    if made.is_empty() {
        let _ = discard(&dir, &swarm).await;
        return Err(failed
            .first()
            .map_or_else(String::new, |(_, e)| e.to_string()));
    }
    let joined = match join_async(&dir, pins(&made), 0).await {
        Ok(joined) => joined,
        Err(error) => {
            // Agents no swarm holds would be strays: they go with it.
            for (name, _, _) in &made {
                let _ = client.request("delete", json!({"bot": name})).await;
            }
            let _ = discard(&dir, &swarm).await;
            return Err(error);
        }
    };
    failed.extend(brief_all(client, &joined, &dir, &made, false).await);
    Ok(json!({
        "swarm": joined.json(&dir),
        "bots": made.iter().map(|(_, _, record)| record).collect::<Vec<_>>(),
        "failed": reasons(&failed),
    }))
}

/// One more agent from `row` of the mix, under the next number no member
/// has. Its budget adds to the swarm's.
pub async fn add(
    client: &Arc<Client>,
    root: &Path,
    swarm: &str,
    row: usize,
) -> Result<Value, String> {
    let dir = folder(root, swarm)?;
    let s = Swarm::read(&dir)?;
    if row >= s.mix.len() {
        return Err(format!("invalid_agents: the mix has no row {row}"));
    }
    let policies = policies(&s, std::iter::once(row))?;
    let each = (s.budget_tokens / s.members.len().max(1) as u64).max(1);
    let mut n = s.members.len() + 1;
    let made = loop {
        let name = format!("{}-{n}", s.name);
        n += 1;
        if s.members.contains(&name) {
            continue;
        }
        let (made, failed) = create(client, &s, &[(name, row)], &policies, each).await;
        match failed.into_iter().next() {
            // A bot of that name that is not a member: take the next number.
            Some((_, error)) if error.code == "bot_exists" && n <= s.members.len() + 64 => {}
            Some((_, error)) => return Err(error.to_string()),
            None => break made,
        }
    };
    let joined = join_async(&dir, pins(&made), each).await?;
    let failed = brief_all(client, &joined, &dir, &made, true).await;
    Ok(json!({
        "swarm": joined.json(&dir),
        "bots": made.iter().map(|(_, _, record)| record).collect::<Vec<_>>(),
        "failed": reasons(&failed),
    }))
}

/// Stop a swarm: it refuses its agents' posts from now on, and every turn
/// its agents have not finished ends. Posts wait on the board's lock, so a
/// post either comes first and its turns end here, or is refused. A name
/// that is no longer its member's bot leaves, its turns untouched.
pub async fn stop(client: &Arc<Client>, root: &Path, swarm: &str) -> Result<Value, String> {
    let dir = folder(root, swarm)?;
    let s = update_async(&dir, |s| {
        s.stopped = true;
        Ok(())
    })
    .await?;
    let mut ends = tokio::task::JoinSet::new();
    for member in &s.members {
        let (client, member, id) = (client.clone(), member.clone(), s.ids.get(member).copied());
        ends.spawn(async move {
            let ended = end_turns(&client, &member, id).await;
            (member, ended)
        });
    }
    let (mut gone, mut failed) = (Vec::new(), Vec::new());
    while let Some(ended) = ends.join_next().await {
        match ended.map_err(|e| e.to_string())? {
            (member, Ok(false)) => gone.push(member),
            (_, Ok(true)) => {}
            (member, Err(error)) => failed.push(json!({"agent": s.short(&member), "error": error})),
        }
    }
    let s = if gone.is_empty() {
        s
    } else {
        update_async(&dir, move |s| {
            s.members.retain(|m| !gone.contains(m));
            s.ids.retain(|m, _| !gone.contains(m));
            s.rows.retain(|m, _| !gone.contains(m));
            Ok(())
        })
        .await?
    };
    Ok(json!({"swarm": s.json(&dir), "failed": failed}))
}

/// End every turn `member` has not finished, the queued ones included,
/// newest first, so none starts as an older one ends; a turn that ended
/// meanwhile is already stopped. False when the name is not the member's
/// bot any more: deleted while no window watched, or made again.
async fn end_turns(client: &Client, member: &str, id: Option<i64>) -> Result<bool, String> {
    match client.request("resume", json!({"bot": member})).await {
        Ok(bot) if bot["id"].as_i64() == id => {}
        Ok(_) => return Ok(false),
        Err(error) if error.code == "bot_not_found" => return Ok(false),
        Err(error) => return Err(error.to_string()),
    }
    const ACTIVE: [&str; 5] = ["running", "waiting", "paced", "queued", "ready"];
    let (mut open, mut after) = (Vec::new(), json!(0));
    loop {
        let page = client
            .request(
                "turns",
                json!({"bot": member, "after": after, "limit": 256}),
            )
            .await
            .map_err(|e| e.to_string())?;
        for turn in page["turns"].as_array().into_iter().flatten() {
            if ACTIVE.contains(&turn["status"].as_str().unwrap_or_default()) {
                open.extend(turn["turn"].as_i64());
            }
        }
        match &page["next_after"] {
            Value::Null => break,
            next => after = next.clone(),
        }
    }
    for turn in open.into_iter().rev() {
        match client
            .request("interrupt", json!({"bot": member, "turn": turn}))
            .await
        {
            Ok(_) => {}
            Err(error) if error.code == "stale_turn" || error.code == "no_active_turn" => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(true)
}

/// Each identity's instructions for the swarm's folder, composed once: a
/// plain agent's are the `swarm` role's, and an identity's are its profile's
/// with the `swarm` role's after them. The board is scripts its agents run,
/// so an identity limited to tools without `shell` could not take part.
fn policies(
    swarm: &Swarm,
    rows: impl Iterator<Item = usize>,
) -> Result<BTreeMap<String, Value>, String> {
    let mut out = BTreeMap::new();
    for row in rows {
        let identity = &swarm.mix[row].identity;
        if out.contains_key(identity) {
            continue;
        }
        let workspace = Path::new(&swarm.workspace);
        let policy = if identity.is_empty() {
            crate::compose(workspace, Some("swarm"), None)?
        } else {
            crate::compose(workspace, Some(identity), Some("swarm"))?
        };
        if policy["tools"]
            .as_array()
            .is_some_and(|tools| !tools.iter().any(|tool| tool == "shell"))
        {
            let who = if identity.is_empty() {
                "swarm"
            } else {
                identity
            };
            return Err(format!(
                "identity_without_shell: the {who} profile's tools leave out shell, which the board's scripts run in"
            ));
        }
        out.insert(identity.clone(), policy);
    }
    Ok(out)
}

type Made = (String, usize, Value);

/// Create these agents at once, each with its row's model and identity.
async fn create(
    client: &Arc<Client>,
    swarm: &Swarm,
    agents: &[(String, usize)],
    policies: &BTreeMap<String, Value>,
    each: u64,
) -> (Vec<Made>, Vec<(String, agent_client::Error)>) {
    let mut creates = tokio::task::JoinSet::new();
    for (i, (name, row)) in agents.iter().enumerate() {
        let mix = &swarm.mix[*row];
        let policy = &policies[&mix.identity];
        let tools = match &policy["tools"] {
            Value::Null => json!(crate::TOOLS),
            tools => tools.clone(),
        };
        let params = json!({
            "bot": name, "workspace": swarm.workspace, "model": mix.model,
            "instructions": policy["instructions"],
            "compaction_instructions": policy["compaction_instructions"],
            "tools": tools, "budget_tokens": each,
        });
        let (client, name, row) = (client.clone(), name.clone(), *row);
        creates.spawn(async move { (i, name, row, client.request("create", params).await) });
    }
    let mut done = Vec::new();
    while let Some(created) = creates.join_next().await {
        match created {
            Ok(created) => done.push(created),
            Err(error) => done.push((
                usize::MAX,
                String::new(),
                0,
                Err(agent_client::Error::new(&error.to_string())),
            )),
        }
    }
    done.sort_by_key(|(i, ..)| *i);
    let (mut made, mut failed) = (Vec::new(), Vec::new());
    for (_, name, row, result) in done {
        match result {
            Ok(record) => made.push((name, row, record)),
            Err(error) => failed.push((name, error)),
        }
    }
    (made, failed)
}

fn pins(made: &[Made]) -> Vec<(String, i64, usize)> {
    made.iter()
        .map(|(name, row, record)| (name.clone(), record["id"].as_i64().unwrap_or(-1), *row))
        .collect()
}

fn reasons(failed: &[(String, agent_client::Error)]) -> Vec<Value> {
    failed
        .iter()
        .map(|(agent, error)| json!({"agent": agent, "error": error.to_string()}))
        .collect()
}

/// Send each new member its brief, as its first message.
async fn brief_all(
    client: &Arc<Client>,
    swarm: &Swarm,
    dir: &Path,
    made: &[Made],
    late: bool,
) -> Vec<(String, agent_client::Error)> {
    let stamp = format!("app-swarm-{}-{}", now_ms(), std::process::id());
    let mut sends = tokio::task::JoinSet::new();
    for (i, (name, _, record)) in made.iter().enumerate() {
        let params = json!({
            "bot": name, "bot_id": record["id"], "request_id": format!("{stamp}-{i}"),
            "prompt": brief(swarm, dir, name, late), "delivery": "reject",
        });
        let (client, name) = (client.clone(), name.clone());
        sends.spawn(async move { (name, client.request("submit", params).await) });
    }
    let mut failed = Vec::new();
    while let Some(sent) = sends.join_next().await {
        if let Ok((name, Err(error))) = sent {
            failed.push((name, error));
        }
    }
    failed
}

/// What an agent is told first: who it is, the goal, the scripts it acts on
/// the board with, and who the others are. How to use them is its role's
/// (`swarm`), so a folder can change it.
fn brief(swarm: &Swarm, dir: &Path, member: &str, late: bool) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    let dir = dir.display();
    let tool = |name: &str, args: &str| {
        let mut title = name.to_owned();
        title[..1].make_ascii_uppercase();
        format!("{title}: {} {args}", quote(&format!("{dir}/{name}")))
    };
    let known = |m: &str| match swarm.row(m) {
        Some(row) if !row.identity.is_empty() => format!("{} ({})", swarm.short(m), row.identity),
        _ => swarm.short(m).to_owned(),
    };
    let others: Vec<String> = (swarm.members.iter())
        .filter(|m| *m != member)
        .map(|m| known(m))
        .collect();
    let mut lines = vec![
        format!(
            "You are {}, one of {} agents in the swarm {}, all working in this folder.",
            swarm.short(member),
            swarm.members.len(),
            swarm.short(&swarm.name)
        ),
        format!("Goal: {}", swarm.goal),
        tool("post", "TEXT"),
        tool("role", "ROLE"),
    ];
    if swarm.council > 0 {
        let seats = swarm.seats();
        lines.push(tool("propose", "STREAM WHY"));
        lines.push(tool("vote", "ID yes|no REASON"));
        lines.push(tool("join", "STREAM"));
        lines.push(format!(
            "Council seats: {}{}",
            seats
                .iter()
                .map(|m| swarm.short(m))
                .collect::<Vec<_>>()
                .join(", "),
            if seats.iter().any(|m| m == member) {
                " (you hold one)"
            } else {
                ""
            }
        ));
    }
    lines.push(format!("Board: {}", quote(&format!("{dir}/board.jsonl"))));
    lines.push(format!(
        "The others: {}",
        if others.is_empty() {
            "none yet".to_owned()
        } else {
            others.join(", ")
        }
    ));
    if late {
        lines.push("You joined after the others started, so read the board first.".into());
    }
    lines.join("\n")
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
    // A swarm you stopped keeps what you decided on the board, and tells
    // nobody; a role or a join tells nobody either.
    let turns = if s.stopped || matches!(act, Act::Role(_) | Act::Join(_)) {
        Vec::new()
    } else {
        running_turns(client, &s).await?
    };
    let running = |m: &str| turns.iter().find(|(n, _)| n == m).and_then(|(_, t)| *t);
    // Each line says how many agents it reached, so what a swarm's posts
    // cost in deliveries is on its board.
    let (sends, mut answer) = locked_with(&mut board, &dir, |state| {
        let (mut lines, notices, answer) = plan(&s, state, bot, act, now_ms())?;
        let mut sends = Vec::new();
        let told = !notices.is_empty() && !s.stopped;
        for notice in notices.into_iter().filter(|_| !s.stopped) {
            let reach = match &notice.audience {
                Audience::Post { scope, text } => {
                    readers(&s, bot, text, scope.as_deref(), &running)
                }
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
            sends.extend(
                reach
                    .into_iter()
                    .map(|(m, r)| (m, r, notice.prompt.clone())),
            );
        }
        if let Some(first) = lines.first_mut().filter(|_| told) {
            first["reached"] = json!(sends.len());
        }
        if let Some(author) = &author {
            for line in lines.iter_mut().filter(|line| line["from"] != "council") {
                line["bot"] = json!(author.bot);
                line["turn"] = json!(author.turn);
            }
        }
        Ok((lines, (sends, answer)))
    })?;
    // Distinct per post, even for two in one millisecond from one process.
    static POSTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = POSTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = format!("swarm-{}-{}-{n}", now_ms(), std::process::id());
    let mut submits = tokio::task::JoinSet::new();
    for (i, (member, reach, prompt)) in sends.into_iter().enumerate() {
        let mut params = json!({
            "bot": member, "bot_id": s.ids.get(&member), "request_id": format!("{stamp}-{i}"),
            "prompt": prompt, "delivery": "steer",
        });
        if let Reach::Running(turn) = reach {
            params["expected_turn"] = json!(turn);
        }
        if let Some(author) = &author {
            params["from"] = json!({"bot": author.bot, "turn": author.turn});
        }
        let client = client.clone();
        submits.spawn(async move { (member, reach, client.request("submit", params).await) });
    }
    let (mut steered, mut woke, mut missed) = (Vec::new(), Vec::new(), Vec::new());
    while let Some(sent) = submits.join_next().await {
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
            budget_tokens: 3_000_000,
            mix: vec![Mix {
                identity: String::new(),
                model: "openai/gpt-6-luna".into(),
                share: 100,
            }],
            members: members.iter().map(|m| m.to_string()).collect(),
            ids: members
                .iter()
                .zip(1..)
                .map(|(m, id)| (m.to_string(), id))
                .collect(),
            rows: members.iter().map(|m| (m.to_string(), 0)).collect(),
            stopped: false,
            council: 0,
        }
    }

    /// Members joining from the mix's first row, as the swarm's JSON says.
    fn enrol(
        root: &Path,
        swarm: &str,
        members: &[(String, i64)],
        added: u64,
    ) -> Result<Value, String> {
        let dir = folder(root, swarm)?;
        let pinned: Vec<_> = members.iter().map(|(m, id)| (m.clone(), *id, 0)).collect();
        join(&dir, &pinned, added).map(|s| s.json(&dir))
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
        enrol(&root, &s.name, &[(agent(1), 1), (agent(2), 2)], 0).unwrap();
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
        let joined = enrol(
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
        let grown = enrol(&root, &s.name, &[("agent.latency-2".into(), 12)], 1_000).unwrap();
        assert_eq!(grown["budget_tokens"], 3_001_000);
        assert!(
            enrol(&root, &s.name, &[("other.x-1".into(), 13)], 0)
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
        update(&dir, |s| {
            s.stopped = true;
            Ok(())
        })
        .unwrap();
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
    fn a_mix_is_rows_of_an_identity_a_model_and_shares_that_make_100() {
        let row = |identity: &str, model: &str, share| Mix {
            identity: identity.into(),
            model: model.into(),
            share,
        };
        assert!(valid_mix(&[row("", "a/x", 60), row("reviewer", "b/y", 40)]).is_ok());
        for (bad, why) in [
            (vec![], "rows"),
            (vec![row("", "a/x", 60)], "add up to 60%"),
            (vec![row("", " ", 100)], "model"),
            (vec![row("", "a/x", 0), row("", "a/x", 100)], "share"),
            (vec![row("../x", "a/x", 100)], "identity"),
            (vec![row("swarm", "a/x", 100)], "identity"),
            (vec![row("coordinator", "a/x", 100)], "identity"),
            ((0..9).map(|_| row("", "a/x", 1)).collect(), "rows"),
        ] {
            let error = valid_mix(&bad).unwrap_err();
            assert!(
                error.starts_with("invalid_mix") && error.contains(why),
                "{error}"
            );
        }
        // A member with no row of the mix makes the swarm unreadable.
        let root = scratch("mix");
        let s = swarm(&[&agent(1)]);
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/app")).unwrap();
        assert_eq!(Swarm::read(&dir).unwrap(), s);
        let good = std::fs::read_to_string(dir.join("swarm.toml")).unwrap();
        for bad in [
            good.replace("\"agent.latency-1\" = 0", "\"agent.latency-1\" = 1"),
            good.replace("share = 100", "share = 90"),
        ] {
            std::fs::write(dir.join("swarm.toml"), bad).unwrap();
            assert!(Swarm::read(&dir).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_brief_names_the_scripts_the_seats_and_the_others_identities() {
        let mut s = council(&[&agent(1), &agent(2), &agent(3), &agent(4)]);
        s.mix.push(Mix {
            identity: "reviewer".into(),
            model: "b/y".into(),
            share: 0,
        });
        s.rows.insert(agent(2), 1);
        let dir = Path::new("/h/.agent/swarms/abc/agent.latency");
        let brief = brief(&s, dir, &agent(1), false);
        assert_eq!(
            brief,
            [
                "You are latency-1, one of 4 agents in the swarm latency, all working in this folder.",
                "Goal: Halve p99.",
                "Post: '/h/.agent/swarms/abc/agent.latency/post' TEXT",
                "Role: '/h/.agent/swarms/abc/agent.latency/role' ROLE",
                "Propose: '/h/.agent/swarms/abc/agent.latency/propose' STREAM WHY",
                "Vote: '/h/.agent/swarms/abc/agent.latency/vote' ID yes|no REASON",
                "Join: '/h/.agent/swarms/abc/agent.latency/join' STREAM",
                "Council seats: latency-1, latency-2, latency-3 (you hold one)",
                "Board: '/h/.agent/swarms/abc/agent.latency/board.jsonl'",
                "The others: latency-2 (reviewer), latency-3, latency-4",
            ]
            .join("\n")
        );
        let late = super::brief(&swarm(&[&agent(1)]), Path::new("/it's"), &agent(1), true);
        assert!(late.contains(r"Post: '/it'\''s/post' TEXT"), "{late}");
        assert!(late.ends_with(
            "The others: none yet\nYou joined after the others started, so read the board first."
        ));
    }

    /// A daemon that answers each request with `answer`, and keeps them.
    struct Fake {
        socket: PathBuf,
        seen: Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    }

    type Answer = dyn Fn(&str, &Value) -> Result<Value, String> + Send + Sync;

    impl Fake {
        fn start(tag: &str, answer: Box<Answer>) -> Self {
            use std::io::BufRead;
            let socket =
                std::env::temp_dir().join(format!("agent-fake-{tag}-{}.sock", std::process::id()));
            let _ = std::fs::remove_file(&socket);
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let kept = seen.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let reader = std::io::BufReader::new(stream.try_clone().unwrap());
                    let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL});
                    writeln!(stream, "{ready}").unwrap();
                    for line in reader.lines() {
                        let Ok(line) = line else { break };
                        let request: Value = serde_json::from_str(&line).unwrap();
                        let op = request["op"].as_str().unwrap().to_owned();
                        kept.lock().unwrap().push((op.clone(), request.clone()));
                        let reply = match answer(&op, &request) {
                            Ok(result) => json!({"id": request["id"], "result": result}),
                            Err(code) => {
                                json!({"id": request["id"], "error": code, "detail": "fake"})
                            }
                        };
                        if writeln!(stream, "{reply}").is_err() {
                            break;
                        }
                    }
                }
            });
            Self { socket, seen }
        }

        fn ops(&self, op: &str) -> Vec<Value> {
            let seen = self.seen.lock().unwrap();
            seen.iter()
                .filter(|(o, _)| o == op)
                .map(|(_, p)| p.clone())
                .collect()
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// A bot record for `create`, numbered from its name.
    fn made(request: &Value) -> Value {
        let name = request["bot"].as_str().unwrap();
        let n: i64 = name.rsplit('-').next().unwrap().parse().unwrap();
        json!({"name": name, "id": 100 + n, "model": request["model"]})
    }

    #[test]
    fn a_swarm_starts_its_agents_from_the_mix_and_one_that_fails_is_reported() {
        let home = scratch("start");
        let (root, project) = (home.join("swarms"), home.join("project"));
        std::fs::create_dir_all(project.join(".agents/agents")).unwrap();
        std::fs::write(
            project.join(".agents/agents/reviewer.md"),
            "---\ndescription: Reviews\ntools: shell, read\n---\nYou review changes.",
        )
        .unwrap();
        let fake = Fake::start(
            "start",
            Box::new(|op, request| match op {
                "create" if request["bot"] == "p.goal-3" || request["bot"] == "p.fail-1" => {
                    Err("provider_unknown".into())
                }
                "create" => Ok(made(request)),
                "submit" => Ok(json!({"status": "running"})),
                _ => Err("unexpected".into()),
            }),
        );
        let rt = runtime();
        let (client, _events) = rt.block_on(Client::connect(&fake.socket)).unwrap();
        let mix = vec![
            Mix {
                identity: String::new(),
                model: "a/x".into(),
                share: 50,
            },
            Mix {
                identity: "reviewer".into(),
                model: "b/y".into(),
                share: 50,
            },
        ];
        let start = |name: &str, rows: Vec<usize>, mix: Vec<Mix>| Start {
            project: "p".into(),
            name: name.into(),
            folder: project.clone(),
            goal: "Ship it.".into(),
            shared: false,
            mix,
            rows,
            budget_tokens: 4_000,
            council: 0,
        };
        let out = rt
            .block_on(super::start(
                &client,
                &root,
                Path::new("/app"),
                start("goal", vec![0, 1, 0, 1], mix.clone()),
            ))
            .unwrap();
        // Each agent has its row's model and identity, and a quarter of the budget.
        let creates = fake.ops("create");
        assert_eq!(creates.len(), 4);
        for create in &creates {
            let reviewer = create["bot"] == "p.goal-2" || create["bot"] == "p.goal-4";
            assert_eq!(create["model"], if reviewer { "b/y" } else { "a/x" });
            assert_eq!(create["budget_tokens"], 1_000);
            assert_eq!(create["workspace"], project.to_str().unwrap());
            let text = create["instructions"].as_str().unwrap();
            assert!(text.contains("You are one of several agents"));
            assert_eq!(
                text.contains("You review changes."),
                reviewer,
                "{}",
                create["bot"]
            );
            // An identity's own text comes first, the swarm's rules after it.
            if reviewer {
                assert!(
                    text.find("You review changes.") < text.find("You are one of several agents")
                );
            }
            assert_eq!(
                create["tools"],
                if reviewer {
                    json!(["shell", "read"])
                } else {
                    json!(crate::TOOLS)
                }
            );
        }
        // The one that failed is reported; the others joined with their rows and got briefs.
        assert_eq!(
            out["failed"],
            json!([{"agent": "p.goal-3", "error": "provider_unknown (fake)"}])
        );
        assert_eq!(
            out["swarm"]["members"],
            json!(["p.goal-1", "p.goal-2", "p.goal-4"])
        );
        assert_eq!(
            out["swarm"]["rows"],
            json!({"p.goal-1": 0, "p.goal-2": 1, "p.goal-4": 1})
        );
        assert_eq!(out["swarm"]["ids"]["p.goal-4"], 104);
        let briefs = fake.ops("submit");
        assert_eq!(briefs.len(), 3);
        let first = briefs.iter().find(|b| b["bot"] == "p.goal-1").unwrap();
        assert_eq!(first["bot_id"], 101);
        assert_eq!(first["delivery"], "reject");
        assert!(
            first["prompt"]
                .as_str()
                .unwrap()
                .contains("The others: goal-2 (reviewer), goal-4")
        );
        // Nothing made at all: nothing is left, not even the name.
        let out = rt.block_on(super::start(
            &client,
            &root,
            Path::new("/app"),
            start("fail", vec![0], mix.clone()),
        ));
        assert_eq!(out.unwrap_err(), "provider_unknown (fake)");
        assert!(!root.join("p.fail").exists());
        // An identity that cannot use the board is refused before any agent is made.
        std::fs::write(
            project.join(".agents/agents/reader.md"),
            "---\ntools: read\n---\nRead.",
        )
        .unwrap();
        let before = fake.ops("create").len();
        let reader = vec![Mix {
            identity: "reader".into(),
            model: "a/x".into(),
            share: 100,
        }];
        let error = rt
            .block_on(super::start(
                &client,
                &root,
                Path::new("/app"),
                start("read", vec![0], reader),
            ))
            .unwrap_err();
        assert!(error.starts_with("identity_without_shell"), "{error}");
        assert_eq!(fake.ops("create").len(), before);
        assert!(!root.join("p.read").exists());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_added_agent_takes_the_next_free_name_and_brings_its_budget() {
        let home = scratch("add");
        let root = home.join("swarms");
        let mut s = swarm(&[&agent(1), &agent(2)]);
        s.workspace = home.to_string_lossy().into_owned();
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/app")).unwrap();
        // latency-3 is some other bot's name already.
        let fake = Fake::start(
            "add",
            Box::new(|op, request| match op {
                "create" if request["bot"] == "agent.latency-3" => Err("bot_exists".into()),
                "create" => Ok(made(request)),
                "submit" => Ok(json!({"status": "running"})),
                _ => Err("unexpected".into()),
            }),
        );
        let rt = runtime();
        let (client, _events) = rt.block_on(Client::connect(&fake.socket)).unwrap();
        let out = rt.block_on(add(&client, &root, &s.name, 0)).unwrap();
        assert_eq!(
            out["swarm"]["members"],
            json!([agent(1), agent(2), agent(4)])
        );
        assert_eq!(out["swarm"]["budget_tokens"], 4_500_000);
        assert_eq!(
            fake.ops("create").last().unwrap()["budget_tokens"],
            1_500_000
        );
        let brief = fake.ops("submit").pop().unwrap();
        assert_eq!(brief["bot"], agent(4));
        assert!(
            brief["prompt"]
                .as_str()
                .unwrap()
                .ends_with("so read the board first.")
        );
        assert!(
            rt.block_on(add(&client, &root, &s.name, 1))
                .unwrap_err()
                .starts_with("invalid_agents")
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn stop_ends_every_unfinished_turn_newest_first_and_a_reused_name_leaves() {
        let root = scratch("stop");
        let s = swarm(&[&agent(1), &agent(2), &agent(3)]);
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/app")).unwrap();
        let fake = Fake::start(
            "stop",
            Box::new(|op, request| {
                let bot = request["bot"].as_str().unwrap_or_default();
                match (op, bot) {
                    // latency-2 was deleted and made again: another bot now.
                    ("resume", "agent.latency-2") => Ok(json!({"id": 99})),
                    ("resume", "agent.latency-3") => Err("bot_not_found".into()),
                    ("resume", _) => Ok(json!({"id": 1})),
                    ("turns", _) if request["after"] == 0 => Ok(json!({
                        "turns": [{"turn": 5, "status": "completed"}, {"turn": 6, "status": "running"}],
                        "next_after": 6,
                    })),
                    ("turns", _) => {
                        Ok(json!({"turns": [{"turn": 7, "status": "queued"}], "next_after": null}))
                    }
                    ("interrupt", _) if request["turn"] == 6 => Err("stale_turn".into()),
                    ("interrupt", _) => Ok(json!({"interrupt_requested": true})),
                    _ => Err("unexpected".into()),
                }
            }),
        );
        let rt = runtime();
        let (client, _events) = rt.block_on(Client::connect(&fake.socket)).unwrap();
        let out = rt.block_on(stop(&client, &root, &s.name)).unwrap();
        assert_eq!(out["failed"], json!([]));
        assert_eq!(out["swarm"]["stopped"], true);
        assert_eq!(out["swarm"]["members"], json!([agent(1)]));
        let ended: Vec<Value> = fake
            .ops("interrupt")
            .iter()
            .map(|i| json!([i["bot"], i["turn"]]))
            .collect();
        assert_eq!(ended, vec![json!([agent(1), 7]), json!([agent(1), 6])]);
        assert!(fake.ops("turns").iter().all(|t| t["bot"] == agent(1)));
        let read = Swarm::read(&dir).unwrap();
        assert!(read.stopped && read.rows.len() == 1 && read.ids.len() == 1);
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
                    enrol(&root, &name, &[(format!("{name}-{i}"), i)], 0).unwrap();
                    update(&root.join(&name), |s| {
                        s.stopped = i % 2 == 0;
                        Ok(())
                    })
                    .unwrap();
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
