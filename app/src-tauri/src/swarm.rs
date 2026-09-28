//! Swarms: many agents on one goal, talking through a board. A swarm is the
//! app's; the daemon knows only its agents, which are ordinary bots named
//! `SWARM-N`. A swarm is a folder, `~/.agent/swarms/SWARM/`:
//!
//! - `swarm.toml`: its project, goal, folder, model, token budget, members,
//!   and whether you stopped it. Only the app writes it.
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

#[derive(Debug, Clone, PartialEq)]
pub struct Swarm {
    pub name: String,
    pub project: String,
    pub goal: String,
    pub workspace: String,
    pub model: String,
    pub budget_tokens: u64,
    pub members: Vec<String>,
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
            stopped: table
                .get("stopped")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false),
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
        let text = toml::to_string(&table).map_err(|e| e.to_string())?;
        // Distinct per write, so two writes at once never share a temporary.
        static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temporary = dir.join(format!(".swarm.toml.{}.{n}", std::process::id()));
        std::fs::write(&temporary, text).map_err(|e| e.to_string())?;
        std::fs::rename(&temporary, dir.join("swarm.toml")).map_err(|e| e.to_string())
    }

    pub fn json(&self, dir: &Path) -> Value {
        json!({
            "swarm": self.name, "dir": dir.to_string_lossy(), "project": self.project,
            "goal": self.goal, "workspace": self.workspace, "model": self.model,
            "budget_tokens": self.budget_tokens, "members": self.members, "stopped": self.stopped,
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

pub fn root() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".agent/swarms"))
        .ok_or_else(|| "no HOME for ~/.agent/swarms".into())
}

/// A swarm's name is `PROJECT.NAME`, used for its folder, its worktree and
/// branch, and its agents' names, so it is held to what all of them accept.
pub fn valid_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && !name.ends_with('.')
        && !name.contains("..")
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
pub fn list(root: &Path) -> Value {
    let mut swarms = Vec::new();
    let mut broken = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
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
    json!({"swarms": swarms, "broken": broken})
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
        let post = dir.join("post");
        std::fs::write(&post, script(app, dir)).map_err(|e| e.to_string())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&post, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())
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
/// naming the project; if that fails, the worktree and its branch go.
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
    let home = std::env::var_os("HOME").ok_or("no HOME for ~/.agent/worktrees")?;
    let tree = PathBuf::from(home).join(".agent/worktrees").join(swarm);
    let branch = format!("agent/{swarm}");
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
        let ran = command
            .current_dir(&workspace)
            .env("AGENT_SOURCE", project)
            .stdin(std::process::Stdio::null())
            .output()
            .await;
        let failed = match ran {
            Ok(out) if out.status.success() => None,
            Ok(out) => Some(format!(
                "setup_failed: {}",
                tail(&[out.stdout, out.stderr].concat())
            )),
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

/// The app moves when it is updated, so it writes its path again on start.
pub fn refresh_scripts(root: &Path, app: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let post = dir.join("post");
        let want = script(app, &dir);
        if post.exists() && std::fs::read_to_string(&post).is_ok_and(|have| have != want) {
            let _ = std::fs::write(&post, want);
        }
    }
}

/// Members join in order and never twice.
pub fn join(root: &Path, swarm: &str, members: &[String]) -> Result<Value, String> {
    let dir = folder(root, swarm)?;
    let mut s = Swarm::read(&dir)?;
    for member in members {
        if !member.starts_with(&format!("{swarm}-")) {
            return Err(format!("invalid_member: {member} is not named {swarm}-N"));
        }
        if !s.members.contains(member) {
            s.members.push(member.clone());
        }
    }
    s.write(&dir)?;
    Ok(s.json(&dir))
}

/// A stopped swarm refuses its agents' posts; your next post resumes it.
pub fn set_stopped(root: &Path, swarm: &str, stopped: bool) -> Result<Value, String> {
    let dir = folder(root, swarm)?;
    let mut s = Swarm::read(&dir)?;
    s.stopped = stopped;
    s.write(&dir)?;
    Ok(s.json(&dir))
}

