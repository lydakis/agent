//! What a folder held after each of an agent's completed turns, kept as a
//! commit beside its repository's history, so a task forked from that
//! turn starts with those files. The lookout takes one as a turn ends
//! `completed`, the only point a fork starts from: the files git does not
//! ignore, tracked or not, added through an index of Sonny's own so the
//! person's staging is never touched, committed on the HEAD of the time
//! (no hook or filter runs) under `refs/agent/snapshots/STORE/BOT_ID/TURN`, STORE
//! naming the daemon's store, whose bot and turn IDs are its own. The
//! commit names the turn's checkpoint, and says it is late when an agent in
//! the folder was already working again by the time it was taken, so its
//! files may hold some of that work.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where the snapshots of the bots of the store `store` are kept: under a
/// hash of its real path, the name Sonny knows it by everywhere.
pub fn refs(store: &Path) -> String {
    let store = agent_client::socket::canonical_store(store).unwrap_or_else(|_| store.to_owned());
    format!("refs/agent/snapshots/{:016x}", fnv(&store))
}

/// Where the lookout of the store `store` keeps the indexes it snapshots
/// through: beside the store, where no task can write, never in a
/// checkout's git folder, which a task's commits write.
pub fn indexes(store: &Path) -> PathBuf {
    let mut indexes = store.as_os_str().to_owned();
    indexes.push(".snapshot-indexes");
    PathBuf::from(indexes)
}

/// FNV-1a of a path: stable across builds, as std's hasher is not.
fn fnv(path: &Path) -> u64 {
    (path.as_os_str().as_encoded_bytes().iter()).fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}
const CHECKPOINT: &str = "Agent-Checkpoint: ";
const LATE: &str = "Agent-Snapshot: late";

/// A folder's files added and written as a tree, not yet committed.
pub struct Staged {
    top: PathBuf,
    tree: String,
    head: Option<String>,
}

/// A turn's snapshot, found to fork from.
#[derive(Debug, PartialEq)]
pub struct Snapshot {
    pub turn: i64,
    pub commit: String,
    /// The commit the folder was on.
    pub base: Option<String>,
    pub checkpoint: i64,
    pub late: bool,
}

