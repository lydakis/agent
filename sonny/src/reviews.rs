//! The code reviews each agent's replies named, kept by the window that
//! sees each turn of a project's agent end ([`crate::waiting`]), so
//! `/status` counts them all and shows the latest, with no history read
//! again: `STORE.reviews/BOT.json`, a list, oldest first. A review is
//! whatever the reply called it, as the copy that read the reply names
//! it: a pull or merge request, a change in Gerrit, a Phabricator
//! revision, an internal CR, in git or not: `o/r#7`, `D1234`, `CR-881`,
//! or a link. Whether one is open or merged is no reply's to say, and
//! Sonny reads no host.
use std::path::{Path, PathBuf};

/// The most a list keeps, the oldest going first.
const MOST: usize = 64;
/// The most of one review's name kept, in characters.
const NAME_CHARS: usize = 80;

/// Where the lists are kept: beside the store, one file per agent.
pub fn dir(store: &Path) -> PathBuf {
    let mut dir = store.as_os_str().to_owned();
    dir.push(".reviews");
    PathBuf::from(dir)
}

fn file(store: &Path, bot: &str) -> PathBuf {
    dir(store).join(format!("{bot}.json"))
}

/// The reviews `bot`'s replies named, oldest first.
pub fn read(store: &Path, bot: &str) -> Vec<String> {
    std::fs::read(file(store, bot))
        .ok()
        .and_then(|kept| serde_json::from_slice(&kept).ok())
        .unwrap_or_default()
}

/// Keep the reviews a reply of `bot`'s `named`: one named again moves to
/// the end, as the latest.
pub fn note(store: &Path, bot: &str, named: &[String]) -> Result<(), String> {
    let named: Vec<String> = (named.iter())
        .map(|name| {
            let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
            name.chars().take(NAME_CHARS).collect()
        })
        .filter(|name: &String| !name.is_empty())
        .collect();
    if named.is_empty() {
        return Ok(());
    }
    let mut kept = read(store, bot);
    let before = kept.clone();
    for review in named {
        kept.retain(|k| *k != review);
        kept.push(review);
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
pub fn said(reviews: &[String]) -> String {
    match reviews {
        [] => String::new(),
        [one] => one.clone(),
        [.., last] => format!("{} reviews · {last}", reviews.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_reply_adds_what_it_names_and_the_latest_comes_last() {
        let store = std::env::temp_dir()
            .join(format!("sonny-reviews-{}", std::process::id()))
            .join("store");
        let named = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        note(&store, "calc.a", &named(&["o/r#3"])).unwrap();
        note(&store, "calc.a", &[]).unwrap();
        note(&store, "calc.a", &named(&["CR-881", " ", "o/r#3"])).unwrap();
        assert_eq!(read(&store, "calc.a"), ["CR-881", "o/r#3"]);
        assert_eq!(said(&read(&store, "calc.a")), "2 reviews · o/r#3");
        assert_eq!(said(&["D1234".to_owned()]), "D1234");
        assert_eq!(read(&store, "calc.b"), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
    }
}
