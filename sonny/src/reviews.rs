//! The code reviews each agent's turns named, kept by the window that
//! sees each turn of a project's agent end ([`crate::waiting`]), so
//! `/status` counts them all and shows the latest, with no history read
//! again: `STORE.reviews/BOT.json`, the bot's id and each review with
//! the turn that last named it, oldest first, so a check that ends late
//! files its reviews in their turn's place, and a bot made again under a
//! deleted one's name starts with none. A review is
//! whatever the turn called it, as the copy that read the turn names it,
//! from a command's output or the reply: a pull or merge request, a change in Gerrit, a Phabricator
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

/// One review and the turn that last named it.
type Named = (String, i64);

fn kept(store: &Path, bot: &str, bot_id: i64) -> Vec<Named> {
    std::fs::read(file(store, bot))
        .ok()
        .and_then(|kept| serde_json::from_slice::<serde_json::Value>(&kept).ok())
        .filter(|kept| kept["bot_id"].as_i64() == Some(bot_id))
        .and_then(|kept| serde_json::from_value(kept["reviews"].clone()).ok())
        .unwrap_or_default()
}

/// The reviews `bot`'s replies named, by the turn that last named each,
/// oldest first, while `bot_id` is the bot they were kept for.
pub fn read(store: &Path, bot: &str, bot_id: i64) -> Vec<String> {
    kept(store, bot, bot_id)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Keep the reviews `bot`'s turn `turn`, as `bot_id`, `named`: each in
/// the place of the latest turn that named it.
pub fn note(
    store: &Path,
    bot: &str,
    bot_id: i64,
    turn: i64,
    named: &[String],
) -> Result<(), String> {
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
    crate::waiting::locked(&dir(store), || {
        let mut kept = kept(store, bot, bot_id);
        let before = kept.clone();
        for name in named {
            let turn = (kept.iter())
                .filter(|(kept, _)| *kept == name)
                .map(|&(_, turn)| turn)
                .fold(turn, i64::max);
            kept.retain(|(kept, _)| *kept != name);
            // After every review of its turn or one before it.
            let at = kept.partition_point(|&(_, kept)| kept <= turn);
            kept.insert(at, (name, turn));
        }
        if kept.len() > MOST {
            kept.drain(..kept.len() - MOST);
        }
        if kept == before {
            return Ok(());
        }
        let json = serde_json::json!({"bot_id": bot_id, "reviews": kept});
        crate::waiting::replace(&file(store, bot), json.to_string().as_bytes())
    })
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
        note(&store, "calc.a", 1, 1, &named(&["o/r#3"])).unwrap();
        note(&store, "calc.a", 1, 2, &[]).unwrap();
        note(&store, "calc.a", 1, 3, &named(&["CR-881", " ", "o/r#3"])).unwrap();
        assert_eq!(read(&store, "calc.a", 1), ["CR-881", "o/r#3"]);
        assert_eq!(said(&read(&store, "calc.a", 1)), "2 reviews · o/r#3");
        // A check of an older turn that ends late files its reviews in
        // that turn's place, and moves none named since.
        note(&store, "calc.a", 1, 2, &named(&["D7", "o/r#3"])).unwrap();
        assert_eq!(read(&store, "calc.a", 1), ["D7", "CR-881", "o/r#3"]);
        // A bot made again under the name starts with none.
        assert_eq!(read(&store, "calc.a", 2), Vec::<String>::new());
        note(&store, "calc.a", 2, 1, &named(&["D1234"])).unwrap();
        assert_eq!(read(&store, "calc.a", 2), ["D1234"]);
        assert_eq!(said(&["D1234".to_owned()]), "D1234");
        assert_eq!(read(&store, "calc.b", 1), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
    }
}
