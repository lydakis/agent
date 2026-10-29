//! What a folder held after each of an agent's completed turns, kept as a
//! commit beside its repository's history, so a task forked from that
//! turn starts with those files. The lookout takes one as a turn ends
//! `completed`, the only point a fork starts from: the files git does not
//! ignore, tracked or not, added through an index of Sonny's own so the
//! person's staging is never touched, committed on the HEAD of the time
//! (no hook runs) under `refs/agent/snapshots/BOT_ID/TURN`. The commit
//! names the turn's checkpoint, and says it is late when an agent in the
//! folder was already working again by the time it was taken, so its files
//! may hold some of that work.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const REFS: &str = "refs/agent/snapshots";
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
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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

/// The top of the git checkout `dir` is in, if it is in one.
pub fn top(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"], None, None)
        .ok()
        .map(PathBuf::from)
}

/// Add the files of the checkout `dir` is in, as they are now, and write
/// them as a tree; `None` outside git.
pub fn stage(dir: &Path) -> Result<Option<Staged>, String> {
    let Some(top) = top(dir) else {
        return Ok(None);
    };
    let gitdir = PathBuf::from(git(&top, &["rev-parse", "--absolute-git-dir"], None, None)?);
    let index = gitdir.join("agent-snapshot.index");
    // From the person's index, what git knows of each file already is
    // kept, so only the files that changed are read again.
    if !index.exists() && gitdir.join("index").exists() {
        std::fs::copy(gitdir.join("index"), &index).map_err(|e| e.to_string())?;
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
    let name = format!("{REFS}/{bot_id}/{turn}");
    git(&staged.top, &["update-ref", &name, &commit], None, None)?;
    Ok(commit)
}

/// The turns of bot `bot_id` with snapshots in the repository `dir` is in.
fn turns(dir: &Path, bot_id: i64) -> Result<Vec<i64>, String> {
    let listed = git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("{REFS}/{bot_id}/"),
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
pub fn find(dir: &Path, bot_id: i64, turn: Option<i64>) -> Result<Snapshot, String> {
    let turns = turns(dir, bot_id)?;
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
    let name = format!("{REFS}/{bot_id}/{turn}");
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
    let target = path.to_string_lossy();
    git(
        here,
        &["worktree", "add", "-q", "-b", branch, &target, base],
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
pub fn forget(dir: &Path, bot_id: i64) -> Result<usize, String> {
    let turns = turns(dir, bot_id)?;
    if turns.is_empty() {
        return Ok(0);
    }
    let input: String = (turns.iter())
        .map(|turn| format!("delete {REFS}/{bot_id}/{turn}\n"))
        .collect();
    git(dir, &["update-ref", "--stdin"], None, Some(&input))?;
    Ok(turns.len())
}

/// The lock each checkout's snapshots take in turn, since they share its
/// index.
static CHECKOUTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<std::sync::Mutex<()>>>>,
> = std::sync::LazyLock::new(Default::default);

/// Snapshot `workspace` as bot `bot_id`'s turn `turn` just completed at
/// `checkpoint`: the lookout's part, beside its stream, so a big
/// checkout never holds it up. What happens is a line of its log.
pub async fn after_turn(
    client: std::sync::Arc<agent_client::Client>,
    workspace: PathBuf,
    bot_id: i64,
    turn: i64,
    checkpoint: i64,
) {
    let staged = tokio::task::spawn_blocking(move || {
        let Some(top) = top(&workspace) else {
            return Ok(None);
        };
        let lock = (CHECKOUTS.lock().unwrap_or_else(|e| e.into_inner()))
            .entry(top.clone())
            .or_default()
            .clone();
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        stage(&top)
    })
    .await;
    let staged = match staged {
        Ok(Ok(Some(staged))) => staged,
        Ok(Ok(None)) | Err(_) => return,
        Ok(Err(error)) => return failed(bot_id, turn, &error),
    };
    let late = busy(&client, &staged.top).await;
    let committed =
        tokio::task::spawn_blocking(move || commit(&staged, bot_id, turn, checkpoint, late)).await;
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
        let staged = stage(&top.join("target")).unwrap().unwrap();
        commit(&staged, 7, 41, 900, false).unwrap();
        let status = git(&top, &["status", "--porcelain"], None, None).unwrap();
        assert!(status.contains("M  kept.txt"), "{status}");
        assert!(status.contains("?? new.txt"), "{status}");
        // Later work is not in the turn's files.
        std::fs::write(top.join("kept.txt"), "three\n").unwrap();
        let staged = stage(&top).unwrap().unwrap();
        commit(&staged, 7, 44, 950, true).unwrap();

        let older = find(&top, 7, Some(41)).unwrap();
        assert_eq!((older.turn, older.checkpoint, older.late), (41, 900, false));
        let newest = find(&top, 7, None).unwrap();
        assert_eq!(
            (newest.turn, newest.checkpoint, newest.late),
            (44, 950, true)
        );
        assert!(find(&top, 7, Some(42)).is_err());
        assert!(find(&top, 8, None).is_err());

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

        assert_eq!(forget(&top, 7).unwrap(), 2);
        assert!(find(&top, 7, None).is_err());
        assert_eq!(forget(&top, 7).unwrap(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_folder_outside_git_has_no_snapshot() {
        assert!(stage(&scratch("outside")).unwrap().is_none());
    }
}