fn git(
    dir: &Path,
    args: &[&str],
    index: Option<&Path>,
    input: Option<&str>,
) -> Result<String, String> {
    let unfiltered = unfiltered(dir)?;
    let mut command = Command::new("git");
    command.env("GIT_CONFIG_COUNT", unfiltered.len().to_string());
    for (at, (key, value)) in unfiltered.into_iter().enumerate() {
        command
            .env(format!("GIT_CONFIG_KEY_{at}"), key)
            .env(format!("GIT_CONFIG_VALUE_{at}"), value);
    }
    // The folder's own code never runs here, where the daemon's keys are:
    // no hook, no fsmonitor, no filter, and none of the keys in its
    // environment. Its attributes still convert line endings and
    // encodings both ways, as git's own code does that.
    command
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in crate::task::kept_keys() {
        command.env_remove(key);
    }
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    // A commit of Sonny's needs no identity of the person's.
    for key in ["GIT_AUTHOR", "GIT_COMMITTER"] {
        command
            .env(format!("{key}_NAME"), "Sonny")
            .env(format!("{key}_EMAIL"), "sonny@localhost");
    }
    let mut child = command.spawn().map_err(|e| format!("running git: {e}"))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        use std::io::Write;
        stdin
            .write_all(input.as_bytes())
            .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("running git: {e}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {}", args.join(" "), said.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Settings that turn off each filter the folder's config names, as a
/// filter may run a script of the folder's when git reads or writes its
/// files: a file is kept, and given back, as its bytes are. An LFS file
/// unchanged since the commit it is on comes back as its pointer. Names
/// are taken as bytes, whatever they hold, and given to git as config of
/// the environment's, which nothing in a name can break; a config that
/// cannot be read stops the snapshot.
fn unfiltered(dir: &Path) -> Result<Vec<(std::ffi::OsString, &'static str)>, String> {
    use std::os::unix::ffi::OsStrExt;
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(["config", "--name-only", "-z", "--get-regexp", r"^filter\."])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    for key in crate::task::kept_keys() {
        command.env_remove(key);
    }
    let output = command.output().map_err(|e| format!("running git: {e}"))?;
    // 1 is none found.
    if !output.status.success() && output.status.code() != Some(1) {
        return Err("git config: the folder's filters could not be read".into());
    }
    let mut names: Vec<&[u8]> = (output.stdout.split(|byte| *byte == 0))
        .filter_map(|key| key.strip_prefix(b"filter."))
        .filter_map(|rest| Some(&rest[..rest.iter().rposition(|byte| *byte == b'.')?]))
        .collect();
    names.sort_unstable();
    names.dedup();
    Ok(names
        .iter()
        .flat_map(|name| {
            [
                ("clean", ""),
                ("smudge", ""),
                ("process", ""),
                ("required", "false"),
            ]
            .map(|(setting, value)| {
                let mut key = b"filter.".to_vec();
                key.extend_from_slice(name);
                key.push(b'.');
                key.extend_from_slice(setting.as_bytes());
                (std::ffi::OsStr::from_bytes(&key).to_owned(), value)
            })
        })
        .collect())
}

/// The top of the git checkout `dir` is in, if it is in one.
pub fn top(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"], None, None)
        .ok()
        .map(PathBuf::from)
}

/// Add the files of the checkout `dir` is in, as they are now, and write
/// them as a tree, through an index of its own in `indexes`; `None`
/// outside git.
pub fn stage(dir: &Path, indexes: &Path) -> Result<Option<Staged>, String> {
    let Some(top) = top(dir) else {
        return Ok(None);
    };
    let gitdir = PathBuf::from(git(&top, &["rev-parse", "--absolute-git-dir"], None, None)?);
    std::fs::create_dir_all(indexes).map_err(|e| format!("{}: {e}", indexes.display()))?;
    let index = indexes.join(format!("{:016x}.index", fnv(&top)));
    // From the checkout's own index each time: what git knows of each file
    // is kept, so only the files that changed are read again, and a file
    // ignored since an earlier snapshot is not taken as tracked. Made anew,
    // so nothing already at its name is written through.
    match std::fs::remove_file(&index) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.to_string()),
        _ => {}
    }
    if let Ok(mut theirs) = std::fs::File::open(gitdir.join("index")) {
        let mut ours = std::fs::File::create_new(&index).map_err(|e| e.to_string())?;
        std::io::copy(&mut theirs, &mut ours).map_err(|e| e.to_string())?;
        // A file marked as not changing would keep its old content.
        let listed = git(&top, &["ls-files", "-v", "-z"], Some(&index), None)?;
        let assumed: String = (listed.split('\0'))
            .filter_map(|entry| entry.split_once(' '))
            .filter(|(tag, _)| tag.chars().all(|c| c.is_ascii_lowercase()))
            .map(|(_, path)| format!("{path}\0"))
            .collect();
        if !assumed.is_empty() {
            let args = ["update-index", "--no-assume-unchanged", "-z", "--stdin"];
            git(&top, &args, Some(&index), Some(&assumed))?;
        }
    }
    git(&top, &["add", "-A", "--", "."], Some(&index), None)?;
    let tree = git(&top, &["write-tree"], Some(&index), None)?;
    let head = git(
        &top,
        &["rev-parse", "-q", "--verify", "HEAD^{commit}"],
        None,
        None,
    )
    .ok();
    Ok(Some(Staged { top, tree, head }))
}

/// Commit what was staged as bot `bot_id`'s turn `turn`, whose checkpoint
/// is `checkpoint`; `late` when it may hold later work.
pub fn commit(
    staged: &Staged,
    refs: &str,
    bot_id: i64,
    turn: i64,
    checkpoint: i64,
    late: bool,
) -> Result<String, String> {
    let mut message = format!("Files after turn {turn}\n\n{CHECKPOINT}{checkpoint}");
    if late {
        message.push('\n');
        message.push_str(LATE);
    }
    let mut args = vec!["commit-tree", staged.tree.as_str(), "-m", message.as_str()];
    if let Some(head) = &staged.head {
        args.extend(["-p", head]);
    }
    let commit = git(&staged.top, &args, None, None)?;
    let name = format!("{refs}/{bot_id}/{turn}");
    git(&staged.top, &["update-ref", &name, &commit], None, None)?;
    Ok(commit)
}

