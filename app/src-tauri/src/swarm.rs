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
//!   many agents it was sent to.
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
pub const START_FLAG: &str = "--swarm-start";
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
    /// The highest number an agent of it was made under: the next takes a
    /// higher one, so a name, and what the board says of it, is never
    /// someone else's.
    pub made: usize,
    /// The bot ids of members that left: helpers they made still count in
    /// its tokens, and Stop still ends them.
    pub left: Vec<i64>,
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
            made: table
                .get("made")
                .and_then(toml::Value::as_integer)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| format!("{}: made is missing", path.display()))?,
            left: table
                .get("left")
                .and_then(toml::Value::as_array)
                .and_then(|ids| ids.iter().map(toml::Value::as_integer).collect())
                .ok_or_else(|| format!("{}: left is missing or not bot ids", path.display()))?,
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
        table.insert("made".into(), (self.made as i64).into());
        table.insert(
            "left".into(),
            toml::Value::Array(self.left.iter().map(|id| (*id).into()).collect()),
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
            "goal": self.goal, "workspace": self.workspace, "budget_tokens": self.budget_tokens,
            "mix": self.mix.iter().map(Mix::json).collect::<Vec<_>>(),
            "members": self.members, "ids": self.ids, "rows": self.rows,
            "stopped": self.stopped, "council": self.council, "seats": self.seats(),
            "left": self.left,
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

/// A council has three seats, its first agents, so it needs three agents;
/// none is a flat board.
pub const COUNCIL: usize = 3;

fn valid_council(council: usize, agents: usize) -> Result<(), String> {
    match council {
        0 => Ok(()),
        COUNCIL if agents >= COUNCIL => Ok(()),
        COUNCIL => Err(format!(
            "invalid_council: a council of {COUNCIL} needs at least {COUNCIL} agents"
        )),
        _ => Err(format!("invalid_council: a council has {COUNCIL} seats")),
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
    // The folder's entry, and the store's and swarms' own when they were
    // just made, survive a power loss once the swarm is acknowledged.
    for parent in root.ancestors().take(3) {
        std::fs::File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    Ok(dir)
}

/// A claimed folder's files, with your goal as the board's first post.
/// Nobody is a member yet: the app adds each agent once the daemon has
/// created it. `swarm.toml` comes last, since a folder without it is not
/// listed as a swarm. A folder that cannot be filled goes, name and all.
pub fn fill(dir: &Path, swarm: &Swarm, app: &Path) -> Result<(), String> {
    let made = (|| {
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
        swarm.write(dir)
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
    worktree(&worktrees()?, project, swarm).await
}

/// `place`'s worktree, made under `trees`.
async fn worktree(trees: &Path, project: &Path, swarm: &str) -> Result<String, String> {
    let env = clean_env().await;
    let git = |args: &[&str]| {
        let mut command = git(&env);
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
    let tree = trees.join(swarm);
    let short = format!("agent/{swarm}");
    let branch = format!("refs/heads/{short}");
    // Worktrees and branches are shared by every store: a name another
    // store's swarm holds is taken here too, and the page picks another.
    let exists = || format!("swarm_exists: {swarm} has a worktree or branch already");
    let head = git(&["rev-parse", "--verify", "HEAD^{commit}"])
        .output()
        .await
        .map_err(|e| format!("git: {e}"))?;
    if !head.status.success() {
        return Err(format!("worktree_failed: {}", tail(&head.stderr)));
    }
    let head = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    // The folder and then the branch are each made only where absent: of
    // two starts, or a start and another git client, racing for one name,
    // one makes it and the other hears the name is taken. What a failed
    // add leaves behind is then this start's own to remove.
    std::fs::create_dir_all(trees).map_err(|e| format!("{}: {e}", trees.display()))?;
    match std::fs::create_dir(&tree) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(exists()),
        Err(e) => return Err(format!("{}: {e}", tree.display())),
    }
    let claimed = git(&["update-ref", &branch, &head, ""])
        .output()
        .await
        .map_err(|e| format!("git: {e}"));
    if !claimed.as_ref().is_ok_and(|out| out.status.success()) {
        let _ = std::fs::remove_dir(&tree);
        let taken = git(&["rev-parse", "--verify", "--quiet", &branch])
            .output()
            .await
            .is_ok_and(|out| out.status.success());
        return Err(match claimed {
            _ if taken => exists(),
            Ok(out) => format!("worktree_failed: {}", tail(&out.stderr)),
            Err(error) => error,
        });
    }
    let added = git(&["worktree", "add"])
        .arg(&tree)
        .arg(&short)
        .output()
        .await
        .map_err(|e| format!("git: {e}"));
    if !added.as_ref().is_ok_and(|out| out.status.success()) {
        // A hook can fail after the worktree was made. The branch goes only
        // while it is still where this start put it.
        let _ = git(&["worktree", "remove", "--force"])
            .arg(&tree)
            .output()
            .await;
        let _ = git(&["update-ref", "-d", &branch, &head]).output().await;
        let _ = std::fs::remove_dir_all(&tree);
        return Err(match added {
            Ok(out) => format!("worktree_failed: {}", tail(&out.stderr)),
            Err(error) => error,
        });
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
        command
            .current_dir(&workspace)
            .env_clear()
            .envs(env.iter().cloned())
            .env("AGENT_SOURCE", project);
        let failed = match run_setup(command, SETUP_TIMEOUT).await {
            Ok(()) => None,
            Err(error) => Some(format!("setup_failed: {error}")),
        };
        if let Some(failed) = failed {
            // The branch goes from where its worktree has it, which setup may
            // have moved with a commit.
            let at = git(&["rev-parse", "--verify", "--quiet", &branch])
                .output()
                .await
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned());
            let _ = git(&["worktree", "remove", "--force"])
                .arg(&tree)
                .output()
                .await;
            if let Some(at) = at {
                let _ = git(&["update-ref", "-d", &branch, &at]).output().await;
            }
            return Err(failed);
        }
    }
    Ok(workspace.to_string_lossy().trim_end_matches('/').to_owned())
}

/// Setup is the project's code, run before any agent exists. It gets the
/// login shell's ordinary variables, so its tools are on PATH, and nothing
/// else: no provider or cloud keys, which the daemon's shell withholds too.
/// This process's own environment stands in only when no login shell
/// answers in time.
async fn clean_env() -> Vec<(OsString, OsString)> {
    match crate::daemon::login().await {
        Some(login) => setup_env(login.iter().cloned()),
        None => setup_env(std::env::vars_os()),
    }
}

/// Git with setup's environment: `git worktree add` runs the repository's
/// hooks, which need the same tools and get no provider or cloud keys either.
fn git(env: &[(OsString, OsString)]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command.env_clear().envs(env.iter().cloned());
    command
}

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

/// What joining does to a swarm's budget: a new swarm's is what its agents
/// were made with, and an added agent's share grows it, so the budget shown
/// is always what its agents may spend.
#[derive(Clone, Copy)]
enum Budget {
    Set(u64),
    Add(u64),
}

/// Members join in order and never twice, each pinned to its bot's id and
/// its row of the mix. A stopped swarm takes nobody.
fn join_to(s: &mut Swarm, members: &[(String, i64, usize)], budget: Budget) -> Result<(), String> {
    if s.stopped {
        return Err(
            "swarm_stopped: post to the board to resume the swarm, then add an agent".into(),
        );
    }
    for (member, _, row) in members {
        if !member.starts_with(&format!("{}-", s.name)) {
            return Err(format!(
                "invalid_member: {member} is not named {}-N",
                s.name
            ));
        }
        if *row >= s.mix.len() {
            return Err(format!("invalid_member: the mix has no row {row}"));
        }
    }
    s.budget_tokens = match budget {
        Budget::Set(total) => total,
        Budget::Add(more) => s.budget_tokens.saturating_add(more),
    };
    for (member, id, row) in members {
        if !s.members.contains(member) {
            s.members.push(member.clone());
        }
        s.ids.insert(member.clone(), *id);
        s.rows.insert(member.clone(), *row);
        let number = member.rsplit('-').next().and_then(|n| n.parse().ok());
        s.made = s.made.max(number.unwrap_or(0));
    }
    Ok(())
}

#[cfg(test)]
fn join(dir: &Path, members: &[(String, i64, usize)], added_budget: u64) -> Result<Swarm, String> {
    update(dir, |s| join_to(s, members, Budget::Add(added_budget)))
}

/// New agents join and get their briefs under the board's lock, so Stop,
/// which changes `swarm.toml` under it too, either comes first and they are
/// refused, or waits and then ends the turns their briefs started. The lock
/// is waited for off the async workers, which the briefs need.
async fn enlist(
    client: &Arc<Client>,
    dir: &Path,
    made: &[Made],
    budget: Budget,
    late: bool,
) -> Result<(Swarm, Vec<(String, agent_client::Error)>), String> {
    let _board = lock_board_async(dir).await?;
    let (path, members) = (dir.to_path_buf(), pins(made));
    let joined = tokio::task::spawn_blocking(move || {
        let mut s = Swarm::read(&path)?;
        join_to(&mut s, &members, budget)?;
        s.write(&path)?;
        Ok::<_, String>(s)
    })
    .await
    .map_err(|e| e.to_string())??;
    let failed = brief_all(client, &joined, dir, made, late).await;
    if failed.is_empty() {
        return Ok((joined, failed));
    }
    // An agent whose brief never arrived never learned the goal or the
    // board: it goes, with its share, unless it cannot be deleted, when it
    // stays a member so Stop still reaches it.
    let share = match budget {
        Budget::Set(total) => total / made.len().max(1) as u64,
        Budget::Add(more) => more,
    };
    let mut dropped = Vec::new();
    for (name, _) in &failed {
        if client.request("delete", json!({"bot": name})).await.is_ok() {
            dropped.push(name.clone());
        }
    }
    let path = dir.to_path_buf();
    let gone = dropped.clone();
    let joined = tokio::task::spawn_blocking(move || {
        let mut s = Swarm::read(&path)?;
        let gone = |m: &String| dropped.contains(m);
        s.budget_tokens = (s.budget_tokens)
            .saturating_sub(share * s.members.iter().filter(|m| gone(m)).count() as u64);
        s.members.retain(|m| !gone(m));
        s.ids.retain(|m, _| !gone(m));
        s.rows.retain(|m, _| !gone(m));
        s.write(&path)?;
        Ok::<_, String>(s)
    })
    .await
    .map_err(|e| e.to_string())??;
    // The agents briefed with them among "the others" hear they are not.
    if !gone.is_empty() {
        let left: Vec<&str> = gone.iter().map(|m| joined.short(m)).collect();
        let others: Vec<&str> = joined.members.iter().map(|m| joined.short(m)).collect();
        let prompt = format!(
            "[board] {} did not get the brief and left the swarm; its agents are {}",
            left.join(", "),
            others.join(", ")
        );
        let told = (made.iter()).filter(|(name, _, _)| joined.members.contains(name));
        for (name, _, _) in told {
            let params = json!({
                "bot": name, "bot_id": joined.ids.get(name), "prompt": prompt, "delivery": "steer",
            });
            let _ = client.request("submit", params).await;
        }
    }
    Ok((joined, failed))
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

/// Delete agents no swarm will hold; the ones that could not be deleted,
/// each with why.
async fn strays(client: &Client, made: &[Made]) -> Vec<String> {
    let mut kept = Vec::new();
    for (name, _, _) in made {
        if let Err(error) = client.request("delete", json!({"bot": name})).await {
            kept.push(format!("{name} ({})", error.code));
        }
    }
    kept
}

/// Said, not hidden: a bot that outlived its swarm is the user's to delete.
fn said(mut error: String, kept: &[String]) -> String {
    if !kept.is_empty() {
        error.push_str(&format!(
            "; not deleted, delete by hand: {}",
            kept.join(", ")
        ));
    }
    error
}

/// The records of the agents made that are members: one whose brief
/// failed was deleted and is not shown.
fn kept<'a>(made: &'a [(String, usize, Value)], joined: &Swarm) -> Vec<&'a Value> {
    (made.iter())
        .filter(|(name, _, _)| joined.members.contains(name))
        .map(|(_, _, record)| record)
        .collect()
}

/// A deleted agent leaves: the swarm stops counting it and posting to it.
pub fn leave(root: &Path, swarm: &str, member: &str) -> Result<Value, String> {
    let dir = folder(root, swarm)?;
    Ok(depart(&dir, &[member.to_owned()])?.json(&dir))
}

/// Members leave: their share of the budget goes with them, their helpers
/// still count, and then what the state holds of anyone who is no longer
/// a member goes too, so a change a crash cut short between the two files
/// is finished by the next one.
fn depart(dir: &Path, gone: &[String]) -> Result<Swarm, String> {
    let mut board = lock_board(dir)?;
    let mut s = Swarm::read(dir)?;
    let leaving: Vec<String> = (s.members.iter())
        .filter(|m| gone.contains(m))
        .cloned()
        .collect();
    if !leaving.is_empty() {
        // Every member holds an even share of the budget.
        let share = s.budget_tokens / s.members.len() as u64;
        s.budget_tokens -= share * leaving.len() as u64;
        s.left.extend(leaving.iter().filter_map(|m| s.ids.get(m)));
        s.members.retain(|m| !leaving.contains(m));
        s.ids.retain(|m, _| !leaving.contains(m));
        s.rows.retain(|m, _| !leaving.contains(m));
        s.write(dir)?;
    }
    let current: Vec<String> = (s.members.iter()).map(|m| s.short(m).to_owned()).collect();
    locked_with(&mut board, dir, |state| {
        for name in state.named() {
            if !current.contains(&name) {
                state.forget(&name);
            }
        }
        Ok((vec![], ()))
    })?;
    Ok(s)
}

/// A swarm that never got an agent goes, with the worktree and branch
/// `place` made for it when it made them; a project folder is never removed,
/// wherever it lives.
async fn discard(dir: &Path, swarm: &Swarm, placed: bool) -> Result<(), String> {
    if placed {
        unplace(&swarm.name).await?;
    }
    std::fs::remove_dir_all(dir).map_err(|e| e.to_string())
}

/// Remove a swarm's worktree and its branch, as `place` made them.
pub async fn unplace(swarm: &str) -> Result<(), String> {
    let tree = worktrees()?.join(swarm);
    let env = clean_env().await;
    // The repository the worktree came from, found before it goes.
    let common = git(&env)
        .arg("-C")
        .arg(&tree)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .await
        .map_err(|e| format!("git: {e}"))?;
    let common = String::from_utf8_lossy(&common.stdout).trim().to_owned();
    if !common.is_empty() {
        let git = |args: &[&str]| {
            let mut command = git(&env);
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

/// What a new swarm is. Its name comes from its goal, and its agents are
/// dealt to the rows of its mix (see `deal`).
pub struct Start {
    pub project: String,
    pub folder: PathBuf,
    pub goal: String,
    pub shared: bool,
    pub mix: Vec<Mix>,
    pub agents: usize,
    pub budget_tokens: u64,
    pub council: usize,
}

/// Each agent's row of the mix: dealt one at a time, each to the row
/// furthest below its share of the agents so far, so any prefix of them is
/// as close to the mix as whole agents allow. The council's seats (its first
/// agents) mix too. The page counts the rows with the same rule.
pub fn deal(mix: &[Mix], agents: usize) -> Vec<usize> {
    let mut counts = vec![0i64; mix.len()];
    let mut rows = Vec::with_capacity(agents);
    for k in 1..=agents as i64 {
        let below = |i: usize| i64::from(mix[i].share) * k - 100 * counts[i];
        let best = (0..mix.len()).fold(0, |best, i| if below(i) > below(best) { i } else { best });
        counts[best] += 1;
        rows.push(best);
    }
    rows
}

/// Words that say little about a goal. `lock` too: Git refuses a branch
/// named `agent/PROJECT.lock`.
const PLAIN: [&str; 21] = [
    "the", "and", "for", "with", "that", "this", "from", "into", "make", "keep", "cut", "all",
    "our", "its", "half", "every", "each", "add", "fix", "get", "lock",
];

/// A swarm's name from its goal: the longest of its first few words that
/// say something, or `swarm`.
pub fn goal_name(goal: &str) -> String {
    let lower = goal.to_ascii_lowercase();
    let words = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() >= 3 && !PLAIN.contains(w))
        .take(6);
    let longest = words.fold("", |a, w| if w.len() > a.len() { w } else { a });
    let base = if longest.is_empty() { "swarm" } else { longest };
    base[..base.len().min(24)].to_owned()
}

/// Names a swarm is tried under before the start gives up.
const NAME_TRIES: usize = 9;

/// Whether a bot has this swarm's name or one of its agents' names: a
/// task's worktree, or strays from a swarm that went.
async fn named_already(client: &Client, full: &str) -> Result<bool, String> {
    match client.request("resume", json!({"bot": full})).await {
        Ok(_) => return Ok(true),
        Err(error) if error.code == "bot_not_found" => {}
        Err(error) => return Err(error.to_string()),
    }
    let page = client
        .request("bots", json!({"after": full, "limit": 1}))
        .await
        .map_err(|e| e.to_string())?;
    let next = page["bots"][0]["name"].as_str().unwrap_or_default();
    Ok(next.starts_with(&format!("{full}-")))
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
    let n = start.agents;
    if n == 0 || n > MAX_START {
        return Err(format!(
            "invalid_agents: a swarm starts with 1 to {MAX_START} agents"
        ));
    }
    valid_council(start.council, n)?;
    let rows = deal(&start.mix, n);
    // The goal's name, then -2, -3 ... past names held here or, as a
    // worktree or branch, by another store.
    let base = goal_name(&start.goal);
    let mut tries = 0;
    let (full, dir, workspace) = loop {
        tries += 1;
        let full = match tries {
            1 => format!("{}.{base}", start.project),
            k => format!("{}.{base}-{k}", start.project),
        };
        let taken = |error: &str| error.starts_with("swarm_exists") && tries < NAME_TRIES;
        if named_already(client, &full).await? {
            if tries < NAME_TRIES {
                continue;
            }
            return Err(format!(
                "swarm_exists: {full} and the {} names before it are taken; give the goal other words",
                NAME_TRIES - 1
            ));
        }
        let dir = match claim(root, &full) {
            Ok(dir) => dir,
            Err(error) if taken(&error) => continue,
            Err(error) => return Err(error),
        };
        match place(&start.folder, &full, start.shared).await {
            Ok(workspace) => break (full, dir, workspace),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&dir);
                if !taken(&error) {
                    return Err(error);
                }
            }
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
        made: n,
        left: Vec::new(),
    };
    if let Err(error) = fill(&dir, &swarm, app) {
        if start.shared {
            let _ = unplace(&swarm.name).await;
        }
        return Err(error);
    }
    // A folder whose instructions cannot compose gets no agents.
    let policies = match policies(&swarm, rows.iter().copied()) {
        Ok(policies) => policies,
        Err(error) => {
            let _ = discard(&dir, &swarm, start.shared).await;
            return Err(error);
        }
    };
    let each = (swarm.budget_tokens / n as u64).max(1);
    let agents: Vec<(String, usize)> = (rows.iter().enumerate())
        .map(|(i, row)| (format!("{}-{}", swarm.name, i + 1), *row))
        .collect();
    let (made, mut failed) = create(client, &swarm, &agents, &policies, each).await;
    // A council fills its seats or does not start: fewer agents than seats
    // would change the majority it was started with.
    if made.len() < swarm.council.max(1) {
        let kept = strays(client, &made).await;
        let _ = discard(&dir, &swarm, start.shared).await;
        let why = failed
            .first()
            .map_or_else(String::new, |(_, e)| e.to_string());
        let error = if made.is_empty() {
            why
        } else {
            format!(
                "invalid_council: only {} of the council's {} seats could be made: {why}",
                made.len(),
                swarm.council
            )
        };
        return Err(said(error, &kept));
    }
    // The budget is what the agents made were given, not what was asked.
    let total = each.saturating_mul(made.len() as u64);
    let joined = match enlist(client, &dir, &made, Budget::Set(total), false).await {
        // No agent got its brief: nothing started.
        Ok((joined, briefs)) if joined.members.is_empty() => {
            let _ = discard(&dir, &swarm, start.shared).await;
            return Err(briefs
                .first()
                .map_or_else(String::new, |(_, e)| e.to_string()));
        }
        Ok((joined, briefs)) => {
            failed.extend(briefs);
            joined
        }
        Err(error) => {
            // Agents no swarm holds would be strays: they go with it.
            let kept = strays(client, &made).await;
            let _ = discard(&dir, &swarm, start.shared).await;
            return Err(said(error, &kept));
        }
    };
    Ok(json!({
        "swarm": joined.json(&dir),
        "bots": kept(&made, &joined),
        "failed": reasons(&failed),
    }))
}

/// One more agent from `row` of the mix, under a number no agent of the
/// swarm ever had. Its budget adds to the swarm's.
pub async fn add(
    client: &Arc<Client>,
    root: &Path,
    swarm: &str,
    row: usize,
) -> Result<Value, String> {
    let dir = folder(root, swarm)?;
    let s = Swarm::read(&dir)?;
    if s.stopped {
        return Err(
            "swarm_stopped: post to the board to resume the swarm, then add an agent".into(),
        );
    }
    if row >= s.mix.len() {
        return Err(format!("invalid_agents: the mix has no row {row}"));
    }
    // Its agents' share of the budget went with them.
    if s.members.is_empty() {
        return Err("swarm_empty: every agent left this swarm; start a new one".into());
    }
    let policies = policies(&s, std::iter::once(row))?;
    let each = s.budget_tokens / s.members.len() as u64;
    let mut n = s.made + 1;
    let made = loop {
        let name = format!("{}-{n}", s.name);
        n += 1;
        let (made, failed) = create(client, &s, &[(name, row)], &policies, each).await;
        match failed.into_iter().next() {
            // A bot of that name that is not a member: take the next number.
            Some((_, error)) if error.code == "bot_exists" && n <= s.made + 64 => {}
            Some((_, error)) => return Err(error.to_string()),
            None => break made,
        }
    };
    let (joined, failed) = match enlist(client, &dir, &made, Budget::Add(each), true).await {
        Ok(enlisted) => enlisted,
        Err(error) => {
            // Stopped meanwhile: the agent it was made for would be a stray.
            let kept = strays(client, &made).await;
            return Err(said(error, &kept));
        }
    };
    Ok(json!({
        "swarm": joined.json(&dir),
        "bots": kept(&made, &joined),
        "failed": reasons(&failed),
    }))
}

/// Stop a swarm: it refuses its agents' posts from now on, and every turn
/// its agents and their helpers have not finished ends. Posts wait on the board's lock, so a
/// post either comes first and its turns end here, or is refused. A name
/// that is no longer its member's bot leaves, its turns untouched.
pub async fn stop(client: &Arc<Client>, root: &Path, swarm: &str) -> Result<Value, String> {
    let dir = folder(root, swarm)?;
    let s = update_async(&dir, |s| {
        s.stopped = true;
        Ok(())
    })
    .await?;
    let (mut gone, mut failed) = (Vec::new(), Vec::new());
    // Members first, then the helpers they made, looked for again after
    // each round: a member or helper whose turn was still running could make
    // one more before it ended, and that one stops too.
    let mut next: Vec<(String, Option<i64>, bool)> = (s.members.iter())
        .map(|m| (m.clone(), s.ids.get(m).copied(), true))
        .collect();
    let mut ended = std::collections::BTreeSet::new();
    for _ in 0..HELPER_ROUNDS {
        let mut ends = tokio::task::JoinSet::new();
        for (bot, id, member) in next.drain(..) {
            ended.insert(bot.clone());
            let client = client.clone();
            ends.spawn(async move {
                let result = end_turns(&client, &bot, id).await;
                (bot, member, result)
            });
        }
        while let Some(result) = ends.join_next().await {
            match result.map_err(|e| e.to_string())? {
                (member, true, Ok(false)) => gone.push(member),
                (_, _, Ok(_)) => {}
                (bot, _, Err(error)) => {
                    failed.push(json!({"agent": s.short(&bot), "error": error}))
                }
            }
        }
        // A member given a turn meanwhile, from another window, is ended
        // again; a helper once.
        match scan(client, &s).await {
            Ok(seen) => {
                next.extend(
                    (seen.turns.into_iter())
                        .filter(|(_, turn)| turn.is_some())
                        .map(|(m, _)| (m.clone(), s.ids.get(&m).copied(), true)),
                );
                next.extend(
                    (seen.helpers.into_iter())
                        .filter(|(h, _)| !ended.contains(h))
                        .map(|(h, id)| (h, Some(id), false)),
                );
            }
            Err(error) => failed.push(json!({"agent": "helpers", "error": error})),
        }
        if next.is_empty() {
            break;
        }
    }
    if !next.is_empty() {
        failed.push(
            json!({"agent": "helpers", "error": "turns or helpers were still being started"}),
        );
    }
    let s = if gone.is_empty() {
        s
    } else {
        let at = dir.clone();
        tokio::task::spawn_blocking(move || depart(&at, &gone))
            .await
            .map_err(|e| e.to_string())??
    };
    Ok(json!({"swarm": s.json(&dir), "failed": failed}))
}

/// Rounds of looking for helpers Stop takes before it gives up on them.
const HELPER_ROUNDS: usize = 8;

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
    let path = dir.join("board.jsonl");
    let mut file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    // The state and the lines are read under a shared lock, which no change
    // holds while it writes them, so they always agree. A change a crash cut
    // short is settled first.
    loop {
        file.lock_shared().map_err(|e| e.to_string())?;
        if !dir.join(PENDING).exists() {
            break;
        }
        file.unlock().map_err(|e| e.to_string())?;
        recover(&lock_board(&dir)?, &dir)?;
    }
    let state = State::read(&dir)?;
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
    /// Open proposals (at most `MAX_OPEN`), approved ones whose stream
    /// someone is in, and the last few denied ones; a decided proposal's
    /// vote reasons are on the board only.
    pub proposals: Vec<Proposal>,
    /// Proposals ever made, which numbers the next one.
    pub made: u32,
    /// The last share of the budget the board announced, in percent.
    pub spent: u8,
    /// Each helper's tokens as the last scan saw them, by bot id: a helper
    /// deleted since keeps counting with what it had used by then.
    pub helpers: BTreeMap<i64, u64>,
    /// Tokens used by helpers that are gone.
    pub gone: u64,
}

/// Denied proposals kept in the state; older ones are on the board only.
const KEEP_DENIED: usize = 16;

/// Proposals open at once: more wait until the council decides some.
const MAX_OPEN: usize = 16;

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
                    made: value["made"]
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .unwrap_or(0),
                    spent: value["spent"]
                        .as_u64()
                        .and_then(|n| u8::try_from(n).ok())
                        .unwrap_or(0),
                    helpers: (value["helpers"].as_object().into_iter().flatten())
                        .filter_map(|(id, used)| Some((id.parse().ok()?, used.as_u64()?)))
                        .collect(),
                    gone: value["gone"].as_u64().unwrap_or(0),
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
            "made": self.made, "spent": self.spent, "helpers": self.helpers, "gone": self.gone,
        })
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

    /// Every agent the state says something of.
    fn named(&self) -> std::collections::BTreeSet<String> {
        let open = self.proposals.iter().filter(|p| p.status == "open");
        (self.roles.keys().chain(self.streams.keys()).cloned())
            .chain(open.flat_map(|p| std::iter::once(p.by.clone()).chain(p.votes.keys().cloned())))
            .filter(|name| name != "user")
            .collect()
    }

    /// A member that left takes its role, its place in a stream, its votes
    /// on open proposals and the open proposals it made with it; the board
    /// keeps all of them.
    fn forget(&mut self, member: &str) {
        self.roles.remove(member);
        self.streams.remove(member);
        self.proposals
            .retain(|p| p.status != "open" || p.by != member);
        for p in self.proposals.iter_mut().filter(|p| p.status == "open") {
            p.votes.remove(member);
        }
        self.prune();
    }

    /// What the state keeps stays bounded: an approved stream nobody is in
    /// any more is closed, and only the last few denied proposals stay. Both
    /// remain on the board.
    fn prune(&mut self) {
        let denied = (self.proposals.iter())
            .filter(|p| p.status == "denied")
            .count();
        let mut drop = denied.saturating_sub(KEEP_DENIED);
        let streams = &self.streams;
        self.proposals.retain(|p| match p.status.as_str() {
            "approved" => streams.values().any(|st| *st == p.stream),
            "denied" if drop > 0 => {
                drop -= 1;
                false
            }
            _ => true,
        });
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
    recover(board, dir)?;
    let before = State::read(dir)?;
    let mut state = before.clone();
    let (lines, out) = change(&mut state)?;
    let mut bytes = Vec::new();
    for line in &lines {
        serde_json::to_writer(&mut bytes, line).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
    }
    // The lines and the state they change commit together: the new state
    // waits beside the old one, naming the board's length before and after
    // the lines, until they are on the board (see `recover`).
    let from = board.metadata().map_err(|e| e.to_string())?.len();
    let changed = state != before;
    if changed {
        // The state as it will be read, with where its lines go on the board.
        let mut pending = state.json();
        pending["from"] = json!(from);
        pending["to"] = json!(from + bytes.len() as u64);
        let bytes = serde_json::to_vec(&pending).map_err(|e| e.to_string())?;
        let temporary = dir.join(format!(".{PENDING}.{}", std::process::id()));
        replace(&temporary, &dir.join(PENDING), &bytes, 0o644)?;
    }
    board.write_all(&bytes).map_err(|e| e.to_string())?;
    // Synced before anyone is told, so a post an agent heard survives a
    // power loss on the board too.
    board.sync_data().map_err(|e| e.to_string())?;
    if changed {
        commit(dir)?;
    }
    Ok(out)
}

/// A change to the state not yet known to be on the board.
const PENDING: &str = "state.pending.json";

fn commit(dir: &Path) -> Result<(), String> {
    std::fs::rename(dir.join(PENDING), dir.join("state.json"))
        .and_then(|()| std::fs::File::open(dir)?.sync_all())
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// Under the board's lock, finish what a write cut short: when the board
/// holds all of a pending change's lines, its state becomes the state;
/// otherwise the board goes back to its length before them, dropping any
/// part of a line, and the change is forgotten, as if never made.
fn recover(board: &std::fs::File, dir: &Path) -> Result<(), String> {
    let path = dir.join(PENDING);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let pending: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let length = board.metadata().map_err(|e| e.to_string())?.len();
    match (pending["from"].as_u64(), pending["to"].as_u64()) {
        (_, Some(to)) if length == to => commit(dir),
        (Some(from), Some(to)) if (from..to).contains(&length) => {
            board.set_len(from).map_err(|e| e.to_string())?;
            board.sync_data().map_err(|e| e.to_string())?;
            std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))
        }
        _ => Err(format!(
            "{}: does not match the board; its lines and state disagree",
            path.display()
        )),
    }
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
    /// Nothing but the budget check every act ends with.
    Check,
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
            let open = (state.proposals.iter())
                .filter(|p| p.status == "open")
                .count();
            if open >= MAX_OPEN {
                return Err(format!(
                    "too_many_open: {open} proposals are open; vote on them before proposing more"
                ));
            }
            state.made += 1;
            let id = format!("P{}", state.made);
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
            // The council's size, not the seats filled now: a council short
            // of seats needs the same majority, and waits for an added agent
            // or for you.
            let seats = swarm.council;
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
            // Only the seats as they are now count: a seat that left gives
            // its place, and its vote, to the next agent.
            let current: Vec<String> = (swarm.seats().iter())
                .map(|m| swarm.short(m).to_owned())
                .collect();
            let counted = || (proposal.votes.iter()).filter(|(seat, _)| current.contains(seat));
            let ayes = counted().filter(|(_, (yes, _))| *yes).count();
            let noes = counted().count() - ayes;
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
                // Its reasons are on the board; the state keeps the tally.
                for (_, reason) in proposal.votes.values_mut() {
                    reason.clear();
                }
                // Its lead is in it, unless the lead has left; a stream
                // nobody is in closes.
                if approved && full(&by).is_some() {
                    state.streams.insert(by.clone(), stream.clone());
                }
                state.prune();
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
            // The stream it left closes if nobody else is in it.
            state.prune();
            Ok((
                vec![line(json!({"kind": "join", "stream": stream}))],
                vec![],
                json!({"stream": stream}),
            ))
        }
        Act::Check => Ok((vec![], vec![], json!({}))),
    }
}

