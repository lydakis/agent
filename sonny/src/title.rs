//! A task's title: a few words the person knows its work by, beside its
//! name, which stays the handle. `start_task` sets it, written by whoever
//! starts the task, and Sonny notes it beside the store, as it notes the
//! profile a task was started as; the daemon keeps none. Any other agent
//! (a helper made with `spawn_agent`, a `/new` agent) has none and shows
//! its name. A lead takes its project's, which the project file keeps
//! where the person can change it. A title noted before this stays.
use std::path::{Path, PathBuf};

/// The most characters a title takes, a sentence that fits the pane's
/// row beside a name: a task's, as `start_task` takes it, and a project's,
/// as `new_project` asks for it. Sonny draws it as written and changes no
/// letter.
pub const WRITTEN: usize = 30;

/// Where Sonny keeps the title each agent was started with: beside the
/// store, one file per agent, in this folder, found once for each store.
fn folder(store: &Path) -> PathBuf {
    static FOUND: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>>,
    > = std::sync::LazyLock::new(Default::default);
    let mut found = FOUND.lock().unwrap_or_else(|e| e.into_inner());
    (found.entry(store.to_owned()))
        .or_insert_with(|| {
            let store =
                agent_client::socket::canonical_store(store).unwrap_or_else(|_| store.to_owned());
            let mut dir = store.into_os_string();
            dir.push(".titles");
            PathBuf::from(dir)
        })
        .clone()
}

/// The names noted in each folder, read whole only once the folder
/// changed since (its modified time), and as it was when read.
#[derive(Default)]
struct Names {
    at: Option<std::time::SystemTime>,
    names: std::collections::HashSet<std::ffi::OsString>,
}

static NAMES: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<PathBuf, Names>>> =
    std::sync::LazyLock::new(Default::default);

/// Each title read or noted, by its folder, then its agent's name and id.
type Read = std::collections::HashMap<PathBuf, std::collections::HashMap<(String, i64), String>>;
static READ: std::sync::LazyLock<std::sync::Mutex<Read>> =
    std::sync::LazyLock::new(Default::default);

fn changed_at(dir: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(dir).and_then(|meta| meta.modified()).ok()
}

/// What was read or noted of `bot` in `dir`, for every listing after.
fn known(dir: &Path, bot: &str, id: i64, title: &str) {
    let mut read = READ.lock().unwrap_or_else(|e| e.into_inner());
    let read = read.entry(dir.to_owned()).or_default();
    read.insert((bot.to_owned(), id), title.to_owned());
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    (names.entry(dir.to_owned()).or_default().names).insert(bot.into());
}

/// Note `title` as the title of `bot`, the agent of id `id`: its id
/// first, so an agent given the name later takes none of it.
pub fn note(store: &Path, bot: &str, id: i64, title: &str) -> std::io::Result<()> {
    write(&folder(store), bot, id, title)
}

fn write(dir: &Path, bot: &str, id: i64, title: &str) -> std::io::Result<()> {
    write_all(&[(dir.to_owned(), bot.to_owned(), id, title.to_owned())])
}

/// Write each title noted, each folder made and its time read once for
/// the run of notes in it (a window's are all in its store's).
fn write_all(jobs: &[(PathBuf, String, i64, String)]) -> std::io::Result<()> {
    let mut done = Ok(());
    for run in jobs.chunk_by(|a, b| a.0 == b.0) {
        let dir = &run[0].0;
        let made = !dir.is_dir();
        if let Err(error) = std::fs::create_dir_all(dir) {
            done = Err(error);
            continue;
        }
        let before = changed_at(dir);
        for (dir, bot, id, title) in run {
            match std::fs::write(dir.join(bot), format!("{id}\n{}\n", title.trim())) {
                Ok(()) => known(dir, bot, *id, title.trim()),
                Err(error) => done = Err(error),
            }
        }
        let after = changed_at(dir);
        // Its own notes leave the names as read, in a folder they made
        // too: only another's reads them again.
        let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(names) = names.get_mut(dir).filter(|names| match names.at {
            Some(_) => names.at == before,
            None => made,
        }) {
            names.at = after;
        }
    }
    done
}

/// The titles noted beside a store, for a listing of many agents: the
/// folder read again only once it changed, and each title read once for
/// each agent, by its id, since a title is set when its agent starts and
/// never changes.
pub struct Noted {
    dir: PathBuf,
}

impl Noted {
    pub fn read(store: &Path) -> Self {
        let dir = folder(store);
        let at = changed_at(&dir);
        let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
        let names = names.entry(dir.clone()).or_default();
        if at.is_none() || names.at != at {
            names.names = (std::fs::read_dir(&dir).into_iter().flatten())
                .filter_map(|entry| Some(entry.ok()?.file_name()))
                .collect();
            names.at = at;
        }
        Self { dir }
    }

    /// The title noted for `bot`, the agent of id `id`: none where it
    /// was noted for another agent of that name.
    pub fn of(&self, bot: &str, id: Option<i64>) -> Option<String> {
        let id = id?;
        // One read or noted here, its file written yet or not.
        let read = READ.lock().unwrap_or_else(|e| e.into_inner());
        let key = (bot.to_owned(), id);
        if let Some(title) = read.get(&self.dir).and_then(|read| read.get(&key)) {
            return Some(title.clone());
        }
        drop(read);
        let names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
        if !(names.get(&self.dir))
            .is_some_and(|names| names.names.contains(std::ffi::OsStr::new(bot)))
        {
            return None;
        }
        drop(names);
        noted_in(&self.dir, bot, id)
    }
}