/// The board's complete lines from `offset`, or its last stretch when there
/// is none, and where the next read starts. A line that is not JSON comes
/// back as its text, so a hand edit shows instead of vanishing.
pub fn board(root: &Path, swarm: &str, offset: Option<u64>) -> Result<Value, String> {
    let path = folder(root, swarm)?.join("board.jsonl");
    let mut file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    let (mut start, skip_partial) = match offset {
        Some(offset) if offset <= size => (offset, false),
        // Shorter than we read before: someone rewrote it; read it again.
        Some(_) => (size.saturating_sub(TAIL), size > TAIL),
        None => (size.saturating_sub(TAIL), size > TAIL),
    };
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
    Ok(json!({"lines": lines, "offset": start + end as u64, "more": start + (end as u64) < size}))
}

/// One line, one write, under an exclusive lock, so posts never interleave.
fn append(dir: &Path, line: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(line).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("board.jsonl"))
        .map_err(|e| e.to_string())?;
    file.lock().map_err(|e| e.to_string())?;
    file.write_all(&bytes).map_err(|e| e.to_string())
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
/// `SWARM-N`, so they sit together in its name order.
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
            if swarm.members.iter().any(|m| m == name) {
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
    let mut s = Swarm::read(&dir)?;
    if let Some(author) = &author {
        if !s.members.contains(&author.bot) {
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
    append(&dir, &line)?;
    let turns = running_turns(client, &s).await?;
    let running = |m: &str| turns.iter().find(|(n, _)| n == m).and_then(|(_, t)| *t);
    let readers = readers(&s, author.as_ref().map(|a| a.bot.as_str()), text, &running);
    let prompt = format!("[board] {from}: {text}");
    let stamp = format!("swarm-{}-{}", now_ms(), std::process::id());
    let mut sends = tokio::task::JoinSet::new();
    for (i, (member, reach)) in readers.into_iter().enumerate() {
        let mut params = json!({
            "bot": member, "request_id": format!("{stamp}-{i}"), "prompt": prompt, "delivery": "steer",
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
    let author = match (
        std::env::var("AGENT_BOT"),
        std::env::var("AGENT_TURN").map(|t| t.parse::<i64>()),
    ) {
        (Ok(bot), Ok(Ok(turn))) if !bot.is_empty() => Author { bot, turn },
        _ => {
            return fail(
                "post runs in a swarm agent's shell, which names AGENT_BOT and AGENT_TURN".into(),
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
        let root = scratch("folder");
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
            &["agent.latency-1".into(), "agent.latency-2".into()],
        )
        .unwrap();
        join(&root, &s.name, &["agent.latency-2".into()]).unwrap();
        assert!(
            join(&root, &s.name, &["other.x-1".into()])
                .unwrap_err()
                .starts_with("invalid_member")
        );
        assert_eq!(
            joined["members"],
            json!(["agent.latency-1", "agent.latency-2"])
        );
        let read = Swarm::read(&dir).unwrap();
        assert_eq!(read.members.len(), 2);
        assert_eq!(read.goal, "Halve p99.");
        assert!(
            set_stopped(&root, &s.name, true).unwrap()["stopped"]
                .as_bool()
                .unwrap()
        );
        let listed = list(&root);
        assert_eq!(listed["swarms"][0]["swarm"], "agent.latency");
        std::fs::write(root.join("agent.latency/swarm.toml"), "goal = 1").unwrap();
        assert_eq!(list(&root)["broken"].as_array().unwrap().len(), 1);
        for bad in ["", ".x", "a..b", "a/b", "a.", &"a".repeat(101)] {
            assert!(valid_name(bad).is_err(), "{bad}");
        }
        refresh_scripts(&root, Path::new("/moved/agent-app"));
        assert!(
            std::fs::read_to_string(dir.join("post"))
                .unwrap()
                .contains("'/moved/agent-app'")
        );
        std::fs::remove_dir_all(root).unwrap();
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
        std::fs::remove_dir_all(root).unwrap();
    }
}