/// The shares of its budget a swarm is told it has spent.
const SPENT: [u8; 3] = [50, 75, 90];

/// A line for the highest share of the budget `used` has passed that the
/// board has not announced yet: once each, however many windows check.
fn spent(swarm: &Swarm, state: &mut State, used: u64, at: u64) -> Option<(Value, Notice)> {
    let share = u128::from(used) * 100 / u128::from(swarm.budget_tokens.max(1));
    let passed = SPENT.into_iter().rfind(|p| u128::from(*p) <= share)?;
    if passed <= state.spent {
        return None;
    }
    state.spent = passed;
    let text = format!(
        "the swarm has used {passed}% of its budget ({} of {} tokens)",
        millions(used),
        millions(swarm.budget_tokens)
    );
    let line = json!({"at": at, "from": "budget", "text": text, "spent": passed});
    let notice = Notice {
        prompt: format!("[board] budget: {text}"),
        audience: Audience::Wake {
            who: Vec::new(),
            working: true,
        },
    };
    Some((line, notice))
}

fn millions(tokens: u64) -> String {
    let m = tokens as f64 / 1e6;
    if m >= 10.0 {
        format!("{m:.0}M")
    } else {
        format!("{m:.1}M").replace(".0M", "M")
    }
}

/// What the daemon's list says of a swarm: each member's running turn, the
/// helpers its members made, and the tokens all of them used. Members are
/// named `SWARM-N` and a helper is named after the agent that made it
/// (`SWARM-N.WHAT`), so they sit together in its name order. A bot under a
/// member's name that is not the member's bot is left out, and so is a bot
/// in that range no member or helper made.
#[derive(Debug, Default)]
struct Scan {
    turns: Vec<(String, Option<i64>)>,
    helpers: Vec<(String, i64)>,
    used: u64,
    /// Whether the daemon was asked at all.
    listed: bool,
    /// Each helper's tokens, by bot id.
    spent: BTreeMap<i64, u64>,
    /// The members that left and still have a helper, or a helper's helper.
    rooted: std::collections::HashSet<i64>,
}