/// A `start_task` title as given, or why it is not one: words on one
/// line, [`WRITTEN`] characters at most.
pub fn checked(title: &str) -> Result<String, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("a task starts with a title: a few words the person knows the work by".into());
    }
    if title.contains(['\n', '\r']) {
        return Err("a title is one line".into());
    }
    let count = title.chars().count();
    if count > WRITTEN {
        return Err(format!(
            "a title takes {WRITTEN} characters at most, and this one has {count}"
        ));
    }
    Ok(title.to_owned())
}

/// The title noted for `bot`, the agent of id `id`, read from its file.
pub fn noted(store: &Path, bot: &str, id: i64) -> Option<String> {
    noted_in(&folder(store), bot, id)
}

/// [`noted`] in the folder `dir`: each read once for the process.
fn noted_in(dir: &Path, bot: &str, id: i64) -> Option<String> {
    let key = (bot.to_owned(), id);
    let read = READ.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(title) = read.get(dir).and_then(|read| read.get(&key)) {
        return Some(title.clone());
    }
    drop(read);
    let text = std::fs::read_to_string(dir.join(bot)).ok()?;
    let (noted, title) = text.split_once('\n')?;
    let title = (noted.trim().parse::<i64>().ok()? == id)
        .then(|| title.trim().to_owned())
        .filter(|title| !title.is_empty())?;
    known(dir, bot, id, &title);
    Some(title)
}

/// The title a project's file gives it (`project::read`'s `title`).
pub fn of_project(info: &serde_json::Value) -> Option<String> {
    (info["title"].as_str())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
}

/// Whether `bot` is a project's lead, `tally.lead`, whose title is its
/// project's: not another lead of its folder, which the person started
/// with a message as any agent.
pub fn is_project_lead(bot: &str) -> bool {
    bot.ends_with(".lead") && crate::project::is_lead(bot)
}

/// Agents' titles, for a listing of many: a project's lead its project's,
/// each project file read once; any other agent its noted one.
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
        self.named(name, bot["bot_id"].as_i64(), bot["workspace"].as_str())
    }

    /// The title of the agent `name`, with its `bot_id` and `workspace`.
    pub fn named(
        &mut self,
        name: &str,
        id: Option<i64>,
        workspace: Option<&str>,
    ) -> Option<String> {
        match is_project_lead(name) {
            true => self.project(workspace.unwrap_or("")),
            false => self.noted.of(name, id),
        }
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

/// The columns `title` takes: a byte each when every byte is a printable
/// ASCII character, as nearly every title is, without decoding it.
pub fn width(title: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    match title.bytes().all(|byte| (0x20..0x7f).contains(&byte)) {
        true => title.len(),
        false => title.width(),
    }
}

/// `title` cut to `room` columns at a word, with `…`; where no word fits,
/// cut where the room ends; with room for no more than the `…`, none.
pub fn cut(title: &str, room: usize) -> String {
    if width(title) <= room {
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
        if width(&next) + 1 > room {
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
    fn a_listing_sees_its_own_notes_and_another_window_s() {
        let dir = std::env::temp_dir().join(format!("sonny-titles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        note(&store, "calc.fix", 2, "Fix the sums").unwrap();
        assert_eq!(
            Noted::read(&store).of("calc.fix", Some(2)).as_deref(),
            Some("Fix the sums")
        );
        // Another agent of that name takes none of it.
        assert_eq!(Noted::read(&store).of("calc.fix", Some(9)), None);
        // Another window's note, written straight to the folder.
        std::fs::write(folder(&store).join("calc.docs"), "3\nDocument the sums\n").unwrap();
        assert_eq!(
            Noted::read(&store).of("calc.docs", Some(3)).as_deref(),
            Some("Document the sums")
        );
        assert_eq!(
            noted(&store, "calc.docs", 3).as_deref(),
            Some("Document the sums")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_title_s_width_is_its_columns() {
        assert_eq!(width("Sum the column"), 14);
        // Wide characters take two columns, and anything else what
        // unicode-width counts.
        assert_eq!(width("Sum 合計"), 8);
        let tab = "Sum\tit";
        assert_eq!(width(tab), unicode_width::UnicodeWidthStr::width(tab));
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
    fn a_task_s_title_is_one_line_of_30_characters_at_most() {
        assert_eq!(
            checked("  Round half cents to even ").as_deref(),
            Ok("Round half cents to even")
        );
        assert!(checked("").is_err());
        assert!(checked("Fix the sums\nin the ledger").is_err());
        let long = "Check each README example against what calc prints";
        assert_eq!(
            checked(long),
            Err("a title takes 30 characters at most, and this one has 50".into())
        );
        assert!(checked(&"a".repeat(WRITTEN)).is_ok());
    }

    #[test]
    fn a_project_s_lead_takes_its_project_s_title_and_any_other_agent_its_own() {
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
        // Never its first message, noted before a project's lead took
        // its project's alone.
        note(
            &store,
            "tally.lead",
            1,
            "hey can you look at why the invoices are off",
        )
        .unwrap();
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
        // A title noted before stays, as a /new lead's from before.
        note(&store, "tally.lead.notes", 3, "Draft the release notes").unwrap();
        assert_eq!(
            titles(&bot("tally.lead.notes", 3)).as_deref(),
            Some("Draft the release notes")
        );
        // A project file with none: the lead has none, so its name shows,
        // and not the first message noted.
        std::fs::write(dir.join(agent_client::project::FILE), "name = \"tally\"\n").unwrap();
        assert_eq!(titles(&bot("tally.lead", 1)), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
