//! A task's worktree made again when its agent runs again. Past
//! `[sonny] task_worktrees`, a start removes the least recently active
//! task worktrees and keeps each branch that holds commits not merged; an
//! invite or a message can wake such a task later, whose folder is gone.
//! As the Codex app restores a worktree it cleaned up once its chat is
//! opened again, Sonny makes it again at the same path from its kept
//! branch `agent/NAME`: when the tool host sees a message taken into the
//! task's turn, and before it runs one of Sonny's tools for it, whichever
//! comes first. With the branch gone too, or a folder that was never a
//! task's worktree, Sonny's tools run in the agent's state folder and say
//! its folder is gone.
use std::path::{Path, PathBuf};

use agent_client::Client;
use serde_json::{Value, json};

use crate::task::{Again, Lost};

/// Where one of Sonny's tools runs for an agent.
#[derive(Debug, PartialEq)]
pub enum Folder {
    /// In its folder, there as recorded or made again from its branch.
    There,
    /// Its folder is gone, and not had back.
    Gone(Gone),
}

/// Why a folder is not had back.
#[derive(Debug, PartialEq)]
pub enum Gone {
    /// It was no task's worktree Sonny makes again.
    NotATask,
    /// The task's branch, named, is gone too.
    NoBranch(String),
    /// git could not make it again from the task's branch, named.
    Failed(String),
}

/// The person's home, where `~/.agent/worktrees` is.
#[cfg(not(test))]
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Tests keep their worktrees apart from the person's.
#[cfg(test)]
pub(crate) fn home() -> Option<PathBuf> {
    Some(std::env::temp_dir().join(format!("sonny-restore-home-{}", std::process::id())))
}

/// The task whose worktree in `home`'s `~/.agent/worktrees` holds
/// `workspace`, by the path as written or the worktrees' real path, as
/// the daemon records a workspace. Not a workflow's worker, whose run
/// makes its own again.
fn task_of(home: &Path, workspace: &Path) -> Option<String> {
    let folders = home.join(".agent/worktrees");
    let below = match workspace.strip_prefix(&folders) {
        Ok(below) => below.to_owned(),
        Err(_) => workspace
            .strip_prefix(std::fs::canonicalize(&folders).ok()?)
            .ok()?
            .to_owned(),
    };
    let name = below.components().next()?.as_os_str().to_str()?;
    (!name.starts_with('.') && !crate::workflow::is_worker(name)).then(|| name.to_owned())
}

/// Whether a turn in `workspace` wants its folder made again: a task's
/// worktree that is not there. A look at the path, and at the folder for a
/// task's alone.
pub fn wanted(workspace: &Path) -> bool {
    home().is_some_and(|home| task_of(&home, workspace).is_some()) && !workspace.exists()
}

/// The folder `workspace` that the agent `record`, as the daemon lists
/// it, works in, made again when it is a task's worktree that is gone
/// and its branch is kept: with git in the sandbox unless the agent has
/// none, for the daemon at `store`. A restore, or one that failed, is
/// written to the tool host's log.
pub async fn folder(client: &Client, record: &Value, workspace: &Path, store: &Path) -> Folder {
    // There, and not one a restore is still checking out: looked at after
    // the folder, which a restore makes only once it is under way.
    if workspace.is_dir() && !restoring(workspace) {
        return Folder::There;
    }
    let Some(home) = home() else {
        return Folder::Gone(Gone::NotATask);
    };
    let Some(task) = task_of(&home, workspace) else {
        return Folder::Gone(Gone::NotATask);
    };
    let _under_way = UnderWay::start(&task);
    let branch = format!("agent/{task}");
    let places = above(client, record).await;
    let confine = confined(client, record).await;
    let (name, at, kept) = (task.clone(), workspace.to_owned(), store.to_owned());
    let done = tokio::task::spawn_blocking(move || {
        crate::task::restore_in(&home, &name, &at, &places, confine, &kept)
    })
    .await
    .unwrap_or_else(|e| Err(Lost::Failed(e.to_string())));
    match done {
        Ok(Again::There) => Folder::There,
        Ok(Again::Restored(repository)) => {
            println!(
                "{}",
                json!({"event": "worktree_restored", "task": task, "branch": branch,
                    "repository": repository, "folder": workspace})
            );
            Folder::There
        }
        Err(Lost::NotOurs) => Folder::Gone(Gone::NotATask),
        Err(Lost::NoBranch) => Folder::Gone(Gone::NoBranch(branch)),
        Err(Lost::Failed(error)) => {
            println!(
                "{}",
                json!({"event": "worktree_restore_failed", "task": task, "branch": branch,
                    "folder": workspace, "error": error})
            );
            Folder::Gone(Gone::Failed(branch))
        }
    }
}