/// The turns of bot `bot_id` with snapshots in the repository `dir` is in.
fn turns(dir: &Path, refs: &str, bot_id: i64) -> Result<Vec<i64>, String> {
    let listed = git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("{refs}/{bot_id}/"),
        ],
        None,
        None,
    )?;
    Ok(listed
        .lines()
        .filter_map(|name| name.rsplit('/').next()?.parse().ok())
        .collect())
}

/// Bot `bot_id`'s snapshot of turn `turn`, or of its newest turn that has
/// one, in the repository `dir` is in.
pub fn find(dir: &Path, refs: &str, bot_id: i64, turn: Option<i64>) -> Result<Snapshot, String> {
    let turns = turns(dir, refs, bot_id)?;
    let turn = match turn {
        Some(turn) if turns.contains(&turn) => turn,
        Some(turn) => {
            return Err(format!(
                "turn {turn} has no snapshot of its files: only a turn that completed in a git \
                 folder after snapshots began has one"
            ));
        }
        None => *turns
            .iter()
            .max()
            .ok_or("no turn of it has a snapshot of its files yet")?,
    };
    let name = format!("{refs}/{bot_id}/{turn}");
    let commit = git(
        dir,
        &["rev-parse", "--verify", &format!("{name}^{{commit}}")],
        None,
        None,
    )?;
    let message = git(dir, &["log", "-1", "--format=%B", &commit], None, None)?;
    let checkpoint = (message.lines())
        .find_map(|line| line.strip_prefix(CHECKPOINT)?.trim().parse().ok())
        .ok_or_else(|| format!("{name} names no checkpoint"))?;
    let base = git(
        dir,
        &["rev-parse", "-q", "--verify", &format!("{commit}^")],
        None,
        None,
    )
    .ok();
    Ok(Snapshot {
        turn,
        commit,
        base,
        checkpoint,
        late: message.lines().any(|line| line == LATE),
    })
}

/// Make the folder `path` a worktree of the repository `here` is in, on a
/// new branch `branch` from the commit the snapshot's folder was on, with
/// the snapshot's files: changes since that commit stay uncommitted, as
/// they were.
pub fn restore(here: &Path, snapshot: &Snapshot, path: &Path, branch: &str) -> Result<(), String> {
    let base = snapshot
        .base
        .as_deref()
        .ok_or("the folder had no commit yet, so there is nothing to branch from")?;
    // A submodule is kept as the commit it is on, not its files, so a
    // fork would get an empty folder where they were.
    let listed = git(here, &["ls-tree", "-r", "-z", &snapshot.commit], None, None)?;
    if let Some(module) = (listed.split('\0')).find(|entry| entry.starts_with("160000 ")) {
        let path = module.split_once('\t').map_or(module, |(_, path)| path);
        return Err(format!(
            "its files hold a submodule, {path}, whose own files a snapshot does not keep"
        ));
    }
    let target = path.to_string_lossy();
    // Checked out from inside the new worktree, on its branch, as the
    // config that holds may depend on the branch: no file is written
    // until the filters to turn off are the ones that hold there.
    git(
        here,
        &[
            "worktree",
            "add",
            "-q",
            "--no-checkout",
            "-b",
            branch,
            &target,
            base,
        ],
        None,
        None,
    )?;
    git(
        path,
        &["read-tree", "-u", "--reset", &snapshot.commit],
        None,
        None,
    )?;
    git(path, &["reset", "-q"], None, None)?;
    Ok(())
}

/// Drop bot `bot_id`'s snapshots in the repository `dir` is in; git's own
/// clean-up then frees what nothing else holds.
pub fn forget(dir: &Path, refs: &str, bot_id: i64) -> Result<usize, String> {
    let turns = turns(dir, refs, bot_id)?;
    if turns.is_empty() {
        return Ok(0);
    }
    let input: String = (turns.iter())
        .map(|turn| format!("delete {refs}/{bot_id}/{turn}\n"))
        .collect();
    git(dir, &["update-ref", "--stdin"], None, Some(&input))?;
    Ok(turns.len())
}

