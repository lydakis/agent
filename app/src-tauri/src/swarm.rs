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
//! - `post`: the script its agents run to post. It runs this executable with
//!   `--swarm-post`, so posting needs nothing else installed and costs one
//!   daemon connection whatever the swarm's size.
//!
//! A post reaches the others as a steer, so a working agent reads it at its
//! next step. An agent's post goes to the agents working now and wakes only
//! the ones it names with @NAME; your post wakes everyone, or only the ones
//! it names. Agents talking among themselves never wake a swarm that has
//! gone quiet.
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
}

impl Swarm {
    fn read(dir: &Path) -> Result<Self, String> {
        let path = dir.join("swarm.toml");
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
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
        Ok(Self {
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
        })
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
            "stopped": self.stopped,
        })
    }

    /// A member's name without the project, as posts and @names use it.
    fn short<'a>(&self, member: &'a str) -> &'a str {
        member
            .strip_prefix(&self.project)
            .and_then(|rest| rest.strip_prefix('.'))
            .unwrap_or(member)
    }
}

/// `~/.agent/swarms`, which holds a folder of swarms per daemon.
pub fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".agent/swarms"))
        .ok_or_else(|| "no HOME for ~/.agent/swarms".into())
}

/// The swarms of the daemon on `socket`. Their agents are that daemon's
/// bots, so the same names in another store are other bots.
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

pub fn root(socket: &Path) -> Result<PathBuf, String> {
    Ok(home()?.join(daemon_key(socket)))
}

/// FNV-1a over the socket's path: a stable folder name, not a secret.
fn daemon_key(socket: &Path) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in socket.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
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
        append(
            dir,
            &json!({"at": now_ms(), "from": "user", "text": swarm.goal}),
        )?;
        write_script(dir, app)
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

/// The script names this executable and this folder, single-quoted.
fn script(app: &Path, dir: &Path) -> String {
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"));
    format!(
        "#!/bin/sh\n# post TEXT: post to this swarm's board; @NAME wakes that agent.\nexec {} {POST_FLAG} {} \"$@\"\n",
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
        let post = dir.join("post");
        if post.exists()
            && std::fs::read_to_string(&post).is_ok_and(|have| have != script(app, &dir))
        {
            let _ = write_script(&dir, app);
        }
    }
}

/// Replaced whole, so an agent running it meanwhile runs one version or
/// the other, never half a file.
fn write_script(dir: &Path, app: &Path) -> Result<(), String> {
    let temporary = dir.join(format!(".post.{}", std::process::id()));
    replace(
        &temporary,
        &dir.join("post"),
        script(app, dir).as_bytes(),
        0o755,
    )
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

/// The board's complete lines from `offset`, and where the next read
/// starts. With no offset, or one so far behind that the lines between
/// would not be kept, it reads the board's last stretch and says `reset`:
/// the lines replace what the reader has. A line that is not JSON comes
/// back as its text, so a hand edit shows instead of vanishing.
pub fn board(root: &Path, swarm: &str, offset: Option<u64>) -> Result<Value, String> {
    let path = folder(root, swarm)?.join("board.jsonl");
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
    }))
}

/// One line, one write, under an exclusive lock, so posts never interleave.
fn append(dir: &Path, line: &Value) -> Result<(), String> {
    append_to(&mut lock_board(dir)?, line)
}

/// The board open for appending, locked until it is dropped. The lock also
/// guards `swarm.toml`.
fn lock_board(dir: &Path) -> Result<std::fs::File, String> {
    let file = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("board.jsonl"))
        .map_err(|e| e.to_string())?;
    file.lock().map_err(|e| e.to_string())?;
    Ok(file)
}

fn append_to(board: &mut std::fs::File, line: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(line).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    board.write_all(&bytes).map_err(|e| e.to_string())
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

/// Who a post reaches and how, given each member's running turn.
fn readers(
    swarm: &Swarm,
    author: Option<&str>,
    text: &str,
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
        } else {
            running(member).map(Reach::Running)
        };
        if let Some(reach) = reach {
            out.push((member.clone(), reach));
        }
    }
    out
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