/// The tasks whose worktree this host is making again now, each with how
/// many callers make it: a caller that finds its folder there waits for
/// the restore under way to end, as its folder is checked out only then.
static UNDER_WAY: std::sync::Mutex<std::collections::BTreeMap<String, usize>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Whether a restore here is making the worktree `workspace` is in. With
/// none under way, as nearly always, nothing more is read.
fn restoring(workspace: &Path) -> bool {
    let under_way = UNDER_WAY.lock().unwrap_or_else(|e| e.into_inner());
    !under_way.is_empty()
        && home()
            .and_then(|home| task_of(&home, workspace))
            .is_some_and(|task| under_way.contains_key(&task))
}

/// A caller's restore of a task's worktree, under way until dropped.
struct UnderWay(String);

impl UnderWay {
    fn start(task: &str) -> UnderWay {
        let mut under_way = UNDER_WAY.lock().unwrap_or_else(|e| e.into_inner());
        *under_way.entry(task.to_owned()).or_default() += 1;
        UnderWay(task.to_owned())
    }
}

impl Drop for UnderWay {
    fn drop(&mut self) {
        let mut under_way = UNDER_WAY.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = under_way.get_mut(&self.0) {
            *count -= 1;
            if *count == 0 {
                under_way.remove(&self.0);
            }
        }
    }
}

/// [`folder`] for the turn of `bot` that the tool host saw start in
/// `workspace`, from a connection of its own to the daemon at `socket`.
pub async fn on_turn(store: PathBuf, socket: PathBuf, bot: String, workspace: PathBuf) {
    let Ok((client, _events)) = Client::connect(&socket).await else {
        return;
    };
    let page = client
        .request("bots", json!({"name": bot, "limit": 1}))
        .await
        .unwrap_or_default();
    if let Some(record) = page["bots"]
        .get(0)
        .filter(|found| found["name"] == bot.as_str())
    {
        folder(&client, record, &workspace, &store).await;
    }
    client.close().await;
}

/// The folders of the agents above the agent `record`, each made by the
/// next, its lead's last: where its task's branch may be. Those that are
/// there.
async fn above(client: &Client, record: &Value) -> Vec<PathBuf> {
    let mut places = Vec::new();
    let mut by = record["created_by"].as_str().map(str::to_owned);
    // A line is a few deep: as deep as the config lets tasks start helpers.
    for _ in 0..16 {
        let Some(name) = by.take() else {
            break;
        };
        let Ok(page) = client
            .request("bots", json!({"name": name, "limit": 1}))
            .await
        else {
            break;
        };
        let Some(found) = page["bots"]
            .get(0)
            .filter(|found| found["name"] == name.as_str())
        else {
            break;
        };
        let folder = (found["workspace"].as_str())
            .filter(|folder| !folder.is_empty())
            .map(PathBuf::from)
            .filter(|folder| folder.is_dir());
        places.extend(folder);
        by = found["created_by"].as_str().map(str::to_owned);
    }
    places
}

/// Whether Sonny's git runs in the sandbox for the agent `record`, as for
/// a task started by a lead with one: unless the agent runs with none.
/// Confined when it can't be read.
async fn confined(client: &Client, record: &Value) -> bool {
    let asked = json!({"bot": record["name"], "bot_id": record["bot_id"]});
    match client.request("inspect", asked).await {
        Ok(inspected) => inspected["settings"]["sandbox"] != "danger-full-access",
        Err(_) => true,
    }
}

/// What a tool's answer says of a folder that is gone, `why`, when the
/// tool ran in `ran_in` instead: as a field of an answer that is a JSON
/// object, which a script reads, else a line after it.
pub fn told(output: String, workspace: &Path, ran_in: &Path, why: &Gone) -> String {
    let (workspace, ran_in) = (workspace.display(), ran_in.display());
    let gone = match why {
        Gone::NotATask => format!("your folder {workspace} is gone"),
        Gone::NoBranch(branch) => format!(
            "your folder {workspace} is gone: its worktree was removed and its branch \
             {branch} is gone too"
        ),
        Gone::Failed(branch) => format!(
            "your folder {workspace} is gone: its worktree was removed and could not be made \
             again from its branch {branch}"
        ),
    };
    let gone = format!("{gone}, so Sonny ran this in your state folder {ran_in}");
    match serde_json::from_str::<Value>(&output) {
        Ok(Value::Object(mut said)) => {
            said.insert("folder_gone".into(), Value::String(gone));
            Value::Object(said).to_string()
        }
        _ => format!("{output}\n\nNote: {gone}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_a_restore_here_still_checks_out_is_not_taken_as_there() {
        let home = home().unwrap();
        let workspace = home.join(".agent/worktrees/shop.slow/sub");
        assert!(!restoring(&workspace));
        let one = UnderWay::start("shop.slow");
        let two = UnderWay::start("shop.slow");
        assert!(restoring(&workspace));
        assert!(!restoring(&home.join(".agent/worktrees/shop.other")));
        drop(one);
        assert!(restoring(&workspace), "another caller's is still under way");
        drop(two);
        assert!(!restoring(&workspace));
    }
}