impl Scan {
    /// Tokens used by the swarm's agents and its helpers, counting helpers
    /// that are gone, once the state has taken this scan in.
    fn used(&self, state: &State) -> u64 {
        let gone: u64 = (state.helpers.iter())
            .filter(|(id, _)| !self.spent.contains_key(id))
            .map(|(_, used)| used)
            .sum();
        self.used + state.gone + gone
    }

    /// Keep what this scan saw of helpers: one gone since the last scan
    /// counts from now on with what it had used when last seen.
    fn record(&self, state: &mut State) {
        if !self.listed {
            return;
        }
        let spent = &self.spent;
        let mut gone = 0;
        state.helpers.retain(|id, used| {
            let kept = spent.contains_key(id);
            if !kept {
                gone += *used;
            }
            kept
        });
        state.gone = state.gone.saturating_add(gone);
        for (id, used) in spent {
            let at = state.helpers.entry(*id).or_default();
            *at = (*at).max(*used);
        }
    }
}

async fn scan(client: &Client, swarm: &Swarm) -> Result<Scan, String> {
    let prefix = format!("{}-", swarm.name);
    let mut after = swarm.name.clone();
    let mut out = Scan {
        listed: true,
        ..Scan::default()
    };
    // A helper's maker sorts before it, so one pass finds helpers' helpers;
    // each maker is kept with the member, or member that left, it comes from.
    let mut makers: std::collections::HashMap<i64, i64> = (swarm.ids.values().chain(&swarm.left))
        .map(|id| (*id, *id))
        .collect();
    loop {
        let page = client
            .request("bots", json!({"after": after, "limit": 256}))
            .await
            .map_err(|e| e.to_string())?;
        for bot in page["bots"].as_array().into_iter().flatten() {
            let name = bot["name"].as_str().unwrap_or_default();
            if name > prefix.as_str() && !name.starts_with(&prefix) {
                return Ok(out);
            }
            let (Some(id), used) = (bot["id"].as_i64(), bot["tokens_used"].as_u64()) else {
                continue;
            };
            if swarm.members.iter().any(|m| m == name) {
                if swarm.ids.get(name) == Some(&id) {
                    out.turns
                        .push((name.to_owned(), bot["running_turn"].as_i64()));
                    out.used += used.unwrap_or(0);
                }
            } else if let Some(root) = (bot["created_by_id"].as_i64())
                .and_then(|by| makers.get(&by))
                .copied()
            {
                makers.insert(id, root);
                out.rooted.insert(root);
                out.helpers.push((name.to_owned(), id));
                out.spent.insert(id, used.unwrap_or(0));
                out.used += used.unwrap_or(0);
            }
        }
        match page["next_after"].as_str() {
            Some(next) => after = next.to_owned(),
            None => return Ok(out),
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
    }
    // Your post resumes a stopped swarm, once it is on the board.
    let resumed = author.is_none() && s.stopped && matches!(act, Act::Post { .. });
    if resumed {
        // One the board would refuse leaves it stopped.
        plan(&s, &mut State::read(&dir)?, None, act.clone(), now_ms())?;
    }
    s.stopped &= !resumed;
    let bot = author.as_ref().map(|a| a.bot.as_str());
    // A swarm you stopped keeps what you decided on the board, and tells
    // nobody; a role or a join tells nobody either.
    let seen = if s.stopped || matches!(act, Act::Role(_) | Act::Join(_)) {
        Scan::default()
    } else {
        scan(client, &s).await?
    };
    // A member that left stops being a maker once nothing it made is left.
    let rooted = &seen.rooted;
    let before = s.left.len();
    if seen.listed {
        s.left.retain(|id| rooted.contains(id));
    }
    // The swarm resumes before your post goes on the board: a failure or a
    // crash between leaves it running with nothing new to do until you post
    // again, never your post on the board and the swarm still stopped.
    if resumed || s.left.len() != before {
        s.write(&dir)?;
    }
    let turns = &seen.turns;
    let running = |m: &str| turns.iter().find(|(n, _)| n == m).and_then(|(_, t)| *t);
    // Each line says how many agents it is sent to, so what a swarm's posts
    // cost in deliveries is on its board. The line is written before the
    // sends; a send that failed, or found its turn over, is in the answer.
    let (sends, mut answer) = locked_with(&mut board, &dir, |state| {
        seen.record(state);
        let checked = !matches!(act, Act::Role(_) | Act::Join(_));
        let (mut lines, notices, mut answer) = plan(&s, state, bot, act, now_ms())?;
        let reach = |notice: &Notice| -> Vec<(String, Reach)> {
            match &notice.audience {
                Audience::Post { scope, text } => {
                    readers(&s, bot, text, scope.as_deref(), &running)
                }
                Audience::Wake { who, working } => (s.members.iter())
                    .filter(|m| Some(m.as_str()) != bot)
                    .filter_map(|m| match (who.contains(m), running(m)) {
                        (true, _) => Some((m.clone(), Reach::Wake)),
                        (false, Some(turn)) if *working => Some((m.clone(), Reach::Running(turn))),
                        _ => None,
                    })
                    .collect(),
            }
        };
        // Each send: to whom, how, what, and whether the author sent it.
        let mut sends = Vec::new();
        let told = !notices.is_empty() && !s.stopped;
        for notice in notices.iter().filter(|_| !s.stopped) {
            let to = reach(notice).into_iter();
            sends.extend(to.map(|(m, r)| (m, r, notice.prompt.clone(), true)));
        }
        if let Some(first) = lines.first_mut().filter(|_| told) {
            first["sent"] = json!(sends.len());
        }
        if let Some(author) = &author {
            for line in lines.iter_mut().filter(|line| line["from"] != "council") {
                line["bot"] = json!(author.bot);
                line["turn"] = json!(author.turn);
            }
        }
        // Every act that asked the daemon also tells the working agents when
        // the swarm passes a share of its budget; the author hears it in its
        // answer.
        if checked
            && !s.stopped
            && let Some((mut line, notice)) = spent(&s, state, seen.used(state), now_ms())
        {
            let to: Vec<_> = reach(&notice).into_iter().collect();
            line["sent"] = json!(to.len());
            answer["budget"] = line["text"].clone();
            lines.push(line);
            sends.extend(
                to.into_iter()
                    .map(|(m, r)| (m, r, notice.prompt.clone(), false)),
            );
        }
        Ok((lines, (sends, answer)))
    })?;
    // Distinct per post, even for two in one millisecond from one process.
    static POSTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = POSTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = format!("swarm-{}-{}-{n}", now_ms(), std::process::id());
    let mut submits = tokio::task::JoinSet::new();
    for (i, (member, reach, prompt, authored)) in sends.into_iter().enumerate() {
        let mut params = json!({
            "bot": member, "bot_id": s.ids.get(&member), "request_id": format!("{stamp}-{i}"),
            "prompt": prompt, "delivery": "steer",
        });
        if let Reach::Running(turn) = reach {
            params["expected_turn"] = json!(turn);
        }
        if let Some(author) = author.as_ref().filter(|_| authored) {
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

/// Set in the start's own process, which runs apart from the shell's group.
const ALONE: &str = "AGENT_SWARM_START_ALONE";

/// What `start` takes from a coordinator's shell, besides the goal after `--`.
const START_USAGE: &str = "usage: start [--agents N] [--budget MILLIONS] [--council 3] [--in-project] [--row MODEL,SHARE[,IDENTITY]]... -- GOAL";

/// Agents, budget in millions of tokens and organization when not given:
/// what the page's sheet offers first.
const START_AGENTS: usize = 4;
const START_BUDGET_M: f64 = 3.0;
const MAX_BUDGET_M: f64 = 1000.0;

/// A swarm as a coordinator asks for one. Without rows, every agent runs
/// the coordinator's own model as a plain agent.
#[derive(Debug, PartialEq)]
struct Asked {
    agents: usize,
    budget_tokens: u64,
    council: usize,
    shared: bool,
    mix: Vec<Mix>,
    goal: String,
}

fn parse_start(args: &[String]) -> Result<Asked, String> {
    let bad = |what: String| format!("{what}\n{START_USAGE}");
    let (flags, goal) = match args.iter().position(|a| a == "--") {
        Some(at) => (&args[..at], args[at + 1..].join(" ")),
        None => return Err(bad("invalid_start: the goal follows --".into())),
    };
    let mut asked = Asked {
        agents: START_AGENTS,
        budget_tokens: (START_BUDGET_M * 1e6) as u64,
        council: 0,
        shared: true,
        mix: Vec::new(),
        goal,
    };
    let mut flags = flags.iter();
    while let Some(flag) = flags.next() {
        let mut value = || {
            flags
                .next()
                .cloned()
                .ok_or_else(|| bad(format!("invalid_start: {flag} needs a value")))
        };
        match flag.as_str() {
            "--agents" => {
                asked.agents = value()?
                    .parse()
                    .map_err(|_| bad("invalid_agents: --agents is a whole number".into()))?;
            }
            "--budget" => {
                let m: f64 = value()?.parse().unwrap_or(f64::NAN);
                if !(0.1..=MAX_BUDGET_M).contains(&m) {
                    return Err(bad(format!(
                        "invalid_budget: --budget is 0.1 to {MAX_BUDGET_M} million tokens"
                    )));
                }
                asked.budget_tokens = (m * 1e6).round() as u64;
            }
            "--council" => {
                asked.council = match value()?.as_str() {
                    "3" => 3,
                    _ => return Err(bad("invalid_council: a council has 3 seats".into())),
                };
            }
            "--in-project" => asked.shared = false,
            "--row" => {
                let row = value()?;
                let mut parts = row.splitn(3, ',');
                let (model, share, identity) = (parts.next(), parts.next(), parts.next());
                let share = share.and_then(|s| s.trim().trim_end_matches('%').parse().ok());
                let (Some(model), Some(share)) = (model, share) else {
                    return Err(bad(format!(
                        "invalid_mix: {row} is not MODEL,SHARE[,IDENTITY]"
                    )));
                };
                asked.mix.push(Mix {
                    identity: identity.unwrap_or_default().trim().to_owned(),
                    model: model.trim().to_owned(),
                    share,
                });
            }
            _ => return Err(bad(format!("invalid_start: {flag}"))),
        }
    }
    Ok(asked)
}

/// `APP --swarm-start ...`, run by a project's coordinator (`PROJECT.lead`)
/// from its shell: a swarm in its project, started as the page starts one,
/// working in its folder or a worktree of it. Prints the swarm and its
/// board's path.
pub fn start_cli(args: &[String]) -> i32 {
    let fail = |message: String| {
        eprintln!("{}", json!({"error": message}));
        1
    };
    let asked = match parse_start(args) {
        Ok(asked) => asked,
        Err(error) => return fail(error),
    };
    // A shell that gives up on the start, at its timeout or when its turn
    // ends, kills its process group. The start runs in a group of its own,
    // so it always finishes or undoes what it made, and this process relays
    // its answer while the shell waits.
    if std::env::var_os(ALONE).is_none() {
        use std::os::unix::process::CommandExt;
        let status = std::env::current_exe().and_then(|app| {
            std::process::Command::new(app)
                .arg(START_FLAG)
                .args(args)
                .env(ALONE, "1")
                .stdin(std::process::Stdio::null())
                .process_group(0)
                .status()
        });
        return match status {
            Ok(status) => status.code().unwrap_or(1),
            Err(error) => fail(error.to_string()),
        };
    }
    let lead = std::env::var("AGENT_BOT").unwrap_or_default();
    let id = std::env::var("AGENT_BOT_ID")
        .ok()
        .and_then(|v| v.parse::<i64>().ok());
    let (Some(project), Some(id)) = (lead.strip_suffix(".lead"), id) else {
        return fail("start runs in a project coordinator's shell, which names AGENT_BOT (PROJECT.lead) and AGENT_BOT_ID".into());
    };
    let (socket, app) = match (socket(), std::env::current_exe()) {
        (Ok(socket), Ok(app)) => (socket, app),
        (Err(error), _) => return fail(error),
        (_, Err(error)) => return fail(error.to_string()),
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
        let client = Arc::new(client);
        let started = async {
            let root = root(client.store().ok_or("the daemon named no store")?)?;
            let me = client
                .request("resume", json!({"bot": lead}))
                .await
                .map_err(|e| e.to_string())?;
            let (Some(folder), Some(provider), Some(model)) = (
                me["workspace"].as_str(),
                me["provider"].as_str(),
                me["model"].as_str(),
            ) else {
                return Err(format!("{lead} has no folder or model"));
            };
            if me["id"].as_i64() != Some(id) {
                return Err(format!("{lead} is not this shell's bot any more"));
            }
            // This turn's model, which the shell names, else the bot's own.
            let model = std::env::var("AGENT_MODEL")
                .ok()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| format!("{provider}/{model}"));
            let mix = if asked.mix.is_empty() {
                vec![Mix {
                    identity: String::new(),
                    model,
                    share: 100,
                }]
            } else {
                asked.mix
            };
            let start = Start {
                project: project.to_owned(),
                folder: folder.into(),
                goal: asked.goal,
                shared: asked.shared,
                mix,
                agents: asked.agents,
                budget_tokens: asked.budget_tokens,
                council: asked.council,
            };
            let mut out = self::start(&client, &root, &app, start).await?;
            let dir = out["swarm"]["dir"]
                .as_str()
                .map(|d| Path::new(d).join("board.jsonl"));
            out["board"] = json!(dir);
            // The records are the page's to seat; the coordinator needs names.
            let names: Vec<Value> = (out["bots"].as_array().into_iter().flatten())
                .map(|b| b["name"].clone())
                .collect();
            out["bots"] = json!(names);
            Ok(out)
        }
        .await;
        client.close().await;
        started
    });
    match result {
        Ok(value) => {
            println!("{value}");
            0
        }
        Err(error) => fail(error),
    }
}

/// The script a coordinator runs, `~/.agent/swarms/start`, written again
/// whenever the app starts from somewhere else.
pub fn write_start_script(home: &Path, app: &Path) -> Result<(), String> {
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"));
    let text = format!(
        "#!/bin/sh\n# {START_USAGE}\nexec {} {START_FLAG} \"$@\"\n",
        quote(app)
    );
    let path = home.join("start");
    if std::fs::read_to_string(&path).is_ok_and(|have| have == text) {
        return Ok(());
    }
    std::fs::create_dir_all(home).map_err(|e| format!("{}: {e}", home.display()))?;
    let temporary = home.join(format!(".start.{}", std::process::id()));
    replace(&temporary, &path, text.as_bytes(), 0o755)
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
            made: members.len(),
            left: Vec::new(),
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
    fn only_the_seats_as_they_are_now_decide_and_old_denials_leave_the_state() {
        let mut s = council(&[
            "agent.latency-1",
            "agent.latency-2",
            "agent.latency-3",
            "agent.latency-4",
        ]);
        let mut state = State::default();
        let propose = |stream: &str| Act::Propose {
            stream: stream.into(),
            why: "why".into(),
        };
        plan(&s, &mut state, Some(&agent(4)), propose("fsync"), 1).unwrap();
        let yes = || Act::Vote {
            id: "P1".into(),
            yes: true,
            reason: "measured".into(),
        };
        plan(&s, &mut state, Some(&agent(1)), yes(), 2).unwrap();
        // latency-1 leaves; latency-4 takes its seat, and latency-1's yes no longer counts.
        s.members.remove(0);
        let (_, _, answer) = plan(&s, &mut state, Some(&agent(2)), yes(), 3).unwrap();
        assert_eq!(answer["decided"], Value::Null);
        let (_, _, answer) = plan(&s, &mut state, Some(&agent(3)), yes(), 4).unwrap();
        assert_eq!(answer["decided"], "approved");
        // Decided, its reasons are on the board only.
        assert!(
            state.proposals[0]
                .votes
                .values()
                .all(|(_, reason)| reason.is_empty())
        );
        // Denied proposals beyond the last few leave the state; numbers are never reused.
        for k in 0..KEEP_DENIED + 3 {
            let (_, _, made) = plan(&s, &mut state, Some(&agent(4)), propose("idea"), 5).unwrap();
            let id = made["id"].as_str().unwrap().to_owned();
            assert_eq!(id, format!("P{}", k + 2));
            plan(
                &s,
                &mut state,
                None,
                Act::Vote {
                    id,
                    yes: false,
                    reason: String::new(),
                },
                6,
            )
            .unwrap();
        }
        let denied = state
            .proposals
            .iter()
            .filter(|p| p.status == "denied")
            .count();
        assert_eq!(denied, KEEP_DENIED);
        assert_eq!(
            state.proposals[0].stream, "fsync",
            "an approved stream stays"
        );
        assert_eq!(
            state.proposals.last().unwrap().id,
            format!("P{}", KEEP_DENIED + 4)
        );
        // At most a few proposals are open at once.
        for k in 0..MAX_OPEN {
            plan(
                &s,
                &mut state,
                Some(&agent(2)),
                propose(&format!("s{k}")),
                7,
            )
            .unwrap();
        }
        let refused = plan(&s, &mut state, Some(&agent(2)), propose("more"), 8).unwrap_err();
        assert!(refused.starts_with("too_many_open"), "{refused}");
        let open = state.proposals.iter().find(|p| p.status == "open").unwrap();
        let (id, reason) = (open.id.clone(), "later".to_owned());
        plan(
            &s,
            &mut state,
            Some(&agent(4)),
            Act::Vote {
                id,
                yes: true,
                reason,
            },
            9,
        )
        .unwrap();
        // A council down to one seat needs the same two votes: one yes or one no decides nothing.
        let lone = council(&["agent.latency-4"]);
        let mut quiet = State::default();
        plan(&lone, &mut quiet, Some(&agent(4)), propose("solo"), 10).unwrap();
        let vote = |yes| Act::Vote {
            id: "P1".into(),
            yes,
            reason: "alone".into(),
        };
        let (_, _, answer) = plan(&lone, &mut quiet, Some(&agent(4)), vote(true), 11).unwrap();
        assert_eq!(answer["decided"], Value::Null);
        // The open proposals latency-2 made go when it leaves; the board keeps them.
        state.forget("latency-2");
        assert!(
            state
                .proposals
                .iter()
                .all(|p| p.status != "open" || p.by != "latency-2")
        );
        // latency-4 leaves: its role, its vote and its place go, and the stream it alone was in
        // closes; everything stays on the board.
        state.roles.insert("latency-4".into(), "lead".into());
        state.forget("latency-4");
        assert!(state.roles.is_empty() && state.streams.is_empty());
        assert!(state.proposals.iter().all(|p| p.status != "approved"));
        assert!(
            state
                .proposals
                .iter()
                .all(|p| !p.votes.contains_key("latency-4") || p.status != "open")
        );
    }

    #[test]
    fn a_change_cut_short_is_finished_or_undone_at_the_next_lock() {
        let root = scratch("pending");
        let s = swarm(&[]);
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/app")).unwrap();
        let length = || std::fs::metadata(dir.join("board.jsonl")).unwrap().len();
        let role = |role: &str| {
            let mut state = State::default();
            state.roles.insert("latency-1".into(), role.into());
            state
        };
        // Its lines never reached the board, or only half of one did: the board goes back and
        // the state stays as it was.
        let from = length();
        let mut pending = role("lost").json();
        pending["from"] = json!(from);
        pending["to"] = json!(from + 40);
        std::fs::write(dir.join(PENDING), pending.to_string()).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("board.jsonl"))
            .unwrap()
            .write_all(b"{\"half")
            .unwrap();
        locked(&dir, |_| Ok((vec![], ()))).unwrap();
        assert_eq!(length(), from);
        assert!(!dir.join(PENDING).exists());
        assert!(State::read(&dir).unwrap().roles.is_empty());
        // Its lines are all on the board: its state is the state.
        let line = b"{\"kind\":\"role\"}\n";
        let mut pending = role("kept").json();
        pending["from"] = json!(from);
        pending["to"] = json!(from + line.len() as u64);
        std::fs::write(dir.join(PENDING), pending.to_string()).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("board.jsonl"))
            .unwrap()
            .write_all(line)
            .unwrap();
        // A read settles it too, so a quiet swarm's board and state agree.
        let read = board(&root, &s.name, None).unwrap();
        assert_eq!(read["state"]["roles"]["latency-1"], "kept");
        assert_eq!(State::read(&dir).unwrap().roles["latency-1"], "kept");
        assert!(!dir.join(PENDING).exists());
        std::fs::remove_dir_all(root).unwrap();
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
        // A deleted agent leaves, id, role and all, so an agent made under its name starts fresh.
        locked(&dir, |state| {
            state.roles.insert("latency-1".into(), "profiler".into());
            state.roles.insert("latency-2".into(), "tester".into());
            Ok((vec![], ()))
        })
        .unwrap();
        let left = leave(&root, &s.name, "agent.latency-1").unwrap();
        assert_eq!(left["members"], json!(["agent.latency-2"]));
        assert_eq!(left["ids"], json!({"agent.latency-2": 12}));
        let roles = State::read(&dir).unwrap().roles;
        assert_eq!(
            roles,
            BTreeMap::from([("latency-2".into(), "tester".into())])
        );
        // Its share of the budget goes with it, and its id stays so its helpers still count.
        assert_eq!(left["budget_tokens"], 1_500_500);
        assert_eq!(Swarm::read(&dir).unwrap().left.len(), 1);
        // What the state holds of a name that is no member any more goes at the next departure.
        locked(&dir, |state| {
            state.roles.insert("latency-9".into(), "ghost".into());
            Ok((vec![], ()))
        })
        .unwrap();
        leave(&root, &s.name, "agent.nobody").unwrap();
        assert!(!State::read(&dir).unwrap().roles.contains_key("latency-9"));
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
                "create"
                    if ["p.goal-3", "p.fail-1", "p.seat-2"]
                        .contains(&request["bot"].as_str().unwrap_or_default()) =>
                {
                    Err("provider_unknown".into())
                }
                "create" => Ok(made(request)),
                "submit" if request["bot"] == "p.brief-2" => Err("busy".into()),
                "submit" => Ok(json!({"status": "running"})),
                "resume" => Err("bot_not_found".into()),
                "bots" => Ok(json!({"bots": []})),
                "delete" => Ok(json!({})),
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
        // The goal names the swarm; its agents are dealt to the mix's rows.
        let start = |goal: &str, agents: usize, mix: Vec<Mix>| Start {
            project: "p".into(),
            folder: project.clone(),
            goal: goal.into(),
            shared: false,
            mix,
            agents,
            budget_tokens: 4_000,
            council: 0,
        };
        let out = rt
            .block_on(super::start(
                &client,
                &root,
                Path::new("/app"),
                start("goal", 4, mix.clone()),
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
        // Three agents were made with a quarter each: the budget is theirs, not the four asked.
        assert_eq!(out["swarm"]["budget_tokens"], 3_000);
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
            start("fail", 1, mix.clone()),
        ));
        assert_eq!(out.unwrap_err(), "provider_unknown (fake)");
        assert!(!root.join("p.fail").exists());
        // An agent whose brief did not arrive goes, with its share.
        let out = rt
            .block_on(super::start(
                &client,
                &root,
                Path::new("/app"),
                start("brief", 2, mix.clone()),
            ))
            .unwrap();
        assert_eq!(out["swarm"]["members"], json!(["p.brief-1"]));
        assert_eq!(out["swarm"]["budget_tokens"], 2_000);
        assert_eq!(out["failed"][0]["agent"], "p.brief-2");
        let shown: Vec<&Value> = (out["bots"].as_array().unwrap().iter())
            .map(|b| &b["name"])
            .collect();
        assert_eq!(shown, [&json!("p.brief-1")]);
        assert!(fake.ops("delete").iter().any(|d| d["bot"] == "p.brief-2"));
        // The agent briefed with it among the others hears that it left.
        let told = fake.ops("submit").pop().unwrap();
        assert_eq!(told["bot"], "p.brief-1");
        assert_eq!(
            told["prompt"],
            "[board] brief-2 did not get the brief and left the swarm; its agents are brief-1"
        );
        // A council that cannot fill its three seats does not start, and its agents go.
        let mut council = start("seat", 3, mix.clone());
        council.council = 3;
        let error = rt
            .block_on(super::start(&client, &root, Path::new("/app"), council))
            .unwrap_err();
        assert!(error.starts_with("invalid_council: only 2 of"), "{error}");
        let deleted: Vec<Value> = (fake.ops("delete").iter())
            .map(|d| d["bot"].clone())
            .filter(|b| b.as_str().is_some_and(|b| b.starts_with("p.seat")))
            .collect();
        assert_eq!(deleted, vec![json!("p.seat-1"), json!("p.seat-3")]);
        assert!(!root.join("p.seat").exists());
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
                start("read", 1, reader),
            ))
            .unwrap_err();
        assert!(error.starts_with("identity_without_shell"), "{error}");
        assert_eq!(fake.ops("create").len(), before);
        assert!(!root.join("p.read").exists());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn agents_are_dealt_to_the_rows_furthest_below_their_shares() {
        let row = |share| Mix {
            identity: String::new(),
            model: "a/x".into(),
            share,
        };
        assert_eq!(deal(&[row(50), row(50)], 4), [0, 1, 0, 1]);
        assert_eq!(deal(&[row(75), row(25)], 4), [0, 0, 1, 0]);
        assert_eq!(deal(&[row(60), row(30), row(10)], 5), [0, 1, 0, 0, 1]);
        assert_eq!(deal(&[row(100)], 3), [0, 0, 0]);
    }

    #[test]
    fn a_swarm_is_named_after_its_goal() {
        assert_eq!(goal_name("Halve the daemon's p99 latency"), "latency");
        assert_eq!(goal_name("Fix all the things"), "things");
        assert_eq!(goal_name("?"), "swarm");
        assert_eq!(goal_name("lock it"), "swarm");
        assert_eq!(goal_name(&"x".repeat(40)), "x".repeat(24));
    }

    #[test]
    fn a_coordinator_asks_for_a_swarm_with_flags_and_a_goal() {
        let args = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let asked = parse_start(&args(
            "--agents 6 --budget 12.5 --council 3 --in-project --row openai/gpt-6-luna,70 --row b/y,30,reviewer -- Halve p99.",
        ))
        .unwrap();
        assert_eq!(
            asked,
            Asked {
                agents: 6,
                budget_tokens: 12_500_000,
                council: 3,
                shared: false,
                mix: vec![
                    Mix {
                        identity: String::new(),
                        model: "openai/gpt-6-luna".into(),
                        share: 70
                    },
                    Mix {
                        identity: "reviewer".into(),
                        model: "b/y".into(),
                        share: 30
                    },
                ],
                goal: "Halve p99.".into(),
            }
        );
        // What the sheet offers first, and the coordinator's model once its rows are known.
        let plain = parse_start(&args("-- Ship it.")).unwrap();
        assert_eq!(
            (
                plain.agents,
                plain.budget_tokens,
                plain.council,
                plain.shared
            ),
            (4, 3_000_000, 0, true)
        );
        assert!(plain.mix.is_empty());
        for bad in [
            "Ship it.",
            "--budget 5000 -- x",
            "--council 2 -- x",
            "--row a/x -- x",
            "--agents -- x",
            "--swarm -- x",
        ] {
            let error = parse_start(&args(bad)).unwrap_err();
            assert!(error.ends_with(START_USAGE), "{bad}: {error}");
        }
    }

    #[test]
    fn the_start_script_names_this_executable() {
        let home = scratch("start-script");
        write_start_script(&home, Path::new("/Apps/It's/agent-app")).unwrap();
        let text = std::fs::read_to_string(home.join("start")).unwrap();
        assert!(
            text.ends_with("exec '/Apps/It'\\''s/agent-app' --swarm-start \"$@\"\n"),
            "{text}"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            home.join("start").metadata().unwrap().permissions().mode() & 0o777,
            0o755
        );
        // Other swarms' scripts are refreshed around it.
        refresh_scripts(&home, Path::new("/Apps/agent-app"));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_start_whose_every_name_is_held_makes_nothing() {
        let home = scratch("names-held");
        let (root, project) = (home.join("swarms"), home.join("project"));
        std::fs::create_dir_all(&project).unwrap();
        let fake = Fake::start(
            "names-held",
            Box::new(|op, _| match op {
                "resume" => Ok(json!({"id": 7})),
                _ => Err("unexpected".into()),
            }),
        );
        let rt = runtime();
        let (client, _events) = rt.block_on(Client::connect(&fake.socket)).unwrap();
        let start = Start {
            project: "p".into(),
            folder: project,
            goal: "Ship it".into(),
            shared: false,
            mix: vec![Mix {
                identity: String::new(),
                model: "a/x".into(),
                share: 100,
            }],
            agents: 1,
            budget_tokens: 1_000,
            council: 0,
        };
        let error = rt
            .block_on(super::start(&client, &root, Path::new("/app"), start))
            .unwrap_err();
        assert!(error.starts_with("swarm_exists: p.ship-9"), "{error}");
        assert!(!root.exists() || std::fs::read_dir(&root).unwrap().next().is_none());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_name_a_bot_or_another_swarm_holds_is_passed_over() {
        let home = scratch("names");
        let (root, project) = (home.join("swarms"), home.join("project"));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(root.join("p.ship-3")).unwrap();
        let fake = Fake::start(
            "names",
            Box::new(|op, request| match op {
                // A task is named p.ship; strays of an old p.ship-2 are left.
                "resume" if request["bot"] == "p.ship" => Ok(json!({"id": 7})),
                "resume" => Err("bot_not_found".into()),
                "bots" if request["after"] == "p.ship-2" => {
                    Ok(json!({"bots": [{"name": "p.ship-2-1"}]}))
                }
                "bots" => Ok(json!({"bots": [{"name": "q.lead"}]})),
                "create" => Ok(made(request)),
                "submit" => Ok(json!({"status": "running"})),
                _ => Err("unexpected".into()),
            }),
        );
        let rt = runtime();
        let (client, _events) = rt.block_on(Client::connect(&fake.socket)).unwrap();
        let start = Start {
            project: "p".into(),
            folder: project,
            goal: "Ship it".into(),
            shared: false,
            mix: vec![Mix {
                identity: String::new(),
                model: "a/x".into(),
                share: 100,
            }],
            agents: 2,
            budget_tokens: 2_000,
            council: 3,
        };
        let refused = rt.block_on(super::start(&client, &root, Path::new("/app"), start));
        assert!(refused.unwrap_err().starts_with("invalid_council"));
        let start = Start {
            project: "p".into(),
            folder: home.join("project"),
            goal: "Ship it".into(),
            shared: false,
            mix: vec![Mix {
                identity: String::new(),
                model: "a/x".into(),
                share: 100,
            }],
            agents: 2,
            budget_tokens: 2_000,
            council: 0,
        };
        let out = rt
            .block_on(super::start(&client, &root, Path::new("/app"), start))
            .unwrap();
        assert_eq!(out["swarm"]["swarm"], "p.ship-4");
        assert_eq!(out["swarm"]["members"], json!(["p.ship-4-1", "p.ship-4-2"]));
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
        // latency-4 leaves with its share; the next agent is latency-5, never a second latency-4.
        let left = leave(&root, &s.name, &agent(4)).unwrap();
        assert_eq!(left["budget_tokens"], 3_000_000);
        let out = rt.block_on(add(&client, &root, &s.name, 0)).unwrap();
        assert_eq!(
            out["swarm"]["members"],
            json!([agent(1), agent(2), agent(5)])
        );
        assert_eq!(out["swarm"]["budget_tokens"], 4_500_000);
        // A swarm every agent left has no share to give a new one.
        for n in [1, 2, 5] {
            leave(&root, &s.name, &agent(n)).unwrap();
        }
        let empty = rt.block_on(add(&client, &root, &s.name, 0)).unwrap_err();
        assert!(empty.starts_with("swarm_empty"), "{empty}");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn stop_ends_every_unfinished_turn_newest_first_and_a_reused_name_leaves() {
        let root = scratch("stop");
        let s = swarm(&[&agent(1), &agent(2), &agent(3)]);
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/app")).unwrap();
        let looks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let looked = looks.clone();
        let fake = Fake::start(
            "stop",
            Box::new(move |op, request| {
                let bot = request["bot"].as_str().unwrap_or_default();
                match (op, bot) {
                    // latency-1 made a helper, which made its own just
                    // before its turn ended, so only a second look finds
                    // it; a bot in the swarm's range that no agent made is
                    // not a helper.
                    ("bots", _) => {
                        let first = looked.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0;
                        let deep = json!({"name": "agent.latency-1.fix.deep", "id": 41, "created_by_id": 40});
                        // Another window gave latency-1 a turn after its first end.
                        let running = if first { json!(9) } else { Value::Null };
                        let mut bots = vec![
                            json!({"name": agent(1), "id": 1, "tokens_used": 10, "running_turn": running}),
                            json!({"name": "agent.latency-1.fix", "id": 40, "created_by_id": 1}),
                        ];
                        bots.extend((!first).then_some(deep));
                        bots.extend([
                            json!({"name": "agent.latency-1.stray", "id": 42, "created_by_id": 77}),
                            json!({"name": agent(2), "id": 99}),
                            json!({"name": "agent.other", "id": 50}),
                        ]);
                        Ok(json!({"bots": bots, "next_after": null}))
                    }
                    ("resume", "agent.latency-1.fix") => Ok(json!({"id": 40})),
                    ("resume", "agent.latency-1.fix.deep") => Ok(json!({"id": 41})),
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
        let ended = |bot: &str| -> Vec<Value> {
            (fake.ops("interrupt").iter())
                .filter(|i| i["bot"] == bot)
                .map(|i| i["turn"].clone())
                .collect()
        };
        for bot in ["agent.latency-1.fix", "agent.latency-1.fix.deep"] {
            assert_eq!(ended(bot), vec![json!(7), json!(6)], "{bot}");
        }
        // latency-1, running again at the first look, was ended again.
        assert_eq!(
            ended(&agent(1)),
            vec![json!(7), json!(6), json!(7), json!(6)]
        );
        let listed: std::collections::BTreeSet<String> = (fake.ops("turns").iter())
            .map(|t| t["bot"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(listed.len(), 3, "{listed:?}");
        assert!(!listed.contains("agent.latency-1.stray"));
        // It looked until a look found no helper it had not stopped.
        assert_eq!(looks.load(std::sync::atomic::Ordering::Relaxed), 3);
        let read = Swarm::read(&dir).unwrap();
        assert!(read.stopped && read.rows.len() == 1 && read.ids.len() == 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_board_says_each_share_of_the_budget_once() {
        let root = scratch("spent");
        let mut s = swarm(&[&agent(1), &agent(2)]);
        // latency-3 left with a helper still at work; latency-7 left with none.
        s.left = vec![3, 7];
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/app")).unwrap();
        let used = Arc::new(std::sync::atomic::AtomicU64::new(1_000_000));
        let now = used.clone();
        let helpers = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let alive = helpers.clone();
        let fake = Fake::start(
            "spent",
            Box::new(move |op, _| match op {
                // latency-1 works; its helper's tokens count too.
                "bots" => {
                    let mut bots = vec![
                        json!({"name": agent(1), "id": 1, "running_turn": 4, "tokens_used": now.load(std::sync::atomic::Ordering::Relaxed)}),
                        json!({"name": "agent.latency-1.fix", "id": 40, "created_by_id": 1, "tokens_used": 600_000}),
                        json!({"name": agent(2), "id": 2}),
                        json!({"name": "agent.latency-3.fix", "id": 43, "created_by_id": 3}),
                    ];
                    if !alive.load(std::sync::atomic::Ordering::Relaxed) {
                        bots.retain(|b| b["created_by_id"].is_null());
                    }
                    Ok(json!({"bots": bots, "next_after": null}))
                }
                "submit" => Ok(json!({"status": "steered"})),
                _ => Err("unexpected".into()),
            }),
        );
        let rt = runtime();
        let (client, _events) = rt.block_on(Client::connect(&fake.socket)).unwrap();
        let check = || {
            rt.block_on(act(&client, &root, &s.name, None, Act::Check))
                .unwrap()
        };
        // 1.6M of 3M: past half, once.
        assert_eq!(
            check()["budget"],
            "the swarm has used 50% of its budget (1.6M of 3M tokens)"
        );
        assert_eq!(check()["budget"], Value::Null);
        // A member that left stays a maker only while something it made is.
        assert_eq!(Swarm::read(&dir).unwrap().left, vec![3]);
        // The helpers are deleted: what they used still counts.
        helpers.store(false, std::sync::atomic::Ordering::Relaxed);
        // Past 90% at once: only the highest share is said.
        used.store(2_200_000, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            check()["budget"],
            "the swarm has used 90% of its budget (2.8M of 3M tokens)"
        );
        let steers = fake.ops("submit");
        assert_eq!(steers.len(), 2);
        assert!(
            steers
                .iter()
                .all(|q| q["bot"] == agent(1) && q["expected_turn"] == 4 && q["from"].is_null())
        );
        let board = std::fs::read_to_string(dir.join("board.jsonl")).unwrap();
        let spent: Vec<Value> = (board.lines())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|l| l["from"] == "budget")
            .map(|l| json!([l["spent"], l["sent"]]))
            .collect();
        assert_eq!(spent, vec![json!([50, 1]), json!([90, 1])]);
        let state = State::read(&dir).unwrap();
        assert_eq!(
            (state.spent, state.helpers.len(), state.gone),
            (90, 0, 600_000)
        );
        assert_eq!(Swarm::read(&dir).unwrap().left, Vec::<i64>::new());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_hooks_get_no_keys() {
        // Cargo gives a test run variables no ordinary shell has.
        if std::env::var_os("CARGO_MANIFEST_DIR").is_none() {
            return;
        }
        let repo = scratch("hooks");
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap();
            assert!(status.status.success(), "{status:?}");
        };
        run(&["init", "-q"]);
        run(&["commit", "-q", "--allow-empty", "-m", "first"]);
        let hook = repo.join(".git/hooks/post-checkout");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\nenv > '{}/seen'\n", repo.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let out = runtime().block_on(async {
            git(&clean_env().await)
                .arg("-C")
                .arg(&repo)
                .args(["checkout", "-q", "-b", "other"])
                .output()
                .await
        });
        assert!(out.unwrap().status.success());
        let seen = std::fs::read_to_string(repo.join("seen")).unwrap();
        assert!(seen.lines().any(|l| l.starts_with("PATH=")), "{seen}");
        assert!(!seen.contains("CARGO_MANIFEST_DIR="), "{seen}");
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn a_worktree_name_is_claimed_by_its_folder_and_a_failed_add_leaves_nothing() {
        let home = scratch("trees");
        let (repo, trees) = (home.join("repo"), home.join("trees"));
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap()
        };
        assert!(run(&["init", "-q"]).status.success());
        assert!(
            run(&["commit", "-q", "--allow-empty", "-m", "first"])
                .status
                .success()
        );
        let hook = repo.join(".git/hooks/post-checkout");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let rt = runtime();
        // A hook failing after the add made the worktree and branch: both go.
        let failed = rt.block_on(worktree(&trees, &repo, "p.fix")).unwrap_err();
        assert!(failed.starts_with("worktree_failed"), "{failed}");
        assert!(!trees.join("p.fix").exists());
        assert!(
            !run(&["rev-parse", "--verify", "--quiet", "refs/heads/agent/p.fix"])
                .status
                .success()
        );
        // A folder another start made first means the name is taken.
        std::fs::remove_file(&hook).unwrap();
        std::fs::create_dir_all(trees.join("p.held")).unwrap();
        let held = rt.block_on(worktree(&trees, &repo, "p.held")).unwrap_err();
        assert!(held.starts_with("swarm_exists"), "{held}");
        assert!(trees.join("p.held").exists());
        // So does a branch another git client made, and it is left alone.
        assert!(run(&["branch", "agent/p.theirs"]).status.success());
        let theirs = rt
            .block_on(worktree(&trees, &repo, "p.theirs"))
            .unwrap_err();
        assert!(theirs.starts_with("swarm_exists"), "{theirs}");
        assert!(!trees.join("p.theirs").exists());
        assert!(
            run(&[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/agent/p.theirs"
            ])
            .status
            .success()
        );
        // A setup that fails takes the worktree and its branch with it.
        let setup = repo.join(".agents/setup");
        std::fs::create_dir_all(setup.parent().unwrap()).unwrap();
        // It commits first, moving the branch.
        std::fs::write(
            &setup,
            "#!/bin/sh\ngit -c user.name=t -c user.email=t@example.com commit -q --allow-empty -m setup\nexit 3\n",
        )
        .unwrap();
        std::fs::set_permissions(&setup, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let failed = rt.block_on(worktree(&trees, &repo, "p.setup")).unwrap_err();
        assert!(failed.starts_with("setup_failed"), "{failed}");
        assert!(!trees.join("p.setup").exists());
        assert!(
            !run(&[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/agent/p.setup"
            ])
            .status
            .success()
        );
        std::fs::remove_file(&setup).unwrap();
        let made = rt.block_on(worktree(&trees, &repo, "p.fix")).unwrap();
        assert!(Path::new(&made).join(".git").exists());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_failed_start_never_removes_a_project_folder() {
        let root = scratch("owned");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut s = swarm(&[]);
        s.workspace = project.display().to_string();
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/app")).unwrap();
        runtime().block_on(discard(&dir, &s, false)).unwrap();
        assert!(!dir.exists() && project.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_agent_whose_swarm_stops_while_it_is_made_is_not_briefed_but_deleted() {
        let root = scratch("add-stop");
        let s = swarm(&[&agent(1)]);
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/app")).unwrap();
        enrol(&root, &s.name, &[(agent(1), 1)], 0).unwrap();
        // Another window stops the swarm while the daemon makes the agent.
        let stopping = dir.clone();
        let fake = Fake::start(
            "add-stop",
            Box::new(move |op, request| match op {
                "create" => {
                    update(&stopping, |s| {
                        s.stopped = true;
                        Ok(())
                    })
                    .unwrap();
                    Ok(made(request))
                }
                "delete" => Ok(json!({})),
                _ => Err("unexpected".into()),
            }),
        );
        let rt = runtime();
        let (client, _events) = rt.block_on(Client::connect(&fake.socket)).unwrap();
        let refused = rt.block_on(add(&client, &root, &s.name, 0)).unwrap_err();
        assert!(refused.starts_with("swarm_stopped"), "{refused}");
        assert_eq!(fake.ops("submit").len(), 0);
        assert_eq!(fake.ops("delete")[0]["bot"], agent(2));
        assert_eq!(Swarm::read(&dir).unwrap().members, vec![agent(1)]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_council_has_three_seats_and_needs_three_agents() {
        assert!(valid_council(0, 1).is_ok());
        assert!(valid_council(3, 3).is_ok());
        assert!(
            valid_council(3, 2)
                .unwrap_err()
                .starts_with("invalid_council")
        );
        assert!(
            valid_council(2, 8)
                .unwrap_err()
                .starts_with("invalid_council")
        );
    }

    #[test]
    fn a_stopped_swarm_takes_no_agent_and_resumes_only_with_a_post_on_the_board() {
        let root = scratch("resume");
        let mut s = swarm(&[&agent(1)]);
        s.stopped = true;
        let dir = claim(&root, &s.name).unwrap();
        fill(&dir, &s, Path::new("/app")).unwrap();
        let listing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let up = listing.clone();
        let fake = Fake::start(
            "resume",
            Box::new(move |op, _| match op {
                "bots" if up.load(std::sync::atomic::Ordering::Relaxed) => Ok(json!({
                    "bots": [{"name": agent(1), "id": 1}], "next_after": null,
                })),
                "bots" => Err("unavailable".into()),
                "submit" => Ok(json!({"status": "started"})),
                _ => Err("unexpected".into()),
            }),
        );
        let rt = runtime();
        let (client, _events) = rt.block_on(Client::connect(&fake.socket)).unwrap();
        let added = rt.block_on(add(&client, &root, &s.name, 0)).unwrap_err();
        assert!(added.starts_with("swarm_stopped"), "{added}");
        let post = || Act::Post {
            text: "go on".into(),
            all: true,
        };
        // A post that never reached the board leaves the swarm stopped.
        assert!(
            rt.block_on(act(&client, &root, &s.name, None, post()))
                .is_err()
        );
        assert!(Swarm::read(&dir).unwrap().stopped);
        listing.store(true, std::sync::atomic::Ordering::Relaxed);
        // So does one the board refuses.
        let empty = Act::Post {
            text: " ".into(),
            all: true,
        };
        let refused = rt
            .block_on(act(&client, &root, &s.name, None, empty))
            .unwrap_err();
        assert!(refused.starts_with("post_empty"), "{refused}");
        assert!(Swarm::read(&dir).unwrap().stopped);
        rt.block_on(act(&client, &root, &s.name, None, post()))
            .unwrap();
        assert!(!Swarm::read(&dir).unwrap().stopped);
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
        enrol(&root, &s.name, &[("agent.latency-1".into(), 1)], 0).unwrap();
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
                        s.council = (i % 2) as usize;
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
            // A kill lands at once but takes effect when the child next runs,
            // which a busy machine can put off for a moment.
            let running = || {
                let stat = std::fs::read_to_string(format!("/proc/{}/stat", child.trim()));
                stat.is_ok_and(|s| !s.contains(") Z "))
            };
            for _ in 0..100 {
                if !running() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert!(!running(), "setup's background child outlived it");
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