/// Put a post on the board, then steer it into the agents it reaches, all
/// at once over one connection.
pub async fn post(
    client: &Arc<Client>,
    root: &Path,
    swarm: &str,
    author: Option<Author>,
    text: &str,
) -> Result<Value, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("post_empty: post TEXT".into());
    }
    if text.len() > MAX_POST {
        return Err(format!(
            "post_too_long: a post is at most {MAX_POST} bytes; write the details to a file in your folder and post its path"
        ));
    }
    let dir = folder(root, swarm)?;
    // Held until every steer is sent. Stop changes `swarm.toml` under this
    // lock, so it either comes first and this post is refused, or waits and
    // then ends whatever turns this post started.
    let mut board = lock_board(&dir)?;
    let mut s = Swarm::read(&dir)?;
    if let Some(author) = &author {
        if s.ids.get(&author.bot) != Some(&author.id) {
            return Err(format!("not_a_member: {} is not in {}", author.bot, s.name));
        }
        if s.stopped {
            return Err("swarm_stopped: the user stopped this swarm; end your turn".into());
        }
    } else if s.stopped {
        s.stopped = false;
        s.write(&dir)?;
    }
    let from = author
        .as_ref()
        .map_or("user", |a| s.short(&a.bot))
        .to_owned();
    let mut line = json!({"at": now_ms(), "from": from, "text": text});
    if let Some(author) = &author {
        line["bot"] = json!(author.bot);
        line["turn"] = json!(author.turn);
    }
    append_to(&mut board, &line)?;
    let turns = running_turns(client, &s).await?;
    let running = |m: &str| turns.iter().find(|(n, _)| n == m).and_then(|(_, t)| *t);
    let readers = readers(&s, author.as_ref().map(|a| a.bot.as_str()), text, &running);
    let prompt = format!("[board] {from}: {text}");
    // Distinct per post, even for two in one millisecond from one process.
    static POSTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = POSTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = format!("swarm-{}-{}-{n}", now_ms(), std::process::id());
    let mut sends = tokio::task::JoinSet::new();
    for (i, (member, reach)) in readers.into_iter().enumerate() {
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
        sends.spawn(async move { (member, reach, client.request("submit", params).await) });
    }
    let (mut steered, mut woke, mut missed) = (Vec::new(), Vec::new(), Vec::new());
    while let Some(sent) = sends.join_next().await {
        let (member, reach, result) = sent.map_err(|e| e.to_string())?;
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
    }
    Ok(json!({"posted": true, "steered": steered, "woke": woke, "missed": missed}))
}

/// `post` run by an agent: `APP --swarm-post DIR TEXT...`. The agent is the
/// bot and turn its shell's environment names; the daemon is the one that
/// shell belongs to.
pub fn cli(args: &[String]) -> i32 {
    let fail = |message: String| {
        eprintln!("{}", json!({"error": message}));
        1
    };
    let Some((dir, words)) = args.split_first() else {
        return fail("usage: post TEXT".into());
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
    let text = words.join(" ");
    let result = runtime.block_on(async {
        let (client, _events) = Client::connect(&socket).await.map_err(|e| e.to_string())?;
        let posted = post(&client, root, swarm, Some(author), &text).await;
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
        let reach = readers(&s, Some("agent.latency-1"), "profile is up", &running);
        assert_eq!(reach, vec![("agent.latency-2".into(), Reach::Running(7))]);
        let reach = readers(
            &s,
            Some("agent.latency-1"),
            "@latency-4 can you take the tests?",
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
        let reach = readers(&s, None, "Keep cargo test green.", &running);
        assert_eq!(reach.len(), 4);
        assert!(reach.iter().all(|(_, r)| *r == Reach::Wake));
        let reach = readers(&s, None, "@agent.latency-3, stop.", &running);
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
            &running,
        );
        assert!(reach.is_empty());
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
    fn each_daemon_has_its_own_swarms() {
        let a = root(Path::new("/tmp/agent-501/v1-a.sock")).unwrap();
        let b = root(Path::new("/tmp/agent-501/v1-b.sock")).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent(), b.parent());
        assert_eq!(a, root(Path::new("/tmp/agent-501/v1-a.sock")).unwrap());
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