/// The lock each checkout's snapshots and their removal take in turn:
/// they share its index, and a removal waits for a snapshot under way.
static CHECKOUTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<tokio::sync::Mutex<()>>>>,
> = std::sync::LazyLock::new(Default::default);

/// The bots deleted while the lookout ran: a snapshot of one that comes
/// after its removal is not kept. IDs are never given again.
static GONE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<i64>>> =
    std::sync::LazyLock::new(Default::default);

/// How many snapshots run at once, across every checkout: each reads its
/// whole checkout, so many turns ending together wait their turn rather
/// than all read the disk at once.
static AT_ONCE: std::sync::LazyLock<tokio::sync::Semaphore> = std::sync::LazyLock::new(|| {
    tokio::sync::Semaphore::new(std::thread::available_parallelism().map_or(4, |n| n.get()))
});

/// When each folder last had a turn start or end, in the order the
/// lookout saw them, and how many it has seen.
static WORKED: std::sync::LazyLock<
    std::sync::Mutex<(u64, std::collections::HashMap<PathBuf, u64>)>,
> = std::sync::LazyLock::new(Default::default);

/// Note that a turn started or ended in `workspace`; how many have now.
pub fn worked(workspace: &Path) -> u64 {
    let real = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_owned());
    let mut worked = WORKED.lock().unwrap_or_else(|e| e.into_inner());
    worked.0 += 1;
    let at = worked.0;
    worked.1.insert(real, at);
    at
}

/// Whether a turn started or ended in the checkout `top` after the
/// `since`th.
fn worked_since(top: &Path, since: u64) -> bool {
    let worked = WORKED.lock().unwrap_or_else(|e| e.into_inner());
    (worked.1.iter()).any(|(folder, at)| *at > since && folder.starts_with(top))
}

fn gone(bot_id: i64) -> bool {
    (GONE.lock().unwrap_or_else(|e| e.into_inner())).contains(&bot_id)
}

/// The checkout `dir` is in, and its lock: only a task worktree's own,
/// as a repository a task made inside one has config the task wrote.
async fn checkout(dir: PathBuf) -> Option<(PathBuf, std::sync::Arc<tokio::sync::Mutex<()>>)> {
    let top = tokio::task::spawn_blocking(move || top(&dir))
        .await
        .ok()??;
    if !crate::task::is_worktree(&top) {
        return None;
    }
    let lock = (CHECKOUTS.lock().unwrap_or_else(|e| e.into_inner()))
        .entry(top.clone())
        .or_default()
        .clone();
    Some((top, lock))
}

/// A turn just completed, to snapshot.
pub struct Completed {
    /// Where its bot's store keeps snapshots, and the indexes for them.
    pub refs: String,
    pub indexes: PathBuf,
    /// The folder it worked in.
    pub workspace: PathBuf,
    pub bot_id: i64,
    pub turn: i64,
    pub checkpoint: i64,
    /// What `worked` said as it ended.
    pub ended: u64,
}

/// Snapshot the folder of the turn `done`: the lookout's part, beside its
/// stream, so a big checkout never holds it up. It is late if an agent
/// worked in the checkout since, or works there now. What happens is a
/// line of its log.
pub async fn after_turn(client: std::sync::Arc<agent_client::Client>, done: Completed) {
    let Completed {
        refs,
        indexes,
        workspace,
        bot_id,
        turn,
        checkpoint,
        ended,
    } = done;
    let Some((top, lock)) = checkout(workspace).await else {
        return;
    };
    let _held = lock.lock().await;
    let Ok(_running) = AT_ONCE.acquire().await else {
        return;
    };
    if gone(bot_id) {
        return;
    }
    let staged = match tokio::task::spawn_blocking(move || stage(&top, &indexes)).await {
        Ok(Ok(Some(staged))) => staged,
        Ok(Ok(None)) | Err(_) => return,
        Ok(Err(error)) => return failed(bot_id, turn, &error),
    };
    let late = worked_since(&staged.top, ended) || busy(&client, &staged.top).await;
    let committed =
        tokio::task::spawn_blocking(move || commit(&staged, &refs, bot_id, turn, checkpoint, late))
            .await;
    match committed {
        Ok(Ok(commit)) => println!(
            "{}",
            serde_json::json!({"event": "snapshot", "bot_id": bot_id, "turn": turn,
                "commit": commit, "late": late})
        ),
        Ok(Err(error)) => failed(bot_id, turn, &error),
        Err(_) => {}
    }
}

