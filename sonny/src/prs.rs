//! The pull and merge requests each agent's replies named, kept by the
//! window that sees each turn of a project's agent end ([`crate::waiting`]), so `/status` counts
//! them all and shows the latest, with no history read again:
//! `STORE.prs/BOT.json`, a list of `owner/repo#N`, oldest first. Whether
//! one is open or merged is no reply's to say, and Sonny reads no host.
use std::path::{Path, PathBuf};

/// The most a list keeps, the oldest going first.
const MOST: usize = 64;

/// Where the lists are kept: beside the store, one file per agent.
pub fn dir(store: &Path) -> PathBuf {
    let mut dir = store.as_os_str().to_owned();
    dir.push(".prs");
    PathBuf::from(dir)
}

fn file(store: &Path, bot: &str) -> PathBuf {
    dir(store).join(format!("{bot}.json"))
}

/// The pull requests `bot`'s replies named, oldest first.
pub fn read(store: &Path, bot: &str) -> Vec<String> {
    std::fs::read(file(store, bot))
        .ok()
        .and_then(|kept| serde_json::from_slice(&kept).ok())
        .unwrap_or_default()
}

/// Keep the pull requests `reply` names for `bot`: one named again moves
/// to the end, as the latest.
pub fn note(store: &Path, bot: &str, reply: &str) -> Result<(), String> {
    let named = crate::status::pull_requests(reply);
    if named.is_empty() {
        return Ok(());
    }
    let mut kept = read(store, bot);
    let before = kept.clone();
    for pr in named {
        kept.retain(|k| *k != pr);
        kept.push(pr);
    }
    if kept.len() > MOST {
        kept.drain(..kept.len() - MOST);
    }
    if kept == before {
        return Ok(());
    }
    let path = file(store, bot);
    let failed = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(failed)?;
    }
    // Its own, so two writers never write one file together.
    let next = path.with_extension(format!("json.{}.next", std::process::id()));
    let json = serde_json::to_vec(&kept).map_err(|e| e.to_string())?;
    std::fs::write(&next, json).map_err(failed)?;
    std::fs::rename(&next, &path).map_err(failed)
}

/// How `/status` says them: the latest, with how many in all when more.
pub fn said(prs: &[String]) -> String {
    match prs {
        [] => String::new(),
        [one] => one.clone(),
        [.., last] => format!("{} PRs · {last}", prs.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_reply_adds_what_it_names_and_the_latest_comes_last() {
        let store = std::env::temp_dir()
            .join(format!("sonny-prs-{}", std::process::id()))
            .join("store");
        note(&store, "calc.a", "Opened https://github.com/o/r/pull/3.").unwrap();
        note(&store, "calc.a", "No link here.").unwrap();
        note(
            &store,
            "calc.a",
            "https://github.com/o/r/pull/4 and https://github.com/o/r/pull/3 again",
        )
        .unwrap();
        assert_eq!(read(&store, "calc.a"), ["o/r#4", "o/r#3"]);
        assert_eq!(said(&read(&store, "calc.a")), "2 PRs · o/r#3");
        assert_eq!(said(&["o/r#9".to_owned()]), "o/r#9");
        assert_eq!(read(&store, "calc.b"), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
    }
}
