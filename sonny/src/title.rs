//! An agent's title: a few words the person knows its work by, beside its
//! name, which stays the handle. A task's is set by `start_task`; one the
//! person starts with `/new` takes its first message's first line; a lead
//! with none of its own takes its project's, which the project file keeps
//! where the person can change it. The daemon keeps none: Sonny notes each
//! beside the store, as it notes the profile a task was started as.
use std::path::{Path, PathBuf};

/// The most characters a task's title takes.
pub const MOST: usize = 60;

/// Where Sonny keeps the title each agent was started with: beside the
/// store, one file per agent.
fn file(store: &Path, bot: &str) -> PathBuf {
    let store = agent_client::socket::canonical_store(store).unwrap_or_else(|_| store.to_owned());
    let mut dir = store.into_os_string();
    dir.push(".titles");
    PathBuf::from(dir).join(bot)
}

/// Note `title` as the title of `bot`, the agent of id `id`: its id
/// first, so an agent given the name later takes none of it.
pub fn note(store: &Path, bot: &str, id: i64, title: &str) -> std::io::Result<()> {
    let file = file(store, bot);
    std::fs::create_dir_all(file.parent().unwrap_or(Path::new("/")))?;
    std::fs::write(file, format!("{id}\n{}\n", title.trim()))
}

/// The titles noted beside a store, for a listing of many agents: the
/// folder read once, and each title read once for each agent, by its
/// id, since a title is set when its agent starts and never changes.
pub struct Noted {
    dir: PathBuf,
    names: std::collections::HashSet<std::ffi::OsString>,
}

/// Each title read, by its agent's name and id.
static READ: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<(String, i64), String>>,
> = std::sync::LazyLock::new(Default::default);

impl Noted {
    pub fn read(store: &Path) -> Self {
        let dir = file(store, "-")
            .parent()
            .unwrap_or(Path::new("/"))
            .to_owned();
        let names = (std::fs::read_dir(&dir).into_iter().flatten())
            .filter_map(|entry| Some(entry.ok()?.file_name()))
            .collect();
        Self { dir, names }
    }

    /// The title noted for `bot`, the agent of id `id`: none where it
    /// was noted for another agent of that name.
    pub fn of(&self, bot: &str, id: Option<i64>) -> Option<String> {
        let id = id?;
        if !self.names.contains(std::ffi::OsStr::new(bot)) {
            return None;
        }
        let key = (bot.to_owned(), id);
        let mut read = READ.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(title) = read.get(&key) {
            return Some(title.clone());
        }
        let text = std::fs::read_to_string(self.dir.join(bot)).ok()?;
        let (noted, title) = text.split_once('\n')?;
        if noted.trim().parse::<i64>().ok()? != id {
            return None;
        }
        let title = Some(title.trim().to_owned()).filter(|title| !title.is_empty())?;
        read.insert(key, title.clone());
        Some(title)
    }
}

/// A `start_task` title as given, or why it is not one: words on one line,
/// [`MOST`] characters at most.
pub fn checked(title: &str) -> Result<String, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("a task starts with a title: a few words the person knows the work by".into());
    }
    if title.contains(['\n', '\r']) {
        return Err("a title is one line".into());
    }
    let count = title.chars().count();
    if count > MOST {
        return Err(format!(
            "a title takes {MOST} characters at most, and this one has {count}"
        ));
    }
    Ok(title.to_owned())
}