/// Drop the snapshots of bot `bot_id`, deleted, once one under way is
/// taken, and keep none taken after. A folder already gone has none to
/// come: a snapshot of it fails.
pub async fn deleted(refs: String, workspace: PathBuf, bot_id: i64) {
    (GONE.lock().unwrap_or_else(|e| e.into_inner())).insert(bot_id);
    let Some((top, lock)) = checkout(workspace).await else {
        return;
    };
    let _held = lock.lock().await;
    let _ = tokio::task::spawn_blocking(move || forget(&top, &refs, bot_id)).await;
}

fn failed(bot_id: i64, turn: i64, error: &str) {
    println!(
        "{}",
        serde_json::json!({"event": "snapshot_failed", "bot_id": bot_id, "turn": turn,
            "error": error})
    );
}

/// Whether an agent works in the checkout `top` now; unknown counts as
/// one does.
async fn busy(client: &agent_client::Client, top: &Path) -> bool {
    let mut after = serde_json::Value::Null;
    loop {
        let Ok(page) = client
            .request(
                "bots",
                serde_json::json!({"active": true, "limit": 256, "after": after}),
            )
            .await
        else {
            return true;
        };
        let inside = |bot: &serde_json::Value| {
            bot["workspace"].as_str().is_some_and(|dir| {
                std::fs::canonicalize(dir)
                    .unwrap_or_else(|_| PathBuf::from(dir))
                    .starts_with(top)
            })
        };
        if page["bots"]
            .as_array()
            .is_some_and(|bots| bots.iter().any(inside))
        {
            return true;
        }
        match &page["next_after"] {
            serde_json::Value::Null => return false,
            next => after = next.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFS: &str = "refs/agent/snapshots/test";

    /// A folder of the test's own, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sonny-snapshot-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    fn repo(name: &str) -> (PathBuf, PathBuf) {
        let dir = scratch(name);
        let top = dir.join("repo");
        std::fs::create_dir(&top).unwrap();
        let top = std::fs::canonicalize(&top).unwrap();
        git(&top, &["init", "-q", "-b", "main"], None, None).unwrap();
        std::fs::write(top.join("kept.txt"), "one\n").unwrap();
        std::fs::write(top.join("gone.txt"), "here\n").unwrap();
        std::fs::write(top.join(".gitignore"), "target/\n").unwrap();
        git(&top, &["add", "-A"], None, None).unwrap();
        git(&top, &["commit", "-q", "-m", "first"], None, None).unwrap();
        (dir, top)
    }

    #[test]
    fn a_fork_gets_the_files_of_the_turn_and_none_of_the_persons_staging() {
        let (dir, top) = repo("fork");
        // The person staged something; the snapshot leaves it staged.
        std::fs::write(top.join("kept.txt"), "two\n").unwrap();
        git(&top, &["add", "kept.txt"], None, None).unwrap();
        std::fs::remove_file(top.join("gone.txt")).unwrap();
        std::fs::write(top.join("new.txt"), "made\n").unwrap();
        std::fs::create_dir(top.join("target")).unwrap();
        std::fs::write(top.join("target/build"), "ignored\n").unwrap();
        let staged = stage(&top.join("target"), &top.with_extension("indexes"))
            .unwrap()
            .unwrap();
        commit(&staged, REFS, 7, 41, 900, false).unwrap();
        let status = git(&top, &["status", "--porcelain"], None, None).unwrap();
        assert!(status.contains("M  kept.txt"), "{status}");
        assert!(status.contains("?? new.txt"), "{status}");
        // Later work is not in the turn's files.
        std::fs::write(top.join("kept.txt"), "three\n").unwrap();
        let staged = stage(&top, &top.with_extension("indexes"))
            .unwrap()
            .unwrap();
        commit(&staged, REFS, 7, 44, 950, true).unwrap();

        let older = find(&top, REFS, 7, Some(41)).unwrap();
        assert_eq!((older.turn, older.checkpoint, older.late), (41, 900, false));
        let newest = find(&top, REFS, 7, None).unwrap();
        assert_eq!(
            (newest.turn, newest.checkpoint, newest.late),
            (44, 950, true)
        );
        assert!(find(&top, REFS, 7, Some(42)).is_err());
        assert!(find(&top, REFS, 8, None).is_err());

        let copy = dir.join("copy");
        restore(&top, &older, &copy, "agent/copy").unwrap();
        let read = |name: &str| std::fs::read_to_string(copy.join(name)).ok();
        assert_eq!(read("kept.txt").as_deref(), Some("two\n"));
        assert_eq!(read("new.txt").as_deref(), Some("made\n"));
        assert_eq!(read("gone.txt"), None);
        assert_eq!(read("target/build"), None);
        // On the commit the folder was on, with the turn's changes not
        // committed.
        let branch = git(&copy, &["branch", "--show-current"], None, None).unwrap();
        assert_eq!(branch, "agent/copy");
        let status = git(&copy, &["status", "--porcelain"], None, None).unwrap();
        assert!(status.contains("M kept.txt"), "{status}");
        assert!(status.contains("D gone.txt"), "{status}");
        assert!(status.contains("?? new.txt"), "{status}");
        let staged = git(&copy, &["diff", "--cached", "--name-only"], None, None).unwrap();
        assert_eq!(staged, "");

        assert_eq!(forget(&top, REFS, 7).unwrap(), 2);
        assert!(find(&top, REFS, 7, None).is_err());
        assert_eq!(forget(&top, REFS, 7).unwrap(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_ignored_since_an_earlier_snapshot_is_left_out() {
        let (dir, top) = repo("ignored");
        std::fs::write(top.join("generated.log"), "one\n").unwrap();
        let staged = stage(&top, &top.with_extension("indexes"))
            .unwrap()
            .unwrap();
        commit(&staged, REFS, 7, 1, 10, false).unwrap();
        let first = find(&top, REFS, 7, Some(1)).unwrap();
        let listed =
            |commit: &str| git(&top, &["ls-tree", "--name-only", commit], None, None).unwrap();
        assert!(listed(&first.commit).contains("generated.log"));
        std::fs::write(top.join(".gitignore"), "target/\ngenerated.log\n").unwrap();
        std::fs::write(top.join("generated.log"), "two\n").unwrap();
        let staged = stage(&top, &top.with_extension("indexes"))
            .unwrap()
            .unwrap();
        commit(&staged, REFS, 7, 2, 20, false).unwrap();
        let second = find(&top, REFS, 7, Some(2)).unwrap();
        assert!(!listed(&second.commit).contains("generated.log"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_filter_of_the_folder_runs_and_files_keep_their_bytes() {
        let (dir, top) = repo("filters");
        let ran = dir.join("ran");
        let script = format!("sh -c 'touch {}; cat'", ran.display());
        for (key, value) in [
            ("filter.mine.clean", script.as_str()),
            ("filter.mine.smudge", script.as_str()),
            ("filter.mine.required", "true"),
            ("filter.my.own.process", script.as_str()),
        ] {
            git(&top, &["config", key, value], None, None).unwrap();
        }
        std::fs::write(
            top.join(".gitattributes"),
            "*.a filter=mine\n*.b filter=my.own\n",
        )
        .unwrap();
        std::fs::write(top.join("x.a"), "raw\n").unwrap();
        std::fs::write(top.join("y.b"), "raw too\n").unwrap();
        let staged = stage(&top, &top.with_extension("indexes"))
            .unwrap()
            .unwrap();
        commit(&staged, REFS, 7, 1, 10, false).unwrap();
        let snapshot = find(&top, REFS, 7, None).unwrap();
        let copy = dir.join("copy");
        restore(&top, &snapshot, &copy, "agent/copy").unwrap();
        assert!(!ran.exists());
        assert_eq!(std::fs::read_to_string(copy.join("x.a")).unwrap(), "raw\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_filter_of_the_fork_s_branch_runs() {
        let (dir, top) = repo("onbranch");
        let ran = dir.join("ran");
        let included = dir.join("included");
        let script = format!("sh -c 'touch {}; cat'", ran.display());
        std::fs::write(
            &included,
            format!("[filter \"branchy\"]\n\tsmudge = \"{script}\"\n\tclean = \"{script}\"\n"),
        )
        .unwrap();
        let key = "includeIf.onbranch:agent/**.path";
        git(
            &top,
            &["config", key, &included.to_string_lossy()],
            None,
            None,
        )
        .unwrap();
        std::fs::write(top.join(".gitattributes"), "*.a filter=branchy\n").unwrap();
        std::fs::write(top.join("x.a"), "raw\n").unwrap();
        git(&top, &["add", "-A"], None, None).unwrap();
        git(&top, &["commit", "-q", "-m", "filtered"], None, None).unwrap();
        let staged = stage(&top, &top.with_extension("indexes"))
            .unwrap()
            .unwrap();
        commit(&staged, REFS, 7, 1, 10, false).unwrap();
        let snapshot = find(&top, REFS, 7, None).unwrap();
        let copy = dir.join("copy");
        restore(&top, &snapshot, &copy, "agent/copy").unwrap();
        assert!(!ran.exists());
        assert_eq!(std::fs::read_to_string(copy.join("x.a")).unwrap(), "raw\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_git_converts_comes_back_as_it_was() {
        let (dir, top) = repo("attributes");
        std::fs::write(top.join(".gitattributes"), "*.a text eol=crlf\n").unwrap();
        std::fs::write(top.join("x.a"), "one\r\ntwo\r\n").unwrap();
        // Older than the index, so git takes its entry as is.
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        (std::fs::File::options()
            .write(true)
            .open(top.join("x.a"))
            .unwrap())
        .set_modified(then)
        .unwrap();
        // Committed by the person's own git, which converts it.
        for args in [&["add", "-A"][..], &["commit", "-q", "-m", "converted"]] {
            let done = Command::new("git").arg("-C").arg(&top).args(args).status();
            assert!(done.unwrap().success());
        }
        std::fs::write(top.join("y.a"), "three\r\n").unwrap();
        let staged = stage(&top, &top.with_extension("indexes"))
            .unwrap()
            .unwrap();
        commit(&staged, REFS, 7, 1, 10, false).unwrap();
        let snapshot = find(&top, REFS, 7, None).unwrap();
        let copy = dir.join("copy");
        restore(&top, &snapshot, &copy, "agent/copy").unwrap();
        assert_eq!(std::fs::read(copy.join("x.a")).unwrap(), b"one\r\ntwo\r\n");
        assert_eq!(std::fs::read(copy.join("y.a")).unwrap(), b"three\r\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_filter_whose_name_is_not_text_is_off_too() {
        let (dir, top) = repo("bytes");
        let ran = dir.join("ran");
        let mut config = std::fs::read(top.join(".git/config")).unwrap();
        config.extend_from_slice(b"[filter \"odd\xffname\"]\n\tclean = \"sh -c 'touch ");
        config.extend_from_slice(ran.as_os_str().as_encoded_bytes());
        config.extend_from_slice(b"; cat'\"\n");
        std::fs::write(top.join(".git/config"), config).unwrap();
        std::fs::write(top.join(".gitattributes"), b"*.a filter=odd\xffname\n").unwrap();
        std::fs::write(top.join("x.a"), "raw\n").unwrap();
        let staged = stage(&top, &top.with_extension("indexes"))
            .unwrap()
            .unwrap();
        commit(&staged, REFS, 7, 1, 10, false).unwrap();
        assert!(!ran.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn files_holding_a_submodule_are_not_restored() {
        let (dir, top) = repo("submodule");
        let (_, other) = repo("submodule-dep");
        let added = Command::new("git")
            .arg("-C")
            .arg(&top)
            .args(["-c", "protocol.file.allow=always", "submodule", "add", "-q"])
            .arg(&other)
            .arg("dep")
            .status()
            .unwrap();
        assert!(added.success());
        let staged = stage(&top, &top.with_extension("indexes"))
            .unwrap()
            .unwrap();
        commit(&staged, REFS, 7, 1, 10, false).unwrap();
        let snapshot = find(&top, REFS, 7, None).unwrap();
        let copy = dir.join("copy");
        let error = restore(&top, &snapshot, &copy, "agent/copy").unwrap_err();
        assert!(error.contains("submodule, dep"), "{error}");
        assert!(!copy.exists());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(other.parent().unwrap());
    }

    #[test]
    fn a_file_marked_as_not_changing_is_kept_as_it_is_now() {
        let (dir, top) = repo("assumed");
        git(
            &top,
            &["update-index", "--assume-unchanged", "kept.txt"],
            None,
            None,
        )
        .unwrap();
        std::fs::write(top.join("kept.txt"), "changed\n").unwrap();
        let staged = stage(&top, &top.with_extension("indexes"))
            .unwrap()
            .unwrap();
        commit(&staged, REFS, 7, 1, 10, false).unwrap();
        let snapshot = find(&top, REFS, 7, None).unwrap();
        let kept = git(
            &top,
            &["show", &format!("{}:kept.txt", snapshot.commit)],
            None,
            None,
        );
        assert_eq!(kept.unwrap(), "changed");
        // The person's own mark stays.
        let listed = git(&top, &["ls-files", "-v", "kept.txt"], None, None).unwrap();
        assert!(listed.starts_with('h'), "{listed}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_index_is_kept_out_of_the_git_folder_and_made_anew() {
        let (dir, top) = repo("index");
        let indexes = dir.join("indexes");
        std::fs::create_dir(&indexes).unwrap();
        // Something already at the index's name is replaced, not written
        // through.
        let outside = dir.join("outside");
        std::fs::write(&outside, "mine\n").unwrap();
        let index = indexes.join(format!("{:016x}.index", fnv(&top)));
        std::os::unix::fs::symlink(&outside, &index).unwrap();
        stage(&top, &indexes).unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "mine\n");
        assert!(!std::fs::symlink_metadata(&index).unwrap().is_symlink());
        assert!(!top.join(".git/agent-snapshot.index").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn work_in_the_checkout_after_a_turn_ends_makes_its_snapshot_late() {
        let dir = scratch("worked");
        let (one, two) = (dir.join("one"), dir.join("two"));
        std::fs::create_dir_all(one.join("sub")).unwrap();
        std::fs::create_dir_all(&two).unwrap();
        let ended = worked(&one);
        assert!(!worked_since(&one, ended));
        worked(&two);
        assert!(!worked_since(&one, ended));
        worked(&one.join("sub"));
        assert!(worked_since(&one, ended));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_hook_of_the_folder_runs() {
        let (dir, top) = repo("hooks");
        let ran = dir.join("ran");
        for hook in [
            "reference-transaction",
            "post-checkout",
            "post-index-change",
        ] {
            let path = top.join(".git/hooks").join(hook);
            std::fs::write(&path, format!("#!/bin/sh\ntouch '{}'\n", ran.display())).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(top.join("new.txt"), "x\n").unwrap();
        let staged = stage(&top, &top.with_extension("indexes"))
            .unwrap()
            .unwrap();
        commit(&staged, REFS, 7, 1, 10, false).unwrap();
        let snapshot = find(&top, REFS, 7, None).unwrap();
        restore(&top, &snapshot, &dir.join("copy"), "agent/copy").unwrap();
        forget(&top, REFS, 7).unwrap();
        assert!(!ran.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn each_store_keeps_its_snapshots_apart() {
        let one = refs(Path::new("/tmp/one/state.sqlite"));
        assert!(one.starts_with("refs/agent/snapshots/"));
        assert_eq!(one, refs(Path::new("/tmp/one/state.sqlite")));
        assert_ne!(one, refs(Path::new("/tmp/two/state.sqlite")));
    }

    #[test]
    fn a_folder_outside_git_has_no_snapshot() {
        assert!(
            stage(&scratch("outside"), &std::env::temp_dir())
                .unwrap()
                .is_none()
        );
    }
}
