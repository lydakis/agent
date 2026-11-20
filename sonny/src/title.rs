//! An agent's title: a few words the person knows its work by, beside its
//! name, which stays the handle. Every agent's is its first message's first
//! line, as Codex names a thread from its first message: a task's brief or
//! a `/new` agent's message noted as it starts, any other (a helper, a task
//! started before titles) read once from its history and noted. A lead with
//! none of its own takes its project's, which the project file keeps where
//! the person can change it. The daemon keeps none: Sonny notes each beside
//! the store, as it notes the profile a task was started as.
use std::path::{Path, PathBuf};

/// The most characters a title takes: what Sonny keeps of a first line
/// no one wrote as a title, cut at a word, changing no letter of it.
pub const MOST: usize = 60;

/// The most characters a title written for the person takes, a task's by
/// its lead and a project's: what the pane's row holds beside a name. The
/// tools that take one say so; Sonny keeps [`MOST`] of what was written.
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

/// [`note`], written by a thread of its own: known to this process at
/// once, and on disk after, so a backfill of many waits on no file.
pub fn note_later(store: &Path, bot: &str, id: i64, title: &str) {
    type Job = (PathBuf, String, i64, String);
    static WRITER: std::sync::LazyLock<std::sync::Mutex<Option<std::sync::mpsc::Sender<Job>>>> =
        std::sync::LazyLock::new(|| {
            let (send, take) = std::sync::mpsc::channel::<Job>();
            let spawned = std::thread::Builder::new()
                .name("titles".into())
                .spawn(move || {
                    // Behind the daemon and the loop on a busy machine: what
                    // it writes is known to the window already. On Linux a
                    // thread has a niceness of its own.
                    #[cfg(target_os = "linux")]
                    // SAFETY: setpriority changes only this thread's niceness.
                    unsafe {
                        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
                    }
                    // Written once the notes stop coming for a moment, all
                    // together: while /status fills, the disk is left to the
                    // daemon, and the folder is made and its time read once.
                    while let Ok(job) = take.recv() {
                        let mut jobs = vec![job];
                        while let Ok(job) = take.recv_timeout(std::time::Duration::from_secs(1)) {
                            jobs.push(job);
                        }
                        let _ = write_all(&jobs);
                    }
                });
            std::sync::Mutex::new(spawned.ok().map(|_| send))
        });
    let dir = folder(store);
    known(&dir, bot, id, title.trim());
    let job = (dir, bot.to_owned(), id, title.to_owned());
    let writer = WRITER.lock().unwrap_or_else(|e| e.into_inner());
    let unsent = match writer.as_ref() {
        Some(send) => send.send(job).err().map(|unsent| unsent.0),
        None => Some(job),
    };
    drop(writer);
    // No thread to write it: written here.
    if let Some((dir, bot, id, title)) = unsent {
        let _ = write(&dir, &bot, id, &title);
    }
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

/// The title of an agent whose first message is `message`: that
/// message's first line with words on it, cut at a word to [`MOST`].
pub fn of_message(message: &str) -> String {
    let line = (message.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    cut(line, MOST)
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

/// The agents, by name and id, whose first line was asked for, each
/// with whether its read ended: each is read once, whichever of a
/// window's reads gets to it first, and the others wait for that one.
static ASKED: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<(String, i64), bool>>,
> = std::sync::LazyLock::new(Default::default);

/// Woken as each read of a first line ends.
static READ_ENDED: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);

/// Whether `bot`'s first line was asked for already.
pub fn asked(bot: &str, id: i64) -> bool {
    let asked = ASKED.lock().unwrap_or_else(|e| e.into_inner());
    asked.contains_key(&(bot.to_owned(), id))
}

/// Take `bot`'s first line to read: true for the read that takes it,
/// false once another read of it ended, which this one waits for.
pub async fn ask(bot: &str, id: i64) -> bool {
    let key = (bot.to_owned(), id);
    loop {
        let ended = READ_ENDED.notified();
        tokio::pin!(ended);
        ended.as_mut().enable();
        {
            let mut asked = ASKED.lock().unwrap_or_else(|e| e.into_inner());
            match asked.get(&key) {
                None => {
                    asked.insert(key, false);
                    return true;
                }
                Some(true) => return false,
                Some(false) => {}
            }
        }
        ended.await;
    }
}

/// `bot`'s first line was read, and noted where it has words; or, where
/// the read failed, the next read asks again.
pub fn answered(bot: &str, id: i64, read: bool) {
    let mut asked = ASKED.lock().unwrap_or_else(|e| e.into_inner());
    match read {
        true => asked.insert((bot.to_owned(), id), true),
        false => asked.remove(&(bot.to_owned(), id)),
    };
    drop(asked);
    READ_ENDED.notify_waiters();
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
        self.named(name, bot["bot_id"].as_i64(), bot["workspace"].as_str())
    }

    /// The title of the agent `name`, with its `bot_id` and `workspace`.
    pub fn named(
        &mut self,
        name: &str,
        id: Option<i64>,
        workspace: Option<&str>,
    ) -> Option<String> {
        if let Some(title) = self.noted.of(name, id) {
            return Some(title);
        }
        if !crate::project::is_lead(name) {
            return None;
        }
        self.project(workspace.unwrap_or(""))
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
    fn a_first_line_noted_shows_before_its_file_is_written() {
        let dir = std::env::temp_dir().join(format!("sonny-later-{}", std::process::id()));
        let store = dir.join("state.sqlite");
        note_later(&store, "calc.sum", 4, "Sum the column");
        // No folder yet: the listing reads none, and shows it all the same.
        assert_eq!(
            Noted::read(&store).of("calc.sum", Some(4)).as_deref(),
            Some("Sum the column")
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
    fn an_agent_takes_its_first_message_s_first_line_cut_at_a_word() {
        assert_eq!(
            of_message("\n  Tally the invoices  \nby month"),
            "Tally the invoices"
        );
        assert_eq!(of_message(""), "");
        let long = "Round every half cent to the nearest even cent in the ledger and the invoices";
        let title = of_message(long);
        assert_eq!(
            title,
            "Round every half cent to the nearest even cent in the…"
        );
        assert!(title.chars().count() <= MOST);
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