/// The title of an agent the person starts with its first message,
/// `message`: that message's first line with words on it.
pub fn first_line(message: &str) -> String {
    (message.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// The title a project's file gives it (`project::read`'s `title`).
pub fn of_project(info: &serde_json::Value) -> Option<String> {
    (info["title"].as_str())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
}

/// Agents' titles, for a listing of many: each noted one, else, for a
/// lead, its project's, each project file read once.
pub struct Titles {
    noted: Noted,
    projects: std::collections::HashMap<String, Option<String>>,
}

impl Titles {
    pub fn read(store: &Path) -> Self {
        Self {
            noted: Noted::read(store),
            projects: Default::default(),
        }
    }

    /// The title of `bot`, a row of the daemon's `bots`.
    pub fn of(&mut self, bot: &serde_json::Value) -> Option<String> {
        let name = bot["name"].as_str()?;
        if let Some(title) = self.noted.of(name, bot["bot_id"].as_i64()) {
            return Some(title);
        }
        if !crate::project::is_lead(name) {
            return None;
        }
        self.project(bot["workspace"].as_str().unwrap_or(""))
    }

    /// The title of the project in `folder`, as its file gives it.
    pub fn project(&mut self, folder: &str) -> Option<String> {
        (self.projects.entry(folder.to_owned()))
            .or_insert_with_key(|folder| match folder.as_str() {
                "" => None,
                folder => of_project(&crate::project::read(Path::new(folder)).ok()?),
            })
            .clone()
    }
}

/// `title` cut to `room` columns at a word, with `…`; where no word fits,
/// cut where the room ends; with room for no more than the `…`, none.
pub fn cut(title: &str, room: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    if title.width() <= room {
        return title.to_owned();
    }
    if room < 2 {
        return String::new();
    }
    let mut kept = String::new();
    for word in title.split(' ') {
        let next = match kept.is_empty() {
            true => word.to_owned(),
            false => format!("{kept} {word}"),
        };
        if next.width() + 1 > room {
            break;
        }
        kept = next;
    }
    match kept.is_empty() {
        true => crate::transcript::cut(title, room),
        false => format!("{}…", kept.trim_end_matches([',', ';', ':', '·', '-', ' '])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_is_words_on_one_line_of_sixty_characters_at_most() {
        assert_eq!(
            checked("  Round half cents to even ").as_deref(),
            Ok("Round half cents to even")
        );
        assert!(checked("   ").is_err());
        assert!(checked("Round half cents\nto even").is_err());
        let sixty = "a".repeat(MOST);
        assert_eq!(checked(&sixty), Ok(sixty.clone()));
        assert!(checked(&format!("{sixty}b")).unwrap_err().contains("61"));
        // Characters, not bytes.
        assert!(checked(&"é".repeat(MOST)).is_ok());
    }

    #[test]
    fn a_title_is_cut_at_a_word() {
        let title = "Check the README against the code";
        assert_eq!(cut(title, 40), title);
        assert_eq!(cut(title, 20), "Check the README…");
        assert_eq!(cut(title, 6), "Check…");
        // No word fits: cut where the room ends.
        assert_eq!(cut("Internationalization", 8).chars().count(), 8);
        // No room past the `…`: no title, so the row keeps its name whole.
        assert_eq!(cut(title, 1), "");
        assert_eq!(cut(title, 0), "");
    }

    #[test]
    fn an_agent_started_by_the_person_takes_its_first_line() {
        assert_eq!(
            first_line("\n  Tally the invoices  \nby month"),
            "Tally the invoices"
        );
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn a_noted_title_wins_and_a_lead_takes_its_project_s() {
        let root = std::env::temp_dir().join(format!("sonny-title-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (store, dir) = (root.join("state.sqlite"), root.join("tally"));
        std::fs::create_dir_all(dir.join(".agents")).unwrap();
        std::fs::write(
            dir.join(agent_client::project::FILE),
            "name = \"tally\"\ntitle = \"Tally invoices\"\n",
        )
        .unwrap();
        let bot = |name: &str, id: i64| serde_json::json!({"name": name, "bot_id": id, "workspace": dir.to_str().unwrap()});
        let titles = |bot: &serde_json::Value| Titles::read(&store).of(bot);
        assert_eq!(
            titles(&bot("tally.lead", 1)).as_deref(),
            Some("Tally invoices")
        );
        // A task never takes its project's.
        assert_eq!(titles(&bot("tally.halves", 2)), None);
        note(&store, "tally.halves", 2, "Round half cents to even").unwrap();
        assert_eq!(
            titles(&bot("tally.halves", 2)).as_deref(),
            Some("Round half cents to even")
        );
        // Another agent given the name later takes none of it.
        assert_eq!(titles(&bot("tally.halves", 9)), None);
        // A lead the person started with /new keeps its own.
        note(&store, "tally.lead.notes", 3, "Draft the release notes").unwrap();
        assert_eq!(
            titles(&bot("tally.lead.notes", 3)).as_deref(),
            Some("Draft the release notes")
        );
        // A project file with none: the lead has none.
        std::fs::write(dir.join(agent_client::project::FILE), "name = \"tally\"\n").unwrap();
        assert_eq!(titles(&bot("tally.lead", 1)), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
